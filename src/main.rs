use smol::{
    Timer,
    channel::{Receiver, Sender, bounded},
    fs::{read, read_dir},
    stream::StreamExt,
};
use std::{fmt::Display, path::PathBuf, time::Duration};

const HELP: &str = r#"
    Usage:
        --log-root <path>  Path to the nginx log directory
        --fast-generator   Activate the fast generator (for testing)
        --slow-generator   Activate the slow generator (for testing)
        -h, --help         Show this help message
"#;

#[derive(Debug)]
struct AppArgs {
    fast_generator: bool,
    slow_generator: bool,
    log_dir: std::path::PathBuf,
}

fn parse_path(s: &std::ffi::OsStr) -> Result<std::path::PathBuf, &'static str> {
    Ok(s.into())
}

fn main() {
    let mut pargs = pico_args::Arguments::from_env();
    if pargs.contains(["-h", "--help"]) {
        println!("{}", HELP);
        std::process::exit(0);
    }

    let log_dir = pargs
        .opt_value_from_os_str("--log-root", parse_path)
        .unwrap()
        .unwrap_or_else(|| "/var/log/nginx/".into());

    let args = AppArgs {
        fast_generator: pargs.contains("--fast"),
        slow_generator: pargs.contains("--slow"),
        log_dir,
    };
    match smol::block_on(innermain(args)) {
        Ok(_) => {}
        Err(ex) => eprintln!("Runtime failure: {}", ex),
    }
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
    Timer::after(Duration::from_secs(1)).await;
    channel.send(format!("Slept for {:?}", file)).await.unwrap();
}

#[cfg(debug_assertions)]
async fn fake_slow(channel: Sender<String>) {
    loop {
        Timer::after(Duration::from_secs(2)).await;
        channel.send("Fake slow msg".to_string()).await.unwrap();
    }
}
#[cfg(debug_assertions)]
async fn fake_fast(channel: Sender<String>) {
    loop {
        Timer::after(Duration::from_millis(5)).await;
        channel.send("Fake fast msg".to_string()).await.unwrap();
    }
}

async fn process(channel: Receiver<String>) {
    let mut count = 0;
    loop {
        let msg = channel.recv().await.unwrap();
        count += 1;
        println!("I got: {}", msg);
        if count == 2 {
            println!("We've seen 2 messsages, that's plenty for now");
            return;
        }
    }
}

async fn innermain(args: AppArgs) -> Result<(), Error> {
    let (sender, receiver) = bounded(100);

    let mut dirs_to_check = vec![args.log_dir];

    let mut readers = 0;

    #[cfg(debug_assertions)]
    {
        if args.fast_generator {
            println!("Fast generator enabled");
            smol::spawn(fake_fast(sender.clone())).detach();
            readers += 1;
        }
        if args.slow_generator {
            println!("Slow generator enabled");
            smol::spawn(fake_slow(sender.clone())).detach();
            readers += 1;
        }
    }

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
                        readers += 1;
                    }
                }
            }
        }
    }
    if readers == 0 {
        return Err(Error("No useable log files found".to_string()));
    }

    smol::spawn(process(receiver)).await;
    Ok(())
}
