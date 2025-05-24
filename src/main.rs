use nginx_top::InstantSpeedometer;
use nginx_top::RingbufferSpeedometer;
use nginx_top::SmootherSpeedometer;
use nginx_top::Speedometer;
use smol::LocalExecutor;
use smol::channel::SendError;
use smol::fs::File;
use smol::future;
use smol::io::AsyncReadExt as _;
use smol::io::AsyncSeekExt as _;
use smol::{
    Timer,
    channel::{Receiver, Sender, bounded},
    fs::read_dir,
    stream::StreamExt,
};
use std::collections::VecDeque;
use std::io::Write as _;
use std::str::FromStr;
use std::vec;
use std::{fmt::Display, path::PathBuf, time::Duration};

const HELP: &str = r#"
    Usage:
        --log-root <path>                    Path to the nginx log directory
        --log-file <file>                    Path to the nginx log file
        --tagger [ filename | status_code ]  How to group the stats
        -h, --help                           Show this help message
"#;

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
impl From<SendError<Message>> for Error {
    fn from(value: SendError<Message>) -> Self {
        Error(format!("{value:?}"))
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("[{}] {}", env!("CARGO_PKG_NAME"), self.0))
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
        // TODO: the only-look-for-newlines-in-readbuf might be too optimized
        // for no gain but more complex code. We'll have to do some testing but
        // perhaps we can simply copy all read data at the end of pending and
        // then check for newlines: easier and just as fast?
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
    if file.seek(std::io::SeekFrom::End(0)).await.is_err() {
        eprintln!("Error seeking to end of file {file:?}");
        return;
    }
    channel
        .send(Message::RegisterTag(filename.clone()))
        .await
        .unwrap();
    let mut processor = LineReader::new(&mut file);
    loop {
        match processor.read_lines().await {
            Ok(lines) => {
                if lines.is_empty() {
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
                if lines.is_empty() {
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

/// Message to be sent to the processing thread
#[derive(Debug, PartialEq)]
enum Message {
    Print,               // trigger a print
    RegisterTag(String), // optional; can be used when you know upfront what the tags are
    Line { text: String, tag: String },
}
async fn periodic_print(channel: SenderChannel) -> Result<(), Error> {
    // update GUI right away
    channel.send(Message::Print).await?;
    Timer::after(Duration::from_millis(100)).await;
    channel.send(Message::Print).await?;
    Timer::after(Duration::from_millis(200)).await;
    channel.send(Message::Print).await?;
    Timer::after(Duration::from_millis(300)).await;
    loop {
        channel.send(Message::Print).await?;
        Timer::after(Duration::from_millis(1000)).await;
    }
}

#[cfg(debug_assertions)]
async fn fake_slow(channel: SenderChannel) {
    let mut counter: u32 = 0;
    loop {
        Timer::after(Duration::from_secs(2)).await;
        match channel
            .send(Message::Line {
                text: format!("Fake slow msg {}", counter),
                tag: "slowloris".to_owned(),
            })
            .await
        {
            Ok(_) => counter += 1,
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

struct TagStats {
    tag: String,
    start: std::time::Instant,
    pending: u32,                // pending since start
    instant: InstantSpeedometer, // processed before start
    slow: RingbufferSpeedometer, // processed before start
    smooth: SmootherSpeedometer, // processed before start
}
impl TagStats {
    fn new(tag: String) -> Self {
        Self {
            tag,
            start: std::time::Instant::now(),
            pending: 0,
            instant: InstantSpeedometer::new(),
            slow: RingbufferSpeedometer::new(2 << 8),
            smooth: SmootherSpeedometer::new(0.2),
        }
    }
    /// Update the speedometers
    fn process(&mut self) {
        let elapsed = self.start.elapsed().as_millis() as u32;
        if elapsed == 0 {
            return;
        }
        self.slow.add_measurement(elapsed, self.pending);
        self.smooth.add_measurement(elapsed, self.pending);
        self.instant.add_measurement(elapsed, self.pending);
        self.start = std::time::Instant::now();
        self.pending = 0;
    }
}

struct TagMap(Vec<TagStats>);
impl TagMap {
    fn new() -> Self {
        Self(vec![])
    }
    fn get_or_create(&mut self, tag: String) -> &mut TagStats {
        // we only max ~5 tags so looping is faster than a hashmap
        if let Some(index) = self.0.iter().position(|x| x.tag == tag) {
            &mut self.0[index]
        } else {
            self.0.push(TagStats::new(tag));
            self.0.last_mut().unwrap()
        }
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn iter(&mut self) -> impl Iterator<Item = &TagStats> {
        self.0.iter()
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = &mut TagStats> {
        self.0.iter_mut()
    }
}

async fn process(channel: Receiver<Message>, screenheight: u16) {
    let start = std::time::Instant::now();
    let mut pending_lines: VecDeque<String> = VecDeque::with_capacity(screenheight as usize);
    let mut lines_skipped: u32 = 0;
    let mut tags = TagMap::new();
    let mut last_gutter_count: u32 = 0;

    loop {
        let number_of_lines = screenheight - tags.len() as u16 - 2; // we'll try to show the last output line of last time at the top
        match channel.recv().await {
            Err(_) => {
                eprintln!("Channel closed.");
                return;
            }
            Ok(Message::RegisterTag(tag)) => {
                let _ = tags.get_or_create(tag);
            }
            Ok(Message::Line { text, tag }) => {
                if pending_lines.len() >= number_of_lines.into() {
                    pending_lines.pop_front();
                    lines_skipped += 1;
                };
                pending_lines.push_back(text);

                // we only expect up to 5 tags so is prolly faster than a hashmap
                let tagmap = tags.get_or_create(tag.clone());
                tagmap.pending += 1;
            }
            Ok(Message::Print) => {
                // making space so we can scroll up later
                print!("{}", "\n".repeat(tags.len() - last_gutter_count as usize));
                if tags.is_empty() {
                    // using CSI<n>A with n = 0 still moves the cursor up
                    print!("\r{CSI}J");
                } else {
                    //             _______________________ move cursor to beginning of line
                    //            |       ________________ move cursor up X lines
                    //            |      |       ________ clear to end of screen
                    //            |      |      |
                    print!("{CSI}\r{CSI}{}A{CSI}J", tags.len());
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
                pending_lines.clear();
                lines_skipped = 0;

                let mut toflush = format!("-- Output sampled at {samplerate}%");
                let maxtagname = tags.iter().map(|x| x.tag.len()).max().unwrap_or(0); // if we have no tags we don't care about the answer
                for tagmap in tags.iter_mut() {
                    tagmap.process();
                    let padded_tag =
                        tagmap.tag.clone() + &" ".repeat(maxtagname - tagmap.tag.len());
                    toflush += &format!(
                        "\n-- {padded_tag} {:7.1} {:7.1} {:7.1} req/s",
                        tagmap.instant.get_speed(),
                        tagmap.slow.get_speed(),
                        tagmap.smooth.get_speed()
                    );
                }
                print!("{}", toflush);
                std::io::stdout().flush().unwrap();
                if std::time::Instant::now().duration_since(start).as_secs() > 15 {
                    println!("\nExiting after 10 seconds");
                    break;
                }
                last_gutter_count = tags.len() as u32;
            }
        }
    }
}

const TAGGER_AUTO: &str = "auto";
const TAGGER_FILENAME: &str = "filename";
const TAGGER_STATUSCODE: &str = "statuscode";
const TAGGER_STATUSCODE2: &str = "status_code";

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
            TAGGER_AUTO => Ok(Tagger::Auto),
            TAGGER_FILENAME => Ok(Tagger::ByFilename),
            TAGGER_STATUSCODE => Ok(Tagger::ByStatusCode),
            TAGGER_STATUSCODE2 => Ok(Tagger::ByStatusCode),
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

    let number_of_lines: u16 = pargs.value_from_str("--screenheight").unwrap_or_else(|_| {
        use rustix::termios::tcgetwinsize;
        match tcgetwinsize(std::io::stderr()) {
            Ok(x) => x.ws_row,
            _ => 3,
        }
    });

    let tagger: Tagger = pargs.value_from_str("--tagger").unwrap_or(Tagger::Auto);

    if let Ok(unknown) = pargs.value_from_str::<&str, String>("--tagger") {
        eprintln!(
            "Unknown tagger {}. Valid choices: {} {} {}",
            unknown, TAGGER_AUTO, TAGGER_FILENAME, TAGGER_STATUSCODE
        );
        std::process::exit(2);
    }

    let args = AppArgs {
        #[cfg(debug_assertions)]
        fast_generator: pargs.contains("--fast"),
        #[cfg(debug_assertions)]
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
        Err(ex) => eprintln!("{ex}"),
    }
}

async fn innermain(args: AppArgs) -> Result<(), Error> {
    let mut senders = 0;
    let (sender, receiver) = bounded(1_000_000);

    let async_exec = LocalExecutor::new();

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
                    log_file.to_string_lossy().to_string(),
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
            async_exec.spawn(fake_fast(sender.clone())).detach();
            senders += 1;
        }
        if args.slow_generator {
            println!("Slow generator enabled");
            async_exec.spawn(fake_slow(sender.clone())).detach();
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
                        async_exec
                            .spawn(follow_tag_filename(
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

    async_exec.spawn(periodic_print(sender.clone())).detach();

    future::block_on(async_exec.run(process(receiver, args.number_of_lines)));

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::Message;
    use crate::follow_tag_filename;
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
            smol::spawn(follow_tag_filename(
                sender,
                "/tmp/access.log".into(),
                "/tmp/access.log".to_owned(),
            ))
            .detach();

            // No data written yet
            Timer::after(Duration::from_millis(70)).await;
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::RegisterTag("/tmp/access.log".to_owned())
            );

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
                Message::Line {
                    text: "line 3".to_owned(),
                    tag: "/tmp/access.log".to_owned()
                },
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
                Message::Line {
                    text: "line 4... and a bit".to_owned(),
                    tag: "/tmp/access.log".to_owned()
                },
            );

            // Two lines written at once
            file.write_all(b"line 5\nline 6\n").unwrap();
            Timer::after(Duration::from_millis(70)).await;
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line {
                    text: "line 5".to_owned(),
                    tag: "/tmp/access.log".to_owned()
                },
            );
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line {
                    text: "line 6".to_owned(),
                    tag: "/tmp/access.log".to_owned()
                }
            );

            // Three and a half lines at once
            file.write_all(b"line 7\nline 8\nline 9\nline 0").unwrap();
            Timer::after(Duration::from_millis(70)).await;
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line {
                    text: "line 7".to_owned(),
                    tag: "/tmp/access.log".to_owned()
                },
            );
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line {
                    text: "line 8".to_owned(),
                    tag: "/tmp/access.log".to_owned()
                },
            );
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line {
                    text: "line 9".to_owned(),
                    tag: "/tmp/access.log".to_owned()
                },
            );
            assert!(receiver.try_recv().is_err());
        }));
    }
}
