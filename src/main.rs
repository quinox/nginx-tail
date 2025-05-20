use nginx_top::InstantSpeedometer;
use nginx_top::RingbufferSpeedometer;
use nginx_top::SmootherSpeedometer;
use nginx_top::Speedometer;
use smol::io::AsyncReadExt as _;
use smol::io::AsyncSeekExt as _;
use smol::{
    Timer,
    channel::{Receiver, Sender, bounded},
    fs::read_dir,
    stream::StreamExt,
};
use std::io::Write as _;
use std::{fmt::Display, path::PathBuf, time::Duration};

const HELP: &str = r#"
    Usage:
        --log-root <path>  Path to the nginx log directory
        --log-file <file>  Path to the nginx log file
        -h, --help         Show this help message
"#;

#[derive(Debug)]
struct AppArgs {
    log_dirs: Vec<std::path::PathBuf>,
    log_files: Vec<std::path::PathBuf>,
    #[cfg(debug_assertions)]
    fast_generator: bool,
    #[cfg(debug_assertions)]
    slow_generator: bool,
}

fn parse_path(s: &std::ffi::OsStr) -> Result<std::path::PathBuf, &'static str> {
    Ok(s.into())
}

struct Error(String);
impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Error(format!("{value:?}"))
    }
}
impl From<String> for Error {
    fn from(value: String) -> Self {
        Error(value)
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}", self.0))
    }
}

async fn follow(file: PathBuf, channel: Sender<String>) {
    let mut file = smol::fs::File::open(&file).await.unwrap();
    // Seek to the end of the file
    println!("Following file {:?}", file);
    if file.seek(std::io::SeekFrom::End(0)).await.is_err() {
        eprintln!("Error seeking to end of file {:?}", file);
        return;
    }
    let mut pending = vec![];
    let mut readbuf = vec![0; 1024];
    loop {
        match file.read(&mut readbuf).await {
            Ok(0) => {
                // EOF, at least for now
                Timer::after(Duration::from_millis(50)).await;
            }
            Ok(n) => {
                // newline in the data we just read?
                if let Some(first_newline) = readbuf.iter().position(|x| *x == b'\n') {
                    // There was! Let's process the entire block of text we have now
                    let uninterrupted_slice = &[&pending, &readbuf[..n]].concat();

                    let mut starting_pointer = 0;
                    let mut ending_pointer = pending.len() + first_newline;
                    // slightly peculiar loop instead of a more common `if let()`
                    // because we already know we have at least one line to process
                    loop {
                        let line = String::from_utf8_lossy(
                            &uninterrupted_slice[starting_pointer..ending_pointer],
                        )
                        .to_string();
                        if channel.send(line).await.is_err() {
                            // Channel closed
                            return;
                        }
                        starting_pointer = ending_pointer + 1;
                        match uninterrupted_slice[starting_pointer..]
                            .iter()
                            .position(|x| *x == b'\n')
                        {
                            None => break,
                            Some(x) => ending_pointer += x + 1,
                        }
                    }
                    pending.clear();
                    pending.extend_from_slice(&uninterrupted_slice[starting_pointer..]);
                } else {
                    // No newline this time; we'll have to keep the data around for next time
                    pending.extend_from_slice(&readbuf[..n]);
                }
            }
            Err(e) => {
                eprintln!("Error reading file {:?}: {}", file, e);
                return;
            }
        }
    }
}

#[cfg(debug_assertions)]
async fn fake_slow(channel: Sender<String>) {
    loop {
        Timer::after(Duration::from_secs(2)).await;
        match channel.send("Fake slow msg".to_string()).await {
            Ok(_) => {}
            Err(_) => {
                // Channel closed
                return;
            }
        }
    }
}
#[cfg(debug_assertions)]
async fn fake_fast(channel: Sender<String>) {
    loop {
        Timer::after(Duration::from_millis(100)).await;
        for i in 0..100 {
            match channel.send(format!("Fake fast msg {}", i)).await {
                Ok(_) => {}
                Err(_) => {
                    // Channel closed
                    return;
                }
            }
        }
    }
}

// https://en.wikipedia.org/wiki/ANSI_escape_code#CSI_(Control_Sequence_Introducer)_sequences
const CSI: &str = "\x1b[";

async fn process(channel: Receiver<String>) {
    let mut instant_rate = InstantSpeedometer::new();
    let mut fast_rate = RingbufferSpeedometer::new(2 << 1);
    let mut slow_rate = RingbufferSpeedometer::new(2 << 8);
    let mut smooth_rate = SmootherSpeedometer::new(0.2);
    let start = std::time::Instant::now();

    let gutter = 3;
    loop {
        let last_timestamp = std::time::Instant::now();
        Timer::after(Duration::from_secs(1)).await;
        let time_passed = std::time::Instant::now()
            .duration_since(last_timestamp)
            .as_millis();

        //             _______________________ move cursor to beginning of line
        //            |              _________ move cursor up X lines
        //            |             |      ___ clear to end of screen
        //            |             |     |
        print!("{CSI}\r{CSI}{gutter}A{CSI}J");
        let mut count = 0;
        while let Ok(msg) = channel.try_recv() {
            count += 1;
            println!("{}", msg);
        }

        instant_rate.add_measurement(time_passed, count);
        fast_rate.add_measurement(time_passed, count);
        slow_rate.add_measurement(time_passed, count);
        smooth_rate.add_measurement(time_passed, count);

        print!(
            r#"smooth:    {:6.1} msg/s
slow-ring: {:6.1} msg/s
fast-ring: {:6.1} msg/s
instant:   {:6.1} msg/s"#,
            instant_rate.get_speed(),
            fast_rate.get_speed(),
            slow_rate.get_speed(),
            smooth_rate.get_speed()
        );
        std::io::stdout().flush().unwrap();
        if std::time::Instant::now().duration_since(start).as_secs() > 15 {
            println!("\nExiting after 10 seconds");
            break;
        }
    }
}

fn main() {
    let mut pargs = pico_args::Arguments::from_env();
    if pargs.contains(["-h", "--help"]) {
        println!("{}", HELP);
        std::process::exit(0);
    }

    let mut log_dirs = vec![];
    while let Some(log_dir) = pargs
        .opt_value_from_os_str("--log-root", parse_path)
        .unwrap()
    {
        if !log_dir.is_dir() {
            eprintln!("Error: --log-root must be a directory");
            std::process::exit(1);
        }
        log_dirs.push(log_dir);
    }
    if log_dirs.is_empty() {
        log_dirs.push("/var/log/nginx/".into());
    }

    let mut log_files = vec![];
    while let Some(log_file) = pargs
        .opt_value_from_os_str("--log-file", parse_path)
        .unwrap()
    {
        log_files.push(log_file);
    }

    let args = AppArgs {
        fast_generator: pargs.contains("--fast"),
        slow_generator: pargs.contains("--slow"),
        log_dirs,
        log_files,
    };

    let remaining = pargs.finish();
    if !remaining.is_empty() {
        eprintln!("{}", HELP);
        eprintln!("Warning: unused arguments left: {:?}.", remaining);
        std::process::exit(2);
    }

    match smol::block_on(innermain(args)) {
        Ok(_) => {}
        Err(ex) => eprintln!("Runtime failure: {}", ex),
    }
}

async fn innermain(args: AppArgs) -> Result<(), Error> {
    let (sender, receiver) = bounded(10000);
    let mut senders = 0;

    for log_file in args.log_files {
        if !log_file.is_file() {
            // things can still go wrong (if the file isn't readable or something)
            // but at least we tried our best
            eprintln!("WARNING: Log file {:?} is not a file", log_file);
        } else {
            smol::spawn(follow(log_file, sender.clone())).detach();
            senders += 1;
        }
    }

    let mut dirs_to_check = args.log_dirs;

    #[cfg(debug_assertions)]
    {
        if args.fast_generator {
            println!("Fast generator enabled");
            smol::spawn(fake_fast(sender.clone())).detach();
            senders += 1;
        }
        if args.slow_generator {
            println!("Slow generator enabled");
            smol::spawn(fake_slow(sender.clone())).detach();
            senders += 1;
        }
    }

    #[allow(clippy::manual_while_let_some)]
    while !dirs_to_check.is_empty() {
        let dir_to_check = dirs_to_check.pop().unwrap();

        match read_dir(dir_to_check.clone()).await {
            Err(e) => {
                println!(
                    "WARNING: Failed to read directory {:?}: {}",
                    dir_to_check, e
                );
                continue;
            }
            Ok(mut entries) => {
                while let Some(entry) = entries.try_next().await? {
                    let meta = entry.metadata().await?;
                    if meta.is_dir() {
                        dirs_to_check.push(entry.path());
                    } else if meta.is_file()
                        && (entry.file_name() == "access.log" || entry.file_name() == "error.log")
                    {
                        println!("Added {:?} as reader", entry.path());
                        smol::spawn(follow(entry.path(), sender.clone())).detach();
                        senders += 1;
                    }
                }
            }
        }
    }

    if senders == 0 {
        return Err(Error("No useable log files found".to_string()));
    }

    smol::spawn(process(receiver)).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::follow;
    use smol::LocalExecutor;
    use smol::Timer;
    use smol::future;
    use std::time::Duration;
    use std::{fs::File, io::Write};

    #[test]
    fn test_reading_files() {
        let local_ex = LocalExecutor::new();

        future::block_on(local_ex.run(async {
            // TODO: use mktemp -d or something (or in-memory files?)
            let mut file = File::create("/tmp/access.log").unwrap();
            file.write_all(b"line 1\n").unwrap();
            file.write_all(b"line 2\n").unwrap();

            let (sender, receiver) = smol::channel::bounded(10000);
            smol::spawn(follow("/tmp/access.log".into(), sender)).detach();

            // No data written yet
            Timer::after(Duration::from_millis(70)).await;
            let received = receiver.try_recv();
            assert!(
                received.is_err(),
                "after line 2: didn't expect any messages yet, got {:?}",
                received
            );

            // One whole line written
            file.write_all(b"line 3\n").unwrap();
            Timer::after(Duration::from_millis(70)).await;
            assert_eq!(receiver.try_recv().unwrap(), "line 3");

            // One line written in 2 separate parts
            // First bit...
            file.write_all(b"line 4...").unwrap();
            Timer::after(Duration::from_millis(70)).await;
            let received = receiver.try_recv();
            assert!(
                received.is_err(),
                "after line 4...: didn't expect any messages yet, got {:?}",
                received
            );

            // ..and the last bit
            file.write_all(b" and a bit\n").unwrap();
            Timer::after(Duration::from_millis(70)).await;
            assert_eq!(receiver.try_recv().unwrap(), "line 4... and a bit");

            // Two lines written at once
            file.write_all(b"line 5\nline 6\n").unwrap();
            Timer::after(Duration::from_millis(70)).await;
            assert_eq!(receiver.try_recv().unwrap(), "line 5");
            assert_eq!(receiver.try_recv().unwrap(), "line 6");

            // Three and a half lines at once
            file.write_all(b"line 7\nline 8\nline 9\nline 0").unwrap();
            Timer::after(Duration::from_millis(70)).await;
            assert_eq!(receiver.try_recv().unwrap(), "line 7");
            assert_eq!(receiver.try_recv().unwrap(), "line 8");
            assert_eq!(receiver.try_recv().unwrap(), "line 9");
            assert!(receiver.try_recv().is_err());
        }));
    }
}
