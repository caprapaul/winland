use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use winland_config::{
    Config, HotkeyBindingConfig, HotkeyKey, HotkeyMode, HotkeyModifier, OverflowFocusPolicy,
    TextMatcherConfig,
};
use winland_core::{
    DwindleSplit, LayoutConfig, LayoutKind, MonitorId, MonitorInfo, Point, Rect, TileAssignment,
    WindowHandle, WindowInfo, WindowLayoutInfo, WindowParticipation, WindowRule,
    WindowRuleDecision, WindowSizeConstraints, WorkspaceId, WorkspaceManager,
    WorkspaceVisibilityChange, evaluate_window_rules, split_direction_for_point,
    tile_assignments_fit_work_area, tile_layout_windows_with_config,
    tile_layout_windows_with_state,
};
use winland_ipc::{
    DaemonStateSnapshot, IpcCommand, IpcRequest, IpcResponse, decode_request, encode_response,
};
use winland_win32::{
    BorderColor, BorderManager, BorderUpdate, HotkeyBinding, HotkeyBypassRules, HotkeyEvent,
    HotkeyId, HotkeyLowLevelEvent, HotkeyModifierSet, HotkeyOverrideOptions, IpcTransportRequest,
    ModifierDragOptions, MouseDragEvent, MouseDragEventKind, VirtualKey, WindowEvent,
    WindowEventKind,
};

const RECONCILE_DEBOUNCE: Duration = Duration::from_millis(50);
const MAX_BATCH_SIZE: usize = 512;
const TILE_FEEDBACK_PASSES: usize = 3;
const TILE_FEEDBACK_TOLERANCE_PX: i32 = 0;

fn main() -> Result<()> {
    let loaded_config = winland_config::load_or_default(None).context("load Winland config")?;
    init_tracing(&loaded_config.config.general.log_level);
    log_loaded_config(&loaded_config);
    let runtime_config = RuntimeConfig::from_config(&loaded_config.config)?;

    let (daemon_sender, daemon_receiver) = mpsc::channel();
    let (window_sender, window_receiver) = mpsc::channel();
    let subscription = winland_win32::subscribe_window_events(window_sender)
        .context("install documented Win32 window event hooks")?;
    let window_bridge = spawn_window_bridge(window_receiver, daemon_sender.clone())
        .context("spawn window event bridge")?;
    let (ipc_sender, ipc_receiver) = mpsc::channel();
    let _ipc_server =
        winland_win32::spawn_ipc_server(winland_win32::DEFAULT_IPC_PIPE_NAME, ipc_sender)
            .context("start local IPC named pipe server")?;
    let _ipc_bridge = spawn_ipc_bridge(ipc_receiver, daemon_sender.clone())
        .context("spawn IPC request bridge")?;

    let (hotkey_sender, hotkey_receiver) = mpsc::channel();
    let (hotkey_bindings, hotkey_commands) = hotkey_bindings_from_config(&loaded_config.config)?;
    let hotkey_backend = install_hotkey_backend(
        &loaded_config.config,
        hotkey_bindings.clone(),
        hotkey_sender,
    )?;
    debug!(
        backend = hotkey_backend.name(),
        "installed daemon hotkey backend"
    );
    let hotkey_bridge = spawn_hotkey_bridge(hotkey_receiver, daemon_sender.clone())
        .context("spawn hotkey bridge")?;
    let (mouse_drag_sender, mouse_drag_receiver) = mpsc::channel();
    let modifier_drag = install_modifier_drag(&loaded_config.config, mouse_drag_sender)?;
    let mouse_drag_bridge = spawn_mouse_drag_bridge(mouse_drag_receiver, daemon_sender.clone())
        .context("spawn modifier drag bridge")?;
    drop(daemon_sender);

    let mut state = DaemonState::discover(runtime_config, hotkey_commands)
        .context("build initial window snapshot")?;
    state.border_manager = Some(BorderManager::new().context("start border overlay manager")?);
    state.apply_startup_retile()?;
    state.sync_borders("startup border sync")?;
    let processor = thread::Builder::new()
        .name("winland-daemon-events".to_owned())
        .spawn(move || process_daemon_events(daemon_receiver, state))
        .context("spawn daemon event processor")?;

    info!("winland daemon started; entering Win32 message loop");
    let message_loop_result =
        winland_win32::run_message_loop().context("run Win32 daemon message loop");

    drop(hotkey_backend);
    drop(modifier_drag);
    drop(subscription);
    join_bridge(window_bridge, "window event bridge")?;
    join_bridge(hotkey_bridge, "hotkey bridge")?;
    join_bridge(mouse_drag_bridge, "modifier drag bridge")?;

    match processor.join() {
        Ok(Ok(())) => message_loop_result,
        Ok(Err(error)) => Err(error).context("process daemon events"),
        Err(_) => Err(anyhow!("daemon event processor thread panicked")),
    }
}

fn init_tracing(default_level: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    let log_path = daemon_log_path();
    let log_file = open_daemon_log_file(&log_path);
    let writer = DaemonLogWriter {
        file: log_file.map(|file| Arc::new(Mutex::new(file))),
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(false)
        .with_writer(writer)
        .init();

    info!(path = %log_path.display(), "writing daemon logs to file");
}

#[derive(Clone)]
struct DaemonLogWriter {
    file: Option<Arc<Mutex<File>>>,
}

struct DaemonLogOutput {
    file: Option<Arc<Mutex<File>>>,
}

impl<'a> MakeWriter<'a> for DaemonLogWriter {
    type Writer = DaemonLogOutput;

    fn make_writer(&'a self) -> Self::Writer {
        DaemonLogOutput {
            file: self.file.clone(),
        }
    }
}

impl Write for DaemonLogOutput {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if let Some(file) = &self.file
            && let Ok(mut file) = file.lock()
        {
            let _ = file.write_all(buffer);
        }

        io::stdout().write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = &self.file
            && let Ok(mut file) = file.lock()
        {
            let _ = file.flush();
        }

        io::stdout().flush()
    }
}

fn daemon_log_path() -> PathBuf {
    if let Some(path) = std::env::var_os("WINLAND_LOG_FILE") {
        return PathBuf::from(path);
    }

    std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.parent()
                .map(|parent| parent.join("winland-daemon.log"))
        })
        .unwrap_or_else(|| std::env::temp_dir().join("winland-daemon.log"))
}

fn open_daemon_log_file(path: &Path) -> Option<File> {
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "failed to create Winland log directory '{}': {error}",
            parent.display()
        );
        return None;
    }

    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(file) => Some(file),
        Err(error) => {
            eprintln!(
                "failed to open Winland log file '{}': {error}",
                path.display()
            );
            None
        }
    }
}

fn log_loaded_config(loaded: &winland_config::LoadedConfig) {
    match &loaded.path {
        Some(path) => info!(path = %path.display(), "loaded Winland config"),
        None => info!("no Winland config file found; using built-in defaults"),
    }
}

#[derive(Debug, Clone)]
struct RuntimeConfig {
    layout: LayoutConfig,
    layout_per_monitor: BTreeMap<String, LayoutConfig>,
    layout_per_workspace: BTreeMap<WorkspaceId, LayoutConfig>,
    workspace_count: u16,
    window_rules: Vec<WindowRule>,
    startup_retile: bool,
    dynamic_retile: bool,
    drag_to_float: bool,
    retile_on_drag_end: bool,
    overflow_focus_policy: OverflowFocusPolicy,
    borders: RuntimeBorderConfig,
}

impl RuntimeConfig {
    fn from_config(config: &Config) -> Result<Self> {
        Ok(Self {
            layout: config.layout_config(),
            layout_per_monitor: layout_per_monitor_from_config(config),
            layout_per_workspace: layout_per_workspace_from_config(config),
            workspace_count: config.workspace_count(),
            window_rules: config.window_rules().context("convert window rules")?,
            startup_retile: config.behavior.startup_retile,
            dynamic_retile: config.behavior.dynamic_retile,
            drag_to_float: config.behavior.drag_to_float,
            retile_on_drag_end: config.behavior.retile_on_drag_end,
            overflow_focus_policy: config.behavior.overflow_focus_policy,
            borders: RuntimeBorderConfig::from_config(&config.borders)?,
        })
    }

    fn layout_for_monitor(&self, monitor: &MonitorInfo, workspace: WorkspaceId) -> LayoutConfig {
        layout_config_for_monitor(
            self.layout,
            &self.layout_per_monitor,
            &self.layout_per_workspace,
            monitor,
            workspace,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuntimeBorderConfig {
    enabled: bool,
    width: i32,
    active_color: BorderColor,
    inactive_color: BorderColor,
    floating_color: BorderColor,
    show_inactive: bool,
    disable_when_fullscreen: bool,
}

impl RuntimeBorderConfig {
    fn from_config(config: &winland_config::BordersConfig) -> Result<Self> {
        Ok(Self {
            enabled: config.enabled,
            width: i32::from(config.width),
            active_color: parse_border_color(&config.active_color)
                .context("parse borders.active_color")?,
            inactive_color: parse_border_color(&config.inactive_color)
                .context("parse borders.inactive_color")?,
            floating_color: parse_border_color(&config.floating_color)
                .context("parse borders.floating_color")?,
            show_inactive: config.show_inactive,
            disable_when_fullscreen: config.disable_when_fullscreen,
        })
    }
}

fn parse_border_color(input: &str) -> Result<BorderColor> {
    let hex = input
        .strip_prefix('#')
        .ok_or_else(|| anyhow!("border color must use #RRGGBB syntax"))?;
    if hex.len() != 6 {
        return Err(anyhow!("border color must use #RRGGBB syntax"));
    }

    let red = u8::from_str_radix(&hex[0..2], 16)?;
    let green = u8::from_str_radix(&hex[2..4], 16)?;
    let blue = u8::from_str_radix(&hex[4..6], 16)?;
    Ok(BorderColor::new(red, green, blue))
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self::from_config(&Config::default()).expect("built-in config defaults are valid")
    }
}

fn layout_per_monitor_from_config(config: &Config) -> BTreeMap<String, LayoutConfig> {
    config
        .layout
        .per_monitor
        .iter()
        .map(|(monitor, override_config)| {
            (
                monitor.clone(),
                merge_layout_override(config.layout_config(), override_config),
            )
        })
        .collect()
}

fn layout_per_workspace_from_config(config: &Config) -> BTreeMap<WorkspaceId, LayoutConfig> {
    config
        .layout
        .per_workspace
        .iter()
        .filter_map(|(workspace, override_config)| {
            let workspace = workspace.parse::<u16>().ok().map(WorkspaceId)?;
            Some((
                workspace,
                merge_layout_override(config.layout_config(), override_config),
            ))
        })
        .collect()
}

fn merge_layout_override(
    base: LayoutConfig,
    override_config: &winland_config::LayoutOverride,
) -> LayoutConfig {
    LayoutConfig {
        kind: override_config
            .layout
            .as_deref()
            .and_then(LayoutKind::from_name)
            .unwrap_or(base.kind),
        gap: override_config.gap.map(i32::from).unwrap_or(base.gap),
        border: override_config.border.map(i32::from).unwrap_or(base.border),
        master_ratio_percent: override_config
            .master_ratio_percent
            .unwrap_or(base.master_ratio_percent),
        smart_split: override_config.smart_split.unwrap_or(base.smart_split),
        preserve_split: override_config
            .preserve_split
            .unwrap_or(base.preserve_split),
    }
    .normalized()
}

fn layout_config_for_monitor(
    default_layout: LayoutConfig,
    per_monitor: &BTreeMap<String, LayoutConfig>,
    per_workspace: &BTreeMap<WorkspaceId, LayoutConfig>,
    monitor: &MonitorInfo,
    workspace: WorkspaceId,
) -> LayoutConfig {
    let mut layout = per_workspace
        .get(&workspace)
        .copied()
        .unwrap_or(default_layout);

    if let Some(primary_layout) = monitor
        .is_primary
        .then(|| per_monitor.get("primary").copied())
        .flatten()
    {
        layout = primary_layout;
    }

    per_monitor
        .get(&monitor.id.to_string())
        .copied()
        .unwrap_or(layout)
        .normalized()
}

fn spawn_window_bridge(
    receiver: Receiver<WindowEvent>,
    sender: Sender<DaemonEvent>,
) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("winland-window-event-bridge".to_owned())
        .spawn(move || {
            for event in receiver {
                if sender.send(DaemonEvent::Window(event)).is_err() {
                    break;
                }
            }
        })
        .map_err(Into::into)
}

fn spawn_hotkey_bridge(
    receiver: Receiver<HotkeyEvent>,
    sender: Sender<DaemonEvent>,
) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("winland-hotkey-bridge".to_owned())
        .spawn(move || {
            for event in receiver {
                if sender.send(DaemonEvent::Hotkey(event)).is_err() {
                    break;
                }
            }
        })
        .map_err(Into::into)
}

fn spawn_mouse_drag_bridge(
    receiver: Receiver<MouseDragEvent>,
    sender: Sender<DaemonEvent>,
) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("winland-modifier-drag-bridge".to_owned())
        .spawn(move || {
            for event in receiver {
                if sender.send(DaemonEvent::MouseDrag(event)).is_err() {
                    break;
                }
            }
        })
        .map_err(Into::into)
}

fn spawn_ipc_bridge(
    receiver: Receiver<IpcTransportRequest>,
    sender: Sender<DaemonEvent>,
) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("winland-ipc-bridge".to_owned())
        .spawn(move || {
            for request in receiver {
                if sender.send(DaemonEvent::Ipc(request)).is_err() {
                    break;
                }
            }
        })
        .map_err(Into::into)
}

fn join_bridge(handle: JoinHandle<()>, name: &'static str) -> Result<()> {
    handle.join().map_err(|_| anyhow!("{name} thread panicked"))
}

fn process_daemon_events(receiver: Receiver<DaemonEvent>, mut state: DaemonState) -> Result<()> {
    state.log_snapshot("initial window snapshot");

    while let Ok(event) = receiver.recv() {
        match event {
            DaemonEvent::Window(first_event) => {
                if should_process_window_event_immediately(first_event) {
                    state.reconcile_after_events(&[first_event])?;
                } else {
                    let batch = receive_window_batch(&receiver, &mut state, first_event)?;
                    state.reconcile_after_events(&batch)?;
                }
            }
            DaemonEvent::Hotkey(event) => state.handle_hotkey(event)?,
            DaemonEvent::MouseDrag(event) => state.handle_mouse_drag(event)?,
            DaemonEvent::Ipc(request) => state.handle_ipc(request),
        }
    }

    info!("daemon event channel closed; event processor stopping");
    Ok(())
}

fn should_process_window_event_immediately(event: WindowEvent) -> bool {
    matches!(
        event.kind,
        WindowEventKind::Moved
            | WindowEventKind::MoveSizeStart
            | WindowEventKind::MoveSizeEnd
            | WindowEventKind::ForegroundChanged
    )
}

fn receive_window_batch(
    receiver: &Receiver<DaemonEvent>,
    state: &mut DaemonState,
    first_event: WindowEvent,
) -> Result<Vec<WindowEvent>> {
    let mut batch = vec![first_event];

    while batch.len() < MAX_BATCH_SIZE {
        match receiver.recv_timeout(RECONCILE_DEBOUNCE) {
            Ok(DaemonEvent::Window(event)) => batch.push(event),
            Ok(DaemonEvent::Hotkey(event)) => state.handle_hotkey(event)?,
            Ok(DaemonEvent::MouseDrag(event)) => state.handle_mouse_drag(event)?,
            Ok(DaemonEvent::Ipc(request)) => state.handle_ipc(request),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(batch)
}

#[derive(Debug)]
enum DaemonEvent {
    Window(WindowEvent),
    Hotkey(HotkeyEvent),
    MouseDrag(MouseDragEvent),
    Ipc(IpcTransportRequest),
}

#[derive(Debug)]
struct DaemonState {
    windows: BTreeMap<WindowHandle, WindowInfo>,
    foreground: Option<WindowHandle>,
    tile_order: Vec<WindowHandle>,
    participation: BTreeMap<WindowHandle, WindowParticipation>,
    dwindle_splits: BTreeMap<(WorkspaceId, MonitorId), Vec<DwindleSplit>>,
    previous_rects: BTreeMap<WindowHandle, Rect>,
    learned_size_constraints: BTreeMap<WindowHandle, WindowSizeConstraints>,
    window_monitor_overrides: BTreeMap<WindowHandle, MonitorId>,
    overflow_floating: BTreeSet<WindowHandle>,
    workspaces: WorkspaceManager,
    active_modifier_drag: Option<ActiveModifierDrag>,
    suppressed_modifier_drag_events: BTreeSet<WindowHandle>,
    config: RuntimeConfig,
    hotkey_commands: HotkeyCommandMap,
    border_manager: Option<BorderManager>,
}

#[derive(Debug, Clone, Copy)]
struct ActiveModifierDrag {
    window: WindowHandle,
    start_cursor: Point,
    last_cursor: Point,
    start_rect: Rect,
    move_count: u32,
    started_temporary_float: bool,
}

#[derive(Debug, Clone, Copy)]
struct DropContext {
    rect: Rect,
    cursor: Option<Point>,
}

impl DaemonState {
    fn discover(config: RuntimeConfig, hotkey_commands: HotkeyCommandMap) -> Result<Self> {
        let windows = winland_win32::enumerate_windows()
            .context("enumerate windows for daemon snapshot")?
            .into_iter()
            .map(|window| (window.handle, window))
            .collect();
        let foreground = winland_win32::foreground_window().context("read foreground window")?;
        let monitors = winland_win32::enumerate_monitors()
            .context("enumerate monitors for daemon snapshot")?;

        let mut state = Self {
            windows,
            foreground,
            tile_order: Vec::new(),
            participation: BTreeMap::new(),
            dwindle_splits: BTreeMap::new(),
            previous_rects: BTreeMap::new(),
            learned_size_constraints: BTreeMap::new(),
            window_monitor_overrides: BTreeMap::new(),
            overflow_floating: BTreeSet::new(),
            workspaces: WorkspaceManager::new(config.workspace_count),
            active_modifier_drag: None,
            suppressed_modifier_drag_events: BTreeSet::new(),
            config,
            hotkey_commands,
            border_manager: None,
        };
        state.tile_order = state.manageable_handles_sorted();
        state.sync_workspace_state(&monitors);

        Ok(state)
    }

    fn apply_startup_retile(&mut self) -> Result<()> {
        if !self.config.startup_retile {
            return Ok(());
        }

        let monitors =
            winland_win32::enumerate_monitors().context("enumerate monitors for startup retile")?;
        self.sync_workspace_state(&monitors);
        let assignments = self.tile_assignments(&monitors);
        self.apply_tile_assignments_with_feedback(&assignments, &monitors, "startup retile");
        info!(
            move_count = assignments.len(),
            "completed startup retile request"
        );
        Ok(())
    }

    fn reconcile_after_events(&mut self, batch: &[WindowEvent]) -> Result<()> {
        for event in batch {
            debug!(
                kind = ?event.kind,
                window = %event.window,
                event_time = event.event_time,
                "observed window event"
            );
        }

        let filtered_batch: Vec<_> = batch
            .iter()
            .copied()
            .filter(|event| !self.should_ignore_modifier_drag_window_event(*event))
            .collect();
        let ignored_modifier_drag_events = batch.len().saturating_sub(filtered_batch.len());
        self.suppressed_modifier_drag_events.clear();
        if ignored_modifier_drag_events > 0 {
            debug!(
                ignored_modifier_drag_events,
                "ignored modifier-drag window events"
            );
        }
        if filtered_batch.is_empty() {
            return Ok(());
        }
        let batch = filtered_batch.as_slice();

        if self.reconcile_low_latency_window_events(batch)? {
            return Ok(());
        }

        let mut refreshed = Self::discover(self.config.clone(), self.hotkey_commands.clone())
            .context("refresh window snapshot after event batch")?;
        let monitors = winland_win32::enumerate_monitors()
            .context("enumerate monitors while preserving daemon state")?;
        let diff = self.diff(&refreshed, &monitors);
        self.preserve_keyboard_state(&mut refreshed, &monitors);
        let border_manager = self.border_manager.take();
        *self = refreshed;
        self.border_manager = border_manager;
        let event_plan = self.plan_after_window_events(batch, &diff, &monitors);
        self.apply_tile_assignments_with_feedback(&event_plan.moves, &monitors, "dynamic retile");
        self.sync_borders_with_monitors(&monitors, "event reconciliation borders")?;

        debug!(
            event_count = batch.len(),
            created_events = count_events(batch, WindowEventKind::Created),
            destroyed_events = count_events(batch, WindowEventKind::Destroyed),
            shown_events = count_events(batch, WindowEventKind::Shown),
            hidden_events = count_events(batch, WindowEventKind::Hidden),
            moved_events = count_events(batch, WindowEventKind::Moved),
            movesize_start_events = count_events(batch, WindowEventKind::MoveSizeStart),
            movesize_end_events = count_events(batch, WindowEventKind::MoveSizeEnd),
            minimized_events = count_events(batch, WindowEventKind::Minimized),
            restored_events = count_events(batch, WindowEventKind::Restored),
            foreground_events = count_events(batch, WindowEventKind::ForegroundChanged),
            metadata_events = count_events(batch, WindowEventKind::MetadataChanged),
            total_windows = self.windows.len(),
            manageable_windows = self.manageable_window_count(),
            floating_windows = self.floating_window_count(),
            temporary_floating_windows = self.temporary_floating_window_count(),
            active_workspace = %self.workspaces.active_workspace(),
            added = diff.added.len(),
            removed = diff.removed.len(),
            changed = diff.changed,
            moved_between_monitors = diff.moved_between_monitors,
            foreground_changed = diff.foreground_changed,
            retile_moves = event_plan.moves.len(),
            "reconciled window snapshot details"
        );

        info!(
            event_count = batch.len(),
            moved_events = count_events(batch, WindowEventKind::Moved),
            movesize_start_events = count_events(batch, WindowEventKind::MoveSizeStart),
            movesize_end_events = count_events(batch, WindowEventKind::MoveSizeEnd),
            retile_moves = event_plan.moves.len(),
            "reconciled window snapshot"
        );

        if !diff.added.is_empty() {
            debug!(windows = ?diff.added, "windows added to snapshot");
        }
        if !diff.removed.is_empty() {
            debug!(windows = ?diff.removed, "windows removed from snapshot");
        }

        Ok(())
    }

    fn handle_hotkey(&mut self, event: HotkeyEvent) -> Result<()> {
        let Some(command) = self.hotkey_commands.command(event.id) else {
            warn!(id = event.id.0, "ignoring unrecognized daemon hotkey");
            return Ok(());
        };

        info!(?command, "routing daemon hotkey command");
        self.execute_command(command)
    }

    fn should_ignore_modifier_drag_window_event(&self, event: WindowEvent) -> bool {
        matches!(
            event.kind,
            WindowEventKind::Moved | WindowEventKind::MoveSizeStart | WindowEventKind::MoveSizeEnd
        ) && (self
            .active_modifier_drag
            .is_some_and(|drag| drag.window == event.window)
            || self.suppressed_modifier_drag_events.contains(&event.window))
    }

    fn reconcile_low_latency_window_events(&mut self, batch: &[WindowEvent]) -> Result<bool> {
        if batch.is_empty() {
            return Ok(true);
        }

        if batch
            .iter()
            .all(|event| event.kind == WindowEventKind::Moved)
        {
            return self.reconcile_moved_window_events(batch);
        }

        if batch.len() != 1 {
            return Ok(false);
        }

        match batch[0].kind {
            WindowEventKind::MoveSizeStart | WindowEventKind::MoveSizeEnd => {
                self.reconcile_movesize_event(batch[0])
            }
            WindowEventKind::ForegroundChanged => {
                self.foreground =
                    winland_win32::foreground_window().context("read foreground window")?;
                self.sync_borders("foreground border update")?;
                debug!(
                    foreground = ?self.foreground,
                    "updated foreground state without full snapshot rebuild"
                );
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn reconcile_moved_window_events(&mut self, batch: &[WindowEvent]) -> Result<bool> {
        let mut handles = BTreeSet::new();
        for event in batch {
            if !self.windows.contains_key(&event.window) {
                return Ok(false);
            }
            handles.insert(event.window);
        }

        let monitors = winland_win32::enumerate_monitors()
            .context("enumerate monitors for low-latency move event")?;
        let mut moved_between_monitors = false;
        let mut updated = 0usize;

        for window in handles {
            let Some(old_rect) = self.windows.get(&window).map(|info| info.rect) else {
                return Ok(false);
            };
            let old_monitor = monitor_for_rect(old_rect, &monitors);
            let rect = winland_win32::window_rect_for_handle(window)
                .with_context(|| format!("read moved window rect for {window}"))?;
            let new_monitor = monitor_for_rect(rect, &monitors);

            if old_rect != rect {
                if let Some(info) = self.windows.get_mut(&window) {
                    info.rect = rect;
                }
                self.workspaces.update_window_rect(window, rect);
                updated += 1;
            }

            if old_monitor.is_some() && new_monitor.is_some() && old_monitor != new_monitor {
                moved_between_monitors = true;
            }
        }

        if moved_between_monitors && self.config.dynamic_retile {
            let assignments = self.tile_assignments(&monitors);
            self.apply_tile_assignments_with_feedback(
                &assignments,
                &monitors,
                "low-latency monitor move retile",
            );
            debug!(
                updated_windows = updated,
                retile_moves = assignments.len(),
                "handled moved window events with monitor retile"
            );
        } else {
            debug!(
                updated_windows = updated,
                moved_between_monitors, "handled moved window events without full snapshot rebuild"
            );
        }

        self.sync_borders_with_monitors(&monitors, "low-latency moved borders")?;
        Ok(true)
    }

    fn reconcile_movesize_event(&mut self, event: WindowEvent) -> Result<bool> {
        if !self.windows.contains_key(&event.window) {
            return Ok(false);
        }

        if let Ok(rect) = winland_win32::window_rect_for_handle(event.window) {
            if let Some(info) = self.windows.get_mut(&event.window) {
                info.rect = rect;
            }
            self.workspaces.update_window_rect(event.window, rect);
        }

        let monitors = winland_win32::enumerate_monitors()
            .context("enumerate monitors for low-latency movesize event")?;
        let plan = self.plan_after_window_events(&[event], &SnapshotDiff::default(), &monitors);
        self.apply_tile_assignments_with_feedback(
            &plan.moves,
            &monitors,
            "low-latency movesize retile",
        );
        self.sync_borders_with_monitors(&monitors, "low-latency movesize borders")?;
        debug!(
            kind = ?event.kind,
            window = %event.window,
            retile_moves = plan.moves.len(),
            "handled movesize event without full snapshot rebuild"
        );
        Ok(true)
    }

    fn handle_mouse_drag(&mut self, event: MouseDragEvent) -> Result<()> {
        match event.kind {
            MouseDragEventKind::Started => self.start_modifier_drag(event),
            MouseDragEventKind::Moved => self.move_modifier_drag(event),
            MouseDragEventKind::Ended => self.end_modifier_drag(event),
        }
    }

    fn start_modifier_drag(&mut self, event: MouseDragEvent) -> Result<()> {
        self.foreground = Some(event.window);
        if let Err(error) = winland_win32::focus_window(event.window) {
            debug!(
                window = %event.window,
                %error,
                "failed to focus window at modifier drag start"
            );
        }

        let start_rect =
            winland_win32::window_rect_for_handle(event.window).with_context(|| {
                format!("read window rect before modifier drag for {}", event.window)
            })?;
        let started_temporary_float = self.handle_movesize_start(event.window);

        self.active_modifier_drag = Some(ActiveModifierDrag {
            window: event.window,
            start_cursor: event.cursor,
            last_cursor: event.cursor,
            start_rect,
            move_count: 0,
            started_temporary_float,
        });
        if let Err(error) = self.sync_borders("modifier drag start borders") {
            debug!(%error, "failed to sync borders after modifier drag start");
        }

        if started_temporary_float && self.config.dynamic_retile {
            let monitors = winland_win32::enumerate_monitors()
                .context("enumerate monitors after modifier drag start")?;
            let assignments = self.tile_assignments(&monitors);
            self.apply_tile_assignments_with_feedback(
                &assignments,
                &monitors,
                "modifier drag start",
            );
        }

        info!(
            window = %event.window,
            start_cursor = ?event.cursor,
            start_rect = %start_rect,
            started_temporary_float,
            participation = ?self.window_participation(event.window),
            "drag.start"
        );
        Ok(())
    }

    fn move_modifier_drag(&mut self, event: MouseDragEvent) -> Result<()> {
        let Some(drag) = self.active_modifier_drag.as_mut() else {
            return Ok(());
        };
        if drag.window != event.window {
            return Ok(());
        }

        let rect = offset_rect_by_cursor_delta(drag.start_rect, drag.start_cursor, event.cursor);
        drag.last_cursor = event.cursor;
        drag.move_count = drag.move_count.saturating_add(1);
        self.apply_modifier_drag_move_rect(event.window, rect);

        Ok(())
    }

    fn end_modifier_drag(&mut self, event: MouseDragEvent) -> Result<()> {
        let Some(drag) = self.active_modifier_drag.take() else {
            return Ok(());
        };
        if drag.window != event.window {
            return Ok(());
        }

        let final_rect =
            offset_rect_by_cursor_delta(drag.start_rect, drag.start_cursor, event.cursor);
        let dropped_rect = self.apply_modifier_drag_final_rect(event.window, final_rect);

        info!(
            window = %event.window,
            end_cursor = ?event.cursor,
            requested_rect = %final_rect,
            accepted_rect = %dropped_rect,
            started_temporary_float = drag.started_temporary_float,
            move_count = drag.move_count,
            last_move_cursor = ?drag.last_cursor,
            "drag.end"
        );

        if drag.started_temporary_float {
            let monitors = winland_win32::enumerate_monitors()
                .context("enumerate monitors after modifier drag end")?;
            let drop_context = DropContext {
                rect: dropped_rect,
                cursor: Some(event.cursor),
            };
            if self.handle_movesize_end(event.window, &monitors, Some(drop_context))
                && self.config.retile_on_drag_end
            {
                self.suppressed_modifier_drag_events.insert(event.window);
                let assignments = self.tile_assignments(&monitors);
                info!(
                    window = %event.window,
                    tile_order = ?self.tile_order,
                    assignment_count = assignments.len(),
                    "drag.retile"
                );
                self.apply_tile_assignments_with_feedback(
                    &assignments,
                    &monitors,
                    "modifier drag end",
                );
            }
        }
        Ok(())
    }

    fn apply_modifier_drag_final_rect(&mut self, window: WindowHandle, rect: Rect) -> Rect {
        if let Err(error) = winland_win32::move_resize_window(window, rect) {
            warn!(
                window = %window,
                rect = %rect,
                %error,
                "failed to apply final modifier drag rect"
            );
        }

        let accepted_rect = match winland_win32::window_rect_for_handle(window) {
            Ok(actual) => actual,
            Err(error) => {
                debug!(
                    window = %window,
                    rect = %rect,
                    %error,
                    "failed to read accepted modifier drag rect; using requested rect"
                );
                rect
            }
        };

        if accepted_rect != rect {
            debug!(
                window = %window,
                requested = %rect,
                accepted = %accepted_rect,
                "Windows adjusted final modifier drag rect"
            );
        }

        if let Some(info) = self.windows.get_mut(&window) {
            info.rect = accepted_rect;
        }
        self.workspaces.update_window_rect(window, accepted_rect);
        if let Err(error) = self.sync_borders("modifier drag final borders") {
            debug!(%error, "failed to sync borders after modifier drag final rect");
        }
        accepted_rect
    }

    fn apply_modifier_drag_move_rect(&mut self, window: WindowHandle, rect: Rect) {
        if let Err(error) = winland_win32::move_resize_window(window, rect) {
            warn!(
                window = %window,
                rect = %rect,
                %error,
                "failed to move window during modifier drag"
            );
        }
        if let Some(info) = self.windows.get_mut(&window) {
            info.rect = rect;
        }
        self.workspaces.update_window_rect(window, rect);
        if let Err(error) = self.sync_borders("modifier drag move borders") {
            debug!(%error, "failed to sync borders after modifier drag move");
        }
    }

    fn handle_ipc(&mut self, transport: IpcTransportRequest) {
        let response = match decode_request(&transport.request) {
            Ok(request) => self.handle_ipc_request(request),
            Err(error) => {
                warn!(%error, "rejecting invalid IPC request");
                IpcResponse::error(error.to_string())
            }
        };

        match encode_response(&response) {
            Ok(encoded) => {
                let _ = transport.response.send(encoded);
            }
            Err(error) => {
                warn!(%error, "failed to encode IPC response");
                let fallback = IpcResponse::error(error.to_string());
                if let Ok(encoded) = encode_response(&fallback) {
                    let _ = transport.response.send(encoded);
                }
            }
        }
    }

    fn handle_ipc_request(&self, request: IpcRequest) -> IpcResponse {
        match request.command {
            IpcCommand::State => IpcResponse::state(self.state_snapshot()),
        }
    }

    fn state_snapshot(&self) -> DaemonStateSnapshot {
        DaemonStateSnapshot {
            total_windows: self.windows.len(),
            manageable_windows: self.manageable_window_count(),
            floating_windows: self.floating_window_count(),
            temporary_floating_windows: self.temporary_floating_window_count(),
            active_workspace: self.workspaces.active_workspace().0,
            foreground_window: self.foreground.map(|handle| handle.0),
        }
    }

    fn execute_command(&mut self, command: DaemonCommand) -> Result<()> {
        if let DaemonCommand::Launch(command_line) = &command {
            winland_win32::launch_app(command_line)
                .with_context(|| format!("launch app from hotkey '{command_line}'"))?;
            return Ok(());
        }

        if command == DaemonCommand::Reload {
            let loaded_config =
                winland_config::load_or_default(None).context("reload Winland config")?;
            log_loaded_config(&loaded_config);
            let runtime_config = RuntimeConfig::from_config(&loaded_config.config)?;
            let mut refreshed = Self::discover(runtime_config, self.hotkey_commands.clone())
                .context("reload daemon window snapshot")?;
            let monitors = winland_win32::enumerate_monitors()
                .context("enumerate monitors while reloading daemon state")?;
            self.preserve_keyboard_state(&mut refreshed, &monitors);
            refreshed.dwindle_splits.clear();
            let border_manager = self.border_manager.take();
            *self = refreshed;
            self.border_manager = border_manager;
            self.log_snapshot("reloaded daemon state");
            self.sync_borders_with_monitors(&monitors, "reload borders")?;
            return Ok(());
        }

        let monitors = if command.needs_layout() {
            winland_win32::enumerate_monitors().context("enumerate monitors for daemon command")?
        } else {
            Vec::new()
        };
        let plan = self.plan_command(command, &monitors);

        if let Some(target) = plan.focus
            && let Err(error) = winland_win32::focus_window(target)
        {
            warn!(window = %target, %error, "failed to focus window from hotkey command");
        }

        for window in &plan.hide {
            if let Err(error) = winland_win32::hide_window(*window) {
                warn!(window = %window, %error, "failed to hide window during workspace command");
            }
        }

        for change in &plan.show {
            if let Some(rect) = change.restore_rect
                && let Err(error) = winland_win32::move_resize_window(change.window, rect)
            {
                warn!(
                    window = %change.window,
                    rect = %rect,
                    %error,
                    "failed to restore workspace window placement before showing it"
                );
            }

            if let Err(error) = winland_win32::show_window_without_activate(change.window) {
                warn!(
                    window = %change.window,
                    %error,
                    "failed to show window during workspace command"
                );
            }
        }

        self.apply_tile_assignments_with_feedback(&plan.moves, &monitors, "hotkey command");
        self.sync_borders("hotkey command borders")?;

        if plan.quit {
            winland_win32::request_message_loop_stop()
                .context("request daemon message loop stop from hotkey command")?;
        }

        Ok(())
    }

    fn plan_command(&mut self, command: DaemonCommand, monitors: &[MonitorInfo]) -> CommandPlan {
        match command {
            DaemonCommand::Focus(direction) => {
                let focus = self.focus_target(direction);
                if let Some(target) = focus {
                    self.foreground = Some(target);
                }

                CommandPlan {
                    focus,
                    ..CommandPlan::default()
                }
            }
            DaemonCommand::Swap(direction) => {
                self.swap_focused_with(direction);
                CommandPlan {
                    moves: self.tile_assignments(monitors),
                    ..CommandPlan::default()
                }
            }
            DaemonCommand::Retile => CommandPlan {
                moves: self.tile_assignments(monitors),
                ..CommandPlan::default()
            },
            DaemonCommand::ToggleFloat => {
                self.toggle_focused_float();
                CommandPlan {
                    moves: self.tile_assignments(monitors),
                    ..CommandPlan::default()
                }
            }
            DaemonCommand::SwitchWorkspace(workspace) => {
                let workspace_plan = self.switch_workspace(workspace, monitors);
                CommandPlan {
                    hide: workspace_plan.hide,
                    show: workspace_plan.show,
                    moves: self.tile_assignments(monitors),
                    ..CommandPlan::default()
                }
            }
            DaemonCommand::MoveFocusedToWorkspace(workspace) => {
                let hide = self.move_focused_to_workspace(workspace, monitors);
                CommandPlan {
                    hide,
                    moves: self.tile_assignments(monitors),
                    ..CommandPlan::default()
                }
            }
            DaemonCommand::Quit => CommandPlan {
                quit: true,
                ..CommandPlan::default()
            },
            DaemonCommand::Reload => CommandPlan {
                reload: true,
                ..CommandPlan::default()
            },
            DaemonCommand::Launch(_) => CommandPlan::default(),
        }
    }

    fn focus_target(&self, direction: FocusDirection) -> Option<WindowHandle> {
        let current = self
            .foreground
            .filter(|handle| self.is_manageable_window(*handle))
            .or_else(|| self.focusable_handles().into_iter().next())?;

        let current_center = self.windows.get(&current)?.rect.center();
        self.focusable_handles()
            .into_iter()
            .filter(|handle| *handle != current)
            .filter_map(|handle| {
                let center = self.windows.get(&handle)?.rect.center();
                let key = match direction {
                    FocusDirection::Left if center.x < current_center.x => Some((
                        current_center.x - center.x,
                        (current_center.y - center.y).abs(),
                    )),
                    FocusDirection::Right if center.x > current_center.x => Some((
                        center.x - current_center.x,
                        (current_center.y - center.y).abs(),
                    )),
                    FocusDirection::Up if center.y < current_center.y => Some((
                        current_center.y - center.y,
                        (current_center.x - center.x).abs(),
                    )),
                    FocusDirection::Down if center.y > current_center.y => Some((
                        center.y - current_center.y,
                        (current_center.x - center.x).abs(),
                    )),
                    _ => None,
                }?;

                Some((key, handle))
            })
            .min_by_key(|(key, _)| *key)
            .map(|(_, handle)| handle)
            .or_else(|| self.wrapping_focus_target(current, direction))
    }

    fn wrapping_focus_target(
        &self,
        current: WindowHandle,
        direction: FocusDirection,
    ) -> Option<WindowHandle> {
        let handles = self.focusable_handles();
        if handles.is_empty() {
            return None;
        }

        let index = handles
            .iter()
            .position(|handle| *handle == current)
            .unwrap_or(0);
        let next_index = match direction {
            FocusDirection::Left | FocusDirection::Up => {
                if index == 0 {
                    handles.len() - 1
                } else {
                    index - 1
                }
            }
            FocusDirection::Right | FocusDirection::Down => (index + 1) % handles.len(),
        };

        Some(handles[next_index])
    }

    fn swap_focused_with(&mut self, direction: FocusDirection) {
        let Some(current) = self
            .foreground
            .filter(|handle| self.is_manageable_window(*handle))
        else {
            return;
        };
        let Some(target) = self.focus_target(direction) else {
            return;
        };

        let current_index = self.tile_order.iter().position(|handle| *handle == current);
        let target_index = self.tile_order.iter().position(|handle| *handle == target);
        if let (Some(current_index), Some(target_index)) = (current_index, target_index) {
            self.tile_order.swap(current_index, target_index);
        }
    }

    fn toggle_focused_float(&mut self) {
        let Some(current) = self
            .foreground
            .filter(|handle| self.is_manageable_window(*handle))
        else {
            return;
        };

        self.remember_previous_rect(current);
        let next = match self.window_participation(current) {
            WindowParticipation::Floating | WindowParticipation::TemporarilyFloating => {
                WindowParticipation::Tiled
            }
            WindowParticipation::Tiled => WindowParticipation::Floating,
        };
        self.set_window_participation(current, next);
    }

    fn plan_after_window_events(
        &mut self,
        batch: &[WindowEvent],
        diff: &SnapshotDiff,
        monitors: &[MonitorInfo],
    ) -> CommandPlan {
        let mut should_retile = self.should_retile_after_events(batch, diff);

        if self.config.drag_to_float {
            for event in batch {
                match event.kind {
                    WindowEventKind::MoveSizeStart => {
                        if self.handle_movesize_start(event.window) {
                            should_retile = should_retile || self.config.dynamic_retile;
                        }
                    }
                    WindowEventKind::MoveSizeEnd => {
                        if self.handle_movesize_end(event.window, monitors, None)
                            && self.config.retile_on_drag_end
                        {
                            should_retile = true;
                        }
                    }
                    _ => {}
                }
            }
        }

        for event in batch {
            if event.kind == WindowEventKind::MetadataChanged {
                self.forget_learned_size_constraint(event.window);
            }
        }

        if should_retile {
            CommandPlan {
                moves: self.tile_assignments(monitors),
                ..CommandPlan::default()
            }
        } else {
            CommandPlan::default()
        }
    }

    fn should_retile_after_events(&self, batch: &[WindowEvent], diff: &SnapshotDiff) -> bool {
        if !self.config.dynamic_retile {
            return false;
        }

        batch.iter().any(|event| {
            matches!(
                event.kind,
                WindowEventKind::Created
                    | WindowEventKind::Destroyed
                    | WindowEventKind::Shown
                    | WindowEventKind::Hidden
                    | WindowEventKind::Minimized
                    | WindowEventKind::Restored
                    | WindowEventKind::MetadataChanged
            )
        }) || diff.moved_between_monitors > 0
    }

    fn handle_movesize_start(&mut self, window: WindowHandle) -> bool {
        if !self.config.drag_to_float {
            return false;
        }

        self.forget_learned_size_constraint(window);
        self.start_temporary_float(window)
    }

    fn handle_movesize_end(
        &mut self,
        window: WindowHandle,
        monitors: &[MonitorInfo],
        drop_context: Option<DropContext>,
    ) -> bool {
        if !self.config.drag_to_float {
            return false;
        }

        if self.window_participation(window) == WindowParticipation::TemporarilyFloating {
            if let Some(drop_context) = drop_context {
                self.reorder_temporary_float_by_drop_at(
                    window,
                    monitors,
                    drop_context.rect,
                    drop_context.cursor,
                );
            } else {
                self.reorder_temporary_float_by_drop(window, monitors);
            }
        }

        self.clear_temporary_float(window)
    }

    fn start_temporary_float(&mut self, window: WindowHandle) -> bool {
        if self.window_participation(window) == WindowParticipation::Floating
            || !self.is_tilable_window(window)
        {
            return false;
        }

        self.remember_previous_rect(window);
        self.set_window_participation(window, WindowParticipation::TemporarilyFloating);
        true
    }

    fn clear_temporary_float(&mut self, window: WindowHandle) -> bool {
        if self.window_participation(window) != WindowParticipation::TemporarilyFloating {
            return false;
        }

        self.set_window_participation(window, WindowParticipation::Tiled);
        true
    }

    fn reorder_temporary_float_by_drop(
        &mut self,
        window: WindowHandle,
        monitors: &[MonitorInfo],
    ) -> bool {
        let Some(window_info) = self.windows.get(&window) else {
            return false;
        };
        self.reorder_temporary_float_by_drop_at(window, monitors, window_info.rect, None)
    }

    fn reorder_temporary_float_by_drop_at(
        &mut self,
        window: WindowHandle,
        monitors: &[MonitorInfo],
        dropped_rect: Rect,
        cursor_position: Option<Point>,
    ) -> bool {
        let Some(target_monitor) = cursor_position
            .and_then(|point| monitor_for_point(point, monitors))
            .or_else(|| monitor_for_rect(dropped_rect, monitors))
        else {
            return false;
        };
        let Some(monitor) = monitors.iter().find(|monitor| monitor.id == target_monitor) else {
            return false;
        };
        self.window_monitor_overrides.insert(window, target_monitor);

        let target_handles: Vec<_> = self
            .tile_order
            .iter()
            .copied()
            .filter(|handle| *handle != window)
            .filter(|handle| self.window_participation(*handle).is_tiled())
            .filter(|handle| {
                self.windows.get(handle).is_some_and(|candidate| {
                    self.is_tilable_window(*handle)
                        && self.monitor_owns_window_rect(
                            *handle,
                            monitor,
                            self.window_layout_rect(*handle, candidate),
                            monitors,
                        )
                })
            })
            .collect();
        let layout = self
            .config
            .layout_for_monitor(monitor, self.workspaces.active_workspace());

        if layout.kind == LayoutKind::Dwindle
            && self.retarget_dwindle_drop(
                window,
                dropped_rect,
                cursor_position,
                &target_handles,
                monitor,
                layout,
            )
        {
            return true;
        }

        let drop_point = cursor_position.unwrap_or_else(|| dropped_rect.center());
        let local_index =
            self.drop_insert_index_at_point(window, drop_point, &target_handles, monitor);

        self.reinsert_window_at_local_index(window, &target_handles, local_index)
    }

    fn retarget_dwindle_drop(
        &mut self,
        window: WindowHandle,
        dropped_rect: Rect,
        cursor_position: Option<winland_core::Point>,
        target_handles: &[WindowHandle],
        monitor: &MonitorInfo,
        layout: LayoutConfig,
    ) -> bool {
        let drop_point = cursor_position.unwrap_or_else(|| dropped_rect.center());
        let workspace = self.workspaces.active_workspace();
        let split_key = (workspace, monitor.id);
        let mut preview_splits = self
            .dwindle_splits
            .get(&split_key)
            .cloned()
            .unwrap_or_default();
        let layout_windows = self.layout_windows_for_handles(target_handles);
        let assignments = tile_layout_windows_with_state(
            monitor.work_area,
            &layout_windows,
            layout,
            None,
            Some(&mut preview_splits),
        );
        let Some(target_assignment) = assignments
            .iter()
            .find(|assignment| assignment.rect.contains(drop_point))
            .or_else(|| nearest_assignment(drop_point, &assignments))
        else {
            return false;
        };

        let direction = split_direction_for_point(target_assignment.rect, drop_point);
        let splits_changed = {
            let splits = self.dwindle_splits.entry(split_key).or_default();
            let old_splits = splits.clone();
            *splits = preview_splits;
            splits.push(DwindleSplit {
                target: target_assignment.window,
                new_window: window,
                direction,
            });
            *splits != old_splits
        };

        let order_changed = self.reinsert_window_after_target(window, target_assignment.window);
        order_changed || splits_changed
    }

    fn drop_insert_index_at_point(
        &self,
        window: WindowHandle,
        drop_point: Point,
        target_handles: &[WindowHandle],
        monitor: &MonitorInfo,
    ) -> usize {
        (0..=target_handles.len())
            .filter_map(|index| {
                let mut handles = target_handles.to_vec();
                handles.insert(index, window);
                let layout = self
                    .config
                    .layout_for_monitor(monitor, self.workspaces.active_workspace());
                let layout_windows = self.layout_windows_for_handles(&handles);
                let assignment =
                    tile_layout_windows_with_config(monitor.work_area, &layout_windows, layout)
                        .into_iter()
                        .find(|assignment| assignment.window == window)?;
                let center = assignment.rect.center();
                let dx = i64::from(center.x - drop_point.x);
                let dy = i64::from(center.y - drop_point.y);
                Some((dx * dx + dy * dy, index))
            })
            .min_by_key(|(distance, index)| (*distance, *index))
            .map(|(_, index)| index)
            .unwrap_or(target_handles.len())
    }

    fn reinsert_window_at_local_index(
        &mut self,
        window: WindowHandle,
        target_handles: &[WindowHandle],
        local_index: usize,
    ) -> bool {
        let old_order = self.tile_order.clone();
        self.tile_order.retain(|handle| *handle != window);

        let insert_at = if let Some(before) = target_handles.get(local_index) {
            self.tile_order
                .iter()
                .position(|handle| handle == before)
                .unwrap_or(self.tile_order.len())
        } else if let Some(after) = target_handles.last() {
            self.tile_order
                .iter()
                .position(|handle| handle == after)
                .map(|index| index + 1)
                .unwrap_or(self.tile_order.len())
        } else {
            self.tile_order.len()
        };

        self.tile_order.insert(insert_at, window);
        self.tile_order != old_order
    }

    fn reinsert_window_after_target(&mut self, window: WindowHandle, target: WindowHandle) -> bool {
        let old_order = self.tile_order.clone();
        self.tile_order.retain(|handle| *handle != window);
        let insert_at = self
            .tile_order
            .iter()
            .position(|handle| *handle == target)
            .map(|index| index + 1)
            .unwrap_or(self.tile_order.len());
        self.tile_order.insert(insert_at, window);
        self.tile_order != old_order
    }

    fn switch_workspace(
        &mut self,
        workspace: WorkspaceId,
        monitors: &[MonitorInfo],
    ) -> WorkspaceCommandPlan {
        self.sync_workspace_state(monitors);
        let mut plan = self.workspaces.switch_to(workspace);
        plan.hide
            .retain(|window| self.should_hide_for_workspace(*window, monitors));

        if self
            .foreground
            .is_some_and(|window| !self.workspaces.is_window_on_active_workspace(window))
        {
            self.foreground = None;
        }

        WorkspaceCommandPlan {
            hide: plan.hide,
            show: plan.show,
        }
    }

    fn move_focused_to_workspace(
        &mut self,
        workspace: WorkspaceId,
        monitors: &[MonitorInfo],
    ) -> Vec<WindowHandle> {
        let Some(current) = self
            .foreground
            .filter(|handle| self.is_manageable_window(*handle))
        else {
            return Vec::new();
        };

        self.sync_workspace_state(monitors);
        if let Some(window) = self.windows.get(&current) {
            self.workspaces.update_window_rect(current, window.rect);
        }

        let was_active = self.workspaces.is_window_on_active_workspace(current);
        if !self.workspaces.move_window_to_workspace(current, workspace) {
            return Vec::new();
        }

        if was_active && workspace != self.workspaces.active_workspace() {
            self.set_window_participation(current, WindowParticipation::Tiled);
            self.foreground = None;
            if self.should_hide_for_workspace(current, monitors) {
                return vec![current];
            }
        }

        Vec::new()
    }

    fn tile_assignments(&mut self, monitors: &[MonitorInfo]) -> Vec<TileAssignment> {
        let cursor_position = winland_win32::cursor_position().ok();
        let active_workspace = self.workspaces.active_workspace();
        let mut assignments = Vec::new();
        let mut next_overflow_floating = BTreeSet::new();

        for monitor in monitors {
            let handles = self.tiled_handles_for_monitor(monitor, monitors);
            let layout = self.config.layout_for_monitor(monitor, active_workspace);
            let overflow_plan =
                self.resolve_overflow_for_monitor(monitor, &handles, layout, cursor_position);
            next_overflow_floating.extend(overflow_plan.overflow_windows.iter().copied());
            let layout_windows = self.layout_windows_for_handles(&overflow_plan.tiled_windows);
            let monitor_assignments = if layout.kind == LayoutKind::Dwindle {
                let splits = self
                    .dwindle_splits
                    .entry((active_workspace, monitor.id))
                    .or_default();
                tile_layout_windows_with_state(
                    monitor.work_area,
                    &layout_windows,
                    layout,
                    cursor_position,
                    Some(splits),
                )
            } else {
                self.dwindle_splits.remove(&(active_workspace, monitor.id));
                tile_layout_windows_with_config(monitor.work_area, &layout_windows, layout)
            };

            assignments.extend(monitor_assignments);
        }

        if self.overflow_floating != next_overflow_floating {
            debug!(
                old_count = self.overflow_floating.len(),
                new_count = next_overflow_floating.len(),
                "updated automatic overflow floating windows"
            );
        }
        self.overflow_floating = next_overflow_floating;

        assignments
    }

    fn resolve_overflow_for_monitor(
        &self,
        monitor: &MonitorInfo,
        handles: &[WindowHandle],
        layout: LayoutConfig,
        cursor_position: Option<winland_core::Point>,
    ) -> MonitorOverflowPlan {
        let mut tiled_windows = handles.to_vec();
        let mut overflow_windows = Vec::new();

        loop {
            let assignments =
                self.preview_tile_assignments(monitor, &tiled_windows, layout, cursor_position);
            if tile_assignments_fit_work_area(monitor.work_area, &assignments) {
                return MonitorOverflowPlan {
                    tiled_windows,
                    overflow_windows,
                };
            }

            let Some(overflow) = self.next_overflow_window(&tiled_windows) else {
                return MonitorOverflowPlan {
                    tiled_windows: Vec::new(),
                    overflow_windows,
                };
            };

            tiled_windows.retain(|window| *window != overflow);
            overflow_windows.push(overflow);
        }
    }

    fn preview_tile_assignments(
        &self,
        monitor: &MonitorInfo,
        handles: &[WindowHandle],
        layout: LayoutConfig,
        cursor_position: Option<winland_core::Point>,
    ) -> Vec<TileAssignment> {
        let layout_windows = self.layout_windows_for_handles(handles);
        if layout.kind == LayoutKind::Dwindle {
            let mut splits = self
                .dwindle_splits
                .get(&(self.workspaces.active_workspace(), monitor.id))
                .cloned()
                .unwrap_or_default();
            tile_layout_windows_with_state(
                monitor.work_area,
                &layout_windows,
                layout,
                cursor_position,
                Some(&mut splits),
            )
        } else {
            tile_layout_windows_with_config(monitor.work_area, &layout_windows, layout)
        }
    }

    fn next_overflow_window(&self, tiled_windows: &[WindowHandle]) -> Option<WindowHandle> {
        let focused = self
            .foreground
            .filter(|window| tiled_windows.contains(window));

        if self.config.overflow_focus_policy == OverflowFocusPolicy::FloatFocused
            && let Some(focused) = focused
        {
            return Some(focused);
        }

        tiled_windows
            .iter()
            .rev()
            .copied()
            .find(|window| Some(*window) != focused)
            .or_else(|| tiled_windows.last().copied())
    }

    fn apply_tile_assignments_with_feedback(
        &mut self,
        assignments: &[TileAssignment],
        monitors: &[MonitorInfo],
        operation: &'static str,
    ) {
        let mut assignments = assignments.to_vec();

        for pass in 0..TILE_FEEDBACK_PASSES {
            if assignments.is_empty() {
                return;
            }

            apply_tile_assignments_once(&assignments, operation);

            if !self.learn_constraints_from_actual_rects(&assignments, operation) {
                return;
            }

            if pass + 1 < TILE_FEEDBACK_PASSES {
                assignments = self.tile_assignments(monitors);
            }
        }
    }

    fn learn_constraints_from_actual_rects(
        &mut self,
        assignments: &[TileAssignment],
        operation: &'static str,
    ) -> bool {
        let mut changed = false;

        for assignment in assignments {
            let Ok(actual) = winland_win32::window_rect_for_handle(assignment.window) else {
                continue;
            };

            let extra_width = actual
                .width()
                .saturating_sub(assignment.rect.width())
                .max(0);
            let extra_height = actual
                .height()
                .saturating_sub(assignment.rect.height())
                .max(0);
            if extra_width <= TILE_FEEDBACK_TOLERANCE_PX
                && extra_height <= TILE_FEEDBACK_TOLERANCE_PX
            {
                continue;
            }

            let current = self
                .learned_size_constraints
                .get(&assignment.window)
                .copied()
                .unwrap_or_default();
            let learned = WindowSizeConstraints::minimum(
                current.min.width.max(actual.width()),
                current.min.height.max(actual.height()),
            );

            if learned != current {
                self.learned_size_constraints
                    .insert(assignment.window, learned);
                changed = true;
                debug!(
                    window = %assignment.window,
                    requested = %assignment.rect,
                    actual = %actual,
                    operation,
                    "learned window minimum size from accepted geometry"
                );
            }
        }

        changed
    }

    fn forget_learned_size_constraint(&mut self, window: WindowHandle) {
        if self.learned_size_constraints.remove(&window).is_some() {
            debug!(
                window = %window,
                "cleared learned window minimum size after window state changed"
            );
        }
    }

    fn layout_windows_for_handles(&self, handles: &[WindowHandle]) -> Vec<WindowLayoutInfo> {
        handles
            .iter()
            .filter_map(|handle| {
                self.windows.get(handle).map(|window| WindowLayoutInfo {
                    handle: *handle,
                    size_constraints: merge_size_constraints(
                        window.size_constraints,
                        self.learned_size_constraints
                            .get(handle)
                            .copied()
                            .unwrap_or_default(),
                    ),
                })
            })
            .collect()
    }

    fn tiled_handles_for_monitor(
        &self,
        monitor: &MonitorInfo,
        monitors: &[MonitorInfo],
    ) -> Vec<WindowHandle> {
        self.tile_order
            .iter()
            .copied()
            .filter(|handle| self.window_participation(*handle).is_tiled())
            .filter(|handle| {
                self.windows.get(handle).is_some_and(|window| {
                    self.is_tilable_window(*handle)
                        && self.monitor_owns_window_rect(
                            *handle,
                            monitor,
                            self.window_layout_rect(*handle, window),
                            monitors,
                        )
                })
            })
            .collect()
    }

    fn monitor_owns_window_rect(
        &self,
        window: WindowHandle,
        monitor: &MonitorInfo,
        rect: Rect,
        monitors: &[MonitorInfo],
    ) -> bool {
        if let Some(override_monitor) = self.window_monitor_overrides.get(&window).copied()
            && monitors
                .iter()
                .any(|candidate| candidate.id == override_monitor)
        {
            return override_monitor == monitor.id;
        }

        monitor_owns_rect(monitor, rect, monitors)
    }

    fn sync_workspace_state(&mut self, monitors: &[MonitorInfo]) {
        let existing: BTreeSet<_> = self.windows.keys().copied().collect();
        self.workspaces.retain_windows(&existing);

        let windows: Vec<_> = self
            .windows
            .iter()
            .map(|(handle, window)| (*handle, window.clone()))
            .collect();

        for (handle, window) in windows {
            let decision = self.rule_decision(&window);
            if self.workspaces.window_state(handle).is_some() {
                if self.is_workspace_manageable_by_rules(&window, &decision) {
                    self.workspaces.update_window_rect(handle, window.rect);
                }
            } else if self.is_manageable_by_rules(&window, &decision)
                && !is_fullscreen_window(&window, monitors)
            {
                if let Some(workspace) = decision.target_workspace {
                    self.workspaces
                        .track_window_on_workspace(handle, workspace, window.rect);
                } else {
                    self.workspaces.track_window(handle, window.rect);
                }

                if decision.float == Some(true) {
                    self.set_window_participation(handle, WindowParticipation::Floating);
                }
                if decision.always_on_workspace == Some(true) {
                    self.workspaces.set_visible_on_all_workspaces(handle, true);
                }
            }
        }
    }

    fn should_hide_for_workspace(&self, window: WindowHandle, monitors: &[MonitorInfo]) -> bool {
        self.windows.get(&window).is_some_and(|info| {
            self.is_workspace_manageable_by_rules(info, &self.rule_decision(info))
                && !is_fullscreen_window(info, monitors)
        })
    }

    fn is_tilable_window(&self, handle: WindowHandle) -> bool {
        self.workspaces.is_window_on_active_workspace(handle)
            && self.windows.get(&handle).is_some_and(|window| {
                self.is_workspace_manageable_by_rules(window, &self.rule_decision(window))
            })
    }

    fn window_layout_rect(&self, handle: WindowHandle, window: &WindowInfo) -> winland_core::Rect {
        self.workspaces
            .window_state(handle)
            .and_then(|state| state.last_rect)
            .unwrap_or(window.rect)
    }

    fn preserve_keyboard_state(&self, refreshed: &mut Self, monitors: &[MonitorInfo]) {
        refreshed.workspaces = self.workspaces.clone();
        refreshed.sync_workspace_state(monitors);

        let known_workspace_windows: BTreeSet<_> = refreshed
            .windows
            .keys()
            .copied()
            .filter(|handle| refreshed.workspaces.window_state(*handle).is_some())
            .collect();
        refreshed.tile_order = self
            .tile_order
            .iter()
            .copied()
            .filter(|handle| known_workspace_windows.contains(handle))
            .collect();
        for handle in &known_workspace_windows {
            if !refreshed.tile_order.contains(handle) {
                refreshed.tile_order.push(*handle);
            }
        }
        refreshed.participation = self
            .participation
            .iter()
            .filter(|(handle, _)| known_workspace_windows.contains(handle))
            .map(|(handle, participation)| (*handle, *participation))
            .collect();
        refreshed.dwindle_splits = self.dwindle_splits.clone();
        refreshed.previous_rects = self
            .previous_rects
            .iter()
            .filter(|(handle, _)| known_workspace_windows.contains(handle))
            .map(|(handle, rect)| (*handle, *rect))
            .collect();
        refreshed.learned_size_constraints = self
            .learned_size_constraints
            .iter()
            .filter(|(handle, _)| known_workspace_windows.contains(handle))
            .map(|(handle, constraints)| (*handle, *constraints))
            .collect();
        refreshed.window_monitor_overrides = self
            .window_monitor_overrides
            .iter()
            .filter(|(handle, monitor)| {
                known_workspace_windows.contains(handle)
                    && monitors.iter().any(|candidate| candidate.id == **monitor)
            })
            .map(|(handle, monitor)| (*handle, *monitor))
            .collect();
        refreshed.overflow_floating = self
            .overflow_floating
            .iter()
            .copied()
            .filter(|handle| known_workspace_windows.contains(handle))
            .collect();
        refreshed.active_modifier_drag = self
            .active_modifier_drag
            .filter(|drag| refreshed.windows.contains_key(&drag.window));
        refreshed.suppressed_modifier_drag_events = self
            .suppressed_modifier_drag_events
            .iter()
            .copied()
            .filter(|handle| refreshed.windows.contains_key(handle))
            .collect();
    }

    fn diff(&self, refreshed: &Self, monitors: &[MonitorInfo]) -> SnapshotDiff {
        let old_handles: BTreeSet<_> = self.windows.keys().copied().collect();
        let new_handles: BTreeSet<_> = refreshed.windows.keys().copied().collect();

        let added = new_handles.difference(&old_handles).copied().collect();
        let removed = old_handles.difference(&new_handles).copied().collect();
        let changed = new_handles
            .intersection(&old_handles)
            .filter(|handle| self.windows.get(handle) != refreshed.windows.get(handle))
            .count();
        let moved_between_monitors = new_handles
            .intersection(&old_handles)
            .filter(|handle| {
                let old_monitor = self
                    .windows
                    .get(handle)
                    .and_then(|window| monitor_for_rect(window.rect, monitors));
                let new_monitor = refreshed
                    .windows
                    .get(handle)
                    .and_then(|window| monitor_for_rect(window.rect, monitors));

                old_monitor.is_some() && new_monitor.is_some() && old_monitor != new_monitor
            })
            .count();

        SnapshotDiff {
            added,
            removed,
            changed,
            moved_between_monitors,
            foreground_changed: self.foreground != refreshed.foreground,
        }
    }

    fn manageable_window_count(&self) -> usize {
        self.windows
            .values()
            .filter(|window| self.is_manageable_by_rules(window, &self.rule_decision(window)))
            .count()
    }

    fn floating_window_count(&self) -> usize {
        self.participation
            .values()
            .filter(|participation| **participation == WindowParticipation::Floating)
            .count()
    }

    fn temporary_floating_window_count(&self) -> usize {
        self.participation
            .values()
            .filter(|participation| **participation == WindowParticipation::TemporarilyFloating)
            .count()
    }

    fn window_participation(&self, window: WindowHandle) -> WindowParticipation {
        self.participation.get(&window).copied().unwrap_or_default()
    }

    fn set_window_participation(
        &mut self,
        window: WindowHandle,
        participation: WindowParticipation,
    ) {
        match participation {
            WindowParticipation::Tiled => {
                self.participation.remove(&window);
            }
            WindowParticipation::Floating | WindowParticipation::TemporarilyFloating => {
                self.participation.insert(window, participation);
            }
        }
    }

    fn remember_previous_rect(&mut self, window: WindowHandle) {
        if let Some(info) = self.windows.get(&window) {
            self.previous_rects.insert(window, info.rect);
        }
    }

    fn manageable_handles_sorted(&self) -> Vec<WindowHandle> {
        self.windows
            .iter()
            .filter(|(_, window)| self.is_manageable_by_rules(window, &self.rule_decision(window)))
            .map(|(handle, _)| *handle)
            .collect()
    }

    fn focusable_handles(&self) -> Vec<WindowHandle> {
        self.tile_order
            .iter()
            .copied()
            .filter(|handle| self.is_manageable_window(*handle))
            .collect()
    }

    fn is_manageable_window(&self, handle: WindowHandle) -> bool {
        self.workspaces.is_window_on_active_workspace(handle)
            && self.windows.get(&handle).is_some_and(|window| {
                self.is_manageable_by_rules(window, &self.rule_decision(window))
            })
    }

    fn rule_decision(&self, window: &WindowInfo) -> WindowRuleDecision {
        evaluate_window_rules(window, &self.config.window_rules)
    }

    fn is_manageable_by_rules(&self, window: &WindowInfo, decision: &WindowRuleDecision) -> bool {
        window.is_manageable() && decision.manage != Some(false)
    }

    fn is_workspace_manageable_by_rules(
        &self,
        window: &WindowInfo,
        decision: &WindowRuleDecision,
    ) -> bool {
        window.is_workspace_manageable() && decision.manage != Some(false)
    }

    fn sync_borders(&mut self, operation: &'static str) -> Result<()> {
        let monitors =
            winland_win32::enumerate_monitors().context("enumerate monitors for border sync")?;
        self.sync_borders_with_monitors(&monitors, operation)
    }

    fn sync_borders_with_monitors(
        &mut self,
        monitors: &[MonitorInfo],
        operation: &'static str,
    ) -> Result<()> {
        let Some(manager) = &self.border_manager else {
            return Ok(());
        };

        let candidates = self.border_candidates(monitors);
        if candidates.is_empty() {
            manager.clear().context("clear border overlays")?;
            debug!(operation, "cleared border overlays");
            return Ok(());
        }

        let updates: Vec<_> = candidates
            .into_iter()
            .map(|candidate| {
                let rect = winland_win32::window_rect_for_handle(candidate.window)
                    .unwrap_or(candidate.rect);
                BorderUpdate {
                    window: candidate.window,
                    rect,
                    color: candidate.color,
                }
            })
            .collect();
        manager
            .sync(updates, self.config.borders.width)
            .context("sync border overlays")?;
        debug!(operation, "synced border overlays");
        Ok(())
    }

    fn border_candidates(&self, monitors: &[MonitorInfo]) -> Vec<BorderCandidate> {
        let config = self.config.borders;
        if !config.enabled {
            return Vec::new();
        }

        if config.disable_when_fullscreen
            && self.foreground.is_some_and(|window| {
                self.windows.get(&window).is_some_and(|info| {
                    is_fullscreen_window(info, monitors)
                        || !self.is_manageable_by_rules(info, &self.rule_decision(info))
                })
            })
        {
            return Vec::new();
        }

        self.windows
            .iter()
            .filter(|(handle, window)| {
                self.is_manageable_window(**handle) && !is_fullscreen_window(window, monitors)
            })
            .filter_map(|(handle, window)| {
                let participation = self.window_participation(*handle);
                let focused = self.foreground == Some(*handle);
                let floating =
                    participation.is_floating() || self.overflow_floating.contains(handle);

                let color = if focused {
                    config.active_color
                } else if floating {
                    config.floating_color
                } else if config.show_inactive {
                    config.inactive_color
                } else {
                    return None;
                };

                Some(BorderCandidate {
                    window: *handle,
                    rect: window.rect,
                    color,
                })
            })
            .collect()
    }

    fn log_snapshot(&self, message: &'static str) {
        info!(
            total_windows = self.windows.len(),
            manageable_windows = self.manageable_window_count(),
            floating_windows = self.floating_window_count(),
            temporary_floating_windows = self.temporary_floating_window_count(),
            active_workspace = %self.workspaces.active_workspace(),
            foreground = ?self.foreground,
            message
        );
    }
}

#[derive(Debug, Default)]
struct SnapshotDiff {
    added: Vec<WindowHandle>,
    removed: Vec<WindowHandle>,
    changed: usize,
    moved_between_monitors: usize,
    foreground_changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DaemonCommand {
    Focus(FocusDirection),
    Swap(FocusDirection),
    Retile,
    ToggleFloat,
    SwitchWorkspace(WorkspaceId),
    MoveFocusedToWorkspace(WorkspaceId),
    Reload,
    Quit,
    Launch(String),
}

impl DaemonCommand {
    fn needs_layout(&self) -> bool {
        matches!(
            self,
            Self::Swap(_)
                | Self::Retile
                | Self::ToggleFloat
                | Self::SwitchWorkspace(_)
                | Self::MoveFocusedToWorkspace(_)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusDirection {
    Left,
    Down,
    Up,
    Right,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CommandPlan {
    focus: Option<WindowHandle>,
    hide: Vec<WindowHandle>,
    show: Vec<WorkspaceVisibilityChange>,
    moves: Vec<TileAssignment>,
    reload: bool,
    quit: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct WorkspaceCommandPlan {
    hide: Vec<WindowHandle>,
    show: Vec<WorkspaceVisibilityChange>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct MonitorOverflowPlan {
    tiled_windows: Vec<WindowHandle>,
    overflow_windows: Vec<WindowHandle>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BorderCandidate {
    window: WindowHandle,
    rect: Rect,
    color: BorderColor,
}

fn count_events(batch: &[WindowEvent], kind: WindowEventKind) -> usize {
    batch.iter().filter(|event| event.kind == kind).count()
}

fn offset_rect_by_cursor_delta(rect: Rect, start: Point, current: Point) -> Rect {
    let dx = current.x.saturating_sub(start.x);
    let dy = current.y.saturating_sub(start.y);

    Rect {
        left: rect.left.saturating_add(dx),
        top: rect.top.saturating_add(dy),
        right: rect.right.saturating_add(dx),
        bottom: rect.bottom.saturating_add(dy),
    }
}

#[derive(Debug, Clone, Default)]
struct HotkeyCommandMap {
    commands: BTreeMap<HotkeyId, DaemonCommand>,
}

impl HotkeyCommandMap {
    fn command(&self, id: HotkeyId) -> Option<DaemonCommand> {
        self.commands.get(&id).cloned()
    }
}

enum HotkeyBackend {
    Registered {
        _registration: winland_win32::HotkeyRegistration,
    },
    Override {
        _registration: winland_win32::HotkeyOverrideRegistration,
    },
}

impl HotkeyBackend {
    fn name(&self) -> &'static str {
        match self {
            Self::Registered { .. } => "register-hotkey",
            Self::Override { .. } => "advanced-interception",
        }
    }
}

fn install_hotkey_backend(
    config: &Config,
    hotkey_bindings: Vec<HotkeyBinding>,
    hotkey_sender: Sender<HotkeyEvent>,
) -> Result<HotkeyBackend> {
    match config.hotkeys.mode {
        HotkeyMode::Normal => {
            let registration =
                winland_win32::register_hotkeys(hotkey_bindings.clone(), hotkey_sender)
                    .context("register documented Win32 daemon hotkeys")?;
            log_hotkey_registration(&hotkey_bindings, &registration);
            Ok(HotkeyBackend::Registered {
                _registration: registration,
            })
        }
        HotkeyMode::AdvancedInterception => {
            let options = hotkey_override_options_from_config(config)?;
            log_hotkey_override_bindings(&hotkey_bindings, &options);
            let registration =
                winland_win32::install_hotkey_override(hotkey_bindings, options, hotkey_sender)
                    .context("install documented low-level keyboard hook for hotkey override")?;
            Ok(HotkeyBackend::Override {
                _registration: registration,
            })
        }
    }
}

fn install_modifier_drag(
    config: &Config,
    mouse_drag_sender: Sender<MouseDragEvent>,
) -> Result<Option<winland_win32::ModifierDragRegistration>> {
    if !config.hotkeys.modifier_drag.enabled {
        info!("modifier drag is disabled by config");
        return Ok(None);
    }

    let modifiers = modifier_drag_modifiers_from_config(config)?;
    let options = ModifierDragOptions {
        modifiers,
        bypass: HotkeyBypassRules {
            fullscreen: config.hotkeys.bypass.fullscreen,
            class_names: text_matchers_from_config(&config.hotkeys.bypass.class)?,
            executable_paths: text_matchers_from_config(&config.hotkeys.bypass.executable_path)?,
            process_names: text_matchers_from_config(&config.hotkeys.bypass.process_name)?,
        },
    };
    let registration = winland_win32::install_modifier_drag(options, mouse_drag_sender)
        .context("install documented low-level mouse hook for modifier drag")?;

    info!(
        modifiers = %config.hotkeys.modifier_drag.modifiers,
        "installed modifier drag hook"
    );
    Ok(Some(registration))
}

fn hotkey_bindings_from_config(config: &Config) -> Result<(Vec<HotkeyBinding>, HotkeyCommandMap)> {
    let mut bindings = Vec::with_capacity(config.hotkeys.bindings.len());
    let mut commands = BTreeMap::new();

    for (index, binding_config) in config.hotkeys.bindings.iter().enumerate() {
        let id = HotkeyId((index + 1) as i32);
        let (command, description) = daemon_command_from_binding(binding_config)
            .with_context(|| format!("map hotkey binding '{}'", binding_config.keys))?;
        let chord = binding_config
            .chord()
            .with_context(|| format!("parse hotkey '{}'", binding_config.keys))?;
        bindings.push(
            HotkeyBinding::new(
                id,
                hotkey_modifiers_from_config(&chord),
                virtual_key_from_config(&chord.key),
                description,
            )
            .with_suppression(
                config.hotkeys.mode == HotkeyMode::AdvancedInterception
                    && binding_config.override_app,
            ),
        );
        commands.insert(id, command);
    }

    Ok((bindings, HotkeyCommandMap { commands }))
}

fn daemon_command_from_binding(binding: &HotkeyBindingConfig) -> Result<(DaemonCommand, String)> {
    if let Some(command) = binding.command.as_deref() {
        let command = command.trim();
        let daemon_command = daemon_command_from_name(command)
            .with_context(|| format!("map hotkey command '{command}'"))?;
        return Ok((daemon_command, command.to_owned()));
    }

    if let Some(command_line) = binding.launch.as_deref() {
        let command_line = command_line.trim();
        if command_line.is_empty() {
            return Err(anyhow!("launch command line must not be empty"));
        }

        return Ok((
            DaemonCommand::Launch(command_line.to_owned()),
            format!("launch {command_line}"),
        ));
    }

    Err(anyhow!("hotkey binding must set command or launch"))
}

fn hotkey_override_options_from_config(config: &Config) -> Result<HotkeyOverrideOptions> {
    let panic_chord = winland_config::parse_hotkey_chord(&config.hotkeys.panic_hotkey)
        .with_context(|| format!("parse panic hotkey '{}'", config.hotkeys.panic_hotkey))?;

    Ok(HotkeyOverrideOptions {
        panic_hotkey: HotkeyLowLevelEvent {
            modifiers: hotkey_modifiers_from_config(&panic_chord),
            virtual_key: virtual_key_from_config(&panic_chord.key),
        },
        bypass: HotkeyBypassRules {
            fullscreen: config.hotkeys.bypass.fullscreen,
            class_names: text_matchers_from_config(&config.hotkeys.bypass.class)?,
            executable_paths: text_matchers_from_config(&config.hotkeys.bypass.executable_path)?,
            process_names: text_matchers_from_config(&config.hotkeys.bypass.process_name)?,
        },
        latency_budget: Duration::from_micros(config.hotkeys.override_latency_budget_micros),
    })
}

fn text_matchers_from_config(
    matchers: &[TextMatcherConfig],
) -> Result<Vec<winland_core::TextMatcher>> {
    matchers
        .iter()
        .map(TextMatcherConfig::to_core)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("convert hotkey bypass matcher")
}

fn daemon_command_from_name(command: &str) -> Option<DaemonCommand> {
    if let Some(workspace) = command
        .strip_prefix("switch-workspace-")
        .and_then(|value| value.parse::<u16>().ok())
    {
        return Some(DaemonCommand::SwitchWorkspace(WorkspaceId(workspace)));
    }

    if let Some(workspace) = command
        .strip_prefix("move-to-workspace-")
        .and_then(|value| value.parse::<u16>().ok())
    {
        return Some(DaemonCommand::MoveFocusedToWorkspace(WorkspaceId(
            workspace,
        )));
    }

    match command {
        "focus-left" => Some(DaemonCommand::Focus(FocusDirection::Left)),
        "focus-down" => Some(DaemonCommand::Focus(FocusDirection::Down)),
        "focus-up" => Some(DaemonCommand::Focus(FocusDirection::Up)),
        "focus-right" => Some(DaemonCommand::Focus(FocusDirection::Right)),
        "swap-left" => Some(DaemonCommand::Swap(FocusDirection::Left)),
        "swap-down" => Some(DaemonCommand::Swap(FocusDirection::Down)),
        "swap-up" => Some(DaemonCommand::Swap(FocusDirection::Up)),
        "swap-right" => Some(DaemonCommand::Swap(FocusDirection::Right)),
        "retile" => Some(DaemonCommand::Retile),
        "toggle-float" => Some(DaemonCommand::ToggleFloat),
        "reload" => Some(DaemonCommand::Reload),
        "quit" => Some(DaemonCommand::Quit),
        _ => None,
    }
}

fn hotkey_modifiers_from_config(chord: &winland_config::HotkeyChord) -> HotkeyModifierSet {
    hotkey_modifier_set_from_modifiers(&chord.modifiers)
}

fn modifier_drag_modifiers_from_config(config: &Config) -> Result<HotkeyModifierSet> {
    let modifiers = winland_config::parse_hotkey_modifiers(&config.hotkeys.modifier_drag.modifiers)
        .with_context(|| {
            format!(
                "parse modifier drag modifiers '{}'",
                config.hotkeys.modifier_drag.modifiers
            )
        })?;
    Ok(hotkey_modifier_set_from_modifiers(&modifiers))
}

fn hotkey_modifier_set_from_modifiers(
    modifiers: &BTreeSet<winland_config::HotkeyModifier>,
) -> HotkeyModifierSet {
    let mut set = HotkeyModifierSet::new();
    for modifier in modifiers {
        set = match modifier {
            HotkeyModifier::Alt => set.alt(),
            HotkeyModifier::Control => set.control(),
            HotkeyModifier::Shift => set.shift(),
            HotkeyModifier::Super => set.super_key(),
        };
    }
    set
}

fn virtual_key_from_config(key: &HotkeyKey) -> VirtualKey {
    match key {
        HotkeyKey::Character(ch) => VirtualKey::ascii_uppercase(*ch as u8),
        HotkeyKey::ArrowLeft => VirtualKey::ARROW_LEFT,
        HotkeyKey::ArrowDown => VirtualKey::ARROW_DOWN,
        HotkeyKey::ArrowUp => VirtualKey::ARROW_UP,
        HotkeyKey::ArrowRight => VirtualKey::ARROW_RIGHT,
        HotkeyKey::Escape => VirtualKey::ESCAPE,
        HotkeyKey::Space => VirtualKey::SPACE,
    }
}

fn log_hotkey_registration(
    bindings: &[HotkeyBinding],
    registration: &winland_win32::HotkeyRegistration,
) {
    let failed_ids: BTreeSet<_> = registration
        .failures()
        .iter()
        .map(|failure| failure.id)
        .collect();

    for binding in bindings {
        if failed_ids.contains(&binding.id) {
            continue;
        }

        info!(
            hotkey = %hotkey_label(binding),
            description = %binding.description,
            "registered daemon hotkey"
        );
    }

    for failure in registration.failures() {
        warn!(
            id = failure.id.0,
            description = %failure.description,
            error = %failure.error,
            "daemon hotkey was not registered; another application may already own it"
        );
    }
}

fn log_hotkey_override_bindings(bindings: &[HotkeyBinding], options: &HotkeyOverrideOptions) {
    info!(
        binding_count = bindings.len(),
        suppressing_bindings = bindings
            .iter()
            .filter(|binding| binding.suppress_app)
            .count(),
        latency_budget_micros = options.latency_budget.as_micros(),
        fullscreen_bypass = options.bypass.fullscreen,
        class_bypass_rules = options.bypass.class_names.len(),
        executable_path_bypass_rules = options.bypass.executable_paths.len(),
        process_name_bypass_rules = options.bypass.process_names.len(),
        "installing opt-in hotkey override mode"
    );

    for binding in bindings {
        info!(
            hotkey = %hotkey_label(binding),
            description = %binding.description,
            suppress_app = binding.suppress_app,
            "configured intercepted daemon hotkey"
        );
    }

    info!(
        hotkey = %hotkey_low_level_label(&options.panic_hotkey),
        "panic hotkey will always pass through without interception"
    );
}

fn hotkey_label(binding: &HotkeyBinding) -> String {
    let mut parts = Vec::new();
    if binding.modifiers.super_key {
        parts.push("Win".to_owned());
    }
    if binding.modifiers.control {
        parts.push("Ctrl".to_owned());
    }
    if binding.modifiers.alt {
        parts.push("Alt".to_owned());
    }
    if binding.modifiers.shift {
        parts.push("Shift".to_owned());
    }

    parts.push(virtual_key_label(binding.virtual_key));
    parts.join("+")
}

fn hotkey_low_level_label(event: &HotkeyLowLevelEvent) -> String {
    let binding = HotkeyBinding::new(
        HotkeyId(0),
        event.modifiers,
        event.virtual_key,
        "panic hotkey",
    );
    hotkey_label(&binding)
}

fn virtual_key_label(key: VirtualKey) -> String {
    if key == VirtualKey::ARROW_LEFT {
        "Left".to_owned()
    } else if key == VirtualKey::ARROW_DOWN {
        "Down".to_owned()
    } else if key == VirtualKey::ARROW_UP {
        "Up".to_owned()
    } else if key == VirtualKey::ARROW_RIGHT {
        "Right".to_owned()
    } else if key == VirtualKey::ESCAPE {
        "Escape".to_owned()
    } else if key.0 == 0x20 {
        "Space".to_owned()
    } else if (b'0' as u32..=b'9' as u32).contains(&key.0)
        || (b'A' as u32..=b'Z' as u32).contains(&key.0)
    {
        char::from_u32(key.0).unwrap_or('?').to_string()
    } else {
        format!("VK_{:X}", key.0)
    }
}

fn merge_size_constraints(
    base: WindowSizeConstraints,
    learned: WindowSizeConstraints,
) -> WindowSizeConstraints {
    let base = base.normalized();
    let learned = learned.normalized();
    let min_width = base.min.width.max(learned.min.width);
    let min_height = base.min.height.max(learned.min.height);

    WindowSizeConstraints {
        min: winland_core::Size::new(min_width, min_height),
        max: base.max.map(|max| {
            winland_core::Size::new(max.width.max(min_width), max.height.max(min_height))
        }),
    }
    .normalized()
}

fn apply_tile_assignments_once(assignments: &[TileAssignment], operation: &'static str) {
    for assignment in assignments {
        if let Err(error) = winland_win32::move_resize_window(assignment.window, assignment.rect) {
            warn!(
                window = %assignment.window,
                rect = %assignment.rect,
                %error,
                operation,
                "failed to move window"
            );
        }
    }
}

fn is_fullscreen_window(window: &WindowInfo, monitors: &[MonitorInfo]) -> bool {
    monitors.iter().any(|monitor| window.rect == monitor.rect)
}

fn monitor_owns_rect(monitor: &MonitorInfo, rect: Rect, monitors: &[MonitorInfo]) -> bool {
    monitor_for_rect(rect, monitors) == Some(monitor.id)
}

fn monitor_for_point(
    point: winland_core::Point,
    monitors: &[MonitorInfo],
) -> Option<winland_core::MonitorId> {
    monitors
        .iter()
        .find(|monitor| monitor.rect.contains(point))
        .map(|monitor| monitor.id)
        .or_else(|| {
            monitors
                .iter()
                .min_by_key(|monitor| (squared_distance_to_rect(point, monitor.rect), monitor.id))
                .map(|monitor| monitor.id)
        })
}

fn monitor_for_rect(rect: Rect, monitors: &[MonitorInfo]) -> Option<winland_core::MonitorId> {
    monitors
        .iter()
        .filter_map(|monitor| {
            let overlap = rect_overlap_area(rect, monitor.rect);
            (overlap > 0).then_some((overlap, monitor.id))
        })
        .max_by_key(|(overlap, id)| (*overlap, std::cmp::Reverse(*id)))
        .map(|(_, id)| id)
        .or_else(|| {
            let center = rect.center();
            monitors
                .iter()
                .min_by_key(|monitor| (squared_distance_to_rect(center, monitor.rect), monitor.id))
                .map(|monitor| monitor.id)
        })
}

fn rect_overlap_area(a: Rect, b: Rect) -> i64 {
    let left = a.left.max(b.left);
    let top = a.top.max(b.top);
    let right = a.right.min(b.right);
    let bottom = a.bottom.min(b.bottom);
    let width = i64::from(right.saturating_sub(left).max(0));
    let height = i64::from(bottom.saturating_sub(top).max(0));
    width * height
}

fn squared_distance_to_rect(point: winland_core::Point, rect: Rect) -> i64 {
    let dx = if point.x < rect.left {
        rect.left.saturating_sub(point.x)
    } else if point.x >= rect.right {
        point.x.saturating_sub(rect.right.saturating_sub(1))
    } else {
        0
    };
    let dy = if point.y < rect.top {
        rect.top.saturating_sub(point.y)
    } else if point.y >= rect.bottom {
        point.y.saturating_sub(rect.bottom.saturating_sub(1))
    } else {
        0
    };

    let dx = i64::from(dx);
    let dy = i64::from(dy);
    dx * dx + dy * dy
}

fn nearest_assignment(
    point: winland_core::Point,
    assignments: &[TileAssignment],
) -> Option<&TileAssignment> {
    assignments.iter().min_by_key(|assignment| {
        let center = assignment.rect.center();
        let dx = i64::from(center.x - point.x);
        let dy = i64::from(center.y - point.y);
        dx * dx + dy * dy
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use winland_core::{MonitorId, Rect, WindowStyles};

    #[test]
    fn config_hotkey_commands_route_without_real_hotkeys() {
        assert_eq!(
            daemon_command_from_name("focus-right"),
            Some(DaemonCommand::Focus(FocusDirection::Right))
        );
        assert_eq!(
            daemon_command_from_name("retile"),
            Some(DaemonCommand::Retile)
        );
        assert_eq!(
            daemon_command_from_name("toggle-float"),
            Some(DaemonCommand::ToggleFloat)
        );
        assert_eq!(
            daemon_command_from_name("switch-workspace-2"),
            Some(DaemonCommand::SwitchWorkspace(WorkspaceId(2)))
        );
        assert_eq!(
            daemon_command_from_name("move-to-workspace-9"),
            Some(DaemonCommand::MoveFocusedToWorkspace(WorkspaceId(9)))
        );
        assert_eq!(daemon_command_from_name("quit"), Some(DaemonCommand::Quit));
        assert_eq!(daemon_command_from_name("unknown"), None);

        let launch = HotkeyBindingConfig {
            keys: "Win+T".to_owned(),
            command: None,
            launch: Some("wt.exe".to_owned()),
            override_app: false,
        };
        assert_eq!(
            daemon_command_from_binding(&launch).unwrap(),
            (
                DaemonCommand::Launch("wt.exe".to_owned()),
                "launch wt.exe".to_owned()
            )
        );
    }

    #[test]
    fn hotkey_label_is_human_readable() {
        let binding = HotkeyBinding::new(
            HotkeyId(1),
            HotkeyModifierSet::new().control().alt(),
            VirtualKey::SPACE,
            "toggle float",
        );

        assert_eq!(hotkey_label(&binding), "Ctrl+Alt+Space");
    }

    #[test]
    fn focus_command_selects_directional_neighbor() {
        let mut state = daemon_state([
            window(1, "Left", Rect::from_size(0, 0, 100, 100)),
            window(2, "Current", Rect::from_size(200, 0, 100, 100)),
            window(3, "Right", Rect::from_size(400, 0, 100, 100)),
        ]);
        state.foreground = Some(WindowHandle(2));

        let plan = state.plan_command(
            DaemonCommand::Focus(FocusDirection::Right),
            &[primary_test_monitor()],
        );

        assert_eq!(plan.focus, Some(WindowHandle(3)));
        assert_eq!(state.foreground, Some(WindowHandle(3)));
    }

    #[test]
    fn swap_command_reorders_tiling_without_real_hotkeys() {
        let mut state = daemon_state([
            window(1, "One", Rect::from_size(0, 0, 100, 100)),
            window(2, "Two", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.foreground = Some(WindowHandle(1));

        let plan = state.plan_command(
            DaemonCommand::Swap(FocusDirection::Right),
            &[primary_test_monitor()],
        );

        assert_eq!(state.tile_order, vec![WindowHandle(2), WindowHandle(1)]);
        assert_eq!(plan.moves[0].window, WindowHandle(2));
        assert_eq!(plan.moves[1].window, WindowHandle(1));
    }

    #[test]
    fn float_toggle_excludes_focused_window_from_retile() {
        let mut state = daemon_state([
            window(1, "One", Rect::from_size(0, 0, 100, 100)),
            window(2, "Two", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.foreground = Some(WindowHandle(1));

        let plan = state.plan_command(DaemonCommand::ToggleFloat, &[primary_test_monitor()]);

        assert_eq!(
            state.window_participation(WindowHandle(1)),
            WindowParticipation::Floating
        );
        assert_eq!(
            plan.moves,
            vec![TileAssignment {
                window: WindowHandle(2),
                rect: primary_test_monitor().work_area,
            }]
        );
    }

    #[test]
    fn retile_tiles_windows_on_each_monitor_independently() {
        let monitors = [
            primary_test_monitor(),
            MonitorInfo {
                id: MonitorId(2),
                is_primary: false,
                rect: Rect::from_size(1000, 0, 800, 600),
                work_area: Rect::from_size(1000, 0, 800, 560),
            },
        ];
        let mut state = daemon_state([
            window(1, "Primary One", Rect::from_size(0, 0, 100, 100)),
            window(2, "Primary Two", Rect::from_size(200, 0, 100, 100)),
            window(3, "Secondary", Rect::from_size(1100, 0, 100, 100)),
        ]);
        state.sync_workspace_state(&monitors);

        let plan = state.plan_command(DaemonCommand::Retile, &monitors);

        assert_eq!(
            plan.moves,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 500, 760),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(500, 0, 500, 760),
                },
                TileAssignment {
                    window: WindowHandle(3),
                    rect: Rect::from_size(1000, 0, 800, 560),
                },
            ]
        );
    }

    #[test]
    fn retile_uses_window_size_constraints_from_snapshot() {
        let monitor = primary_test_monitor();
        let mut fixed = window(1, "Fixed", Rect::from_size(0, 0, 300, 760));
        fixed.size_constraints = winland_core::WindowSizeConstraints::fixed(300, 760);
        let mut state = daemon_state([
            fixed,
            window(2, "Flexible", Rect::from_size(200, 0, 100, 100)),
        ]);

        let plan = state.plan_command(DaemonCommand::Retile, &[monitor]);

        assert_eq!(
            plan.moves,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 300, 760),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(300, 0, 700, 760),
                },
            ]
        );
    }

    #[test]
    fn overflow_floats_other_windows_to_keep_focused_window_tiled_by_default() {
        let monitor = primary_test_monitor();
        let work_area = monitor.work_area;
        let mut focused = window(1, "Focused", Rect::from_size(0, 0, 100, 100));
        focused.size_constraints = winland_core::WindowSizeConstraints::minimum(700, 0);
        let mut other = window(2, "Other", Rect::from_size(200, 0, 100, 100));
        other.size_constraints = winland_core::WindowSizeConstraints::minimum(700, 0);
        let mut state = daemon_state([focused, other]);
        state.foreground = Some(WindowHandle(1));

        let plan = state.plan_command(DaemonCommand::Retile, &[monitor]);

        assert_eq!(
            plan.moves,
            vec![TileAssignment {
                window: WindowHandle(1),
                rect: work_area,
            }]
        );
        assert_eq!(state.overflow_floating, BTreeSet::from([WindowHandle(2)]));
    }

    #[test]
    fn overflow_can_float_focused_window_first_when_configured() {
        let monitor = primary_test_monitor();
        let work_area = monitor.work_area;
        let mut focused = window(1, "Focused", Rect::from_size(0, 0, 100, 100));
        focused.size_constraints = winland_core::WindowSizeConstraints::minimum(700, 0);
        let mut other = window(2, "Other", Rect::from_size(200, 0, 100, 100));
        other.size_constraints = winland_core::WindowSizeConstraints::minimum(700, 0);
        let mut state = daemon_state([focused, other]);
        state.foreground = Some(WindowHandle(1));
        state.config.overflow_focus_policy = OverflowFocusPolicy::FloatFocused;

        let plan = state.plan_command(DaemonCommand::Retile, &[monitor]);

        assert_eq!(
            plan.moves,
            vec![TileAssignment {
                window: WindowHandle(2),
                rect: work_area,
            }]
        );
        assert_eq!(state.overflow_floating, BTreeSet::from([WindowHandle(1)]));
    }

    #[test]
    fn overflow_can_float_multiple_windows_until_layout_fits() {
        let monitor = primary_test_monitor();
        let work_area = monitor.work_area;
        let mut first = window(1, "Focused", Rect::from_size(0, 0, 100, 100));
        first.size_constraints = winland_core::WindowSizeConstraints::minimum(0, 400);
        let mut second = window(2, "Second", Rect::from_size(0, 200, 100, 100));
        second.size_constraints = winland_core::WindowSizeConstraints::minimum(0, 400);
        let mut third = window(3, "Third", Rect::from_size(0, 400, 100, 100));
        third.size_constraints = winland_core::WindowSizeConstraints::minimum(0, 400);
        let mut state = daemon_state([first, second, third]);
        state.config.layout.kind = LayoutKind::VerticalStack;
        state.foreground = Some(WindowHandle(1));

        let plan = state.plan_command(DaemonCommand::Retile, &[monitor]);

        assert_eq!(
            plan.moves,
            vec![TileAssignment {
                window: WindowHandle(1),
                rect: work_area,
            }]
        );
        assert_eq!(
            state.overflow_floating,
            BTreeSet::from([WindowHandle(2), WindowHandle(3)])
        );
    }

    #[test]
    fn overflow_floats_focused_window_when_it_cannot_fit_by_itself() {
        let monitor = primary_test_monitor();
        let mut focused = window(1, "Focused", Rect::from_size(0, 0, 100, 100));
        focused.size_constraints = winland_core::WindowSizeConstraints::minimum(
            monitor.work_area.width() + 1,
            monitor.work_area.height(),
        );
        let mut state = daemon_state([focused]);
        state.foreground = Some(WindowHandle(1));

        let plan = state.plan_command(DaemonCommand::Retile, &[monitor]);

        assert!(plan.moves.is_empty());
        assert_eq!(state.overflow_floating, BTreeSet::from([WindowHandle(1)]));
    }

    #[test]
    fn overflow_windows_are_reconsidered_on_next_retile() {
        let monitor = primary_test_monitor();
        let mut first = window(1, "One", Rect::from_size(0, 0, 100, 100));
        first.size_constraints = winland_core::WindowSizeConstraints::minimum(700, 0);
        let mut second = window(2, "Two", Rect::from_size(200, 0, 100, 100));
        second.size_constraints = winland_core::WindowSizeConstraints::minimum(700, 0);
        let mut state = daemon_state([first, second]);
        state.foreground = Some(WindowHandle(1));

        let _ = state.plan_command(DaemonCommand::Retile, std::slice::from_ref(&monitor));
        assert_eq!(state.overflow_floating, BTreeSet::from([WindowHandle(2)]));

        state
            .windows
            .get_mut(&WindowHandle(2))
            .unwrap()
            .size_constraints = winland_core::WindowSizeConstraints::NONE;
        let plan = state.plan_command(DaemonCommand::Retile, &[monitor]);

        assert_eq!(state.overflow_floating, BTreeSet::new());
        assert_eq!(plan.moves.len(), 2);
    }

    #[test]
    fn retile_keeps_partially_offscreen_windows_on_nearest_monitor() {
        let monitor = primary_test_monitor();
        let mut state = daemon_state([
            window(1, "Mostly Offscreen", Rect::from_size(-900, 0, 1000, 100)),
            window(2, "Visible", Rect::from_size(200, 0, 100, 100)),
        ]);

        let plan = state.plan_command(DaemonCommand::Retile, &[monitor]);

        assert_eq!(
            plan.moves,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 500, 760),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(500, 0, 500, 760),
                },
            ]
        );
    }

    #[test]
    fn monitor_selection_uses_overlap_before_center() {
        let monitors = [primary_test_monitor(), secondary_test_monitor()];

        assert_eq!(
            monitor_for_rect(Rect::from_size(-900, 0, 1000, 100), &monitors),
            Some(MonitorId(1))
        );
    }

    #[test]
    fn monitor_selection_falls_back_to_nearest_monitor_when_fully_offscreen() {
        let monitors = [primary_test_monitor(), secondary_test_monitor()];

        assert_eq!(
            monitor_for_rect(Rect::from_size(2600, 0, 100, 100), &monitors),
            Some(MonitorId(2))
        );
    }

    #[test]
    fn monitor_selection_can_use_cursor_position() {
        let monitors = [primary_test_monitor(), secondary_test_monitor()];

        assert_eq!(
            monitor_for_point(winland_core::Point { x: 1200, y: 300 }, &monitors),
            Some(MonitorId(2))
        );
    }

    #[test]
    fn monitor_override_from_drag_controls_next_retile_assignment() {
        let monitors = [primary_test_monitor(), secondary_test_monitor()];
        let mut state = daemon_state([window(1, "Dragged", Rect::from_size(0, 0, 100, 100))]);
        state
            .window_monitor_overrides
            .insert(WindowHandle(1), MonitorId(2));

        let plan = state.plan_command(DaemonCommand::Retile, &monitors);

        assert_eq!(
            plan.moves,
            vec![TileAssignment {
                window: WindowHandle(1),
                rect: Rect::from_size(1000, 0, 800, 560),
            }]
        );
    }

    #[test]
    fn learned_constraints_are_merged_into_layout_inputs() {
        let mut state = daemon_state([window(1, "Task Manager", Rect::from_size(0, 0, 100, 100))]);
        state.learned_size_constraints.insert(
            WindowHandle(1),
            winland_core::WindowSizeConstraints::minimum(420, 300),
        );

        let windows = state.layout_windows_for_handles(&[WindowHandle(1)]);

        assert_eq!(
            windows,
            vec![WindowLayoutInfo {
                handle: WindowHandle(1),
                size_constraints: winland_core::WindowSizeConstraints::minimum(420, 300),
            }]
        );
    }

    #[test]
    fn retile_uses_monitor_specific_layout_override() {
        let monitors = [primary_test_monitor(), secondary_test_monitor()];
        let mut state = daemon_state([
            window(1, "Primary One", Rect::from_size(0, 0, 100, 100)),
            window(2, "Primary Two", Rect::from_size(200, 0, 100, 100)),
            window(3, "Secondary One", Rect::from_size(1100, 0, 100, 100)),
            window(4, "Secondary Two", Rect::from_size(1300, 0, 100, 100)),
        ]);
        state.config.layout_per_monitor.insert(
            MonitorId(2).to_string(),
            LayoutConfig {
                kind: LayoutKind::VerticalStack,
                ..LayoutConfig::default()
            },
        );
        state.sync_workspace_state(&monitors);

        let plan = state.plan_command(DaemonCommand::Retile, &monitors);

        assert_eq!(
            plan.moves,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 500, 760),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(500, 0, 500, 760),
                },
                TileAssignment {
                    window: WindowHandle(3),
                    rect: Rect::from_size(1000, 0, 800, 280),
                },
                TileAssignment {
                    window: WindowHandle(4),
                    rect: Rect::from_size(1000, 280, 800, 280),
                },
            ]
        );
    }

    #[test]
    fn dwindle_drop_splits_assignment_under_drop_point() {
        let monitor = primary_test_monitor();
        let mut state = daemon_state([
            window(1, "Top Left", Rect::from_size(0, 0, 100, 100)),
            window(2, "Bottom", Rect::from_size(0, 600, 1000, 160)),
            window(3, "Top Right", Rect::from_size(500, 0, 100, 100)),
        ]);
        state.config.layout = LayoutConfig {
            kind: LayoutKind::Dwindle,
            smart_split: true,
            ..LayoutConfig::default()
        };
        state.dwindle_splits.insert(
            (WorkspaceId(1), monitor.id),
            vec![
                DwindleSplit {
                    target: WindowHandle(1),
                    new_window: WindowHandle(2),
                    direction: winland_core::SplitDirection::Down,
                },
                DwindleSplit {
                    target: WindowHandle(1),
                    new_window: WindowHandle(3),
                    direction: winland_core::SplitDirection::Right,
                },
            ],
        );

        assert!(state.retarget_dwindle_drop(
            WindowHandle(2),
            Rect::from_size(700, 700, 100, 100),
            Some(winland_core::Point { x: 200, y: 0 }),
            &[WindowHandle(1), WindowHandle(3)],
            &monitor,
            state.config.layout,
        ));

        assert_eq!(
            state.dwindle_splits.get(&(WorkspaceId(1), monitor.id)),
            Some(&vec![
                DwindleSplit {
                    target: WindowHandle(1),
                    new_window: WindowHandle(3),
                    direction: winland_core::SplitDirection::Right,
                },
                DwindleSplit {
                    target: WindowHandle(1),
                    new_window: WindowHandle(2),
                    direction: winland_core::SplitDirection::Up,
                },
            ])
        );
    }

    #[test]
    fn dwindle_drop_commits_pruned_preview_before_new_split() {
        let monitor = primary_test_monitor();
        let mut state = daemon_state([
            window(1, "B", Rect::from_size(0, 0, 500, 760)),
            window(2, "A", Rect::from_size(500, 0, 500, 760)),
            window(3, "C", Rect::from_size(500, 0, 500, 380)),
        ]);
        state.config.layout = LayoutConfig {
            kind: LayoutKind::Dwindle,
            smart_split: true,
            ..LayoutConfig::default()
        };
        state.dwindle_splits.insert(
            (WorkspaceId(1), monitor.id),
            vec![
                DwindleSplit {
                    target: WindowHandle(1),
                    new_window: WindowHandle(3),
                    direction: winland_core::SplitDirection::Right,
                },
                DwindleSplit {
                    target: WindowHandle(3),
                    new_window: WindowHandle(2),
                    direction: winland_core::SplitDirection::Down,
                },
            ],
        );

        assert!(state.retarget_dwindle_drop(
            WindowHandle(3),
            Rect::from_size(500, 0, 500, 380),
            Some(winland_core::Point { x: 750, y: 10 }),
            &[WindowHandle(1), WindowHandle(2)],
            &monitor,
            state.config.layout,
        ));

        assert_eq!(
            state.dwindle_splits.get(&(WorkspaceId(1), monitor.id)),
            Some(&vec![
                DwindleSplit {
                    target: WindowHandle(1),
                    new_window: WindowHandle(2),
                    direction: winland_core::SplitDirection::Right,
                },
                DwindleSplit {
                    target: WindowHandle(2),
                    new_window: WindowHandle(3),
                    direction: winland_core::SplitDirection::Up,
                },
            ])
        );

        let plan = state.plan_command(DaemonCommand::Retile, &[monitor]);

        assert_eq!(
            plan.moves,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 500, 760),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(500, 380, 500, 380),
                },
                TileAssignment {
                    window: WindowHandle(3),
                    rect: Rect::from_size(500, 0, 500, 380),
                },
            ]
        );
    }

    #[test]
    fn movesize_start_temporarily_floats_tiled_window() {
        let mut state = daemon_state([
            window(1, "One", Rect::from_size(0, 0, 100, 100)),
            window(2, "Two", Rect::from_size(200, 0, 100, 100)),
        ]);

        let plan = state.plan_after_window_events(
            &[event(WindowEventKind::MoveSizeStart, 1)],
            &SnapshotDiff::default(),
            &[primary_test_monitor()],
        );

        assert_eq!(
            state.window_participation(WindowHandle(1)),
            WindowParticipation::TemporarilyFloating
        );
        assert_eq!(
            state.previous_rects.get(&WindowHandle(1)).copied(),
            Some(Rect::from_size(0, 0, 100, 100))
        );
        assert_eq!(
            plan.moves,
            vec![TileAssignment {
                window: WindowHandle(2),
                rect: primary_test_monitor().work_area,
            }]
        );
    }

    #[test]
    fn movesize_end_reabsorbs_temporary_float_by_default() {
        let mut state = daemon_state([
            window(1, "One", Rect::from_size(0, 0, 100, 100)),
            window(2, "Two", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.set_window_participation(WindowHandle(1), WindowParticipation::TemporarilyFloating);

        let plan = state.plan_after_window_events(
            &[event(WindowEventKind::MoveSizeEnd, 1)],
            &SnapshotDiff::default(),
            &[primary_test_monitor()],
        );

        assert_eq!(
            state.window_participation(WindowHandle(1)),
            WindowParticipation::Tiled
        );
        assert_eq!(plan.moves.len(), 2);
        assert_eq!(plan.moves[0].window, WindowHandle(1));
    }

    #[test]
    fn modifier_drag_delta_offsets_original_rect() {
        let rect = offset_rect_by_cursor_delta(
            Rect::from_size(100, 200, 300, 400),
            Point { x: 125, y: 250 },
            Point { x: 175, y: 225 },
        );

        assert_eq!(rect, Rect::from_size(150, 175, 300, 400));
    }

    #[test]
    fn modifier_drag_start_uses_temporary_float_like_normal_drag() {
        let mut state = daemon_state([
            window(1, "One", Rect::from_size(0, 0, 100, 100)),
            window(2, "Two", Rect::from_size(200, 0, 100, 100)),
        ]);

        assert!(state.handle_movesize_start(WindowHandle(1)));

        assert_eq!(
            state.window_participation(WindowHandle(1)),
            WindowParticipation::TemporarilyFloating
        );
        assert_eq!(
            state.previous_rects.get(&WindowHandle(1)).copied(),
            Some(Rect::from_size(0, 0, 100, 100))
        );
    }

    #[test]
    fn movesize_end_reorders_tiled_window_from_drop_position() {
        let mut state = daemon_state([
            window(1, "One", Rect::from_size(700, 500, 100, 100)),
            window(2, "Two", Rect::from_size(100, 0, 100, 100)),
            window(3, "Three", Rect::from_size(300, 0, 100, 100)),
        ]);
        state.set_window_participation(WindowHandle(1), WindowParticipation::TemporarilyFloating);

        let plan = state.plan_after_window_events(
            &[event(WindowEventKind::MoveSizeEnd, 1)],
            &SnapshotDiff::default(),
            &[primary_test_monitor()],
        );

        assert_eq!(
            state.tile_order,
            vec![WindowHandle(2), WindowHandle(3), WindowHandle(1)]
        );
        assert_eq!(
            plan.moves
                .iter()
                .map(|assignment| assignment.window)
                .collect::<Vec<_>>(),
            vec![WindowHandle(2), WindowHandle(3), WindowHandle(1)]
        );
    }

    #[test]
    fn explicit_modifier_drop_uses_supplied_final_position() {
        let mut state = daemon_state([
            window(1, "One", Rect::from_size(0, 0, 100, 100)),
            window(2, "Two", Rect::from_size(300, 0, 100, 100)),
            window(3, "Three", Rect::from_size(600, 0, 100, 100)),
        ]);
        state.set_window_participation(WindowHandle(1), WindowParticipation::TemporarilyFloating);

        assert!(state.reorder_temporary_float_by_drop_at(
            WindowHandle(1),
            &[primary_test_monitor()],
            Rect::from_size(850, 500, 100, 100),
            Some(Point { x: 900, y: 550 }),
        ));

        assert_eq!(
            state.tile_order,
            vec![WindowHandle(2), WindowHandle(3), WindowHandle(1)]
        );
    }

    #[test]
    fn explicit_modifier_drop_uses_cursor_instead_of_final_rect_center() {
        let mut state = daemon_state([
            window(1, "One", Rect::from_size(0, 0, 100, 100)),
            window(2, "Two", Rect::from_size(300, 0, 100, 100)),
            window(3, "Three", Rect::from_size(600, 0, 100, 100)),
        ]);
        state.set_window_participation(WindowHandle(1), WindowParticipation::TemporarilyFloating);

        assert!(state.reorder_temporary_float_by_drop_at(
            WindowHandle(1),
            &[primary_test_monitor()],
            Rect::from_size(0, 0, 100, 100),
            Some(Point { x: 900, y: 550 }),
        ));

        assert_eq!(
            state.tile_order,
            vec![WindowHandle(2), WindowHandle(3), WindowHandle(1)]
        );
    }

    #[test]
    fn movesize_end_accepts_explicit_modifier_drop_context() {
        let mut state = daemon_state([
            window(1, "One", Rect::from_size(0, 0, 100, 100)),
            window(2, "Two", Rect::from_size(300, 0, 100, 100)),
            window(3, "Three", Rect::from_size(600, 0, 100, 100)),
        ]);
        state.set_window_participation(WindowHandle(1), WindowParticipation::TemporarilyFloating);

        assert!(state.handle_movesize_end(
            WindowHandle(1),
            &[primary_test_monitor()],
            Some(DropContext {
                rect: Rect::from_size(0, 0, 100, 100),
                cursor: Some(Point { x: 900, y: 550 }),
            }),
        ));

        assert_eq!(
            state.window_participation(WindowHandle(1)),
            WindowParticipation::Tiled
        );
        assert_eq!(
            state.tile_order,
            vec![WindowHandle(2), WindowHandle(3), WindowHandle(1)]
        );
    }

    #[test]
    fn active_modifier_drag_window_events_are_ignored_while_batching() {
        let mut state = daemon_state([window(1, "One", Rect::from_size(0, 0, 100, 100))]);
        state.active_modifier_drag = Some(ActiveModifierDrag {
            window: WindowHandle(1),
            start_cursor: Point { x: 10, y: 10 },
            last_cursor: Point { x: 10, y: 10 },
            start_rect: Rect::from_size(0, 0, 100, 100),
            move_count: 0,
            started_temporary_float: true,
        });

        assert!(state.should_ignore_modifier_drag_window_event(event(WindowEventKind::Moved, 1)));
        assert!(
            state
                .should_ignore_modifier_drag_window_event(event(WindowEventKind::MoveSizeStart, 1))
        );
        assert!(
            state.should_ignore_modifier_drag_window_event(event(WindowEventKind::MoveSizeEnd, 1))
        );
    }

    #[test]
    fn stale_modifier_drag_window_events_are_ignored_after_drop() {
        let mut state = daemon_state([window(1, "One", Rect::from_size(0, 0, 100, 100))]);
        state
            .suppressed_modifier_drag_events
            .insert(WindowHandle(1));

        state
            .reconcile_after_events(&[
                event(WindowEventKind::Moved, 1),
                event(WindowEventKind::MoveSizeStart, 1),
                event(WindowEventKind::MoveSizeEnd, 1),
            ])
            .unwrap();

        assert!(state.suppressed_modifier_drag_events.is_empty());
        assert_eq!(
            state
                .windows
                .get(&WindowHandle(1))
                .map(|window| window.rect),
            Some(Rect::from_size(0, 0, 100, 100))
        );
    }

    #[test]
    fn permanent_floating_survives_movesize_start() {
        let mut state = daemon_state([
            window(1, "One", Rect::from_size(0, 0, 100, 100)),
            window(2, "Two", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.set_window_participation(WindowHandle(1), WindowParticipation::Floating);

        let plan = state.plan_after_window_events(
            &[event(WindowEventKind::MoveSizeStart, 1)],
            &SnapshotDiff::default(),
            &[primary_test_monitor()],
        );

        assert_eq!(
            state.window_participation(WindowHandle(1)),
            WindowParticipation::Floating
        );
        assert!(plan.moves.is_empty());
    }

    #[test]
    fn dynamic_retile_can_be_disabled() {
        let mut state = daemon_state([
            window(1, "One", Rect::from_size(0, 0, 100, 100)),
            window(2, "Two", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.config.dynamic_retile = false;

        let plan = state.plan_after_window_events(
            &[event(WindowEventKind::Shown, 2)],
            &SnapshotDiff::default(),
            &[primary_test_monitor()],
        );

        assert!(plan.moves.is_empty());
    }

    #[test]
    fn monitor_moves_request_dynamic_retile() {
        let monitors = [primary_test_monitor(), secondary_test_monitor()];
        let old = daemon_state([window(1, "One", Rect::from_size(100, 0, 100, 100))]);
        let refreshed = daemon_state([window(1, "One", Rect::from_size(1100, 0, 100, 100))]);

        let diff = old.diff(&refreshed, &monitors);

        assert_eq!(diff.moved_between_monitors, 1);
        assert!(refreshed.should_retile_after_events(&[event(WindowEventKind::Moved, 1)], &diff));
    }

    #[test]
    fn metadata_changes_request_dynamic_retile() {
        let state = daemon_state([window(1, "Paint", Rect::from_size(0, 0, 100, 100))]);

        assert!(state.should_retile_after_events(
            &[event(WindowEventKind::MetadataChanged, 1)],
            &SnapshotDiff::default(),
        ));
    }

    #[test]
    fn metadata_changes_clear_learned_size_constraint_before_retile() {
        let monitor = primary_test_monitor();
        let work_area = monitor.work_area;
        let mut state = daemon_state([window(1, "Paint", Rect::from_size(0, 0, 100, 100))]);
        state.learned_size_constraints.insert(
            WindowHandle(1),
            WindowSizeConstraints::minimum(work_area.width() + 200, work_area.height() + 200),
        );

        let plan = state.plan_after_window_events(
            &[event(WindowEventKind::MetadataChanged, 1)],
            &SnapshotDiff::default(),
            &[monitor],
        );

        assert!(
            !state
                .learned_size_constraints
                .contains_key(&WindowHandle(1))
        );
        assert_eq!(
            plan.moves,
            vec![TileAssignment {
                window: WindowHandle(1),
                rect: work_area,
            }]
        );
    }

    #[test]
    fn drag_start_clears_learned_size_constraint() {
        let monitor = primary_test_monitor();
        let mut state = daemon_state([window(1, "Paint", Rect::from_size(0, 0, 100, 100))]);
        state
            .learned_size_constraints
            .insert(WindowHandle(1), WindowSizeConstraints::minimum(900, 700));

        let _ = state.plan_after_window_events(
            &[event(WindowEventKind::MoveSizeStart, 1)],
            &SnapshotDiff::default(),
            &[monitor],
        );

        assert!(
            !state
                .learned_size_constraints
                .contains_key(&WindowHandle(1))
        );
    }

    #[test]
    fn switch_workspace_plans_hide_show_and_retile_for_target_workspace() {
        let mut state = daemon_state([
            window(1, "One", Rect::from_size(0, 0, 100, 100)),
            window(2, "Two", Rect::from_size(200, 0, 100, 100)),
        ]);
        state
            .workspaces
            .move_window_to_workspace(WindowHandle(2), WorkspaceId(2));

        let plan = state.plan_command(
            DaemonCommand::SwitchWorkspace(WorkspaceId(2)),
            &[primary_test_monitor()],
        );

        assert_eq!(state.workspaces.active_workspace(), WorkspaceId(2));
        assert_eq!(plan.hide, vec![WindowHandle(1)]);
        assert_eq!(
            plan.show,
            vec![WorkspaceVisibilityChange {
                window: WindowHandle(2),
                restore_rect: Some(Rect::from_size(200, 0, 100, 100)),
            }]
        );
        assert_eq!(
            plan.moves,
            vec![TileAssignment {
                window: WindowHandle(2),
                rect: primary_test_monitor().work_area,
            }]
        );
    }

    #[test]
    fn work_area_sized_window_on_inactive_workspace_is_hidden() {
        let mut state = daemon_state([window(1, "One", primary_test_monitor().work_area)]);
        state
            .workspaces
            .move_window_to_workspace(WindowHandle(1), WorkspaceId(2));
        state.workspaces.switch_to(WorkspaceId(2));

        let plan = state.plan_command(
            DaemonCommand::SwitchWorkspace(WorkspaceId(1)),
            &[primary_test_monitor()],
        );

        assert_eq!(plan.hide, vec![WindowHandle(1)]);
        assert!(plan.moves.is_empty());
    }

    #[test]
    fn move_focused_to_inactive_workspace_hides_it_and_retiles_remaining_windows() {
        let mut state = daemon_state([
            window(1, "One", Rect::from_size(0, 0, 100, 100)),
            window(2, "Two", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.foreground = Some(WindowHandle(1));

        let plan = state.plan_command(
            DaemonCommand::MoveFocusedToWorkspace(WorkspaceId(2)),
            &[primary_test_monitor()],
        );

        assert_eq!(plan.hide, vec![WindowHandle(1)]);
        assert_eq!(plan.show, Vec::<WorkspaceVisibilityChange>::new());
        assert_eq!(
            state
                .workspaces
                .window_state(WindowHandle(1))
                .unwrap()
                .workspace,
            WorkspaceId(2)
        );
        assert_eq!(
            plan.moves,
            vec![TileAssignment {
                window: WindowHandle(2),
                rect: primary_test_monitor().work_area,
            }]
        );
    }

    #[test]
    fn fullscreen_windows_are_not_hidden_by_workspace_switches() {
        let mut state = daemon_state([
            window(1, "Fullscreen", primary_test_monitor().rect),
            window(2, "Two", Rect::from_size(200, 0, 100, 100)),
        ]);
        state
            .workspaces
            .track_window(WindowHandle(1), primary_test_monitor().rect);
        state
            .workspaces
            .move_window_to_workspace(WindowHandle(2), WorkspaceId(2));

        let plan = state.plan_command(
            DaemonCommand::SwitchWorkspace(WorkspaceId(2)),
            &[primary_test_monitor()],
        );

        assert_eq!(plan.hide, Vec::<WindowHandle>::new());
    }

    #[test]
    fn snapshot_diff_reports_added_removed_changed_and_foreground_changes() {
        let mut old = daemon_state([
            window(1, "Editor", Rect::from_size(10, 20, 800, 600)),
            window(2, "Terminal", Rect::from_size(10, 20, 800, 600)),
        ]);
        old.foreground = Some(WindowHandle(1));

        let mut refreshed = daemon_state([
            window(1, "Editor - changed", Rect::from_size(10, 20, 800, 600)),
            window(3, "Browser", Rect::from_size(10, 20, 800, 600)),
        ]);
        refreshed.foreground = Some(WindowHandle(3));

        let diff = old.diff(&refreshed, &[primary_test_monitor()]);

        assert_eq!(diff.added, vec![WindowHandle(3)]);
        assert_eq!(diff.removed, vec![WindowHandle(2)]);
        assert_eq!(diff.changed, 1);
        assert_eq!(diff.moved_between_monitors, 0);
        assert!(diff.foreground_changed);
    }

    #[test]
    fn event_count_only_counts_requested_kind() {
        let batch = [
            event(WindowEventKind::Shown, 1),
            event(WindowEventKind::Moved, 1),
            event(WindowEventKind::Shown, 2),
        ];

        assert_eq!(count_events(&batch, WindowEventKind::Shown), 2);
        assert_eq!(count_events(&batch, WindowEventKind::Moved), 1);
        assert_eq!(count_events(&batch, WindowEventKind::Hidden), 0);
    }

    #[test]
    fn pointer_speed_window_events_bypass_batch_debounce() {
        assert!(should_process_window_event_immediately(event(
            WindowEventKind::Moved,
            1
        )));
        assert!(should_process_window_event_immediately(event(
            WindowEventKind::MoveSizeStart,
            1
        )));
        assert!(should_process_window_event_immediately(event(
            WindowEventKind::MoveSizeEnd,
            1
        )));
        assert!(should_process_window_event_immediately(event(
            WindowEventKind::ForegroundChanged,
            1
        )));
        assert!(!should_process_window_event_immediately(event(
            WindowEventKind::Created,
            1
        )));
    }

    #[test]
    fn ipc_state_snapshot_reports_observable_daemon_counts() {
        let mut state = daemon_state([
            window(1, "One", Rect::from_size(0, 0, 100, 100)),
            window(2, "Two", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.foreground = Some(WindowHandle(2));
        state.set_window_participation(WindowHandle(1), WindowParticipation::Floating);
        state.workspaces.switch_to(WorkspaceId(2));

        let response = state.handle_ipc_request(IpcRequest::state());

        assert_eq!(
            response,
            IpcResponse::state(DaemonStateSnapshot {
                total_windows: 2,
                manageable_windows: 2,
                floating_windows: 1,
                temporary_floating_windows: 0,
                active_workspace: 2,
                foreground_window: Some(2),
            })
        );
    }

    #[test]
    fn border_candidates_use_focus_inactive_and_floating_colors() {
        let mut state = daemon_state([
            window(1, "One", Rect::from_size(0, 0, 100, 100)),
            window(2, "Two", Rect::from_size(200, 0, 100, 100)),
            window(3, "Three", Rect::from_size(400, 0, 100, 100)),
        ]);
        state.config.borders.enabled = true;
        state.config.borders.active_color = BorderColor::new(1, 2, 3);
        state.config.borders.inactive_color = BorderColor::new(4, 5, 6);
        state.config.borders.floating_color = BorderColor::new(7, 8, 9);
        state.foreground = Some(WindowHandle(2));
        state.set_window_participation(WindowHandle(3), WindowParticipation::Floating);

        let candidates = state.border_candidates(&[primary_test_monitor()]);

        assert_eq!(
            candidates
                .iter()
                .map(|candidate| (candidate.window, candidate.color))
                .collect::<Vec<_>>(),
            vec![
                (WindowHandle(1), BorderColor::new(4, 5, 6)),
                (WindowHandle(2), BorderColor::new(1, 2, 3)),
                (WindowHandle(3), BorderColor::new(7, 8, 9)),
            ]
        );
    }

    #[test]
    fn border_candidates_hide_inactive_when_configured() {
        let mut state = daemon_state([
            window(1, "One", Rect::from_size(0, 0, 100, 100)),
            window(2, "Two", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.config.borders.enabled = true;
        state.config.borders.show_inactive = false;
        state.foreground = Some(WindowHandle(2));

        let candidates = state.border_candidates(&[primary_test_monitor()]);

        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.window)
                .collect::<Vec<_>>(),
            vec![WindowHandle(2)]
        );
    }

    #[test]
    fn border_candidates_hide_all_for_focused_fullscreen_window() {
        let mut state = daemon_state([
            window(1, "Fullscreen", primary_test_monitor().rect),
            window(2, "Two", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.config.borders.enabled = true;
        state.foreground = Some(WindowHandle(1));

        let candidates = state.border_candidates(&[primary_test_monitor()]);

        assert!(candidates.is_empty());
    }

    fn daemon_state<const N: usize>(windows: [WindowInfo; N]) -> DaemonState {
        let windows: BTreeMap<_, _> = windows
            .into_iter()
            .map(|window| (window.handle, window))
            .collect();
        let mut state = DaemonState {
            windows,
            foreground: None,
            tile_order: Vec::new(),
            participation: BTreeMap::new(),
            dwindle_splits: BTreeMap::new(),
            previous_rects: BTreeMap::new(),
            learned_size_constraints: BTreeMap::new(),
            window_monitor_overrides: BTreeMap::new(),
            overflow_floating: BTreeSet::new(),
            workspaces: WorkspaceManager::new(9),
            active_modifier_drag: None,
            suppressed_modifier_drag_events: BTreeSet::new(),
            config: RuntimeConfig::default(),
            hotkey_commands: HotkeyCommandMap::default(),
            border_manager: None,
        };
        state.tile_order = state.manageable_handles_sorted();
        state.sync_workspace_state(&[primary_test_monitor()]);
        state
    }

    fn event(kind: WindowEventKind, handle: u64) -> WindowEvent {
        WindowEvent {
            kind,
            window: WindowHandle(handle),
            event_time: 0,
        }
    }

    fn primary_test_monitor() -> MonitorInfo {
        MonitorInfo {
            id: MonitorId(1),
            is_primary: true,
            rect: Rect::from_size(0, 0, 1000, 800),
            work_area: Rect::from_size(0, 0, 1000, 760),
        }
    }

    fn secondary_test_monitor() -> MonitorInfo {
        MonitorInfo {
            id: MonitorId(2),
            is_primary: false,
            rect: Rect::from_size(1000, 0, 800, 600),
            work_area: Rect::from_size(1000, 0, 800, 560),
        }
    }

    fn window(handle: u64, title: &str, rect: Rect) -> WindowInfo {
        WindowInfo {
            handle: WindowHandle(handle),
            title: title.to_owned(),
            class_name: "ApplicationFrameWindow".to_owned(),
            process_id: 42,
            executable_path: Some(r"C:\Windows\System32\notepad.exe".to_owned()),
            is_visible: true,
            is_minimized: false,
            is_dwm_cloaked: false,
            has_owner: false,
            is_tool_window: false,
            styles: WindowStyles {
                style: 0,
                extended_style: 0,
            },
            size_constraints: winland_core::WindowSizeConstraints::NONE,
            rect,
        }
    }
}
