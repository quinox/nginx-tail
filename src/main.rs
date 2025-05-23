use nginx_top::InstantSpeedometer;
use nginx_top::RingbufferSpeedometer;
use nginx_top::SmootherSpeedometer;
use nginx_top::Speedometer;
use smol::fs::File;
use smol::io::AsyncReadExt as _;
use smol::io::AsyncSeekExt as _;
use smol::{
    Timer,
    channel::{Receiver, Sender, bounded},
    fs::read_dir,
    stream::StreamExt,
};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::Write as _;
use std::str::FromStr;
use std::vec;
use std::{fmt::Display, path::PathBuf, time::Duration};

const HELP: &str = r#"
    Usage:
        --log-root <path>  Path to the nginx log directory
        --log-file <file>  Path to the nginx log file
        -x, --speed-test   Test your terminal speed
        -h, --help         Show this help message
"#;

const GUTTER: u16 = 5;

#[derive(Debug)]
struct AppArgs {
    log_dirs: Vec<std::path::PathBuf>,
    log_files: Vec<std::path::PathBuf>,
    #[cfg(debug_assertions)]
    fast_generator: bool,
    #[cfg(debug_assertions)]
    slow_generator: bool,
    number_of_lines: u16,
    tagger: Tagger,
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

type SenderChannel = Sender<Message>;

struct LineReader<'a> {
    file: &'a mut File,
    pending: Vec<u8>,
    readbuf: Vec<u8>,
}

impl<'a> LineReader<'a> {
    fn new(file: &'a mut File) -> Self {
        LineReader {
            file,
            pending: vec![],
            readbuf: vec![0; 1024],
        }
    }
    async fn read_lines(&mut self) -> Result<Vec<String>, ()> {
        match self.file.read(&mut self.readbuf).await {
            Ok(0) => Ok(vec![]),
            Ok(n) => {
                // newline in the data we just read?
                if let Some(first_newline) = self.readbuf.iter().position(|x| *x == b'\n') {
                    let mut whole_lines = vec![];
                    // There was! Let's process the entire block of text we have now
                    let uninterrupted_slice = &[&self.pending, &self.readbuf[..n]].concat();

                    let mut starting_pointer = 0;
                    let mut ending_pointer = self.pending.len() + first_newline;
                    // slightly peculiar loop instead of a more common `if let()`
                    // because we already know we have at least one line to process
                    loop {
                        whole_lines.push(
                            String::from_utf8_lossy(
                                &uninterrupted_slice[starting_pointer..ending_pointer],
                            )
                            .to_string(),
                        );

                        starting_pointer = ending_pointer + 1;
                        match uninterrupted_slice[starting_pointer..]
                            .iter()
                            .position(|x| *x == b'\n')
                        {
                            None => break,
                            Some(x) => ending_pointer += x + 1,
                        }
                    }
                    self.pending.clear();
                    self.pending
                        .extend_from_slice(&uninterrupted_slice[starting_pointer..]);
                    Ok(whole_lines)
                } else {
                    // No newline this time; we'll have to keep the data around for next time
                    self.pending.extend_from_slice(&self.readbuf[..n]);
                    Ok(vec![])
                }
            }
            Err(_) => Err(()),
        }
    }
}

async fn follow_tag_filename(channel: SenderChannel, file: PathBuf, filename: String) {
    let mut file = smol::fs::File::open(&file).await.unwrap();
    // Seek to the end of the file
    println!("Following file {file:?}");
    if file.seek(std::io::SeekFrom::End(0)).await.is_err() {
        eprintln!("Error seeking to end of file {file:?}");
        return;
    }
    let mut processor = LineReader::new(&mut file);
    loop {
        match processor.read_lines().await {
            Ok(lines) => {
                if lines.len() == 0 {
                    // EOF, at least for now
                    Timer::after(Duration::from_millis(50)).await;
                    continue;
                }
                for line in lines {
                    if channel
                        .send(Message::Line {
                            text: line,
                            tag: filename.clone(),
                        })
                        .await
                        .is_err()
                    {
                        // Channel closed
                        return;
                    }
                }
            }
            Err(_) => {
                eprintln!("File {filename} is no longer readable");
                return;
            }
        }
    }
}

async fn follow_tag_statuscode(channel: SenderChannel, file: PathBuf) {
    let mut file = smol::fs::File::open(&file).await.unwrap();
    // Seek to the end of the file
    println!("Following file {file:?}");
    if file.seek(std::io::SeekFrom::End(0)).await.is_err() {
        eprintln!("Error seeking to end of file {file:?}");
        return;
    }
    let mut processor = LineReader::new(&mut file);
    loop {
        match processor.read_lines().await {
            Ok(lines) => {
                if lines.len() == 0 {
                    // EOF, at least for now
                    Timer::after(Duration::from_millis(50)).await;
                    continue;
                }
                for line in lines {
                    if channel
                        .send(Message::Line {
                            text: line,
                            tag: "418".to_owned(), // 418 I'm a teapot
                        })
                        .await
                        .is_err()
                    {
                        // Channel closed
                        return;
                    }
                }
            }
            Err(_) => {
                eprintln!("File is no longer readable");
                return;
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum Message {
    Print,
    Line { text: String, tag: String },
}
async fn periodic_print(channel: SenderChannel) {
    loop {
        match channel.send(Message::Print).await {
            Ok(_) => {
                Timer::after(Duration::from_millis(1000)).await;
            }
            Err(_) => return,
        }
    }
}

#[cfg(debug_assertions)]
async fn fake_slow(channel: SenderChannel) {
    loop {
        Timer::after(Duration::from_secs(2)).await;
        match channel
            .send(Message::Line {
                text: "Fake slow msg".to_string(),
                tag: "fastfake".to_owned(),
            })
            .await
        {
            Ok(_) => {}
            Err(_) => {
                // Channel closed
                return;
            }
        }
    }
}
#[cfg(debug_assertions)]
async fn fake_fast(channel: SenderChannel) {
    loop {
        Timer::after(Duration::from_millis(100)).await;
        for i in 0..100 {
            match channel
                .send(Message::Line {
                    text: format!("Fake fast msg {i}"),
                    tag: "fakefast".to_owned(),
                })
                .await
            {
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

async fn process(channel: Receiver<Message>, number_of_lines: u16) {
    let mut instant_rate = InstantSpeedometer::new();
    let mut fast_rate = RingbufferSpeedometer::new(2 << 1);
    let mut slow_rate = RingbufferSpeedometer::new(2 << 8);
    let mut smooth_rate = SmootherSpeedometer::new(0.2);
    let start = std::time::Instant::now();

    let mut pending_lines: VecDeque<String> = VecDeque::with_capacity(number_of_lines as usize);
    let mut lines_skipped: u32 = 0;

    // print!("{}", "\n".repeat(GUTTER.into())); // making space so we can scroll up later
    let mut first_loop = true;
    loop {
        match channel.recv().await {
            Err(_) => {
                eprintln!("Channel closed.");
                return;
            }
            Ok(Message::Line { text, tag: _tag }) => {
                if pending_lines.len() >= number_of_lines.into() {
                    pending_lines.pop_front();
                    lines_skipped += 1;
                };
                pending_lines.push_back(text);
            }
            Ok(Message::Print) => {
                let last_timestamp = std::time::Instant::now();
                Timer::after(Duration::from_secs(1)).await;
                let time_passed: u32 = std::time::Instant::now()
                    .duration_since(last_timestamp)
                    .as_millis()
                    .try_into()
                    .unwrap_or_else(|_| {
                        eprintln!("An incredible amount of time passed!");
                        u32::MAX
                    });

                if !first_loop {
                    //             _______________________ move cursor to beginning of line
                    //            |              _________ move cursor up X lines
                    //            |             |      ___ clear to end of screen
                    //            |             |     |
                    print!("{CSI}\r{CSI}{GUTTER}A{CSI}J");
                } else {
                    first_loop = false;
                }

                let samplerate: u32 = match lines_skipped {
                    0 => 100,
                    _ => {
                        (100 * pending_lines.len() as u32)
                            / (lines_skipped + pending_lines.len() as u32)
                    }
                };
                for line in pending_lines.iter() {
                    println!("{line}");
                }
                let count = u32::try_from(pending_lines.len())
                    .expect("line count is impossibly high")
                    + lines_skipped;
                pending_lines.clear();
                lines_skipped = 0;

                instant_rate.add_measurement(time_passed, count);
                fast_rate.add_measurement(time_passed, count);
                slow_rate.add_measurement(time_passed, count);
                smooth_rate.add_measurement(time_passed, count);

                print!(
                    r#"
Output sampled at {}%
smooth:    {:6.1} msg/s
slow-ring: {:6.1} msg/s
fast-ring: {:6.1} msg/s
instant:   {:6.1} msg/s"#,
                    samplerate,
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
    }
}

fn speedtest() {
    let start = std::time::Instant::now();
    let mut lines = 0;
    let nginx_line = "measuring... ".repeat(20) + "\n";
    let nginx_line = nginx_line.as_bytes();
    while std::time::Instant::now().duration_since(start).as_secs() < 5 {
        std::io::stdout().write_all(nginx_line).unwrap();
        std::io::stdout().flush().unwrap();
        lines += 1;
    }
    println!(
        "Your setup managed to output {} lines per second",
        lines / std::time::Instant::now().duration_since(start).as_secs()
    );
}

#[derive(Debug)]
enum Tagger {
    Auto,
    ByFilename,
    ByStatusCode,
}

impl FromStr for Tagger {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(Tagger::Auto),
            "filename" => Ok(Tagger::ByFilename),
            "status" => Ok(Tagger::ByStatusCode),
            "statuscode" => Ok(Tagger::ByStatusCode),
            _ => Err(format!("Unknown tagger {s}")),
        }
    }
}

fn main() {
    let mut pargs = pico_args::Arguments::from_env();
    if pargs.contains(["-h", "--help"]) {
        println!("{HELP}");
        std::process::exit(0);
    }
    if pargs.contains(["-x", "--speed-test"]) {
        speedtest();
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

    let mut log_files = vec![];
    while let Some(log_file) = pargs
        .opt_value_from_os_str("--log-file", parse_path)
        .unwrap()
    {
        log_files.push(log_file);
    }

    if log_files.is_empty() && log_dirs.is_empty() {
        log_dirs.push("/var/log/nginx/".into());
    }

    let number_of_lines: u16 = pargs.value_from_str("--lines").unwrap_or_else(|_| {
        use rustix::termios::tcgetwinsize;
        if let Ok(winsize) = tcgetwinsize(std::io::stderr()) {
            // we subtract 2 lines so that the oldest line
            // on the screen is from the previous run
            if winsize.ws_row > GUTTER + 2 {
                return winsize.ws_row - GUTTER - 2;
            }
        }
        3
    });

    let tagger: Tagger = pargs.value_from_str("--tagger").unwrap_or(Tagger::Auto);

    let args = AppArgs {
        fast_generator: pargs.contains("--fast"),
        slow_generator: pargs.contains("--slow"),
        log_dirs,
        log_files,
        number_of_lines,
        tagger,
    };

    let remaining = pargs.finish();
    if !remaining.is_empty() {
        eprintln!("{HELP}");
        eprintln!("Warning: unused arguments left: {remaining:?}.");
        std::process::exit(2);
    }

    match smol::block_on(innermain(args)) {
        Ok(_) => {}
        Err(ex) => eprintln!("Runtime failure: {ex}"),
    }
}

async fn innermain(args: AppArgs) -> Result<(), Error> {
    let mut senders = 0;
    let (sender, receiver) = bounded(10000);

    for log_file in args.log_files {
        if !log_file.is_file() {
            // things can still go wrong (if the file isn't readable or something)
            // but at least we tried our best
            eprintln!("WARNING: Log file {log_file:?} is not a file");
        } else {
            match args.tagger {
                Tagger::ByFilename => smol::spawn(follow_tag_filename(
                    sender.clone(),
                    log_file.clone(),
                    log_file.file_name().unwrap().to_string_lossy().to_string(),
                ))
                .detach(),
                Tagger::ByStatusCode => {
                    smol::spawn(follow_tag_statuscode(sender.clone(), log_file)).detach()
                }
                _ => todo!(),
            };
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
                println!("WARNING: Failed to read directory {dir_to_check:?}: {e}");
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
                        smol::spawn(follow_tag_filename(
                            sender.clone(),
                            entry.path(),
                            entry.file_name().to_string_lossy().to_string(),
                        ))
                        .detach();
                        senders += 1;
                    }
                }
            }
        }
    }

    if senders == 0 {
        return Err(Error("No useable log files found".to_string()));
    }

    smol::spawn(periodic_print(sender.clone())).detach();

    smol::spawn(process(receiver, args.number_of_lines)).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::Message;
    use crate::Tagger;
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
            smol::spawn(follow(
                sender,
                "/tmp/access.log".into(),
                "/tmp/access.log".to_owned(),
                Tagger::Auto,
            ))
            .detach();

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
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line("line 3".to_owned())
            );

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
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line("line 4... and a bit".to_owned())
            );

            // Two lines written at once
            file.write_all(b"line 5\nline 6\n").unwrap();
            Timer::after(Duration::from_millis(70)).await;
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line("line 5".to_owned())
            );
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line("line 6".to_owned())
            );

            // Three and a half lines at once
            file.write_all(b"line 7\nline 8\nline 9\nline 0").unwrap();
            Timer::after(Duration::from_millis(70)).await;
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line("line 7".to_owned())
            );
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line("line 8".to_owned())
            );
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line("line 9".to_owned())
            );
            assert!(receiver.try_recv().is_err());
        }));
    }
}
