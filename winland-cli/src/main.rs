use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use tracing::debug;
use tracing_subscriber::EnvFilter;
use winland_core::{
    GameModeReason, Manageability, MonitorInfo, WindowHandle, WindowInfo, WorkspaceId,
    detect_game_mode, tile_windows_with_config,
};
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
    /// List discovered monitors and their stable Winland IDs.
    Monitors,
    /// Explain fullscreen and game-mode handling for the foreground or selected window.
    DiagnoseWindow(DiagnoseWindowArgs),
    /// Arrange manageable windows on the primary monitor once.
    TileOnce,
    /// Inspect or validate Winland configuration.
    Config(ConfigArgs),
    /// Experimental per-user shell replacement commands.
    Shell(ShellArgs),
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
struct DiagnoseWindowArgs {
    /// HWND to diagnose, as decimal or hex with 0x prefix. Defaults to foreground.
    #[arg(long)]
    hwnd: Option<String>,
}

#[derive(Debug, Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Debug, Args)]
struct ShellArgs {
    #[command(subcommand)]
    command: ShellCommand,
}

#[derive(Debug, Subcommand)]
enum ShellCommand {
    /// Show the current experimental shell replacement status.
    Status,
    /// Install Winland as the per-user experimental shell.
    Install(ShellInstallArgs),
    /// Restore the previous per-user shell value captured by install.
    Uninstall(ShellExperimentalArgs),
    /// Recovery alias for uninstall when running from shell mode.
    Recover(ShellExperimentalArgs),
    /// Launch Winland shell mode for this session without changing the registry.
    Test(ShellLaunchArgs),
    /// Create or update the no-prompt elevated daemon scheduled task.
    InstallElevatedTask(ShellElevatedTaskInstallArgs),
    /// Delete the elevated daemon scheduled task.
    UninstallElevatedTask(ShellExperimentalArgs),
    /// Show whether the elevated daemon scheduled task exists.
    ElevatedTaskStatus,
    /// Launch Explorer manually from Winland shell mode.
    Explorer,
}

#[derive(Debug, Args)]
struct ShellInstallArgs {
    /// Required acknowledgement for persistent experimental shell changes.
    #[arg(long)]
    experimental: bool,
    /// Path to winland-shell.exe. Defaults to winland-shell.exe next to this CLI.
    #[arg(long, conflicts_with = "command")]
    shell: Option<PathBuf>,
    /// Path to winland-daemon.exe passed through to winland-shell.
    #[arg(long, conflicts_with = "command")]
    daemon: Option<PathBuf>,
    /// Ask winland-shell to start the daemon elevated so elevated windows can be tiled.
    #[arg(long, conflicts_with = "command")]
    elevated_daemon: bool,
    /// Full shell command to write. Intended for VM experiments and tests.
    #[arg(long)]
    command: Option<String>,
}

#[derive(Debug, Args)]
struct ShellExperimentalArgs {
    /// Required acknowledgement for persistent experimental shell changes.
    #[arg(long)]
    experimental: bool,
}

#[derive(Debug, Args)]
struct ShellLaunchArgs {
    /// Path to winland-shell.exe. Defaults to winland-shell.exe next to this CLI.
    #[arg(long, conflicts_with = "command")]
    shell: Option<PathBuf>,
    /// Path to winland-daemon.exe passed through to winland-shell.
    #[arg(long, conflicts_with = "command")]
    daemon: Option<PathBuf>,
    /// Ask winland-shell to start the daemon elevated so elevated windows can be tiled.
    #[arg(long, conflicts_with = "command")]
    elevated_daemon: bool,
    /// Full shell command to launch. Intended for VM experiments and tests.
    #[arg(long)]
    command: Option<String>,
}

#[derive(Debug, Args)]
struct ShellElevatedTaskInstallArgs {
    /// Required acknowledgement for persistent experimental scheduled task changes.
    #[arg(long)]
    experimental: bool,
    /// Path to winland-daemon.exe. Defaults to winland-daemon.exe next to this CLI.
    #[arg(long)]
    daemon: Option<PathBuf>,
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
        Command::Monitors => list_monitors(),
        Command::DiagnoseWindow(args) => diagnose_window(args),
        Command::TileOnce => tile_once(),
        Command::Config(args) => handle_config(args),
        Command::Shell(args) => handle_shell(args),
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

fn list_monitors() -> Result<()> {
    let monitors = winland_win32::enumerate_monitors()?;
    if monitors.is_empty() {
        println!("No monitors found.");
        return Ok(());
    }

    let rows: Vec<Vec<String>> = monitors
        .iter()
        .map(|monitor| {
            vec![
                monitor.id.to_string(),
                yes_no(monitor.is_primary).to_owned(),
                monitor.rect.to_string(),
                monitor.work_area.to_string(),
            ]
        })
        .collect();
    let headers = ["ID", "Primary", "Rect", "Work Area"];
    let widths = column_widths(&headers, &rows);

    print_row(&headers, &widths);
    print_separator(&widths);
    for row in rows {
        print_row(&row, &widths);
    }

    Ok(())
}

fn diagnose_window(args: DiagnoseWindowArgs) -> Result<()> {
    let loaded_config = winland_config::load_or_default(None)?;
    let windows = winland_win32::enumerate_windows()?;
    let monitors = winland_win32::enumerate_monitors()?;
    let rules = loaded_config.config.window_rules()?;
    let handle = match args.hwnd.as_deref() {
        Some(input) => parse_hwnd(input)?,
        None => winland_win32::foreground_window()?
            .ok_or_else(|| anyhow::anyhow!("no foreground window is available"))?,
    };
    let window = windows
        .iter()
        .find(|window| window.handle == handle)
        .ok_or_else(|| anyhow::anyhow!("window {handle} was not found in enumeration"))?;
    let policy = loaded_config.config.game_mode_policy();
    let detection = detect_game_mode(Some(window), &monitors, &rules, &policy);

    println!("Window: {}", window.handle);
    println!("Title: {}", empty_dash(&window.title));
    println!("Class: {}", empty_dash(&window.class_name));
    println!(
        "Executable: {}",
        window.executable_path.as_deref().unwrap_or("-")
    );
    println!("Monitor: {}", monitor_label(window, &monitors));
    println!(
        "Manageable: {} ({})",
        yes_no(window.is_manageable()),
        manageability_reason(window)
    );
    println!(
        "Fullscreen: {}{}",
        yes_no(detection.fullscreen.is_fullscreen),
        fullscreen_suffix(&detection.fullscreen)
    );
    println!(
        "Game mode: {}",
        if detection.active {
            "active"
        } else {
            "inactive"
        }
    );
    println!(
        "Detection reason: {}",
        detection
            .reason
            .as_ref()
            .map(game_mode_reason)
            .unwrap_or_else(|| "-".to_owned())
    );
    println!(
        "Matched game exe: {}",
        detection.matched_executable.as_deref().unwrap_or("-")
    );
    println!(
        "Matched game rule: {}",
        if detection.matched_rules.is_empty() {
            "-".to_owned()
        } else {
            detection.matched_rules.join(", ")
        }
    );
    println!(
        "Layout paused: {}",
        yes_no(
            detection.active
                && loaded_config
                    .config
                    .game_mode
                    .pause_all_layouts_when_game_focused
        )
    );
    println!(
        "Layout pause scope: {}",
        if detection.active
            && loaded_config
                .config
                .game_mode
                .pause_all_layouts_when_game_focused
        {
            if loaded_config.config.game_mode.pause_focused_monitor_only
                && detection.fullscreen.monitor.is_some()
            {
                "focused monitor"
            } else {
                "global"
            }
        } else {
            "-"
        }
    );
    println!(
        "Borders paused: {}",
        yes_no(detection.active && loaded_config.config.game_mode.disable_borders)
    );
    println!(
        "Animations paused: {}",
        yes_no(detection.active && loaded_config.config.game_mode.disable_animations)
    );
    println!(
        "Keyboard/mouse hook bypass: {}",
        yes_no(detection.active && loaded_config.config.game_mode.disable_keyboard_hooks)
    );

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
    let layout = loaded.config.layout_config();
    println!(
        "Layout: {} (gap {}, border {}, smart_split {}, preserve_split {})",
        layout.kind.name(),
        layout.gap,
        layout.border,
        yes_no(layout.smart_split),
        yes_no(layout.preserve_split)
    );
    println!(
        "Game mode: {} (fullscreen {}, tolerance {}px, game_exes {}, ignored_exes {})",
        if loaded.config.game_mode.enabled {
            "enabled"
        } else {
            "disabled"
        },
        yes_no(loaded.config.game_mode.pause_on_fullscreen),
        loaded.config.game_mode.fullscreen_tolerance_px,
        loaded.config.game_mode.game_exes.len(),
        loaded.config.game_mode.ignored_exes.len()
    );

    Ok(())
}

fn handle_shell(args: ShellArgs) -> Result<()> {
    match args.command {
        ShellCommand::Status => shell_status(),
        ShellCommand::Install(args) => shell_install(args),
        ShellCommand::Uninstall(args) | ShellCommand::Recover(args) => shell_uninstall(args),
        ShellCommand::Test(args) => shell_test(args),
        ShellCommand::InstallElevatedTask(args) => shell_install_elevated_task(args),
        ShellCommand::UninstallElevatedTask(args) => shell_uninstall_elevated_task(args),
        ShellCommand::ElevatedTaskStatus => shell_elevated_task_status(),
        ShellCommand::Explorer => shell_explorer(),
    }
}

fn shell_status() -> Result<()> {
    let status = winland_win32::shell_replacement_status()?;

    println!("Experimental shell replacement status");
    println!("Registry value: {}\\Shell", status.registry_key);
    println!(
        "Current shell: {}",
        status.current_shell.as_deref().unwrap_or("<not set>")
    );
    println!(
        "Winland shell mode: {}",
        if status.is_winland_shell { "yes" } else { "no" }
    );
    println!(
        "Stored previous shell: {}",
        status.backup_shell.as_deref().unwrap_or("<not stored>")
    );
    println!(
        "Previous Shell value existed: {}",
        status
            .backup_shell_was_present
            .map(yes_no)
            .unwrap_or("<not stored>")
    );

    Ok(())
}

fn shell_install(args: ShellInstallArgs) -> Result<()> {
    require_experimental(args.experimental)?;
    let shell_command = shell_command(
        args.shell.as_deref(),
        args.daemon.as_deref(),
        args.elevated_daemon,
        args.command.as_deref(),
    )?;
    let status = winland_win32::shell_replacement_status()?;

    println!("Installing experimental per-user shell replacement.");
    println!("Registry value: {}\\Shell", status.registry_key);
    println!(
        "Current shell: {}",
        status.current_shell.as_deref().unwrap_or("<not set>")
    );
    println!("New shell: {shell_command}");
    println!("Undo command: winland shell uninstall --experimental");

    let change = winland_win32::install_shell_replacement(&shell_command)?;
    println!(
        "Updated {}\\Shell from {} to {}.",
        change.registry_key,
        change.previous_shell.as_deref().unwrap_or("<not set>"),
        change.new_shell.as_deref().unwrap_or("<not set>")
    );
    println!("Sign out or restart the VM user to enter Winland shell mode.");

    Ok(())
}

fn shell_uninstall(args: ShellExperimentalArgs) -> Result<()> {
    require_experimental(args.experimental)?;
    let change = winland_win32::restore_shell_replacement()?;
    if winland_win32::elevated_daemon_task_installed()? {
        winland_win32::uninstall_elevated_daemon_task()?;
        println!("Deleted elevated daemon scheduled task.");
    }

    println!(
        "Restored {}\\Shell from {} to {}.",
        change.registry_key,
        change.previous_shell.as_deref().unwrap_or("<not set>"),
        change.new_shell.as_deref().unwrap_or("<not set>")
    );
    println!("Launch Explorer now with: winland shell explorer");

    Ok(())
}

fn shell_install_elevated_task(args: ShellElevatedTaskInstallArgs) -> Result<()> {
    require_experimental(args.experimental)?;
    let daemon = match args.daemon {
        Some(path) => path,
        None => default_daemon_path()?,
    };

    println!("Creating experimental elevated daemon scheduled task.");
    println!("Daemon: {}", daemon.display());
    println!(
        "This may show one UAC prompt. Later shell starts can use --elevated-daemon without a prompt."
    );
    winland_win32::install_elevated_daemon_task(&daemon)?;
    println!("Elevated daemon scheduled task is installed.");

    Ok(())
}

fn shell_uninstall_elevated_task(args: ShellExperimentalArgs) -> Result<()> {
    require_experimental(args.experimental)?;
    winland_win32::uninstall_elevated_daemon_task()?;
    println!("Elevated daemon scheduled task is deleted.");
    Ok(())
}

fn shell_elevated_task_status() -> Result<()> {
    let installed = winland_win32::elevated_daemon_task_installed()?;
    println!(
        "Elevated daemon scheduled task: {}",
        if installed {
            "installed"
        } else {
            "not installed"
        }
    );
    Ok(())
}

fn shell_test(args: ShellLaunchArgs) -> Result<()> {
    let shell_command = shell_command(
        args.shell.as_deref(),
        args.daemon.as_deref(),
        args.elevated_daemon,
        args.command.as_deref(),
    )?;
    println!("Launching one-session Winland shell test without registry changes.");
    println!("Command: {shell_command}");
    winland_win32::launch_shell_test(&shell_command)?;
    Ok(())
}

fn shell_explorer() -> Result<()> {
    winland_win32::launch_explorer()?;
    println!("Launched explorer.exe.");
    Ok(())
}

fn require_experimental(experimental: bool) -> Result<()> {
    if experimental {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "persistent shell replacement is experimental; rerun with --experimental after taking a VM checkpoint"
        ))
    }
}

fn shell_command(
    shell: Option<&std::path::Path>,
    daemon: Option<&std::path::Path>,
    elevated_daemon: bool,
    command: Option<&str>,
) -> Result<String> {
    if let Some(command) = command {
        let command = command.trim();
        if command.is_empty() {
            return Err(anyhow::anyhow!("shell command must not be empty"));
        }
        return Ok(command.to_owned());
    }

    let shell = match shell {
        Some(path) => path.to_owned(),
        None => default_shell_path()?,
    };

    Ok(winland_win32::shell_command_with_daemon(
        &shell,
        daemon,
        elevated_daemon,
    ))
}

fn default_shell_path() -> Result<PathBuf> {
    executable_next_to_current("winland-shell")
}

fn default_daemon_path() -> Result<PathBuf> {
    executable_next_to_current("winland-daemon")
}

fn executable_next_to_current(stem: &str) -> Result<PathBuf> {
    let current = std::env::current_exe()?;
    let extension = current.extension().and_then(|value| value.to_str());
    let file_name = match extension {
        Some(extension) if !extension.is_empty() => format!("{stem}.{extension}"),
        _ => stem.to_owned(),
    };

    Ok(current.with_file_name(file_name))
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
    let loaded_config = winland_config::load_or_default(None)?;
    let windows = winland_win32::enumerate_windows()?;
    let monitors = winland_win32::enumerate_monitors()?;
    let monitor = primary_monitor(&monitors)?;
    let layout = loaded_config.config.layout_config_for_monitor(
        monitor.id,
        monitor.is_primary,
        WorkspaceId(1),
    );

    let selected: Vec<_> = windows
        .iter()
        .filter(|window| window.is_manageable())
        .filter(|window| monitor.rect.contains(window.rect.center()))
        .collect();
    let handles: Vec<_> = selected.iter().map(|window| window.handle).collect();
    let assignments = tile_windows_with_config(monitor.work_area, &handles, layout);

    if assignments.is_empty() {
        println!(
            "No manageable windows found on primary monitor {} (work area {}).",
            monitor.id, monitor.work_area
        );
        return Ok(());
    }

    println!(
        "Tiling {} window(s) on primary monitor {} using {} (work area {}).",
        assignments.len(),
        monitor.id,
        layout.kind.name(),
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

fn parse_hwnd(input: &str) -> Result<WindowHandle> {
    let trimmed = input.trim();
    let value = if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16)?
    } else {
        trimmed.parse()?
    };

    Ok(WindowHandle(value))
}

fn monitor_label(window: &WindowInfo, monitors: &[MonitorInfo]) -> String {
    monitors
        .iter()
        .filter_map(|monitor| {
            let overlap = rect_overlap_area(window.rect, monitor.rect);
            (overlap > 0).then_some((overlap, monitor.id))
        })
        .max_by_key(|(overlap, id)| (*overlap, std::cmp::Reverse(*id)))
        .map(|(_, id)| id.to_string())
        .unwrap_or_else(|| "-".to_owned())
}

fn fullscreen_suffix(detection: &winland_core::FullscreenDetection) -> String {
    match (detection.monitor, detection.area) {
        (Some(monitor), Some(area)) => format!(" ({area:?} on {monitor})"),
        _ => String::new(),
    }
}

fn game_mode_reason(reason: &GameModeReason) -> String {
    match reason {
        GameModeReason::ConfiguredExecutable(exe) => format!("configured executable {exe}"),
        GameModeReason::WindowRule {
            mode,
            matched_rules,
        } => format!("window rule mode {mode:?} via {}", matched_rules.join(", ")),
        GameModeReason::Fullscreen { monitor, area } => {
            format!("fullscreen {area:?} on {monitor}")
        }
    }
}

fn rect_overlap_area(a: winland_core::Rect, b: winland_core::Rect) -> i64 {
    let left = a.left.max(b.left);
    let top = a.top.max(b.top);
    let right = a.right.min(b.right);
    let bottom = a.bottom.min(b.bottom);
    let width = i64::from(right.saturating_sub(left).max(0));
    let height = i64::from(bottom.saturating_sub(top).max(0));
    width * height
}

fn empty_dash(value: &str) -> &str {
    if value.trim().is_empty() { "-" } else { value }
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
