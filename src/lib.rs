mod collections;
mod parsing;
mod speedometer;
pub mod terminal;

use std::cmp;
use std::collections::VecDeque;
use std::io::Write as _;
use std::os::fd::AsFd as _;
use std::os::unix::fs::FileTypeExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::sync::Arc;
use std::vec;
use std::{fmt::Display, path::PathBuf, time::Duration};

use signal_hook::consts::SIGINT;
use signal_hook::low_level::raise;
use smol::channel::SendError;
use smol::fs::File;
use smol::io::AsyncReadExt as _;
use smol::io::AsyncSeekExt as _;
use smol::lock::Mutex;
use smol::{
    Timer,
    channel::{Receiver, Sender},
};

use crate::collections::GroupMap;
use crate::parsing::code2color;
use crate::parsing::parse_nginx_line;
use crate::speedometer::{RingbufferSpeedometer, Speedometer};
use crate::terminal::colors;
use crate::terminal::colors::CSI;

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
    file: File,       // the file handle
    pending: Vec<u8>, // data that was read but not yet processed
    readbuf: Vec<u8>,
}

impl LineReader {
    async fn new(filename: PathBuf) -> Result<Self, String> {
        let file = Self::_open_file(filename.clone()).await?;
        Ok(LineReader {
            filename,
            file,
            pending: vec![],
            readbuf: vec![0; 1024],
        })
    }

    async fn _open_file(filename: PathBuf) -> Result<File, String> {
        let mut file = smol::fs::File::open(&filename)
            .await
            .map_err(|e| e.to_string())?;
        if file.seek(std::io::SeekFrom::End(0)).await.is_err() {
            return Err("Error seeking to end of file".into());
        }
        Ok(file)
    }

    /// Whether our filename now points to a different file than the one we
    /// have open, i.e. the log got rotated and a new file has been created.
    ///
    /// We compare inodes instead of paths: paths can be relative or contain
    /// symlinks, and this doesn't depend on /proc being available.
    async fn rotated(&self) -> bool {
        let Ok(current) = self.file.metadata().await else {
            return false;
        };
        let Ok(on_disk) = smol::fs::metadata(&self.filename).await else {
            // nothing at our filename (yet), so nothing to switch to
            return false;
        };
        (current.dev(), current.ino()) != (on_disk.dev(), on_disk.ino())
    }

    async fn read_lines(&mut self) -> Result<Vec<String>, ()> {
        match self.file.read(&mut self.readbuf).await {
            Ok(0) => {
                let reopened = match self.rotated().await {
                    false => None,
                    true => Self::_open_file(self.filename.clone()).await.ok(),
                };
                match reopened {
                    Some(file) => {
                        self.file = file;
                        self.pending.clear();
                    }
                    None => {
                        // Nothing new. Let's wait a bit before trying again;
                        // without this wait follow() would spin at 100% CPU
                        Timer::after(Duration::from_millis(50)).await;
                    }
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
    WinCh(u16, u16), // new terminal width and height
}

/// Whether data is being piped into us, like `tail -f access.log | nginx-tail`
fn stdin_is_pipe() -> bool {
    std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .and_then(|fd| std::fs::File::from(fd).metadata())
        .is_ok_and(|meta| meta.file_type().is_fifo())
}

/// The terminal hung up: there will be no more keypresses
fn keyboard_gone() -> Result<(), Error> {
    if !stdin_is_pipe() {
        // Our output went to that terminal too, nothing left to do.
        // Quit the same way 'q' does, see sigint_handler
        raise(SIGINT).unwrap();
    }
    // Data is being piped into us: keep processing it, just without
    // keyboard input
    Ok(())
}

pub async fn keyboard_reader(channel: SenderChannel) -> Result<(), Error> {
    // NB: We're using tty and not stdin: we want to allow users to pipe
    // data into the program via stdin still

    // Enabling raw mode is a terminal feature (so we can read single keypresses),
    // it's independent of opening the tty device.
    let mut tty = smol::fs::File::open("/dev/tty")
        .await
        .map_err(|e| Error(format!("Failed to open /dev/tty: {e:?}")))?;

    eprintln!("[q] to quit, [l] to flush lines, [s] to update stats");
    loop {
        let mut buffer = [0; 1];
        match tty.read(&mut buffer).await {
            Ok(0) => {
                // EOF: the terminal is gone. Retrying would loop forever.
                return keyboard_gone();
            }
            Ok(_) => {
                let c = buffer[0] as char;
                match c {
                    'q' | 'Q' => {
                        raise(SIGINT).unwrap(); // to trigger terminal restoration, see sigint_handler
                    }
                    's' | 'S' => {
                        channel
                            .send(Message::Print {
                                include_lines: false,
                            })
                            .await?;
                    }
                    'l' | 'L' => {
                        channel
                            .send(Message::Print {
                                include_lines: true,
                            })
                            .await?;
                    }
                    _ => {}
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No input available. NB: don't use std::thread::sleep here,
                // it would block every other task on the executor
                Timer::after(Duration::from_millis(10)).await;
                continue;
            }
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) => {
                // A read that was in progress when the terminal hung up
                // gets EIO, later reads get EOF
                return keyboard_gone();
            }
            Err(e) => {
                eprintln!("Error reading from tty: {e}");
                return Err(Error(e.to_string()));
            }
        }
    }
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
                statuscode: Some("200".to_owned()),
                updowngroup: "slow-generator".to_owned(),
                leftrightgroup: Some("fake".to_owned()),
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
                    updowngroup: "fast-generator".to_owned(),
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
            Ok(Message::WinCh(_, _)) => {
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

/// How the rows of the screen are divided
#[derive(Debug, PartialEq)]
struct Layout {
    stats_rows: u16,   // rows used by per-file stats, including the "... and X more" row
    groups_shown: u16, // groups that get a row of their own
    number_of_lines: u16, // rows left for log lines, 0 if the UI doesn't fit
}

impl Layout {
    /// requested_file_rows:
    /// Some(x) = at most x rows for the per-file stats
    /// None    = at most half of the screen
    fn new(target_height: u16, requested_file_rows: Option<u16>, groups: usize) -> Self {
        let max_rows = requested_file_rows.unwrap_or(cmp::max(1, target_height / 2));
        let groups = u16::try_from(groups).unwrap_or(u16::MAX);
        let (stats_rows, groups_shown) = if groups <= max_rows {
            (groups, groups)
        } else {
            // the last row says how many groups we're not showing
            (max_rows, max_rows - 1)
        };
        // Besides the log lines and the stats we need a line for "Output
        // sampled at" and we'll try to show the last output line of last time at the top
        let number_of_lines = target_height.saturating_sub(stats_rows.saturating_add(2));
        Self {
            stats_rows,
            groups_shown,
            number_of_lines,
        }
    }
}

/// A shown group is only replaced by a hidden group that was active at least
/// this many seconds more recently, so the list doesn't flicker
const SWAP_AFTER_SECS: u64 = 5;

/// Picks which groups get a row, preferring the ones with the most recent writes.
///
/// last_activity: seconds since the last write of each group (None = never)
/// previous:      indexes of the groups that were shown last time
///
/// Returns the indexes of the groups to show, in their original order.
fn pick_groups(last_activity: &[Option<u64>], previous: &[usize], count: usize) -> Vec<usize> {
    let secs_ago = |index: &usize| last_activity[*index].unwrap_or(u64::MAX);
    let (mut shown, mut hidden): (Vec<usize>, Vec<usize>) =
        (0..last_activity.len()).partition(|index| previous.contains(index));
    // most recently active first
    shown.sort_by_key(secs_ago);
    hidden.sort_by_key(secs_ago);

    shown.truncate(count);
    let missing = cmp::min(count.saturating_sub(shown.len()), hidden.len());
    shown.extend(hidden.drain(..missing));
    shown.sort_by_key(secs_ago);

    // swap the least active shown groups for clearly more active hidden ones
    for (hidden_index, shown_index) in hidden.iter().zip(shown.iter_mut().rev()) {
        if secs_ago(hidden_index).saturating_add(SWAP_AFTER_SECS) < secs_ago(shown_index) {
            *shown_index = *hidden_index;
        } else {
            break;
        }
    }
    shown.sort();
    shown
}

///
/// requested_height:
/// Some(x) = use x rows
/// None    = use screen's height    -- sigwinch handler installed
///
/// requested_width:
/// Some(0) = unlimited line length  -- no sigwinch handler installed (unless requested_height is None)
/// Some(x) = cut off at x           -- no sigwinch handler installed (unless requested_height is None)
/// None    = use screen's width     -- sigwinch handler installed
///
/// requested_file_rows: see Layout::new
pub async fn process_as_tui(
    channel: Receiver<Message>,
    requested_height: Option<u16>,
    requested_width: Option<u16>,
    requested_file_rows: Option<u16>,
    filters: Vec<String>,
) -> Result<(), Error> {
    let mut target_height = match requested_height {
        None => terminal::get_terminal_height(),
        Some(x) => x,
    };
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
    let mut shown_groups: Vec<usize> = vec![]; // indexes into groups

    loop {
        let layout = Layout::new(target_height, requested_file_rows, groups.len());
        let number_of_lines = layout.number_of_lines;
        match channel.recv().await {
            Err(_) => {
                eprintln!("Channel closed.");
                return Ok(());
            }
            Ok(Message::WinCh(new_terminal_width, new_terminal_height)) => {
                // the sigwinch handler is connected when the user did not specify
                // a width or a height: only follow the screen for what wasn't specified
                if requested_width.is_none() {
                    cut_width = new_terminal_width;
                }
                if requested_height.is_none() {
                    target_height = new_terminal_height;
                }
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
                // NB: a loop, the number of lines shrinks when the terminal gets smaller
                while !pending_lines.is_empty() && pending_lines.len() >= number_of_lines.into() {
                    pending_lines.pop_front();
                    lines_skipped += 1;
                }
                pending_lines.push_back((text, leftrightgroup));
            }
            Ok(Message::Print { include_lines }) => {
                // Checked here instead of at the top of the loop: by now the
                // groups have been registered, so we know how much we need
                if number_of_lines == 0 {
                    println!("{CSI}?25h"); // show cursor
                    return Err(Error(format!(
                        "Not enough space to draw the UI: won't fit on screen, need at least {} rows, have {target_height} (see --target-height and --file-rows)",
                        layout.stats_rows as u32 + 3
                    )));
                }

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
                }
                // when there are more groups than rows we show the most active ones
                let last_activity: Vec<Option<u64>> =
                    groups.iter().map(|x| x.last_activity()).collect();
                shown_groups =
                    pick_groups(&last_activity, &shown_groups, layout.groups_shown.into());
                let groups_hidden = groups.len() - shown_groups.len();

                for (index, groupstats) in groups.iter_mut().enumerate() {
                    if !shown_groups.contains(&index) {
                        continue;
                    }
                    // never hide the anymous catch-all group
                    if !groupstats.group.is_empty() {
                        match groupstats.last_activity() {
                            Some(secs_ago) => {
                                if secs_ago > 60 {
                                    continue;
                                }
                            }
                            None => continue,
                        }
                    }

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
                if groups_hidden > 0 {
                    toflush_stats += &format!("-- ... and {groups_hidden} more\n");
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
                        //          ____________________________________ move cursor to beginning of line
                        //         |                     _______________ move cursor up X lines
                        //         |                    |      _________ clear to end of screen
                        //ormat!(" |                    |     |
                        &format!("\r{CSI}{lines_to_wipe}A{CSI}J")
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
    use crate::Layout;
    use crate::LineReader;
    use crate::Message;
    use crate::follow;
    use crate::get_statuscode_class;
    use crate::pick_groups;
    use smol::LocalExecutor;
    use smol::Timer;
    use smol::future;
    use std::fs::remove_file;
    use std::path::PathBuf;
    use std::process::Command;
    use std::str::from_utf8;
    use std::time::Duration;
    use std::time::Instant;
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

    #[test]
    fn test_no_busyloop_at_eof() {
        future::block_on(async {
            let tmpfile = TempFile::new();
            let symlink = format!("{}.symlink", tmpfile.filename);
            let rotated = format!("{}.1", tmpfile.filename);
            std::os::unix::fs::symlink(&tmpfile.filename, &symlink).unwrap();

            // A symlinked path doesn't match what /proc/self/fd/N points to,
            // which used to be mistaken for a rotation at every EOF
            let mut reader = LineReader::new(symlink.clone().into()).await.unwrap();
            let start = Instant::now();
            assert_eq!(reader.read_lines().await, Ok(vec![]));
            assert!(
                start.elapsed() >= Duration::from_millis(40),
                "read_lines() at EOF returned without waiting"
            );

            // Rotated away, but no new file has been created yet
            std::fs::rename(&tmpfile.filename, &rotated).unwrap();
            assert!(!reader.rotated().await);
            let start = Instant::now();
            assert_eq!(reader.read_lines().await, Ok(vec![]));
            assert!(
                start.elapsed() >= Duration::from_millis(40),
                "read_lines() at EOF of a rotated file returned without waiting"
            );

            // Once the new file shows up we should switch to it
            File::create(&tmpfile.filename).unwrap();
            assert!(reader.rotated().await);
            assert_eq!(reader.read_lines().await, Ok(vec![]));
            assert!(!reader.rotated().await);

            let _ = remove_file(symlink);
            let _ = remove_file(rotated);
        });
    }

    fn layout(stats_rows: u16, groups_shown: u16, number_of_lines: u16) -> Layout {
        Layout {
            stats_rows,
            groups_shown,
            number_of_lines,
        }
    }

    #[test]
    fn test_layout_automatic() {
        // everything fits in half of the screen
        assert_eq!(Layout::new(50, None, 10), layout(10, 10, 38));
        assert_eq!(Layout::new(50, None, 25), layout(25, 25, 23));
        // too many groups: 24 groups plus "... and 16 more"
        assert_eq!(Layout::new(50, None, 40), layout(25, 24, 23));
        // no groups registered yet
        assert_eq!(Layout::new(50, None, 0), layout(0, 0, 48));
        // tiny screens
        assert_eq!(Layout::new(5, None, 10), layout(2, 1, 1));
        assert_eq!(Layout::new(4, None, 10), layout(2, 1, 0));
        assert_eq!(Layout::new(1, None, 10), layout(1, 0, 0));
        assert_eq!(Layout::new(0, None, 10), layout(1, 0, 0));
    }

    #[test]
    fn test_layout_requested_file_rows() {
        // more rows requested than there are groups
        assert_eq!(Layout::new(50, Some(100), 10), layout(10, 10, 38));
        // more than half of the screen is fine
        assert_eq!(Layout::new(50, Some(40), 40), layout(40, 40, 8));
        // too many groups
        assert_eq!(Layout::new(50, Some(10), 40), layout(10, 9, 38));
        assert_eq!(Layout::new(50, Some(1), 40), layout(1, 0, 47));
        // doesn't fit on the screen
        assert_eq!(Layout::new(50, Some(48), 100), layout(48, 47, 0));
        assert_eq!(Layout::new(50, Some(100), 100), layout(100, 100, 0));
        assert_eq!(Layout::new(50, Some(100), 1000), layout(100, 99, 0));
        // doesn't fit, but there aren't enough groups to need all rows
        assert_eq!(Layout::new(50, Some(100), 47), layout(47, 47, 1));
        // lots of groups
        assert_eq!(Layout::new(50, Some(10), 100_000), layout(10, 9, 38));
    }

    #[test]
    fn test_pick_groups() {
        let activity = [Some(0), Some(3), None, Some(1), Some(100)];
        // first time: the most recently active ones, in their original order
        assert_eq!(pick_groups(&activity, &[], 2), vec![0, 3]);
        assert_eq!(pick_groups(&activity, &[], 3), vec![0, 1, 3]);
        // enough room for all
        assert_eq!(pick_groups(&activity, &[], 5), vec![0, 1, 2, 3, 4]);
        assert_eq!(pick_groups(&activity, &[], 10), vec![0, 1, 2, 3, 4]);
        assert_eq!(pick_groups(&activity, &[], 0), Vec::<usize>::new());
        assert_eq!(pick_groups(&[], &[], 3), Vec::<usize>::new());
    }

    #[test]
    fn test_pick_groups_is_stable() {
        // similar activity: keep showing what we showed before
        let activity = [Some(0), Some(3), Some(1), Some(2)];
        assert_eq!(pick_groups(&activity, &[1, 3], 2), vec![1, 3]);
        // a shown group went quiet: replace it with a more active one
        let activity = [Some(0), Some(10), Some(1), Some(2)];
        assert_eq!(pick_groups(&activity, &[1, 3], 2), vec![0, 3]);
        // both went quiet
        let activity = [Some(0), Some(10), Some(1), Some(20)];
        assert_eq!(pick_groups(&activity, &[1, 3], 2), vec![0, 2]);
        // never active
        let activity = [Some(0), None, Some(1), Some(2)];
        assert_eq!(pick_groups(&activity, &[1, 3], 2), vec![0, 3]);
        // more rows than last time: keep the old ones, add the most active
        let activity = [Some(0), Some(3), Some(1), Some(2)];
        assert_eq!(pick_groups(&activity, &[1, 3], 3), vec![0, 1, 3]);
        // fewer rows than last time: drop the least active
        assert_eq!(pick_groups(&activity, &[0, 1, 3], 2), vec![0, 3]);
    }
}
