use hades_kat_bot::{bot::TradingBot, config::Config};

use clap::Parser;
use log::{error, info};
use std::process;

const LOCKFILE: &str = "bot.lock";

#[derive(Parser)]
#[command(name = "solana-trading-bot")]
#[command(about = "A Solana PumpFun trading bot with PumpPortal integration")]
struct Cli {
    /// Configuration file path
    #[arg(short, long, default_value = "config.toml")]
    config: String,

    /// Enable debug logging
    #[arg(short, long)]
    debug: bool,

    /// Dry run mode (no actual trades)
    #[arg(long)]
    dry_run: bool,
}

/// Check if a process with the given PID is still running
fn is_pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use std::process::Command;
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH"])
            .output()
            .map(|o| {
                let out = String::from_utf8_lossy(&o.stdout);
                !out.contains("No tasks") && out.contains(&pid.to_string())
            })
            .unwrap_or(false)
    }
}

/// Acquire the PID lockfile. Returns Err if another instance is already running.
fn acquire_lock() -> Result<(), String> {
    if let Ok(contents) = std::fs::read_to_string(LOCKFILE) {
        if let Ok(pid) = contents.trim().parse::<u32>() {
            if is_pid_alive(pid) {
                return Err(format!(
                    "Another bot instance is already running (PID {}). \
                     Kill it first or delete {} if stale.",
                    pid, LOCKFILE
                ));
            }
            // Stale lockfile — old process is dead, safe to overwrite
            info!("Removing stale lockfile (PID {} no longer running)", pid);
        }
    }

    let my_pid = process::id();
    std::fs::write(LOCKFILE, my_pid.to_string())
        .map_err(|e| format!("Failed to write lockfile: {}", e))?;
    info!("Acquired lockfile (PID {})", my_pid);
    Ok(())
}

/// Remove the lockfile on shutdown
fn release_lock() {
    if let Err(e) = std::fs::remove_file(LOCKFILE) {
        error!("Failed to remove lockfile: {}", e);
    }
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();

    // Initialize logging
    let log_level = if cli.debug { "debug" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level)).init();

    info!("hades-kat-bot v0.1.6");

    // Prevent dual-instance: acquire PID lockfile
    if let Err(e) = acquire_lock() {
        error!("{}", e);
        process::exit(1);
    }

    // Load configuration
    let config = match Config::load(&cli.config) {
        Ok(config) => {
            info!("Configuration loaded successfully");
            config
        }
        Err(e) => {
            error!("Failed to load configuration: {}", e);
            release_lock();
            process::exit(1);
        }
    };

    // Create and run the trading bot
    let mut bot = match TradingBot::new(config, cli.dry_run).await {
        Ok(bot) => {
            info!("Trading bot initialized successfully");
            bot
        }
        Err(e) => {
            error!("Failed to initialize trading bot: {}", e);
            release_lock();
            process::exit(1);
        }
    };

    // Run the bot
    let result = bot.run().await;
    release_lock();

    if let Err(e) = result {
        error!("Bot error: {}", e);
        process::exit(1);
    }
}
