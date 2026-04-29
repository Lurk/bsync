mod config;
mod filename_map;
mod gitignore;
mod pipeline;
mod service;
mod sync;
mod watcher;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bsync", about = "Bidirectional file sync daemon")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the sync daemon in foreground
    Run {
        /// Path to config file
        #[arg(long, default_value_os_t = config::default_config_path())]
        config: PathBuf,
        /// Write logs to a rotating file instead of stderr
        #[arg(long)]
        log_to_file: bool,
    },
    /// Validate config and print summary
    Validate {
        /// Path to config file
        #[arg(long, default_value_os_t = config::default_config_path())]
        config: PathBuf,
    },
    /// Install as system service (launchd/systemd)
    Install {
        /// Path to config file
        #[arg(long, default_value_os_t = config::default_config_path())]
        config: PathBuf,
    },
    /// Add a sync pair to the config
    Add {
        /// First glob pattern (e.g. ~/docs/**/*.md)
        #[arg(short)]
        a: PathBuf,
        /// Second glob pattern (e.g. ~/backup/**/*.md)
        #[arg(short)]
        b: PathBuf,
        /// Sync deletions
        #[arg(long)]
        delete: bool,
        /// Allow syncing empty files over non-empty files
        #[arg(long)]
        allow_empty_sync: bool,
        /// Shell command (sh -c) that transforms an A-side file into B-side content (stdin -> stdout)
        #[arg(long, requires = "b_to_a")]
        a_to_b: Option<String>,
        /// Shell command (sh -c) that transforms a B-side file into A-side content (stdin -> stdout)
        #[arg(long, requires = "a_to_b")]
        b_to_a: Option<String>,
        /// Per-file timeout (seconds) for the pipeline command. Defaults to 300.
        #[arg(long, requires = "a_to_b")]
        pipeline_timeout_secs: Option<u64>,
        /// Path to config file
        #[arg(long, default_value_os_t = config::default_config_path())]
        config: PathBuf,
    },
    /// Remove a sync pair from the config by number (as shown by validate)
    Remove {
        /// Pair number to remove (as shown by `bsync validate`)
        number: usize,
        /// Path to config file
        #[arg(long, default_value_os_t = config::default_config_path())]
        config: PathBuf,
    },
    /// Remove the system service
    Uninstall,
    /// Reload config (send SIGHUP to running daemon)
    Reload,
    /// Restart the daemon (to pick up a new binary)
    Restart,
}

fn init_logging(log_to_file: bool) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    if log_to_file {
        let log_dir = if cfg!(target_os = "macos") {
            let home = std::env::var("HOME").expect("HOME not set");
            PathBuf::from(home).join("Library/Logs/bsync")
        } else {
            let home = std::env::var("HOME").expect("HOME not set");
            PathBuf::from(home).join(".local/state/bsync")
        };
        std::fs::create_dir_all(&log_dir).expect("Failed to create log directory");

        let file_appender = tracing_appender::rolling::daily(&log_dir, "bsync.log");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(non_blocking)
            .with_ansi(false)
            .init();

        Some(guard)
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();

        None
    }
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            config,
            log_to_file,
        } => {
            let _guard = init_logging(log_to_file);
            run_sync_loop(&config);
        }
        Commands::Validate { config } => {
            let _guard = init_logging(false);
            println!("Config: {}", config.display());
            match config::validate_and_print(&config) {
                Ok(()) => {
                    println!("Config is valid.");
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("Validation error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Commands::Install { config } => {
            let _guard = init_logging(false);
            if let Err(e) = config::load(&config) {
                eprintln!("Config validation failed: {e}");
                std::process::exit(1);
            }

            let binary_path =
                std::env::current_exe().expect("Failed to get current executable path");
            let config_abs = std::fs::canonicalize(&config).expect("Failed to resolve config path");

            match service::install(&binary_path, &config_abs) {
                Ok(()) => println!("Service installed and started."),
                Err(e) => {
                    eprintln!("Failed to install service: {e}");
                    std::process::exit(1);
                }
            }
        }
        Commands::Add {
            a,
            b,
            delete,
            allow_empty_sync,
            a_to_b,
            b_to_a,
            pipeline_timeout_secs,
            config,
        } => {
            let _guard = init_logging(false);
            let a_str = a.display().to_string();
            let b_str = b.display().to_string();
            match config::add_pair(
                &config,
                &a_str,
                &b_str,
                delete,
                allow_empty_sync,
                a_to_b,
                b_to_a,
                pipeline_timeout_secs,
            ) {
                Ok((a_resolved, b_resolved)) => {
                    println!("Added pair: {} <-> {}", a_resolved, b_resolved);
                    println!("Config: {}", config.display());
                }
                Err(e) => {
                    eprintln!("Failed to add pair: {e}");
                    std::process::exit(1);
                }
            }
        }
        Commands::Remove { number, config } => {
            let _guard = init_logging(false);
            match config::remove_pair(&config, number) {
                Ok(removed) => {
                    println!("Removed pair #{}: {} <-> {}", number, removed.a, removed.b);
                    println!("Config: {}", config.display());
                }
                Err(e) => {
                    eprintln!("Failed to remove pair: {e}");
                    std::process::exit(1);
                }
            }
        }
        Commands::Uninstall => {
            let _guard = init_logging(false);
            match service::uninstall() {
                Ok(()) => println!("Service uninstalled."),
                Err(e) => {
                    eprintln!("Failed to uninstall service: {e}");
                    std::process::exit(1);
                }
            }
        }
        Commands::Reload => {
            let _guard = init_logging(false);
            match service::reload() {
                Ok(()) => println!("Reload signal sent."),
                Err(e) => {
                    eprintln!("Failed to send reload signal: {e}");
                    std::process::exit(1);
                }
            }
        }
        Commands::Restart => {
            let _guard = init_logging(false);
            match service::restart() {
                Ok(()) => println!("Service restarted."),
                Err(e) => {
                    eprintln!("Failed to restart service: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}

fn run_sync_loop(config_path: &Path) {
    // Set up signal handlers once (they persist across reloads)
    let shutdown = Arc::new(AtomicBool::new(false));
    let reload = Arc::new(AtomicBool::new(false));

    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown))
        .expect("Failed to register SIGTERM handler");
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown))
        .expect("Failed to register SIGINT handler");
    signal_hook::flag::register(signal_hook::consts::SIGHUP, Arc::clone(&reload))
        .expect("Failed to register SIGHUP handler");

    if let Err(e) = service::write_pid_file() {
        tracing::warn!("Failed to write PID file: {e}");
    }

    let pairs = match config::load(config_path) {
        Ok(pairs) => pairs,
        Err(e) => {
            tracing::error!("Failed to load config: {e}");
            std::process::exit(1);
        }
    };

    let mut pairs = pairs;
    let (mut tx, mut rx) = std::sync::mpsc::channel();
    let mut _watchers;

    'reload: loop {
        let gi_caches: Vec<Option<Arc<Mutex<gitignore::GitignoreCache>>>> = pairs
            .iter()
            .map(|pair| {
                if pair.has_glob {
                    Some(Arc::new(Mutex::new(gitignore::GitignoreCache::new(
                        pair.a_base.clone(),
                        pair.b_base.clone(),
                    ))))
                } else {
                    None
                }
            })
            .collect();

        // Set up watchers before initial sync so events queue in the channel
        _watchers = match watcher::setup_watchers(&pairs, &gi_caches, tx.clone()) {
            Ok(w) => w,
            Err(e) => {
                tracing::error!("Failed to set up watchers: {e}");
                std::process::exit(1);
            }
        };

        for (pair, gi_cache) in pairs.iter().zip(gi_caches.iter()) {
            if let Err(e) = sync::initial_sync(pair, gi_cache.as_ref()) {
                tracing::error!(
                    "Initial sync failed for {} <-> {}: {e}",
                    pair.a_pattern,
                    pair.b_pattern
                );
            }
        }

        tracing::info!("Watching {} pair(s)...", pairs.len());

        loop {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(event) => {
                    let pair = &pairs[event.pair_index];
                    let dest = match event.side {
                        watcher::Side::A => pair.a_to_b_path(&event.relative),
                        watcher::Side::B => pair.b_to_a_path(&event.relative),
                    };
                    let Some(dest) = dest else {
                        tracing::debug!(
                            "No mapping for {} (pattern mismatch)",
                            event.path.display()
                        );
                        continue;
                    };

                    let pipeline = match (&pair.content, event.side) {
                        (config::ContentTransform::Identity, _) => None,
                        (
                            config::ContentTransform::Command {
                                a_to_b, timeout, ..
                            },
                            watcher::Side::A,
                        ) => Some((a_to_b.as_str(), *timeout)),
                        (
                            config::ContentTransform::Command {
                                b_to_a, timeout, ..
                            },
                            watcher::Side::B,
                        ) => Some((b_to_a.as_str(), *timeout)),
                    };

                    match event.kind {
                        watcher::SyncEventKind::CreateOrModify => {
                            if sync::mtimes_match(&event.path, &dest) {
                                tracing::debug!(
                                    "Skipping {} -> {} (already up to date)",
                                    event.path.display(),
                                    dest.display()
                                );
                                continue;
                            }
                            match sync::sync_file(
                                &event.path,
                                &dest,
                                pair.allow_empty_sync,
                                pipeline,
                            ) {
                                Ok(sync::SyncOutcome::Synced) => {}
                                Ok(sync::SyncOutcome::SkippedEmpty) => {
                                    if let Some((c, _)) = pipeline {
                                        tracing::warn!(
                                            "Pipeline command '{c}' produced empty output for {} -> {}; destination preserved",
                                            event.path.display(),
                                            dest.display()
                                        );
                                    }
                                }
                                Err(e) => {
                                    tracing::error!(
                                        "Sync failed {} -> {}: {e}",
                                        event.path.display(),
                                        dest.display()
                                    );
                                }
                            }
                        }
                        watcher::SyncEventKind::Delete => {
                            if !pair.sync_deletions {
                                tracing::debug!(
                                    "Ignoring delete event for {} (sync_deletions=false)",
                                    event.path.display()
                                );
                                continue;
                            }
                            // sync_file's atomic rename fires a Remove event on
                            // the dest path even when the file is being
                            // replaced, not truly deleted. If the source path
                            // still exists, this is an echo — drop it.
                            // Symmetric to the mtimes_match guard above.
                            if event.path.exists() {
                                tracing::debug!(
                                    "Skipping delete echo for {} (path still exists)",
                                    event.path.display()
                                );
                                continue;
                            }
                            if let Err(e) = sync::sync_delete(&dest) {
                                tracing::error!("Delete sync failed {}: {e}", dest.display());
                            }
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Idle wakeup so the shutdown / reload signal flags below get polled.
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    tracing::error!("All watcher channels disconnected");
                    service::remove_pid_file();
                    return;
                }
            }

            if shutdown.load(Ordering::Relaxed) {
                tracing::info!("Shutdown signal received");
                service::remove_pid_file();
                return;
            }

            if reload.load(Ordering::Relaxed) {
                reload.store(false, Ordering::Relaxed);
                tracing::info!("Reload signal received, reloading config...");

                match config::load(config_path) {
                    Ok(new_pairs) => {
                        pairs = new_pairs;
                        let (new_tx, new_rx) = std::sync::mpsc::channel();
                        tx = new_tx;
                        rx = new_rx;
                        continue 'reload;
                    }
                    Err(e) => {
                        tracing::error!("Failed to reload config: {e}");
                        tracing::warn!("Keeping previous config");
                    }
                }
            }
        }
    }
}
