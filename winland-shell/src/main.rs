use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "winland-shell")]
#[command(about = "Experimental Winland user shell entrypoint")]
struct Cli {
    /// Path to winland-daemon.exe. Defaults to winland-daemon.exe next to this shell.
    #[arg(long)]
    daemon: Option<PathBuf>,
    /// Start the daemon elevated so it can tile elevated windows.
    #[arg(long)]
    elevated_daemon: bool,
}

fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let daemon = cli.daemon.unwrap_or(default_daemon_path()?);

    info!(
        daemon = %daemon.display(),
        "starting experimental Winland shell; DWM remains the compositor"
    );

    if cli.elevated_daemon {
        if winland_win32::elevated_daemon_task_installed()
            .context("query elevated daemon scheduled task")?
        {
            info!("starting elevated Winland daemon through scheduled task");
            winland_win32::run_elevated_daemon_task()
                .context("start elevated daemon scheduled task")?;
            wait_as_shell_host();
        } else {
            info!(
                daemon = %daemon.display(),
                "requesting elevated Winland daemon through UAC"
            );
            let exit_code = winland_win32::launch_elevated_process_and_wait(&daemon, &[])
                .with_context(|| {
                    format!("start elevated Winland daemon at {}", daemon.display())
                })?;
            std::process::exit(exit_code as i32);
        }
    } else {
        let status = Command::new(&daemon)
            .status()
            .with_context(|| format!("start Winland daemon at {}", daemon.display()))?;

        exit_with_daemon_status(status);
    }
}

fn wait_as_shell_host() -> ! {
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

fn default_daemon_path() -> Result<PathBuf> {
    executable_next_to_current("winland-daemon")
}

fn executable_next_to_current(stem: &str) -> Result<PathBuf> {
    let current = std::env::current_exe().context("resolve current shell executable")?;
    let extension = current.extension().and_then(|value| value.to_str());
    let file_name = match extension {
        Some(extension) if !extension.is_empty() => format!("{stem}.{extension}"),
        _ => stem.to_owned(),
    };

    Ok(current.with_file_name(file_name))
}

fn exit_with_daemon_status(status: ExitStatus) -> ! {
    match status.code() {
        Some(code) => std::process::exit(code),
        None => std::process::exit(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_executable_path_uses_current_extension() {
        let path = executable_next_to_current("winland-daemon").unwrap();

        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("winland-daemon")
        );
    }
}
