mod collections;
mod parsing;
mod speedometer;
pub mod terminal;

use collections::GroupMap;
use parsing::parse_nginx_line;
use smol::channel::SendError;
use smol::fs::File;
use smol::fs::read_link;
use smol::io::AsyncReadExt as _;
use smol::io::AsyncSeekExt as _;
use smol::lock::Mutex;
use smol::{
    Timer,
    channel::{Receiver, Sender},
};
use speedometer::{RingbufferSpeedometer, Speedometer};
use std::cmp;
use std::collections::VecDeque;
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::sync::Arc;
use std::vec;
use std::{fmt::Display, path::PathBuf, time::Duration};
use terminal::colors;
use terminal::colors::CSI;

use crate::parsing::code2color;

pub fn get_statuscode_class(statuscode: &str) -> Option<String> {
    // As defined in RFC 9110:
    // 1xx (Informational): The request was received, continuing process
    // 2xx (Successful): The request was successfully received, understood, and accepted
    // 3xx (Redirection): Further action needs to be taken in order to complete the request
    // 4xx (Client Error): The request contains bad syntax or cannot be fulfilled
    // 5xx (Server Error): The server failed to fulfill an apparently valid request
    statuscode.chars().next().map(|x| format!("{x}xx"))
}

fn extract_statuscode(line: &str) -> Result<String, String> {
    if let Some(first_quote) = line.find('"') {
        if line.len() < first_quote + 1 {
            return Err("?D".to_owned());
        }
        if let Some(second_quote) = line[first_quote + 1..].find('"') {
            if line.len() < first_quote + 1 + second_quote + 2 {
                return Err("?E".to_owned());
            }
            if let Some(end_space) = line[first_quote + 1 + second_quote + 2..].find(' ') {
                Ok(line[first_quote + 1 + second_quote + 2
                    ..first_quote + 1 + second_quote + 2 + end_space]
                    .to_owned())
            } else {
                Err("?C".to_owned())
            }
        } else {
            Err("?B".to_owned())
        }
    } else {
        Err("?A".to_owned())
    }
}

pub struct Error(pub String);
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

pub type SenderChannel = Sender<Message>;

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

pub async fn follow(
    channel: SenderChannel,
    file: PathBuf,
    updowngroup: String,
    leftrightextractor: fn(&str) -> Option<String>,
) {
    channel
        .send(Message::RegisterGroup(updowngroup.clone()))
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
                    let statuscode = extract_statuscode(&line).ok();
                    let leftrightgroup = match statuscode.as_deref() {
                        None => None,
                        Some(x) => leftrightextractor(x),
                    };
                    if channel
                        .send(Message::Line {
                            text: line,
                            updowngroup: updowngroup.clone(),
                            leftrightgroup,
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
pub enum Message {
    Print {
        include_lines: bool,
    },
    RegisterGroup(String), // optional; can be used when you know upfront what the tags are
    Line {
        text: String,
        updowngroup: String, // usually "/var/log/nginx/site1/access.log", but can be "fe. "Total"
        leftrightgroup: Option<String>, // either 200,403,404 or 2xx,4xx
        statuscode: Option<String>, // 200, 403, 404
    },
    WinCh(u16),
}
pub async fn periodic_print(channel: SenderChannel) -> Result<(), Error> {
    macro_rules! print_stats {
        () => {
            channel
                .send(Message::Print {
                    include_lines: false,
                })
                .await?;
        };
    }
    macro_rules! print_lines {
        () => {
            channel
                .send(Message::Print {
                    include_lines: true,
                })
                .await?;
        };
    }
    // update GUI right away
    Timer::after(Duration::from_millis(100)).await;
    print_stats!();
    Timer::after(Duration::from_millis(200)).await;
    print_stats!();
    Timer::after(Duration::from_millis(300)).await;
    print_stats!();
    Timer::after(Duration::from_millis(500)).await;
    print_lines!();

    let stats_interval = Duration::from_millis(333);
    let lines_every_x_stats = 3 * 5; // roughly once every 5 seconds
    loop {
        for _ in 0..lines_every_x_stats {
            Timer::after(stats_interval).await;
            print_stats!();
        }
        Timer::after(stats_interval).await;
        print_lines!();
    }
}

#[cfg(debug_assertions)]
pub async fn fake_slow(channel: SenderChannel) {
    let mut counter: u32 = 0;
    loop {
        Timer::after(Duration::from_secs(2)).await;
        match channel
            .send(Message::Line {
                text: format!("Fake slow msg {counter}"),
                statuscode: Some("slow".to_owned()),
                updowngroup: "generator".to_owned(),
                leftrightgroup: Some("200".to_owned()),
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
pub async fn fake_fast(channel: SenderChannel) {
    let mut j = 0;
    loop {
        j += 1;
        Timer::after(Duration::from_millis(100)).await;
        for i in 0..100 {
            match channel
                .send(Message::Line {
                    text: format!("[{j}] Fake fast msg {i}"),
                    statuscode: Some("200".to_owned()),
                    updowngroup: "generator".to_owned(),
                    leftrightgroup: Some("fake".to_owned()),
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

pub async fn process_as_streaming(channel: Receiver<Message>, filters: Vec<String>) {
    loop {
        match channel.recv().await {
            Err(_) => {
                eprintln!("Channel closed");
                return;
            }
            Ok(Message::WinCh(_)) => {
                #[cfg(debug_assertions)]
                unreachable!()
            }
            Ok(Message::Print { include_lines: _ }) => {
                #[cfg(debug_assertions)]
                unreachable!()
            }
            Ok(Message::Line {
                text,
                updowngroup: _,
                leftrightgroup: _,
                statuscode,
            }) => {
                // filtering. TODO: DRY
                if !filters.is_empty() && statuscode.is_some() {
                    let statuscode = statuscode.clone().unwrap();

                    if !filters.iter().any(|x| statuscode.starts_with(x)) {
                        continue;
                    }
                }
                println!("{}", parse_nginx_line(&text))
            }
            Ok(Message::RegisterGroup(_)) => {
                // shouldn't happen often
            }
        }
    }
}

///
/// requested_width:
/// Some(0) = unlimited line length  -- no sigwinch handler installed
/// Some(x) = cut off at x           -- no sigwinch handler installed
/// None    = use screen's width     -- sigwinch handler installed
pub async fn process_as_tui(
    channel: Receiver<Message>,
    target_height: u16,
    requested_width: Option<u16>,
    filters: Vec<String>,
) {
    let mut pending_lines: VecDeque<(String, Option<String>)> =
        VecDeque::with_capacity(target_height as usize);
    let mut lines_skipped: u32 = 0;
    // These are unlikely to change often, so we'll track them in memory instead
    // of recomputing them every time
    let global_statuscodes = Arc::new(Mutex::new(vec![]));
    let mut groups = GroupMap::new(global_statuscodes.clone());

    let mut cut_width = match requested_width {
        None => terminal::get_terminal_width(),
        Some(x) => x,
    };

    println!("{CSI}?25l"); // hide cursor

    let mut lastprinted_stats: String = "".to_owned(); // for optimization we want to minimize printing
    let mut lines_to_wipe = 0;

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
                updowngroup,
                leftrightgroup,
                statuscode,
            }) => {
                // accounting
                if let Some(leftrightgroup) = leftrightgroup.clone() {
                    let groupstats = groups.get_or_create(updowngroup.clone());
                    let statusstats = groupstats.get_or_create(leftrightgroup).await;
                    statusstats.pending += 1;
                }

                // filtering. TODO: DRY
                if !filters.is_empty() && statuscode.is_some() {
                    let statuscode = statuscode.clone().unwrap();

                    if !filters.iter().any(|x| statuscode.starts_with(x)) {
                        continue;
                    }
                }
                if pending_lines.len() >= number_of_lines.into() {
                    pending_lines.pop_front();
                    lines_skipped += 1;
                };
                pending_lines.push_back((text, leftrightgroup));
            }
            Ok(Message::Print { include_lines }) => {
                // Printing to a terminal is _really_ slow, so if our current
                // output would be the same as the previous output we'll skip
                // printing
                //
                // This is getting a little bit tricky because we have 2
                // different printing modes (with lines and without lines), and
                // both could end up deciding not to print.
                let mut toflush_lines = "".to_owned();
                let mut toflush_stats = "".to_owned();

                if include_lines && !pending_lines.is_empty() {
                    let samplerate: u32 = match lines_skipped {
                        0 => 100,
                        _ => {
                            (100 * pending_lines.len() as u32)
                                / (lines_skipped + pending_lines.len() as u32)
                        }
                    };
                    for (line, statuscode) in pending_lines.iter() {
                        let (color, reset) = match statuscode {
                            None => (colors::ORANGE, colors::RESET),
                            Some(_) => ("", ""),
                        };
                        let trimmed_line = if cut_width != 0 && line.len() > cut_width as usize {
                            &line[..cut_width as usize]
                        } else {
                            &line[..]
                        };
                        toflush_lines +=
                            &format!("{color}{}{reset}\n", parse_nginx_line(trimmed_line));
                    }
                    pending_lines.clear();
                    lines_skipped = 0;

                    toflush_lines += &format!("-- Output sampled at {samplerate}%\n");
                }

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
                    toflush_stats += &format!("-- {padded_tag} ");

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
                            let (color, reset) = code2color(&unwrapped.statuscode);
                            toflush_stats += &format!(
                                "{:7.1} [{color}{}{reset}] ",
                                unwrapped.ring.get_speed(),
                                unwrapped.statuscode,
                            );
                        } else {
                            #[cfg(debug_assertions)]
                            {
                                toflush_stats += &format!("{:>7}  {}  ", "", statuscode);
                            }
                            #[cfg(not(debug_assertions))]
                            {
                                toflush_stats +=
                                    &format!("{:7}  {}  ", "", " ".repeat(statuscode.len()));
                            }
                        }
                    }
                    toflush_stats += "\n";
                }
                toflush_stats.truncate(toflush_stats.trim_end().len());

                if !toflush_lines.is_empty() || toflush_stats != lastprinted_stats {
                    // the line "Output sampled at 75%" above the stats should:
                    // * get wiped when we want to print lines *and* there are lines
                    // * not get wiped when we want to print lines but there were *no* lines
                    // * not get wiped when we're only printing stats (the stats don't include this line)
                    if include_lines {
                        lines_to_wipe += 1;
                    }

                    let toflush_wiper = if lines_to_wipe == 0 {
                        // special case: using CSI<n>A with n = 0 still moves
                        // the cursor up, and we only want to move to the left
                        // without moving upwards
                        &format!("\r{CSI}J")
                    } else {
                        //                           _______________________ move cursor to beginning of line
                        //                          |        _______________ move cursor up X lines
                        //                          |       |      _________ clear to end of screen
                        //                format!(" |       |     |
                        &format!("\r{CSI}{}A{CSI}J", lines_to_wipe)
                    };
                    print!("{toflush_wiper}{toflush_lines}{toflush_stats}");
                    std::io::stdout().flush().unwrap();

                    lines_to_wipe = toflush_stats.chars().filter(|x| *x == '\n').count(); // wipe next time
                    lastprinted_stats = toflush_stats;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Message;
    use crate::follow;
    use crate::get_statuscode_class;
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
                get_statuscode_class,
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
                    updowngroup: tmpfile.filename.clone(),
                    leftrightgroup: None,
                    statuscode: None,
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
                    updowngroup: tmpfile.filename.clone(),
                    leftrightgroup: None,
                    statuscode: None,
                },
            );

            // Two lines written at once
            file.write_all(b"line 5\nline 6\n").unwrap();
            Timer::after(Duration::from_millis(70)).await;
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line {
                    text: "line 5".to_owned(),
                    updowngroup: tmpfile.filename.clone(),
                    leftrightgroup: None,
                    statuscode: None,
                },
            );
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line {
                    text: "line 6".to_owned(),
                    updowngroup: tmpfile.filename.clone(),
                    leftrightgroup: None,
                    statuscode: None,
                }
            );

            // Three and a half lines at once
            file.write_all(b"line 7\nline 8\nline 9\nline 0").unwrap();
            Timer::after(Duration::from_millis(70)).await;
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line {
                    text: "line 7".to_owned(),
                    updowngroup: tmpfile.filename.clone(),
                    leftrightgroup: None,
                    statuscode: None,
                },
            );
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line {
                    text: "line 8".to_owned(),
                    updowngroup: tmpfile.filename.clone(),
                    leftrightgroup: None,
                    statuscode: None,
                },
            );
            assert_eq!(
                receiver.try_recv().unwrap(),
                Message::Line {
                    text: "line 9".to_owned(),
                    updowngroup: tmpfile.filename.clone(),
                    leftrightgroup: None,
                    statuscode: None,
                },
            );
            assert!(receiver.try_recv().is_err());
        }));
    }
}
