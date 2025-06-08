use nginx_tail::Error;
use nginx_tail::Message;
use nginx_tail::SenderChannel;
use nginx_tail::follow;
use nginx_tail::get_statuscode_class;
use nginx_tail::periodic_print;
use nginx_tail::process_as_streaming;
use nginx_tail::process_as_tui;
use nginx_tail::terminal::colors::CSI;
use nginx_tail::terminal::get_terminal_height;
use nginx_tail::terminal::get_terminal_width;
use smol::LocalExecutor;
use smol::future;
use smol::{Timer, channel::bounded};
use std::fs::read_dir;
use std::io::IsTerminal;
use std::process;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::vec;
use std::{path::PathBuf, time::Duration};

const HELP: &str = r#"
    Usage:
        [ --option | ... ] [ file |  dir | ... ]

    Options:
        -h, --help               Show this help message
            --max-width X        Cut lines to this length X.
                                 Defaults to "screen width", set to 0 for unlimited
            --target-height      Target window height.
                                 Will be met if the log lines fit in the width of your terminal
            --max-runtime X      Terminate after X seconds
            --combine            Combine stats of all files together
            --merge              Combine http statuscodes in groups
            --filter X           Only show log lines matching this status code.
                                 Can be used multiple times, "4xx" can be used to show 403, 404 etc.
                                 The statistics are not affected by this option.
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
    merge_statuscodes: bool,
    max_runtime: Option<u32>,
    requested_width: Option<u16>,
    filters: Vec<String>,
    streaming_output: bool,
}

fn main() {
    let mut pargs = pico_args::Arguments::from_env();
    if pargs.contains(["-h", "--help"]) {
        println!("{HELP}");
        std::process::exit(0);
    }

    let max_runtime: Option<u32> = pargs.opt_value_from_str("--max-runtime").unwrap_or(None);

    // TODO: let the user specify --loglines instead: with dynamic tags you don't know the right screenheight
    let target_height: u16 = pargs
        .value_from_str("--target-height")
        .unwrap_or_else(get_terminal_height);

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
    let merge_statuscodes: bool = pargs.contains("--merge");

    let mut filters = vec![];
    while let Ok(filter) = pargs.value_from_str::<&str, String>("--filter") {
        filters.push(filter.trim_end_matches("x").to_owned());
    }
    filters.sort();
    filters.dedup();

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
            eprintln!("Unknown option {lossy}\nUse ./-- if your local path starts with --");
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
        merge_statuscodes,
        max_runtime,
        requested_width,
        filters,
        streaming_output: !std::io::stdout().is_terminal(),
    };

    match smol::block_on(innermain(args)) {
        Ok(_) => {}
        Err(ex) => eprintln!("{ex}"),
    }
}

async fn sigwinch_handler(channel: SenderChannel) {
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

async fn sigint_handler() {
    let terminated = Arc::new(AtomicBool::new(false));
    let Ok(_) = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&terminated))
    else {
        eprintln!("Failed to register signal handler for INT");
        return;
    };
    loop {
        if terminated.load(std::sync::atomic::Ordering::Relaxed) {
            println!("{CSI}?25h\nBye"); // show cursor
            process::exit(0)
        }
        Timer::after(Duration::from_millis(50)).await;
    }
}

async fn innermain(args: AppArgs) -> Result<(), Error> {
    // channel to send messages to the processing thread
    let (sender, receiver) = bounded(1_000_000);
    let async_exec = LocalExecutor::new();
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
                match args.merge_statuscodes {
                    false => |x| Some(x.to_owned()),
                    true => get_statuscode_class,
                },
            ))
            .detach();
    }

    #[cfg(debug_assertions)]
    {
        if args.fast_generator {
            use nginx_tail::fake_fast;

            println!("Fast generator enabled");
            async_exec.spawn(fake_fast(sender.clone())).detach();
        }
        if args.slow_generator {
            use nginx_tail::fake_slow;

            println!("Slow generator enabled");
            async_exec.spawn(fake_slow(sender.clone())).detach();
        }
    }

    if args.max_runtime.is_some() {
        let max_runtime = args.max_runtime.unwrap();
        async_exec
            .spawn(async move {
                Timer::after(Duration::from_secs(max_runtime.into())).await;
                std::process::exit(0);
            })
            .detach();
    }

    if args.streaming_output {
        // just syntax highlighting (and filtering)
        future::block_on(async_exec.run(process_as_streaming(receiver, args.filters)))
    } else {
        // terminal with live updating stats
        async_exec.spawn(sigint_handler()).detach();
        if args.requested_width.is_none() {
            async_exec.spawn(sigwinch_handler(sender.clone())).detach();
        };
        async_exec.spawn(periodic_print(sender.clone())).detach();

        future::block_on(async_exec.run(process_as_tui(
            receiver,
            args.target_height,
            args.requested_width,
            args.filters,
        )));
    }

    Ok(())
}
