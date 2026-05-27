use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use tracing::debug;
use tracing_subscriber::EnvFilter;
use winland_core::{Manageability, MonitorInfo, WindowInfo, tile_windows};
use winland_ipc::{
    DaemonStateSnapshot, IpcRequest, IpcResponseResult, decode_response, encode_request,
};

#[derive(Debug, Parser)]
#[command(name = "winland")]
#[command(about = "Windows-native tiling window manager experiments")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect the running daemon through local IPC.
    State(StateArgs),
    /// List discovered desktop windows and explain Winland's conservative filter.
    Windows(WindowsArgs),
    /// Arrange manageable windows on the primary monitor once.
    TileOnce,
    /// Inspect or validate Winland configuration.
    Config(ConfigArgs),
}

#[derive(Debug, Args)]
struct StateArgs {
    /// Print the daemon state snapshot as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct WindowsArgs {
    /// Show only windows that Winland would currently consider manageable.
    #[arg(long)]
    manageable_only: bool,
}

#[derive(Debug, Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Validate a Winland TOML config file without mutating daemon state.
    Validate(ConfigValidateArgs),
}

#[derive(Debug, Args)]
struct ConfigValidateArgs {
    /// Config file to validate. If omitted, Winland uses discovery paths or defaults.
    #[arg(short, long)]
    path: Option<PathBuf>,
}

fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();

    match cli.command {
        Command::State(args) => daemon_state(args),
        Command::Windows(args) => list_windows(args),
        Command::TileOnce => tile_once(),
        Command::Config(args) => handle_config(args),
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .init();
}

fn list_windows(args: WindowsArgs) -> Result<()> {
    let windows = winland_win32::enumerate_windows()?;
    debug!(total = windows.len(), "received windows from win32 backend");

    let listed: Vec<_> = windows
        .iter()
        .filter(|window| !args.manageable_only || window.is_manageable())
        .collect();

    print_table(&listed);
    Ok(())
}

fn handle_config(args: ConfigArgs) -> Result<()> {
    match args.command {
        ConfigCommand::Validate(args) => validate_config(args),
    }
}

fn validate_config(args: ConfigValidateArgs) -> Result<()> {
    let loaded = winland_config::load_or_default(args.path.as_deref())?;
    match loaded.path {
        Some(path) => println!("Config is valid: {}", path.display()),
        None => println!("Config is valid: no config file found, using built-in defaults."),
    }

    Ok(())
}

fn daemon_state(args: StateArgs) -> Result<()> {
    let request = encode_request(&IpcRequest::state())?;
    let response =
        match winland_win32::send_ipc_request(winland_win32::DEFAULT_IPC_PIPE_NAME, &request) {
            Ok(response) => response,
            Err(winland_win32::Win32Error::DaemonNotRunning { .. }) => {
                eprintln!(
                    "Winland daemon is not running. Start winland-daemon before using IPC commands."
                );
                std::process::exit(2);
            }
            Err(error) => return Err(error.into()),
        };

    match decode_response(&response)?.result {
        IpcResponseResult::State(snapshot) if args.json => {
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
        }
        IpcResponseResult::State(snapshot) => {
            println!("{}", format_state_snapshot(&snapshot));
        }
        IpcResponseResult::Error(error) => {
            return Err(anyhow::anyhow!("daemon IPC error: {}", error.message));
        }
    }

    Ok(())
}

fn tile_once() -> Result<()> {
    let windows = winland_win32::enumerate_windows()?;
    let monitors = winland_win32::enumerate_monitors()?;
    let monitor = primary_monitor(&monitors)?;

    let selected: Vec<_> = windows
        .iter()
        .filter(|window| window.is_manageable())
        .filter(|window| monitor.rect.contains(window.rect.center()))
        .collect();
    let handles: Vec<_> = selected.iter().map(|window| window.handle).collect();
    let assignments = tile_windows(monitor.work_area, &handles);

    if assignments.is_empty() {
        println!(
            "No manageable windows found on primary monitor {} (work area {}).",
            monitor.id, monitor.work_area
        );
        return Ok(());
    }

    println!(
        "Tiling {} window(s) on primary monitor {} (work area {}).",
        assignments.len(),
        monitor.id,
        monitor.work_area
    );

    let mut failures = Vec::new();
    let mut rows = Vec::new();
    for assignment in &assignments {
        let title = selected
            .iter()
            .find(|window| window.handle == assignment.window)
            .map(|window| truncate(&window.title, 48))
            .unwrap_or_else(|| "-".to_owned());

        match winland_win32::move_resize_window(assignment.window, assignment.rect) {
            Ok(()) => rows.push(vec![
                assignment.window.to_string(),
                title,
                assignment.rect.to_string(),
                "ok".to_owned(),
            ]),
            Err(error) => {
                let message = error.to_string();
                failures.push((assignment.window, message.clone()));
                rows.push(vec![
                    assignment.window.to_string(),
                    title,
                    assignment.rect.to_string(),
                    truncate(&message, 64),
                ]);
            }
        }
    }

    print_move_results(&rows);

    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "failed to move {} of {} window(s)",
            failures.len(),
            assignments.len()
        ))
    }
}

fn print_table(windows: &[&WindowInfo]) {
    if windows.is_empty() {
        println!("No windows found.");
        return;
    }

    let rows: Vec<Vec<String>> = windows
        .iter()
        .map(|window| {
            vec![
                window.handle.to_string(),
                manageability_label(window).to_owned(),
                manageability_reason(window).to_owned(),
                truncate(&window.title, 48),
                truncate(&window.class_name, 28),
                window.process_id.to_string(),
                truncate(window.executable_path.as_deref().unwrap_or("-"), 56),
                window.rect.to_string(),
                format!("0x{:08X}", window.styles.style),
                format!("0x{:08X}", window.styles.extended_style),
                yes_no(window.is_visible).to_owned(),
                yes_no(window.is_minimized).to_owned(),
                yes_no(window.is_dwm_cloaked).to_owned(),
                yes_no(window.has_owner).to_owned(),
                yes_no(window.is_tool_window).to_owned(),
            ]
        })
        .collect();

    let headers = [
        "HWND",
        "Status",
        "Reason",
        "Title",
        "Class",
        "PID",
        "Executable Path",
        "Rect",
        "Style",
        "ExStyle",
        "Visible",
        "Minimized",
        "Cloaked",
        "Owner",
        "Tool",
    ];
    let widths = column_widths(&headers, &rows);

    print_row(&headers, &widths);
    print_separator(&widths);
    for row in rows {
        print_row(&row, &widths);
    }
}

fn print_move_results(rows: &[Vec<String>]) {
    let headers = ["HWND", "Title", "Target Rect", "Result"];
    let widths = column_widths(&headers, rows);

    print_row(&headers, &widths);
    print_separator(&widths);
    for row in rows {
        print_row(row, &widths);
    }
}

fn primary_monitor(monitors: &[MonitorInfo]) -> Result<&MonitorInfo> {
    monitors
        .iter()
        .find(|monitor| monitor.is_primary)
        .or_else(|| monitors.first())
        .ok_or_else(|| anyhow::anyhow!("no monitors were discovered"))
}

fn column_widths(headers: &[&str], rows: &[Vec<String>]) -> Vec<usize> {
    let mut widths: Vec<_> = headers.iter().map(|header| header.len()).collect();

    for row in rows {
        for (index, value) in row.iter().enumerate() {
            widths[index] = widths[index].max(value.len());
        }
    }

    widths
}

fn print_row<T: AsRef<str>>(values: &[T], widths: &[usize]) {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            print!("  ");
        }

        print!("{:<width$}", value.as_ref(), width = widths[index]);
    }

    println!();
}

fn print_separator(widths: &[usize]) {
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            print!("  ");
        }

        print!("{}", "-".repeat(*width));
    }

    println!();
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn manageability_label(window: &WindowInfo) -> &'static str {
    if window.is_manageable() {
        "manage"
    } else {
        "skip"
    }
}

fn manageability_reason(window: &WindowInfo) -> &'static str {
    match window.manageable_reason() {
        Manageability::Manageable => "-",
        Manageability::Unmanageable(reason) => reason,
    }
}

fn format_state_snapshot(snapshot: &DaemonStateSnapshot) -> String {
    let foreground = snapshot
        .foreground_window
        .map(|handle| format!("0x{handle:X}"))
        .unwrap_or_else(|| "-".to_owned());

    format!(
        "Winland daemon is running (IPC protocol v{}).\nWindows: {} total, {} manageable, {} floating, {} temporary floating\nWorkspace: active {}\nForeground: {}",
        winland_ipc::PROTOCOL_VERSION,
        snapshot.total_windows,
        snapshot.manageable_windows,
        snapshot.floating_windows,
        snapshot.temporary_floating_windows,
        snapshot.active_workspace,
        foreground
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_snapshot_format_is_human_readable() {
        let snapshot = DaemonStateSnapshot {
            total_windows: 3,
            manageable_windows: 2,
            floating_windows: 1,
            temporary_floating_windows: 0,
            active_workspace: 4,
            foreground_window: Some(0xCAFE),
        };

        let output = format_state_snapshot(&snapshot);

        assert!(output.contains("IPC protocol v1"));
        assert!(output.contains("3 total, 2 manageable"));
        assert!(output.contains("Workspace: active 4"));
        assert!(output.contains("Foreground: 0xCAFE"));
    }
}
