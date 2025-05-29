use nginx_tail::RingbufferSpeedometer;
use nginx_tail::Speedometer;
use smol::LocalExecutor;
use smol::channel::SendError;
use smol::fs::File;
use smol::fs::read_link;
use smol::future;
use smol::io::AsyncReadExt as _;
use smol::io::AsyncSeekExt as _;
use smol::lock::Mutex;
use smol::{
    Timer,
    channel::{Receiver, Sender, bounded},
};
use std::cmp;
use std::collections::VecDeque;
use std::fs::read_dir;
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::process;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::vec;
use std::{fmt::Display, path::PathBuf, time::Duration};

const HELP: &str = r#"
    Usage:
        [ --option | ... ] [ file |  dir | ... ]

    Options:
        -h, --help               Show this help message
            --max-width X        Cut lines to this length X. Defaults to "screen width", set to 0 for unlimited
            --target-height      Target window height. Will be met if the log lines fit in the width of your terminal
            --max-runtime X      Terminate after X seconds
            --combine            Combine stats of all files together
"#;

#[derive(Debug)]
struct AppArgs {
    log_dirs: Vec<std::path::PathBuf>,
    log_files: Vec<std::path::PathBuf>,
    #[cfg(debug_assertions)]
    fast_generator: bool,
    #[cfg(debug_assertions)]
    slow_generator: bool,
    target_height: u16,
    combine_filestats: bool,
    max_runtime: Option<u32>,
    requested_width: Option<u16>,
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

struct LineReader {
    filename: PathBuf,
    fd_path: PathBuf,
    file: File,       // the file handle
    pending: Vec<u8>, // data that was read but not yet processed
    readbuf: Vec<u8>,
}

impl LineReader {
    async fn new(filename: PathBuf) -> Result<Self, String> {
        let (file, fd_path) = Self::_open_file(filename.clone()).await?;
        Ok(LineReader {
            filename,
            fd_path,
            file,
            pending: vec![],
            readbuf: vec![0; 1024],
        })
    }

    async fn _open_file(filename: PathBuf) -> Result<(File, PathBuf), String> {
        // open the file and get the /proc/self/fd/<fd> path
        let mut file = smol::fs::File::open(&filename)
            .await
            .map_err(|e| e.to_string())?;
        if file.seek(std::io::SeekFrom::End(0)).await.is_err() {
            return Err("Error seeking to end of file".into());
        }
        let fd_path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        Ok((file, fd_path))
    }

    async fn read_lines(&mut self) -> Result<Vec<String>, ()> {
        match self.file.read(&mut self.readbuf).await {
            Ok(0) => {
                // Did the file get rotated perhaps?

                // read_link operates on a virtual filesystem so it should be pretty fast
                let current_filename = read_link(self.fd_path.clone())
                    .await
                    .unwrap_or_else(|_| PathBuf::new());
                if current_filename != self.filename {
                    // yes, it did! Let's try to open the new file
                    if let Ok((file, fd_path)) = Self::_open_file(self.filename.clone()).await {
                        self.file = file;
                        self.fd_path = fd_path;
                        self.pending.clear();
                    }
                } else {
                    // no, the file is still the same. Let's wait a bit before trying again
                    Timer::after(Duration::from_millis(50)).await;
                }
                Ok(vec![])
            }
            Ok(n) => {
                self.pending.extend_from_slice(&self.readbuf[..n]);

                let mut whole_lines = vec![];
                let mut start_of_next = 0;
                let newlines: Vec<usize> = self
                    .pending
                    .iter()
                    .enumerate()
                    .filter_map(
                        |(index, char)| {
                            if *char == b'\n' { Some(index) } else { None }
                        },
                    )
                    .collect();
                for newline in newlines {
                    whole_lines.push(
                        String::from_utf8_lossy(&self.pending[start_of_next..newline]).to_string(),
                    );
                    start_of_next = newline + 1;
                    if start_of_next == self.pending.len() {
                        // we consumed _everything_
                        self.pending.clear();
                        return Ok(whole_lines);
                    }
                }
                // there's still a bit of data left to consume
                self.pending.drain(..start_of_next);
                Ok(whole_lines)
            }
            Err(_) => Err(()),
        }
    }
}

async fn follow(channel: SenderChannel, file: PathBuf, group: String) {
    channel
        .send(Message::RegisterGroup(group.clone()))
        .await
        .unwrap();
    let mut processor = match LineReader::new(file).await {
        Ok(x) => x,
        Err(e) => {
            eprintln!("Error opening file: {e}");
            return;
        }
    };
    loop {
        match processor.read_lines().await {
            Ok(lines) => {
                for line in lines {
                    let statuscode = extract_statuscode(&line);
                    if channel
                        .send(Message::Line {
                            text: line,
                            group: group.clone(),
                            statuscode,
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
    Print,                 // trigger a print
    RegisterGroup(String), // optional; can be used when you know upfront what the tags are
    Line {
        text: String,
        group: String,
        statuscode: String,
    },
    WinCh(u16),
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
                statuscode: "slow".to_owned(),
                group: "generator".to_owned(),
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
                    statuscode: "fast".to_owned(),
                    group: "generator".to_owned(),
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

struct StatusStats {
    statuscode: String,
    start: std::time::Instant,
    pending: u32, // pending since start
    ring: RingbufferSpeedometer,
}

impl StatusStats {
    fn new(statuscode: String) -> Self {
        Self {
            statuscode,
            start: std::time::Instant::now(),
            pending: 0,
            ring: RingbufferSpeedometer::new(5),
        }
    }
    fn process(&mut self) {
        let elapsed = self.start.elapsed().as_millis() as u32;
        if elapsed == 0 {
            return;
        }
        self.ring.add_measurement(elapsed, self.pending);
        self.start = std::time::Instant::now();
        self.pending = 0;
    }
}

impl Ord for StatusStats {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        self.statuscode.cmp(&other.statuscode)
    }
}
impl PartialOrd for StatusStats {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for StatusStats {
    fn eq(&self, other: &Self) -> bool {
        self.statuscode == other.statuscode
    }
}
impl Eq for StatusStats {}

struct GroupStats {
    group: String,
    stats: Vec<StatusStats>,
    global_statuscodes: GlobalStatuscodes,
}
impl GroupStats {
    fn new(group: String, global_statuscodes: GlobalStatuscodes) -> Self {
        Self {
            group,
            stats: vec![],
            global_statuscodes,
        }
    }
    async fn get_or_create(&mut self, statuscode: String) -> &mut StatusStats {
        // we only max ~5 tags so looping is faster than a hashmap
        if let Some(index) = self.stats.iter().position(|x| x.statuscode == statuscode) {
            &mut self.stats[index]
        } else {
            let mut globalstate = self.global_statuscodes.lock().await;
            globalstate.push(statuscode.clone());
            globalstate.sort();
            globalstate.dedup();
            self.stats.push(StatusStats::new(statuscode));
            self.stats.sort();
            self.stats.last_mut().unwrap()
        }
    }
    fn process(&mut self) {
        for statusstats in self.stats.iter_mut() {
            statusstats.process();
        }
    }
    fn iter(&mut self) -> impl Iterator<Item = &StatusStats> {
        self.stats.iter()
    }
}

type GlobalStatuscodes = Arc<Mutex<Vec<String>>>;

struct GroupMap {
    stats: Vec<GroupStats>,
    shared_prefix: String,
    shared_suffix: String,
    global_statuscodes: GlobalStatuscodes,
}
impl GroupMap {
    fn new(global_statuscodes: GlobalStatuscodes) -> Self {
        Self {
            stats: vec![],
            shared_prefix: "".to_owned(),
            shared_suffix: "".to_owned(),
            global_statuscodes,
        }
    }
    fn get_or_create(&mut self, tag: String) -> &mut GroupStats {
        // we only expect a max of ~5 groups so looping is faster than a hashmap
        if let Some(index) = self.stats.iter().position(|x| x.group == tag) {
            &mut self.stats[index]
        } else {
            self.stats
                .push(GroupStats::new(tag, self.global_statuscodes.clone()));
            self.update_trimmed_tags();
            self.stats.last_mut().unwrap()
        }
    }

    fn len(&self) -> usize {
        self.stats.len()
    }

    fn is_empty(&self) -> bool {
        self.stats.is_empty()
    }

    fn iter(&mut self) -> impl Iterator<Item = &GroupStats> {
        self.stats.iter()
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = &mut GroupStats> {
        self.stats.iter_mut()
    }

    fn update_trimmed_tags(&mut self) {
        if self.stats.len() < 2 {
            return;
        }

        self.shared_prefix.clear();
        self.shared_suffix.clear();

        let mut max_tag_length = 0;

        // shared strings can never be longer than the any of the tags, so we'll
        // just use the first one to iterate over
        'outer: for i in 0..self.stats[0].group.len() {
            for tag in self.stats.iter() {
                max_tag_length = cmp::max(max_tag_length, tag.group.len()); // while we're here, let's also find the max tag length
                if tag.group.chars().nth(i) != self.stats[0].group.chars().nth(i) {
                    break 'outer;
                }
            }
            self.shared_prefix
                .push(self.stats[0].group.chars().nth(i).unwrap());
        }

        'outer: for i in 0..self.stats[0].group.len() {
            for tag in self.stats.iter() {
                if tag.group.chars().nth_back(i) != self.stats[0].group.chars().nth_back(i) {
                    break 'outer;
                }
            }
            self.shared_suffix
                .insert(0, self.stats[0].group.chars().nth_back(i).unwrap());
        }

        // we don't need to reduce the tags to nothing,
        // there's space on the screen for some text
        let text_left = max_tag_length - self.shared_prefix.len() - self.shared_suffix.len();
        if text_left < 8 {
            if max_tag_length < 8 {
                self.shared_prefix = "".to_owned();
                self.shared_suffix = "".to_owned();
            } else {
                // we do not alter the suffix: it's probably .log which we want to filter out
                let chars_to_preserve = 8 - text_left;
                let cut_from_prefix = cmp::max(0, self.shared_prefix.len() - chars_to_preserve);
                self.shared_prefix = self.shared_prefix[..cut_from_prefix].to_owned();
            }
        }
    }
}

///
/// requested_width:
/// Some(0) = unlimited line length  -- no sigwinch handler installed
/// Some(x) = cut off at x           -- no sigwinch handler installed
/// None    = use screen's width     -- sigwinch handler installed
async fn process(channel: Receiver<Message>, target_height: u16, requested_width: Option<u16>) {
    let mut pending_lines: VecDeque<String> = VecDeque::with_capacity(target_height as usize);
    let mut lines_skipped: u32 = 0;
    // These are unlikely to change often, so we'll track them in memory instead
    // of recomputing them every time
    let global_statuscodes = Arc::new(Mutex::new(vec![]));
    let mut groups = GroupMap::new(global_statuscodes.clone());
    let mut last_gutter_count: u32 = 0;

    let mut cut_width = match requested_width {
        None => get_terminal_width(),
        Some(x) => x,
    };

    loop {
        let number_of_lines = target_height - groups.len() as u16 - 2; // we'll try to show the last output line of last time at the top
        match channel.recv().await {
            Err(_) => {
                eprintln!("Channel closed.");
                return;
            }
            Ok(Message::WinCh(new_terminal_width)) => {
                // we only connect the sigwinch handler when the user did not specify a width,
                // so every WinCh signal we see meant we have to change our width
                cut_width = new_terminal_width;
            }
            Ok(Message::RegisterGroup(tag)) => {
                let _ = groups.get_or_create(tag);
            }
            Ok(Message::Line {
                text,
                group,
                statuscode,
            }) => {
                if pending_lines.len() >= number_of_lines.into() {
                    pending_lines.pop_front();
                    lines_skipped += 1;
                };
                pending_lines.push_back(text);

                // we only expect up to 5 tags so is prolly faster than a hashmap
                let groupstats = groups.get_or_create(group.clone());
                let statusstats = groupstats.get_or_create(statuscode.clone()).await;
                statusstats.pending += 1;
            }
            Ok(Message::Print) => {
                // making space so we can scroll up later
                print!("{}", "\n".repeat(groups.len() - last_gutter_count as usize));
                if groups.is_empty() {
                    // using CSI<n>A with n = 0 still moves the cursor up
                    print!("\r{CSI}J");
                } else {
                    //             _______________________ move cursor to beginning of line
                    //            |       ________________ move cursor up X lines
                    //            |      |       ________ clear to end of screen
                    //            |      |      |
                    print!("{CSI}\r{CSI}{}A{CSI}J", groups.len());
                }

                let samplerate: u32 = match lines_skipped {
                    0 => 100,
                    _ => {
                        (100 * pending_lines.len() as u32)
                            / (lines_skipped + pending_lines.len() as u32)
                    }
                };
                for line in pending_lines.iter() {
                    if cut_width != 0 && line.len() > cut_width as usize {
                        // truncate the line to fit the screen
                        println!("{}", &line[..cut_width as usize]);
                    } else {
                        println!("{}", line);
                    }
                }
                pending_lines.clear();
                lines_skipped = 0;

                let mut toflush = format!("-- Output sampled at {samplerate}%");
                let maxtagname =
                    cmp::max(8, groups.iter().map(|x| x.group.len()).max().unwrap_or(0)); // if we have no tags we don't care about the answer
                let shared_prefix_len = groups.shared_prefix.len();
                let shared_suffix_len = groups.shared_suffix.len();
                let padded_group_length = maxtagname - shared_prefix_len - shared_suffix_len;

                for groupstats in groups.iter_mut() {
                    groupstats.process();

                    let padded_tag =
                        if groupstats.group.len() <= shared_prefix_len + shared_suffix_len {
                            "@".to_owned() + &" ".repeat(padded_group_length - 1)
                        } else {
                            "".to_owned()
                                + &groupstats.group.clone()
                                    [shared_prefix_len..groupstats.group.len() - shared_suffix_len]
                                + &" ".repeat(maxtagname - groupstats.group.len())
                        };
                    toflush += &format!("\n-- {padded_tag} ");

                    // This looks a bit messy, but roughly:
                    // * global_statuscodes is a list of all status codes we have seen so far, sorted
                    // * groupstats is a list of all status codes we have seen so far for this group, sorted
                    //
                    // groupstats is strictly a subset of global_statuscodes.
                    // Since both are sorted we can iterate over them in parallel which should be quite efficient.
                    let mut group_statusstats = groupstats.iter();
                    let mut pending_group_statusstat = None;
                    for statuscode in global_statuscodes.lock().await.iter() {
                        if pending_group_statusstat.is_none() {
                            pending_group_statusstat = group_statusstats.next()
                        };
                        if pending_group_statusstat.is_some()
                            && &pending_group_statusstat.unwrap().statuscode == statuscode
                        {
                            // This will consuming next_group_statusstat
                            // which is needed for the next iteration
                            let unwrapped = pending_group_statusstat.take().unwrap();
                            let color = match unwrapped.statuscode.chars().next() {
                                Some('2') => format!("{CSI}32m"),
                                Some('3') => format!("{CSI}35m"),
                                Some('4') => format!("{CSI}33m"),
                                Some('5') => format!("{CSI}31m"),
                                _ => format!("{CSI}37m"),
                            };
                            toflush += &format!(
                                "{:7.1} [{}{}{CSI}0m] ",
                                unwrapped.ring.get_speed(),
                                color,
                                unwrapped.statuscode,
                            );
                        } else {
                            #[cfg(debug_assertions)]
                            {
                                toflush += &format!("{:>7}  {}  ", "", statuscode);
                            }
                            #[cfg(not(debug_assertions))]
                            {
                                toflush += &format!("{:7}  {}  ", "", " ".repeat(statuscode.len()));
                            }
                        }
                    }
                    // To test: is it more efficient to spend time printing less characters to the screen?
                    // it looks neater and reduces chance of line wrapping, at any rate.
                    toflush.truncate(toflush.trim_end().len());
                }
                print!("{}", toflush);
                std::io::stdout().flush().unwrap();
                last_gutter_count = groups.len() as u32;
            }
        }
    }
}

fn get_terminal_width() -> u16 {
    use rustix::termios::tcgetwinsize;
    match tcgetwinsize(std::io::stderr()) {
        Ok(x) => x.ws_col,
        Err(_) => 80, // default to 80 columns if we can't get the terminal size
    }
}

fn main() {
    let mut pargs = pico_args::Arguments::from_env();
    if pargs.contains(["-h", "--help"]) {
        println!("{HELP}");
        std::process::exit(0);
    }

    let max_runtime: Option<u32> = pargs.opt_value_from_str("--max-runtime").unwrap_or(None);

    // TODO: let the user specify --loglines instead: with dynamic tags you don't know the right screenheight
    let target_height: u16 = pargs.value_from_str("--target-height").unwrap_or_else(|_| {
        use rustix::termios::tcgetwinsize;
        match tcgetwinsize(std::io::stderr()) {
            Ok(x) => x.ws_row,
            _ => 3,
        }
    });

    let requested_width: Option<u16> =
        if let Ok(reqwidth) = pargs.value_from_str::<&str, String>("--max-width") {
            Some(reqwidth.parse::<u16>().unwrap_or_else(|err| {
                eprintln!("Failed to parse {reqwidth} as a number: {err}");
                process::exit(1)
            }))
        } else {
            None
        };

    let combine_filestats: bool = pargs.contains("--combine");

    #[cfg(debug_assertions)]
    let fast_generator = pargs.contains("--fast");
    #[cfg(debug_assertions)]
    let slow_generator = pargs.contains("--slow");

    let mut log_dirs = vec![];
    let mut log_files = vec![];
    let remaining = pargs.finish();
    for dir_or_file in remaining {
        let lossy = dir_or_file.to_string_lossy();
        if lossy.starts_with("--") {
            eprintln!("{HELP}\n");
            eprintln!(
                "Unknown option {lossy}\nIf you to tail a file/directory that starts with -- use ./--"
            );
            std::process::exit(2);
        }
        let path = PathBuf::from(dir_or_file);
        if path.is_dir() {
            log_dirs.push(path)
        } else {
            log_files.push(path)
        }
    }

    if log_files.is_empty() && log_dirs.is_empty() {
        log_dirs.push("/var/log/nginx/".into());
    }

    let args = AppArgs {
        #[cfg(debug_assertions)]
        fast_generator,
        #[cfg(debug_assertions)]
        slow_generator,
        log_dirs,
        log_files,
        target_height,
        combine_filestats,
        max_runtime,
        requested_width,
    };

    match smol::block_on(innermain(args)) {
        Ok(_) => {}
        Err(ex) => eprintln!("{ex}"),
    }
}

async fn winch_handler(channel: SenderChannel) {
    let window_changed_size = Arc::new(AtomicBool::new(false));
    let Ok(_) = signal_hook::flag::register(
        signal_hook::consts::SIGWINCH,
        Arc::clone(&window_changed_size),
    ) else {
        eprintln!("Failed to register signal handler for SIGWINCH");
        return;
    };

    loop {
        if window_changed_size.load(std::sync::atomic::Ordering::Relaxed)
            && channel
                .send(Message::WinCh(get_terminal_width()))
                .await
                .is_err()
        {
            // Channel closed, exit the handler
            return;
        }
        Timer::after(Duration::from_millis(300)).await;
    }
}

async fn innermain(args: AppArgs) -> Result<(), Error> {
    // channel to send messages to the processing thread
    let (sender, receiver) = bounded(1_000_000);

    let async_exec = LocalExecutor::new();

    if args.requested_width.is_none() {
        async_exec.spawn(winch_handler(sender.clone())).detach();
    };

    #[cfg(debug_assertions)]
    {
        if args.fast_generator {
            println!("Fast generator enabled");
            async_exec.spawn(fake_fast(sender.clone())).detach();
        }
        if args.slow_generator {
            println!("Slow generator enabled");
            async_exec.spawn(fake_slow(sender.clone())).detach();
        }
    }

    let mut logfiles_to_follow = vec![];

    for log_file in args.log_files {
        if !log_file.is_file() {
            // things can still go wrong (if the file isn't readable or something)
            // but at least we tried our best
            eprintln!("WARNING: Log file {log_file:?} is not a file");
        } else {
            logfiles_to_follow.push(log_file);
        }
    }

    let mut dirs_to_check = args.log_dirs;

    #[allow(clippy::manual_while_let_some)]
    // we're modifying the iterator we're looping over on purpose
    while !dirs_to_check.is_empty() {
        let dir_to_check = dirs_to_check.pop().unwrap();

        match read_dir(dir_to_check.clone()) {
            Err(e) => {
                println!("WARNING: Failed to read directory {dir_to_check:?}: {e}");
                continue;
            }
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Err(x) => eprintln!("Failed to process: {x}"),
                        Ok(entry) => match entry.metadata() {
                            Err(x) => eprintln!("Failed to process: {entry:?}: {x}"),
                            Ok(meta) => {
                                if meta.is_dir() {
                                    dirs_to_check.push(entry.path());
                                } else if meta.is_file() && (entry.file_name() == "access.log") {
                                    println!("Added {:?} as reader", entry.path());
                                    logfiles_to_follow.push(entry.path());
                                }
                            }
                        },
                    }
                }
            }
        }
    }

    if logfiles_to_follow.is_empty() {
        return Err(Error("No useable log files found".to_string()));
    }

    logfiles_to_follow.sort();
    logfiles_to_follow.dedup();

    for log_file in logfiles_to_follow {
        async_exec
            .spawn(follow(
                sender.clone(),
                log_file.clone(),
                match args.combine_filestats {
                    true => "".to_owned(),
                    false => log_file.display().to_string(),
                },
            ))
            .detach();
    }

    async_exec.spawn(periodic_print(sender.clone())).detach();

    if args.max_runtime.is_some() {
        let max_runtime = args.max_runtime.unwrap();
        async_exec
            .spawn(async move {
                Timer::after(Duration::from_secs(max_runtime.into())).await;
                std::process::exit(0);
            })
            .detach();
    }

    future::block_on(async_exec.run(process(
        receiver,
        args.target_height,
        dbg!(args.requested_width),
    )));

    Ok(())
}

fn extract_statuscode(line: &str) -> String {
    if let Some(first_quote) = line.find('"') {
        if let Some(second_quote) = line[first_quote + 1..].find('"') {
            if let Some(end_space) = line[first_quote + 1 + second_quote + 2..].find(' ') {
                line[first_quote + 1 + second_quote + 2
                    ..first_quote + 1 + second_quote + 2 + end_space]
                    .to_owned()
            } else {
                "?C".to_owned()
            }
        } else {
            "?B".to_owned()
        }
    } else {
        "?A".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use crate::GlobalStatuscodes;
    use crate::Message;
    use crate::extract_statuscode;
    use crate::follow;
    use smol::LocalExecutor;
    use smol::Timer;
    use smol::future;
    use std::fs::remove_file;
    use std::path::PathBuf;
    use std::process::Command;
    use std::str::from_utf8;
    use std::time::Duration;
    use std::{fs::File, io::Write};

    struct TempFile {
        filename: String,
        file: File,
    }
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = remove_file(self.filename.clone());
        }
    }
    impl TempFile {
        fn new() -> Self {
            let stdout = Command::new("mktemp")
                .args(["--suffix", "nginx-tail-testcase"])
                .output()
                .expect("Failed to run mktemp")
                .stdout;
            let filename = from_utf8(&stdout.strip_suffix(b"\n").unwrap())
                .expect("Failed to interpret mktemp output")
                .to_owned();
            let file = File::options()
                .read(true)
                .write(true)
                .open(PathBuf::from(filename.clone()))
                .expect(&format!("Failed to open tmpfile '{filename}'"));
            TempFile { filename, file }
        }
    }

    #[test]
    fn test_parsing() {
        let variant1 = r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] "GET /v2/installations/74453/stats?interval=hours&type=evcs&start=1748210400 HTTP/1.0" 200 63 - 0.023 0.022 "-" "UserAgent/123" "https" "some.domain.example""#.to_owned();
        assert_eq!("200", extract_statuscode(&variant1));
        let variant2 = r#"123.123.123.123 - - [26/May/2025:19:43:59 +0200] "GET /links.json HTTP/1.1" 200 91 "-" "Monit/5.34.3" 0.004 0.004 ."#.to_owned();
        assert_eq!("200", extract_statuscode(&variant2));
    }

    #[test]
    fn test_tagmap_with_short_tags() {
        let mut tagmap = super::GroupMap::new(GlobalStatuscodes::default());
        assert!(tagmap.is_empty());
        assert_eq!(tagmap.len(), 0);

        let tag1 = tagmap.get_or_create("200".to_owned());
        assert_eq!(tag1.group, "200");
        assert_eq!(tagmap.len(), 1);
        assert_eq!(tagmap.shared_prefix, "");
        assert_eq!(tagmap.shared_suffix, "");

        let tag2 = tagmap.get_or_create("500".to_owned());
        assert_eq!(tag2.group, "500");
        assert_eq!(tagmap.len(), 2);
        assert_eq!(tagmap.shared_prefix, "");
        assert_eq!(tagmap.shared_suffix, "");

        tagmap.get_or_create("404".to_owned());
        assert_eq!(tagmap.len(), 3);
        assert_eq!(tagmap.shared_prefix, "");
        assert_eq!(tagmap.shared_suffix, "");

        // reuse tag
        tagmap.get_or_create("200".to_owned());
        assert_eq!(tagmap.len(), 3);
    }

    #[test]
    fn test_tagmap_with_long_tags() {
        let mut tagmap = super::GroupMap::new(GlobalStatuscodes::default());
        assert!(tagmap.is_empty());
        assert_eq!(tagmap.len(), 0);

        let tag1 =
            tagmap.get_or_create("/var/log/nginx/sites/customer_project_0/access.log".to_owned());
        assert_eq!(
            tag1.group,
            "/var/log/nginx/sites/customer_project_0/access.log"
        );
        assert_eq!(tagmap.len(), 1);
        assert_eq!(tagmap.shared_prefix, "");
        assert_eq!(tagmap.shared_suffix, "");

        let tag2 =
            tagmap.get_or_create("/var/log/nginx/sites/customer_project_1/access.log".to_owned());
        assert_eq!(
            tag2.group,
            "/var/log/nginx/sites/customer_project_1/access.log"
        );
        assert_eq!(tagmap.len(), 2);
        assert_eq!(tagmap.shared_prefix, "/var/log/nginx/sites/customer_p");
        assert_eq!(tagmap.shared_suffix, "/access.log");

        tagmap.get_or_create("/var/log/nginx/sites/customer_project_2/access.log".to_owned());
        assert_eq!(tagmap.len(), 3);
        assert_eq!(tagmap.shared_prefix, "/var/log/nginx/sites/customer_p");
        assert_eq!(tagmap.shared_suffix, "/access.log");

        // reuse tag
        tagmap.get_or_create("/var/log/nginx/sites/customer_project_1/access.log".to_owned());
        assert_eq!(tagmap.len(), 3);
        assert_eq!(tagmap.shared_prefix, "/var/log/nginx/sites/customer_p");
        assert_eq!(tagmap.shared_suffix, "/access.log");

        // like a "root" log file
        let tag5 = tagmap.get_or_create("/var/log/nginx/sites/access.log".to_owned());
        assert_eq!(tag5.group, "/var/log/nginx/sites/access.log");
        assert_eq!(tagmap.len(), 4);
        assert_eq!(tagmap.shared_prefix, "/var/log/nginx/sites/");
        assert_eq!(tagmap.shared_suffix, "/access.log");
    }

    #[test]
    fn test_reading_files() {
        let local_ex = LocalExecutor::new();

        future::block_on(local_ex.run(async {
            // TODO: use mktemp -d or something (or in-memory files?)
            let tmpfile = TempFile::new();
            let mut file = &tmpfile.file;
            file.write_all(b"line 1\n").unwrap();
            file.write_all(b"line 2\n").unwrap();

            let (sender, receiver) = smol::channel::bounded(10000);

            smol::spawn(follow(
                sender,
                tmpfile.filename.clone().into(),
                tmpfile.filename.clone(),
            ))
            .detach();

            // No data written yet
            Timer::after(Duration::from_millis(70)).await;
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::RegisterGroup(tmpfile.filename.clone())
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
                    group: tmpfile.filename.clone(),
                    statuscode: "?A".to_owned(),
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
                    group: tmpfile.filename.clone(),
                    statuscode: "?A".to_owned(),
                },
            );

            // Two lines written at once
            file.write_all(b"line 5\nline 6\n").unwrap();
            Timer::after(Duration::from_millis(70)).await;
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line {
                    text: "line 5".to_owned(),
                    group: tmpfile.filename.clone(),
                    statuscode: "?A".to_owned(),
                },
            );
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line {
                    text: "line 6".to_owned(),
                    group: tmpfile.filename.clone(),
                    statuscode: "?A".to_owned(),
                }
            );

            // Three and a half lines at once
            file.write_all(b"line 7\nline 8\nline 9\nline 0").unwrap();
            Timer::after(Duration::from_millis(70)).await;
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line {
                    text: "line 7".to_owned(),
                    group: tmpfile.filename.clone(),
                    statuscode: "?A".to_owned(),
                },
            );
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line {
                    text: "line 8".to_owned(),
                    group: tmpfile.filename.clone(),
                    statuscode: "?A".to_owned(),
                },
            );
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line {
                    text: "line 9".to_owned(),
                    group: tmpfile.filename.clone(),
                    statuscode: "?A".to_owned(),
                },
            );
            assert!(receiver.try_recv().is_err());
        }));
    }
}
