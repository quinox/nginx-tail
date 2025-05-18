use nginx_top::InstantSpeedometer;
use nginx_top::RingbufferSpeedometer;
use nginx_top::SmootherSpeedometer;
use nginx_top::Speedometer;
use smol::{
    Timer,
    channel::{Receiver, Sender, bounded},
    fs::read_dir,
    stream::StreamExt,
};
use std::{fmt::Display, path::PathBuf, time::Duration};

const HELP: &str = r#"
    Usage:
        --log-root <path>  Path to the nginx log directory
        -h, --help         Show this help message
"#;

#[derive(Debug)]
struct AppArgs {
    log_dir: std::path::PathBuf,
    #[cfg(debug_assertions)]
    fast_generator: bool,
    #[cfg(debug_assertions)]
    slow_generator: bool,
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

async fn process(channel: Receiver<String>) {
    let mut instant_rate = InstantSpeedometer::new();
    let mut fast_rate = RingbufferSpeedometer::new(2 << 1);
    let mut slow_rate = RingbufferSpeedometer::new(2 << 8);
    let mut smooth_rate = SmootherSpeedometer::new(0.2);
    let start = std::time::Instant::now();

    loop {
        let last_timestamp = std::time::Instant::now();
        Timer::after(Duration::from_secs(1)).await;
        let time_passed = std::time::Instant::now()
            .duration_since(last_timestamp)
            .as_millis();

        let mut count = 0;
        while let Ok(_msg) = channel.try_recv() {
            count += 1;
            // println!("I got: {}", msg);
        }
        instant_rate.add_measurement(time_passed, count);
        fast_rate.add_measurement(time_passed, count);
        slow_rate.add_measurement(time_passed, count);
        smooth_rate.add_measurement(time_passed, count);

        println!(
            "{:6.1} msg/s [instant]   {:6.1} msg/s [fast-ring]   {:6.1} msg/s [slow-ring]   {:6.1} msg/s [smooth]",
            instant_rate.get_speed(),
            fast_rate.get_speed(),
            slow_rate.get_speed(),
            smooth_rate.get_speed(),
        );
        if std::time::Instant::now().duration_since(start).as_secs() > 15 {
            println!("Exiting after 10 seconds");
            break;
        }
    }
}

async fn innermain(args: AppArgs) -> Result<(), Error> {
    let (sender, receiver) = bounded(10000);

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
