use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use clap::{ArgAction, Args, Parser, Subcommand};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{ComponentHandle, ModelRc, VecModel};
use slint_interpreter::{ComponentDefinition, ComponentInstance, Struct, Value};
use tracing::debug;
use tracing_subscriber::EnvFilter;
use winland_core::{
    GameModeReason, Manageability, MonitorInfo, Rect, WindowHandle, WindowInfo, WorkspaceId,
    detect_game_mode, tile_windows_with_config,
};
use winland_ipc::{
    CommandReport, DaemonStateSnapshot, IpcRequest, IpcResponseResult, ReloadConfigReport,
    WindowParticipationSnapshot, decode_response, encode_request,
};

const TASKBAR_WIDGET_SOURCE: &str = include_str!("../widgets/taskbar.slint");
const TASKBAR_WIDGET_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/widgets/taskbar.slint");

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
    /// Reload the running daemon's config through local IPC.
    ReloadConfig(ReloadConfigArgs),
    /// Execute a daemon command through local IPC.
    #[command(name = "command")]
    Action(CommandArgs),
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
    /// Run custom Winland UI widgets.
    Widget(WidgetArgs),
}

#[derive(Debug, Args)]
struct StateArgs {
    /// Print the daemon state snapshot as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ReloadConfigArgs {
    /// Print the reload report as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CommandArgs {
    /// Command and arguments to route to the running daemon.
    #[arg(required = true, trailing_var_arg = true)]
    command: Vec<String>,
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

#[derive(Debug, Args)]
struct WidgetArgs {
    #[command(subcommand)]
    command: WidgetCommand,
}

#[derive(Debug, Subcommand)]
enum WidgetCommand {
    /// Run a built-in widget or a user-provided .slint widget.
    Run(WidgetRunArgs),
}

#[derive(Debug, Args)]
struct WidgetRunArgs {
    /// Built-in widget name. Currently: taskbar.
    widget: Option<String>,
    /// Path to a user-defined .slint widget.
    #[arg(long)]
    file: Option<PathBuf>,
    /// Exported component name for --file widgets.
    #[arg(long, default_value = "MainWindow")]
    component: String,
    /// Widget height in physical pixels.
    #[arg(long, default_value_t = 40)]
    height: u32,
    /// Run on every monitor instead of only the primary monitor.
    #[arg(long, default_value_t = true)]
    all_monitors: bool,
    /// Do not keep the widget above normal application windows.
    #[arg(long = "no-topmost", default_value_t = true, action = ArgAction::SetFalse)]
    topmost: bool,
    /// Run an external widget plugin once and display the JSON object it prints.
    #[arg(long = "plugin-once")]
    plugin_once: Vec<String>,
    /// Run an external widget plugin as a JSON-line event stream.
    #[arg(long = "plugin-stream")]
    plugin_stream: Vec<String>,
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
        Command::ReloadConfig(args) => reload_config(args),
        Command::Action(args) => daemon_command(args),
        Command::Windows(args) => list_windows(args),
        Command::Monitors => list_monitors(),
        Command::DiagnoseWindow(args) => diagnose_window(args),
        Command::TileOnce => tile_once(),
        Command::Config(args) => handle_config(args),
        Command::Shell(args) => handle_shell(args),
        Command::Widget(args) => handle_widget(args),
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

fn handle_widget(args: WidgetArgs) -> Result<()> {
    match args.command {
        WidgetCommand::Run(args) => run_widget(args),
    }
}

fn run_widget(args: WidgetRunArgs) -> Result<()> {
    configure_default_widget_backend();

    let plugins = WidgetPluginCommands::from_args(&args);

    if let Some(file) = &args.file {
        return run_slint_file_widget(
            file,
            &args.component,
            args.height,
            args.all_monitors,
            args.topmost,
            plugins,
        );
    }

    match args.widget.as_deref().unwrap_or("taskbar") {
        "taskbar" => run_taskbar_widget(args.height, args.all_monitors, args.topmost, plugins),
        name => Err(anyhow::anyhow!(
            "unknown built-in widget '{name}'; use --file to run a custom .slint widget"
        )),
    }
}

fn configure_default_widget_backend() {
    if std::env::var_os("SLINT_BACKEND").is_some() {
        return;
    }

    // SAFETY: Widget backend selection happens before any Slint windows are
    // created and before this command starts worker threads. Users can still
    // override this with SLINT_BACKEND when they want a GPU renderer.
    unsafe {
        std::env::set_var("SLINT_BACKEND", "winit-software");
    }
}

fn run_taskbar_widget(
    height: u32,
    all_monitors: bool,
    topmost: bool,
    plugins: WidgetPluginCommands,
) -> Result<()> {
    let definition = compile_slint_source(
        TASKBAR_WIDGET_SOURCE,
        &PathBuf::from(TASKBAR_WIDGET_PATH),
        "TaskbarWidget",
    )?;

    run_slint_widget_instances(definition, height, all_monitors, topmost, plugins)
}

fn run_slint_file_widget(
    path: &std::path::Path,
    component: &str,
    height: u32,
    all_monitors: bool,
    topmost: bool,
    plugins: WidgetPluginCommands,
) -> Result<()> {
    let definition = compile_slint_path(path, component)?;

    run_slint_widget_instances(definition, height, all_monitors, topmost, plugins)
}

fn compile_slint_path(path: &std::path::Path, component: &str) -> Result<ComponentDefinition> {
    let compiler = slint_interpreter::Compiler::default();
    let result = spin_on::spin_on(compiler.build_from_path(path));
    print_slint_diagnostics(&result);

    if result.has_errors() {
        return Err(anyhow::anyhow!(
            "failed to compile Slint widget {}",
            path.display()
        ));
    }

    result.component(component).ok_or_else(|| {
        anyhow::anyhow!(
            "component '{component}' was not exported by {}",
            path.display()
        )
    })
}

fn compile_slint_source(
    source: &str,
    path: &std::path::Path,
    component: &str,
) -> Result<ComponentDefinition> {
    let compiler = slint_interpreter::Compiler::default();
    let result = spin_on::spin_on(compiler.build_from_source(source.to_owned(), path.to_owned()));
    print_slint_diagnostics(&result);

    if result.has_errors() {
        return Err(anyhow::anyhow!(
            "failed to compile built-in Slint widget {}",
            path.display()
        ));
    }

    result.component(component).ok_or_else(|| {
        anyhow::anyhow!(
            "component '{component}' was not exported by built-in widget {}",
            path.display()
        )
    })
}

fn print_slint_diagnostics(result: &slint_interpreter::CompilationResult) {
    let diagnostics: Vec<_> = result.diagnostics().collect();
    if !diagnostics.is_empty() {
        slint_interpreter::print_diagnostics(&diagnostics);
    }
}

fn run_slint_widget_instances(
    definition: ComponentDefinition,
    height: u32,
    all_monitors: bool,
    topmost: bool,
    plugins: WidgetPluginCommands,
) -> Result<()> {
    let monitors = widget_target_monitors(all_monitors)?;
    let height = height.max(1);
    let mut instances = Vec::new();
    let workspace_count = winland_config::load_or_default(None)
        .map(|loaded| loaded.config.workspace_count())
        .unwrap_or(9);
    let data = WidgetData::new(workspace_count);

    for monitor in monitors {
        let rect = bottom_widget_rect(&monitor, height);
        let instance = definition.create()?;
        apply_widget_data(&instance, &data, topmost);
        register_widget_callbacks(&instance);
        instance
            .window()
            .set_position(slint::PhysicalPosition::new(rect.left, rect.top));
        instance.window().set_size(slint::PhysicalSize::new(
            rect.width() as u32,
            rect.height() as u32,
        ));
        instance.show()?;
        configure_slint_widget_window(&instance, rect, topmost)?;
        instances.push(instance);
    }

    let (update_sender, update_receiver) = mpsc::channel();
    let _sources = start_widget_sources(update_sender, plugins);
    let shared_data = Arc::new(Mutex::new(data));
    let weak_instances: Vec<_> = instances.iter().map(ComponentHandle::as_weak).collect();
    let _dispatcher =
        start_widget_update_dispatcher(update_receiver, shared_data, weak_instances, topmost);

    slint::run_event_loop()?;
    Ok(())
}

#[derive(Debug, Clone, Default)]
struct WidgetPluginCommands {
    once: Vec<String>,
    stream: Vec<String>,
}

impl WidgetPluginCommands {
    fn from_args(args: &WidgetRunArgs) -> Self {
        Self {
            once: args.plugin_once.clone(),
            stream: args.plugin_stream.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WidgetData {
    workspace_count: u16,
    active_workspace: u16,
    workspaces: Vec<WorkspaceWidgetRow>,
    windows: Vec<WindowWidgetRow>,
    plugin_blocks: Vec<PluginWidgetBlock>,
    time_text: String,
}

impl WidgetData {
    fn new(workspace_count: u16) -> Self {
        let workspace_count = workspace_count.max(1);
        let mut data = Self {
            workspace_count,
            active_workspace: 1,
            workspaces: Vec::new(),
            windows: Vec::new(),
            plugin_blocks: Vec::new(),
            time_text: winland_win32::local_time_hhmm(),
        };
        data.rebuild_workspaces(None);
        data
    }

    fn apply(&mut self, update: WidgetUpdate) {
        match update {
            WidgetUpdate::DaemonState(snapshot) => self.apply_daemon_state(snapshot),
            WidgetUpdate::Time(time_text) => self.time_text = time_text,
            WidgetUpdate::PluginBlock(block) => {
                if let Some(existing) = self
                    .plugin_blocks
                    .iter_mut()
                    .find(|candidate| candidate.source == block.source)
                {
                    *existing = block;
                } else {
                    self.plugin_blocks.push(block);
                }
            }
        }
    }

    fn apply_daemon_state(&mut self, snapshot: DaemonStateSnapshot) {
        let highest_workspace = snapshot
            .monitors
            .iter()
            .map(|monitor| monitor.workspace_id)
            .chain(
                snapshot
                    .windows
                    .iter()
                    .filter_map(|window| window.workspace_id),
            )
            .chain(std::iter::once(snapshot.active_workspace))
            .max()
            .unwrap_or(self.workspace_count);
        self.workspace_count = self.workspace_count.max(highest_workspace).max(1);
        self.active_workspace = taskbar_active_workspace(&snapshot);
        self.rebuild_workspaces(Some(&snapshot));
        self.windows = snapshot
            .windows
            .iter()
            .filter(|window| window.workspace_id == Some(self.active_workspace))
            .map(WindowWidgetRow::from_snapshot)
            .collect();
    }

    fn rebuild_workspaces(&mut self, snapshot: Option<&DaemonStateSnapshot>) {
        self.workspaces = (1..=self.workspace_count)
            .map(|id| {
                let active = snapshot.is_some_and(|snapshot| {
                    snapshot.active_workspace == id
                        || snapshot
                            .monitors
                            .iter()
                            .any(|monitor| monitor.focused && monitor.workspace_id == id)
                });
                let window_count = snapshot
                    .map(|snapshot| {
                        snapshot
                            .windows
                            .iter()
                            .filter(|window| window.workspace_id == Some(id))
                            .count()
                    })
                    .unwrap_or(0);
                WorkspaceWidgetRow {
                    id,
                    name: id.to_string(),
                    command: cli_daemon_command(&format!("switch-workspace {id}")),
                    active,
                    window_count,
                }
            })
            .collect();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceWidgetRow {
    id: u16,
    name: String,
    command: String,
    active: bool,
    window_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WindowWidgetRow {
    handle: u64,
    handle_text: String,
    command: String,
    title: String,
    workspace_id: u16,
    focused: bool,
    visible: bool,
    is_minimized: bool,
    participation: String,
}

impl WindowWidgetRow {
    fn from_snapshot(window: &winland_ipc::WindowStateSnapshot) -> Self {
        Self {
            handle: window.handle,
            handle_text: format_hwnd_like(window.handle),
            command: cli_daemon_command(&format!(
                "focus-window {}",
                format_hwnd_like(window.handle)
            )),
            title: window.title.clone(),
            workspace_id: window.workspace_id.unwrap_or(0),
            focused: window.focused,
            visible: window.visible_on_active_workspace,
            is_minimized: window.is_minimized,
            participation: participation_label(window.participation).to_owned(),
        }
    }
}

fn taskbar_active_workspace(snapshot: &DaemonStateSnapshot) -> u16 {
    snapshot
        .monitors
        .iter()
        .find(|monitor| monitor.focused)
        .map(|monitor| monitor.workspace_id)
        .unwrap_or(snapshot.active_workspace)
        .max(1)
}

fn cli_daemon_command(command: &str) -> String {
    let cli = std::env::current_exe()
        .ok()
        .map(|path| winland_win32::quote_windows_arg(&path.display().to_string()))
        .unwrap_or_else(|| "winland".to_owned());
    format!("{cli} command {command}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginWidgetBlock {
    source: String,
    label: String,
    text: String,
}

enum WidgetUpdate {
    DaemonState(DaemonStateSnapshot),
    Time(String),
    PluginBlock(PluginWidgetBlock),
}

fn apply_widget_data(instance: &ComponentInstance, data: &WidgetData, topmost: bool) {
    set_widget_property(instance, "topmost", Value::Bool(topmost));
    set_widget_property(instance, "always-on-top", Value::Bool(topmost));
    set_widget_property(
        instance,
        "time-text",
        Value::String(data.time_text.clone().into()),
    );
    set_widget_property(
        instance,
        "label",
        Value::String(widget_summary_label(data).into()),
    );
    set_widget_property(instance, "workspaces", rows_model(workspace_values(data)));
    set_widget_property(instance, "windows", rows_model(window_values(data)));
    set_widget_property(
        instance,
        "plugin-blocks",
        rows_model(plugin_block_values(data)),
    );
}

fn set_widget_property(instance: &ComponentInstance, name: &str, value: Value) {
    let _ = instance.set_property(name, value);
}

fn register_widget_callbacks(instance: &ComponentInstance) {
    if let Err(error) = instance.set_callback("run-command", |args| {
        if let Some(command) = args.first().and_then(value_as_string) {
            launch_widget_command(command);
        }
        Value::Void
    }) {
        debug!(%error, "widget does not expose run-command callback");
    }
}

fn value_as_string(value: &Value) -> Option<&str> {
    match value {
        Value::String(value) => Some(value.as_str()),
        _ => None,
    }
}

fn launch_widget_command(command: &str) {
    let command = command.trim();
    if command.is_empty() {
        return;
    }

    debug!(%command, "running widget command");
    log_widget_command_event(&format!("running widget command: {command}"));
    let child = plugin_shell_command(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    match child {
        Ok(child) => watch_widget_command(command.to_owned(), child),
        Err(error) => {
            log_widget_command_event(&format!(
                "failed to run widget command '{command}': {error}"
            ));
            eprintln!("failed to run widget command '{command}': {error}");
        }
    }
}

fn watch_widget_command(command: String, child: std::process::Child) {
    let _ = thread::Builder::new()
        .name("winland-widget-command".to_owned())
        .spawn(move || match child.wait_with_output() {
            Ok(output) => {
                let status = output
                    .status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "terminated".to_owned());
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                log_widget_command_event(&format!(
                    "widget command exited status={status}: {command}"
                ));
                for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
                    log_widget_command_event(&format!("widget command stdout: {line}"));
                }
                for line in stderr.lines().filter(|line| !line.trim().is_empty()) {
                    log_widget_command_event(&format!("widget command stderr: {line}"));
                }
            }
            Err(error) => log_widget_command_event(&format!(
                "failed to wait for widget command '{command}': {error}"
            )),
        });
}

fn log_widget_command_event(message: &str) {
    let path = std::env::temp_dir().join("winland-widget-commands.log");
    let timestamp = winland_win32::local_time_hhmm();
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{timestamp} {message}");
    }
}

fn widget_summary_label(data: &WidgetData) -> String {
    let active_workspace = data
        .workspaces
        .iter()
        .find(|workspace| workspace.active)
        .map(|workspace| workspace.name.as_str())
        .unwrap_or("-");
    let focused_window = data
        .windows
        .iter()
        .find(|window| window.focused)
        .map(|window| window.title.as_str())
        .unwrap_or("no focused window");

    format!(
        "Workspace {active_workspace} | {focused_window} | {}",
        data.time_text
    )
}

fn rows_model(rows: Vec<Value>) -> Value {
    Value::from(ModelRc::new(VecModel::from(rows)))
}

fn workspace_values(data: &WidgetData) -> Vec<Value> {
    data.workspaces
        .iter()
        .map(|workspace| {
            Value::Struct(Struct::from_iter([
                ("id".to_owned(), Value::Number(f64::from(workspace.id))),
                (
                    "name".to_owned(),
                    Value::String(workspace.name.clone().into()),
                ),
                (
                    "command".to_owned(),
                    Value::String(workspace.command.clone().into()),
                ),
                ("active".to_owned(), Value::Bool(workspace.active)),
                (
                    "window-count".to_owned(),
                    Value::Number(workspace.window_count as f64),
                ),
            ]))
        })
        .collect()
}

fn window_values(data: &WidgetData) -> Vec<Value> {
    data.windows
        .iter()
        .map(|window| {
            Value::Struct(Struct::from_iter([
                ("handle".to_owned(), Value::Number(window.handle as f64)),
                (
                    "title".to_owned(),
                    Value::String(window.title.clone().into()),
                ),
                (
                    "handle-text".to_owned(),
                    Value::String(window.handle_text.clone().into()),
                ),
                (
                    "command".to_owned(),
                    Value::String(window.command.clone().into()),
                ),
                (
                    "workspace-id".to_owned(),
                    Value::Number(f64::from(window.workspace_id)),
                ),
                ("focused".to_owned(), Value::Bool(window.focused)),
                ("visible".to_owned(), Value::Bool(window.visible)),
                ("is-minimized".to_owned(), Value::Bool(window.is_minimized)),
                (
                    "participation".to_owned(),
                    Value::String(window.participation.clone().into()),
                ),
            ]))
        })
        .collect()
}

fn plugin_block_values(data: &WidgetData) -> Vec<Value> {
    data.plugin_blocks
        .iter()
        .map(|block| {
            Value::Struct(Struct::from_iter([
                (
                    "source".to_owned(),
                    Value::String(block.source.clone().into()),
                ),
                (
                    "label".to_owned(),
                    Value::String(block.label.clone().into()),
                ),
                ("text".to_owned(), Value::String(block.text.clone().into())),
            ]))
        })
        .collect()
}

fn start_widget_sources(
    update_sender: mpsc::Sender<WidgetUpdate>,
    plugins: WidgetPluginCommands,
) -> Vec<thread::JoinHandle<()>> {
    let mut sources = Vec::new();
    sources.push(start_clock_source(update_sender.clone()));
    sources.push(start_daemon_state_source(update_sender.clone()));

    for command in plugins.once {
        sources.push(start_plugin_once_source(update_sender.clone(), command));
    }

    for command in plugins.stream {
        sources.push(start_plugin_stream_source(update_sender.clone(), command));
    }

    sources
}

fn start_widget_update_dispatcher(
    update_receiver: mpsc::Receiver<WidgetUpdate>,
    data: Arc<Mutex<WidgetData>>,
    instances: Vec<slint::Weak<ComponentInstance>>,
    topmost: bool,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("winland-widget-ui-dispatch".to_owned())
        .spawn(move || {
            while let Ok(update) = update_receiver.recv() {
                let data = Arc::clone(&data);
                let instances = instances.clone();
                if slint::invoke_from_event_loop(move || {
                    let Ok(mut data) = data.lock() else {
                        return;
                    };
                    data.apply(update);
                    for instance in &instances {
                        if let Some(instance) = instance.upgrade() {
                            apply_widget_data(&instance, &data, topmost);
                        }
                    }
                })
                .is_err()
                {
                    break;
                }
            }
        })
        .expect("spawn widget UI update dispatcher")
}

fn start_clock_source(update_sender: mpsc::Sender<WidgetUpdate>) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("winland-widget-clock".to_owned())
        .spawn(move || {
            loop {
                if update_sender
                    .send(WidgetUpdate::Time(winland_win32::local_time_hhmm()))
                    .is_err()
                {
                    break;
                }
                thread::sleep(Duration::from_secs(1));
            }
        })
        .expect("spawn widget clock source")
}

fn start_daemon_state_source(update_sender: mpsc::Sender<WidgetUpdate>) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("winland-widget-daemon-state".to_owned())
        .spawn(move || {
            loop {
                let request = match encode_request(&IpcRequest::subscribe_state()) {
                    Ok(request) => request,
                    Err(error) => {
                        eprintln!("failed to encode daemon state subscription request: {error}");
                        thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                };
                let (raw_sender, raw_receiver) = mpsc::channel();
                if let Err(error) = winland_win32::spawn_ipc_response_stream(
                    winland_win32::DEFAULT_IPC_PIPE_NAME,
                    request,
                    raw_sender,
                ) {
                    eprintln!("failed to subscribe to Winland daemon state: {error}");
                    thread::sleep(Duration::from_millis(500));
                    continue;
                }

                let mut buffer = Vec::new();
                for raw in raw_receiver {
                    for response in decode_framed_ipc_responses(&mut buffer, &raw) {
                        match response {
                            Ok(response) => {
                                if let IpcResponseResult::State(snapshot) = response.result
                                    && update_sender
                                        .send(WidgetUpdate::DaemonState(snapshot))
                                        .is_err()
                                {
                                    return;
                                }
                            }
                            Err(error) => eprintln!("failed to decode daemon state event: {error}"),
                        }
                    }
                }

                thread::sleep(Duration::from_millis(500));
            }
        })
        .expect("spawn widget daemon state source")
}

fn decode_framed_ipc_responses(
    buffer: &mut Vec<u8>,
    chunk: &[u8],
) -> Vec<Result<winland_ipc::IpcResponse, winland_ipc::ProtocolError>> {
    buffer.extend_from_slice(chunk);
    let mut responses = Vec::new();

    while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
        let line: Vec<_> = buffer.drain(..=newline).collect();
        let trimmed = line.trim_ascii_end();
        if !trimmed.is_empty() {
            responses.push(decode_response(trimmed));
        }
    }

    responses
}

fn start_plugin_once_source(
    update_sender: mpsc::Sender<WidgetUpdate>,
    command: String,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("winland-widget-plugin-once".to_owned())
        .spawn(move || {
            match plugin_shell_command(&command)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
            {
                Ok(output) => {
                    let text = String::from_utf8_lossy(&output.stdout);
                    if let Some(block) = parse_plugin_block(&command, text.trim()) {
                        let _ = update_sender.send(WidgetUpdate::PluginBlock(block));
                    }
                }
                Err(error) => eprintln!("failed to run widget plugin '{command}': {error}"),
            }
        })
        .expect("spawn widget plugin once source")
}

fn start_plugin_stream_source(
    update_sender: mpsc::Sender<WidgetUpdate>,
    command: String,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("winland-widget-plugin-stream".to_owned())
        .spawn(move || {
            let mut child = match plugin_shell_command(&command)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(error) => {
                    eprintln!("failed to start widget plugin stream '{command}': {error}");
                    return;
                }
            };
            let Some(stdout) = child.stdout.take() else {
                eprintln!("widget plugin stream '{command}' did not expose stdout");
                return;
            };

            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if let Some(block) = parse_plugin_block(&command, line.trim())
                            && update_sender
                                .send(WidgetUpdate::PluginBlock(block))
                                .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        eprintln!("failed to read widget plugin stream '{command}': {error}");
                        break;
                    }
                }
            }
        })
        .expect("spawn widget plugin stream source")
}

fn plugin_shell_command(command: &str) -> ProcessCommand {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        let mut process = ProcessCommand::new("cmd.exe");
        process.arg("/C").raw_arg(command);
        process
    }

    #[cfg(not(windows))]
    {
        let mut process = ProcessCommand::new("sh");
        process.arg("-c").arg(command);
        process
    }
}

fn parse_plugin_block(source: &str, input: &str) -> Option<PluginWidgetBlock> {
    if input.is_empty() {
        return None;
    }

    let value: serde_json::Value = serde_json::from_str(input).ok()?;
    let label = json_string_field(&value, "label")
        .or_else(|| json_string_field(&value, "name"))
        .unwrap_or_else(|| source.to_owned());
    let text = json_string_field(&value, "text")
        .or_else(|| json_string_field(&value, "value"))
        .or_else(|| json_string_field(&value, "status"))
        .unwrap_or_else(|| value.to_string());

    Some(PluginWidgetBlock {
        source: source.to_owned(),
        label,
        text,
    })
}

fn json_string_field(value: &serde_json::Value, field: &str) -> Option<String> {
    match value.get(field)? {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        serde_json::Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn widget_target_monitors(all_monitors: bool) -> Result<Vec<MonitorInfo>> {
    let monitors = winland_win32::enumerate_monitors()?;
    if all_monitors {
        return Ok(monitors);
    }

    monitors
        .iter()
        .find(|monitor| monitor.is_primary)
        .or_else(|| monitors.first())
        .cloned()
        .map(|monitor| vec![monitor])
        .ok_or_else(|| anyhow::anyhow!("no monitors were discovered"))
}

fn bottom_widget_rect(monitor: &MonitorInfo, height: u32) -> Rect {
    let height = i32::try_from(height).unwrap_or(i32::MAX);
    let height = height.min(monitor.rect.height().max(1));
    Rect {
        left: monitor.rect.left,
        top: monitor.rect.bottom.saturating_sub(height),
        right: monitor.rect.right,
        bottom: monitor.rect.bottom,
    }
}

fn configure_slint_widget_window(
    instance: &ComponentInstance,
    rect: Rect,
    topmost: bool,
) -> Result<()> {
    let handle =
        widget_window_handle(instance).or_else(|_| widget_window_for_current_process(rect))?;
    winland_win32::configure_widget_window(handle, rect, topmost)?;
    Ok(())
}

fn widget_window_handle(instance: &ComponentInstance) -> Result<WindowHandle> {
    let slint_window_handle = instance.window().window_handle();
    let handle = slint_window_handle
        .window_handle()
        .map_err(|error| anyhow::anyhow!("could not get Slint widget window handle: {error}"))?;

    match handle.as_raw() {
        RawWindowHandle::Win32(handle) => Ok(WindowHandle(handle.hwnd.get() as usize as u64)),
        _ => Err(anyhow::anyhow!(
            "Slint widget did not expose a Win32 window handle"
        )),
    }
}

fn widget_window_for_current_process(target: Rect) -> Result<WindowHandle> {
    let process_id = std::process::id();
    let windows = winland_win32::enumerate_windows()?;

    windows
        .into_iter()
        .filter(|window| window.process_id == process_id)
        .min_by_key(|window| rect_distance_score(window.rect, target))
        .map(|window| window.handle)
        .ok_or_else(|| anyhow::anyhow!("could not find the Slint widget window for this process"))
}

fn rect_distance_score(actual: Rect, target: Rect) -> i64 {
    let actual_center = actual.center();
    let target_center = target.center();
    let dx = i64::from(actual_center.x.saturating_sub(target_center.x));
    let dy = i64::from(actual_center.y.saturating_sub(target_center.y));
    let dw = i64::from(actual.width().saturating_sub(target.width()).abs());
    let dh = i64::from(actual.height().saturating_sub(target.height()).abs());

    dx.saturating_mul(dx)
        .saturating_add(dy.saturating_mul(dy))
        .saturating_add(dw.saturating_mul(1024))
        .saturating_add(dh.saturating_mul(1024))
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
        IpcResponseResult::ReloadConfig(_) => {
            return Err(anyhow::anyhow!(
                "daemon returned reload-config response to state request"
            ));
        }
        IpcResponseResult::Command(_) => {
            return Err(anyhow::anyhow!(
                "daemon returned command response to state request"
            ));
        }
    }

    Ok(())
}

fn reload_config(args: ReloadConfigArgs) -> Result<()> {
    let request = encode_request(&IpcRequest::reload_config())?;
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
        IpcResponseResult::ReloadConfig(report) if args.json => {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        IpcResponseResult::ReloadConfig(report) => {
            println!("{}", format_reload_config_report(&report));
        }
        IpcResponseResult::Error(error) => {
            return Err(anyhow::anyhow!("daemon IPC error: {}", error.message));
        }
        IpcResponseResult::State(_) => {
            return Err(anyhow::anyhow!(
                "daemon returned state response to reload-config request"
            ));
        }
        IpcResponseResult::Command(_) => {
            return Err(anyhow::anyhow!(
                "daemon returned command response to reload-config request"
            ));
        }
    }

    Ok(())
}

fn daemon_command(args: CommandArgs) -> Result<()> {
    let command = args.command.join(" ");
    let request = encode_request(&IpcRequest::command(command.clone()))?;
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
        IpcResponseResult::Command(report) => {
            println!("{}", format_command_report(&report));
        }
        IpcResponseResult::Error(error) => {
            return Err(anyhow::anyhow!("daemon IPC error: {}", error.message));
        }
        other => {
            return Err(anyhow::anyhow!(
                "daemon returned unexpected response to command request: {other:?}"
            ));
        }
    }

    Ok(())
}

fn format_command_report(report: &CommandReport) -> String {
    format!("Command executed: {}", report.command)
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
    let mut output = format!(
        "Winland daemon is running (IPC protocol v{}).\nConfig: v{} from {} loaded at {}\nWindows: {} total, {} manageable, {} floating, {} temporary floating\nWorkspace: active {}\nForeground: {}\nPerformance: {} relayouts, {} skipped, last {} ms / {} moves, {} borders, game mode {}, {} config reloads",
        winland_ipc::PROTOCOL_VERSION,
        snapshot.config_version,
        snapshot.config_path.as_deref().unwrap_or("<defaults>"),
        snapshot.config_loaded_at_unix_ms,
        snapshot.total_windows,
        snapshot.manageable_windows,
        snapshot.floating_windows,
        snapshot.temporary_floating_windows,
        snapshot.active_workspace,
        foreground,
        snapshot.performance.relayout_count,
        snapshot.performance.skipped_relayout_count,
        snapshot.performance.last_relayout_duration_ms,
        snapshot.performance.last_relayout_move_count,
        snapshot.performance.border_window_count,
        yes_no(snapshot.performance.game_mode_active),
        snapshot.performance.config_reload_count
    );

    if !snapshot.monitors.is_empty() {
        output.push_str("\n\nMonitors:");
        for monitor in &snapshot.monitors {
            output.push_str(&format!(
                "\n  {} workspace {}{}",
                format_hwnd_like(monitor.monitor_id),
                monitor.workspace_id,
                if monitor.focused { " focused" } else { "" }
            ));
        }
    }

    if !snapshot.windows.is_empty() {
        output.push_str("\n\nWindows:");
        for window in &snapshot.windows {
            output.push_str(&format!(
                "\n  {} ws {} mon {} {}{}{} - {}",
                format_hwnd_like(window.handle),
                window
                    .workspace_id
                    .map(|workspace| workspace.to_string())
                    .unwrap_or_else(|| "-".to_owned()),
                window
                    .monitor_id
                    .map(format_hwnd_like)
                    .unwrap_or_else(|| "-".to_owned()),
                participation_label(window.participation),
                if window.constrained {
                    " constrained"
                } else {
                    ""
                },
                if window.visible_on_active_workspace {
                    " visible"
                } else {
                    " hidden"
                },
                truncate(&window.title, 48)
            ));
        }
    }

    output
}

fn format_reload_config_report(report: &ReloadConfigReport) -> String {
    format!(
        "Config reloaded successfully.\nConfig: v{} from {} loaded at {}\nChanged: {}\nWindows: {} total, {} manageable\nWorkspace: active {}",
        report.config_version,
        report.config_path.as_deref().unwrap_or("<defaults>"),
        report.reloaded_at_unix_ms,
        report.changed_sections.join(", "),
        report.state.total_windows,
        report.state.manageable_windows,
        report.state.active_workspace,
    )
}

fn format_hwnd_like(value: u64) -> String {
    format!("0x{value:X}")
}

fn participation_label(participation: WindowParticipationSnapshot) -> &'static str {
    match participation {
        WindowParticipationSnapshot::Tiled => "tiled",
        WindowParticipationSnapshot::Floating => "floating",
        WindowParticipationSnapshot::TemporarilyFloating => "temporary-floating",
        WindowParticipationSnapshot::OverflowFloating => "overflow-floating",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_snapshot_format_is_human_readable() {
        let snapshot = DaemonStateSnapshot {
            config_path: Some(r"C:\winland.toml".to_owned()),
            config_version: 5,
            config_loaded_at_unix_ms: 1000,
            total_windows: 3,
            manageable_windows: 2,
            floating_windows: 1,
            temporary_floating_windows: 0,
            active_workspace: 4,
            foreground_window: Some(0xCAFE),
            monitors: vec![winland_ipc::MonitorStateSnapshot {
                monitor_id: 1,
                workspace_id: 4,
                focused: true,
            }],
            windows: vec![winland_ipc::WindowStateSnapshot {
                handle: 0xCAFE,
                title: "Editor".to_owned(),
                monitor_id: Some(1),
                workspace_id: Some(4),
                focused: true,
                is_minimized: false,
                participation: WindowParticipationSnapshot::Floating,
                constrained: true,
                visible_on_active_workspace: true,
            }],
            performance: test_performance(),
        };

        let output = format_state_snapshot(&snapshot);

        assert!(output.contains("IPC protocol v1"));
        assert!(output.contains("Config: v5 from C:\\winland.toml loaded at 1000"));
        assert!(output.contains("3 total, 2 manageable"));
        assert!(output.contains("Workspace: active 4"));
        assert!(output.contains("Foreground: 0xCAFE"));
        assert!(output.contains("0x1 workspace 4 focused"));
        assert!(output.contains("0xCAFE ws 4 mon 0x1 floating constrained visible"));
    }

    #[test]
    fn reload_config_report_format_is_human_readable() {
        let report = ReloadConfigReport {
            config_path: None,
            config_version: 2,
            reloaded_at_unix_ms: 42,
            changed_sections: vec!["hotkeys".to_owned(), "window-rules".to_owned()],
            state: DaemonStateSnapshot {
                config_path: None,
                config_version: 2,
                config_loaded_at_unix_ms: 42,
                total_windows: 3,
                manageable_windows: 2,
                floating_windows: 0,
                temporary_floating_windows: 0,
                active_workspace: 1,
                foreground_window: None,
                monitors: Vec::new(),
                windows: Vec::new(),
                performance: test_performance(),
            },
        };

        let output = format_reload_config_report(&report);

        assert!(output.contains("Config reloaded successfully"));
        assert!(output.contains("v2 from <defaults>"));
        assert!(output.contains("hotkeys, window-rules"));
        assert!(output.contains("3 total, 2 manageable"));
    }

    #[test]
    fn built_in_taskbar_slint_source_compiles() {
        let definition = compile_slint_source(
            TASKBAR_WIDGET_SOURCE,
            &PathBuf::from(TASKBAR_WIDGET_PATH),
            "TaskbarWidget",
        )
        .unwrap();

        assert_eq!(definition.name(), "TaskbarWidget");
    }

    #[test]
    fn widget_data_maps_daemon_state_to_workspace_and_window_rows() {
        let mut data = WidgetData::new(2);
        data.apply(WidgetUpdate::DaemonState(DaemonStateSnapshot {
            config_path: None,
            config_version: 1,
            config_loaded_at_unix_ms: 10,
            total_windows: 1,
            manageable_windows: 1,
            floating_windows: 0,
            temporary_floating_windows: 0,
            active_workspace: 2,
            foreground_window: Some(0xCAFE),
            monitors: vec![winland_ipc::MonitorStateSnapshot {
                monitor_id: 1,
                workspace_id: 2,
                focused: true,
            }],
            windows: vec![winland_ipc::WindowStateSnapshot {
                handle: 0xCAFE,
                title: "Editor".to_owned(),
                monitor_id: Some(1),
                workspace_id: Some(2),
                focused: true,
                is_minimized: false,
                participation: WindowParticipationSnapshot::Tiled,
                constrained: false,
                visible_on_active_workspace: true,
            }],
            performance: test_performance(),
        }));

        assert_eq!(data.workspaces.len(), 2);
        assert!(data.workspaces[1].active);
        assert_eq!(data.workspaces[1].window_count, 1);
        assert_eq!(data.windows[0].title, "Editor");
        assert!(data.windows[0].focused);
        assert!(
            data.windows[0]
                .command
                .ends_with(" command focus-window 0xCAFE")
        );
    }

    #[test]
    fn widget_data_shows_current_workspace_windows_and_keeps_minimized_rows() {
        let mut data = WidgetData::new(3);
        data.apply(WidgetUpdate::DaemonState(DaemonStateSnapshot {
            config_path: None,
            config_version: 1,
            config_loaded_at_unix_ms: 10,
            total_windows: 2,
            manageable_windows: 1,
            floating_windows: 0,
            temporary_floating_windows: 0,
            active_workspace: 1,
            foreground_window: Some(0xBEEF),
            monitors: vec![winland_ipc::MonitorStateSnapshot {
                monitor_id: 1,
                workspace_id: 2,
                focused: true,
            }],
            windows: vec![
                winland_ipc::WindowStateSnapshot {
                    handle: 0xCAFE,
                    title: "Other Workspace".to_owned(),
                    monitor_id: Some(1),
                    workspace_id: Some(1),
                    focused: false,
                    is_minimized: false,
                    participation: WindowParticipationSnapshot::Tiled,
                    constrained: false,
                    visible_on_active_workspace: false,
                },
                winland_ipc::WindowStateSnapshot {
                    handle: 0xBEEF,
                    title: "Minimized Editor".to_owned(),
                    monitor_id: Some(1),
                    workspace_id: Some(2),
                    focused: true,
                    is_minimized: true,
                    participation: WindowParticipationSnapshot::Tiled,
                    constrained: false,
                    visible_on_active_workspace: true,
                },
            ],
            performance: test_performance(),
        }));

        assert_eq!(
            data.windows
                .iter()
                .map(|window| window.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Minimized Editor"]
        );
        assert!(data.windows[0].is_minimized);
        assert_eq!(data.active_workspace, 2);
    }

    #[test]
    fn plugin_json_accepts_label_and_text_fields() {
        let block = parse_plugin_block(
            "my-plugin",
            r#"{"label":"CPU","text":"14%","ignored":["nested"]}"#,
        )
        .unwrap();

        assert_eq!(
            block,
            PluginWidgetBlock {
                source: "my-plugin".to_owned(),
                label: "CPU".to_owned(),
                text: "14%".to_owned(),
            }
        );
    }

    #[test]
    fn framed_ipc_decoder_handles_combined_and_split_json_lines() {
        let first =
            winland_ipc::encode_response(&winland_ipc::IpcResponse::state(test_snapshot(1, "One")))
                .unwrap();
        let second =
            winland_ipc::encode_response(&winland_ipc::IpcResponse::state(test_snapshot(2, "Two")))
                .unwrap();
        let mut combined = first.clone();
        combined.extend_from_slice(&second[..8]);

        let mut buffer = Vec::new();
        let decoded = decode_framed_ipc_responses(&mut buffer, &combined);

        assert_eq!(decoded.len(), 1);
        assert!(!buffer.is_empty());

        let decoded = decode_framed_ipc_responses(&mut buffer, &second[8..]);
        assert_eq!(decoded.len(), 1);
        let response = decoded.into_iter().next().unwrap().unwrap();
        let winland_ipc::IpcResponseResult::State(snapshot) = response.result else {
            panic!("expected state response");
        };
        assert_eq!(snapshot.windows[0].title, "Two");
    }

    fn test_performance() -> winland_ipc::DaemonPerformanceSnapshot {
        winland_ipc::DaemonPerformanceSnapshot {
            relayout_count: 0,
            skipped_relayout_count: 0,
            last_relayout_duration_ms: 0,
            last_relayout_move_count: 0,
            managed_window_count: 0,
            border_window_count: 0,
            game_mode_active: false,
            config_reload_count: 0,
        }
    }

    fn test_snapshot(handle: u64, title: &str) -> DaemonStateSnapshot {
        DaemonStateSnapshot {
            config_path: None,
            config_version: 1,
            config_loaded_at_unix_ms: 10,
            total_windows: 1,
            manageable_windows: 1,
            floating_windows: 0,
            temporary_floating_windows: 0,
            active_workspace: 1,
            foreground_window: Some(handle),
            monitors: vec![winland_ipc::MonitorStateSnapshot {
                monitor_id: 1,
                workspace_id: 1,
                focused: true,
            }],
            windows: vec![winland_ipc::WindowStateSnapshot {
                handle,
                title: title.to_owned(),
                monitor_id: Some(1),
                workspace_id: Some(1),
                focused: true,
                is_minimized: false,
                participation: WindowParticipationSnapshot::Tiled,
                constrained: false,
                visible_on_active_workspace: true,
            }],
            performance: test_performance(),
        }
    }
}
