use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use winland_config::{
    Config, HotkeyBindingConfig, HotkeyKey, HotkeyMode, HotkeyModifier, OverflowFloatPersistence,
    OverflowFocusPolicy, TextMatcherConfig,
};
use winland_core::{
    DwindleSplit, FullscreenDetection, GameModeDetection, GameModePolicy, GameModeReason,
    LayoutConfig, LayoutKind, MonitorId, MonitorInfo, Point, Rect, TileAssignment, WindowHandle,
    WindowInfo, WindowLayoutInfo, WindowParticipation, WindowRule, WindowRuleDecision,
    WindowRuleMode, WindowSizeConstraints, WorkspaceId, WorkspaceManager,
    WorkspaceVisibilityChange, detect_fullscreen_window, detect_game_mode, evaluate_window_rules,
    game_mode_executable_matches, split_direction_for_point, tile_assignments_fit_work_area,
    tile_layout_windows_with_config, tile_layout_windows_with_state,
};
use winland_ipc::{
    DaemonPerformanceSnapshot, DaemonStateSnapshot, IpcCommand, IpcRequest, IpcResponse,
    MonitorStateSnapshot, ReloadConfigReport, WindowParticipationSnapshot, WindowStateSnapshot,
    decode_request, encode_response,
};
use winland_win32::{
    BorderColor, BorderManager, BorderUpdate, HotkeyBinding, HotkeyBypassRules, HotkeyEvent,
    HotkeyId, HotkeyLowLevelEvent, HotkeyModifierSet, HotkeyOverrideOptions, IpcTransportRequest,
    ModifierDragOptions, MouseDragEvent, MouseDragEventKind, VirtualKey, WindowEvent,
    WindowEventKind,
};

const RECONCILE_DEBOUNCE: Duration = Duration::from_millis(50);
const LOW_LATENCY_MOVE_DEBOUNCE: Duration = Duration::from_millis(8);
const INTERACTIVE_DRAG_POLL_INTERVAL: Duration = Duration::from_millis(16);
const MAX_BATCH_SIZE: usize = 512;
const TILE_FEEDBACK_PASSES: usize = 3;
const TILE_FEEDBACK_TOLERANCE_PX: i32 = 0;
const LAYOUT_APPLY_TOLERANCE_PX: i32 = 1;
const GAME_MODE_FULLSCREEN_DEACTIVATE_CONFIRMATIONS: u8 = 2;

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
    let ipc_server =
        winland_win32::spawn_ipc_server(winland_win32::DEFAULT_IPC_PIPE_NAME, ipc_sender)
            .context("start local IPC named pipe server")?;
    let ipc_bridge = spawn_ipc_bridge(ipc_receiver, daemon_sender.clone())
        .context("spawn IPC request bridge")?;

    let (hotkey_sender, hotkey_receiver) = mpsc::channel();
    let (hotkey_bindings, hotkey_commands) = hotkey_bindings_from_config(&loaded_config.config)?;
    let hotkey_backend = install_hotkey_backend(
        &loaded_config.config,
        hotkey_bindings.clone(),
        hotkey_sender.clone(),
    )?;
    debug!(
        backend = hotkey_backend.name(),
        "installed daemon hotkey backend"
    );
    let hotkey_bridge = spawn_hotkey_bridge(hotkey_receiver, daemon_sender.clone())
        .context("spawn hotkey bridge")?;
    let (mouse_drag_sender, mouse_drag_receiver) = mpsc::channel();
    let modifier_drag = install_modifier_drag(&loaded_config.config, mouse_drag_sender.clone())?;
    let mouse_drag_bridge = spawn_mouse_drag_bridge(mouse_drag_receiver, daemon_sender.clone())
        .context("spawn modifier drag bridge")?;
    let shutdown_sender = daemon_sender.clone();
    let processor_sender = shutdown_sender.clone();
    drop(daemon_sender);

    let mut state = DaemonState::discover(runtime_config, hotkey_commands)
        .context("build initial window snapshot")?;
    state.source_config = loaded_config.config.clone();
    state.config_path = loaded_config.path.clone();
    state.config_loaded_at = SystemTime::now();
    state.hotkey_backend = Some(hotkey_backend);
    state.hotkey_sender = Some(hotkey_sender);
    state.modifier_drag = modifier_drag;
    state.mouse_drag_sender = Some(mouse_drag_sender);
    state.border_manager = Some(BorderManager::new().context("start border overlay manager")?);
    state.apply_startup_retile()?;
    state.sync_borders("startup border sync")?;
    let processor = thread::Builder::new()
        .name("winland-daemon-events".to_owned())
        .spawn(move || process_daemon_events(daemon_receiver, state, processor_sender))
        .context("spawn daemon event processor")?;

    info!("winland daemon started; entering Win32 message loop");
    let message_loop_result =
        winland_win32::run_message_loop().context("run Win32 daemon message loop");

    drop(subscription);
    drop(ipc_server);
    let _ = shutdown_sender.send(DaemonEvent::Shutdown);

    let processor_result = match processor.join() {
        Ok(Ok(())) => message_loop_result,
        Ok(Err(error)) => Err(error).context("process daemon events"),
        Err(_) => Err(anyhow!("daemon event processor thread panicked")),
    };

    join_bridge(window_bridge, "window event bridge")?;
    join_bridge(hotkey_bridge, "hotkey bridge")?;
    join_bridge(mouse_drag_bridge, "modifier drag bridge")?;
    join_bridge(ipc_bridge, "IPC request bridge")?;

    processor_result
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
    overflow_float_persistence: OverflowFloatPersistence,
    borders: RuntimeBorderConfig,
    game_mode: RuntimeGameModeConfig,
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
            overflow_float_persistence: config.behavior.overflow_float_persistence,
            borders: RuntimeBorderConfig::from_config(&config.borders)?,
            game_mode: RuntimeGameModeConfig::from_config(config),
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeGameModeConfig {
    policy: GameModePolicy,
    pause_all_layouts_when_game_focused: bool,
    pause_focused_monitor_only: bool,
    disable_borders: bool,
    disable_animations: bool,
    disable_keyboard_hooks: bool,
}

impl RuntimeGameModeConfig {
    fn from_config(config: &Config) -> Self {
        Self {
            policy: config.game_mode_policy(),
            pause_all_layouts_when_game_focused: config
                .game_mode
                .pause_all_layouts_when_game_focused,
            pause_focused_monitor_only: config.game_mode.pause_focused_monitor_only,
            disable_borders: config.game_mode.disable_borders,
            disable_animations: config.game_mode.disable_animations,
            disable_keyboard_hooks: config.game_mode.disable_keyboard_hooks,
        }
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

fn process_daemon_events(
    receiver: Receiver<DaemonEvent>,
    mut state: DaemonState,
    sender: Sender<DaemonEvent>,
) -> Result<()> {
    state.log_snapshot("initial window snapshot");
    let mut interactive_drag_tracker = None;

    while let Ok(event) = receiver.recv() {
        match event {
            DaemonEvent::Window(first_event) => {
                if state.should_process_moved_event_immediately(first_event) {
                    process_window_batch(
                        &mut state,
                        &mut interactive_drag_tracker,
                        &sender,
                        &[first_event],
                    )?;
                } else if first_event.kind == WindowEventKind::Moved {
                    let Some(batch) = receive_window_batch_with_timeout(
                        &receiver,
                        &mut state,
                        first_event,
                        LOW_LATENCY_MOVE_DEBOUNCE,
                    )?
                    else {
                        break;
                    };
                    process_window_batch(
                        &mut state,
                        &mut interactive_drag_tracker,
                        &sender,
                        &batch,
                    )?;
                } else if should_process_window_event_immediately(first_event) {
                    process_window_batch(
                        &mut state,
                        &mut interactive_drag_tracker,
                        &sender,
                        &[first_event],
                    )?;
                } else {
                    let Some(batch) = receive_window_batch(&receiver, &mut state, first_event)?
                    else {
                        break;
                    };
                    process_window_batch(
                        &mut state,
                        &mut interactive_drag_tracker,
                        &sender,
                        &batch,
                    )?;
                }
            }
            DaemonEvent::Hotkey(event) => state.handle_hotkey_event(event),
            DaemonEvent::MouseDrag(event) => {
                state.handle_mouse_drag(event)?;
                sync_interactive_drag_tracker(
                    &mut interactive_drag_tracker,
                    &sender,
                    state.active_interactive_drag_window(),
                    &[],
                );
            }
            DaemonEvent::Ipc(request) => state.handle_ipc(request),
            DaemonEvent::InteractiveDragTick { window, cursor } => {
                state.handle_interactive_drag_tick(window, cursor)?
            }
            DaemonEvent::InteractiveDragEnded { window } => {
                state.finish_interactive_drag(window);
                if interactive_drag_tracker
                    .as_ref()
                    .is_some_and(|tracker| tracker.window == window)
                {
                    interactive_drag_tracker = None;
                }
            }
            DaemonEvent::Shutdown => break,
        }
    }

    drop(interactive_drag_tracker);
    info!("daemon event channel closed; event processor stopping");
    Ok(())
}

fn process_window_batch(
    state: &mut DaemonState,
    interactive_drag_tracker: &mut Option<InteractiveDragTracker>,
    sender: &Sender<DaemonEvent>,
    batch: &[WindowEvent],
) -> Result<()> {
    let movesize_start = batch
        .iter()
        .find(|event| event.kind == WindowEventKind::MoveSizeStart)
        .map(|event| event.window);
    if let Some(window) = movesize_start {
        let monitors = winland_win32::enumerate_monitors()
            .context("enumerate monitors for native drag prestart")?;
        state.start_interactive_drag(window, &monitors);
        sync_interactive_drag_tracker(
            interactive_drag_tracker,
            sender,
            state.active_interactive_drag_window(),
            batch,
        );
        state.sync_border_geometry_with_monitors(&monitors, "native drag prestart borders")?;
    } else if let Some(monitors) = state.start_interactive_drag_from_moved_events(batch)? {
        sync_interactive_drag_tracker(
            interactive_drag_tracker,
            sender,
            state.active_interactive_drag_window(),
            batch,
        );
        state
            .sync_border_geometry_with_monitors(&monitors, "native drag inferred-start borders")?;
    }

    state.reconcile_after_events(batch)?;
    sync_interactive_drag_tracker(
        interactive_drag_tracker,
        sender,
        state.active_interactive_drag_window(),
        batch,
    );
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
) -> Result<Option<Vec<WindowEvent>>> {
    receive_window_batch_with_timeout(receiver, state, first_event, RECONCILE_DEBOUNCE)
}

fn receive_window_batch_with_timeout(
    receiver: &Receiver<DaemonEvent>,
    state: &mut DaemonState,
    first_event: WindowEvent,
    timeout: Duration,
) -> Result<Option<Vec<WindowEvent>>> {
    let mut batch = vec![first_event];

    while batch.len() < MAX_BATCH_SIZE {
        match receiver.recv_timeout(timeout) {
            Ok(DaemonEvent::Window(event)) => batch.push(event),
            Ok(DaemonEvent::Hotkey(event)) => state.handle_hotkey_event(event),
            Ok(DaemonEvent::MouseDrag(event)) => state.handle_mouse_drag(event)?,
            Ok(DaemonEvent::Ipc(request)) => state.handle_ipc(request),
            Ok(DaemonEvent::InteractiveDragTick { window, cursor }) => {
                state.handle_interactive_drag_tick(window, cursor)?
            }
            Ok(DaemonEvent::InteractiveDragEnded { window }) => {
                state.finish_interactive_drag(window);
            }
            Ok(DaemonEvent::Shutdown) => return Ok(None),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(Some(batch))
}

fn coalesce_window_events(batch: &[WindowEvent]) -> Vec<WindowEvent> {
    let mut seen = BTreeSet::new();
    let mut coalesced = Vec::with_capacity(batch.len());

    for event in batch.iter().rev().copied() {
        if seen.insert((event.kind, event.window)) {
            coalesced.push(event);
        }
    }

    coalesced.reverse();
    coalesced
}

#[derive(Debug)]
enum DaemonEvent {
    Window(WindowEvent),
    Hotkey(HotkeyEvent),
    MouseDrag(MouseDragEvent),
    Ipc(IpcTransportRequest),
    InteractiveDragTick { window: WindowHandle, cursor: Point },
    InteractiveDragEnded { window: WindowHandle },
    Shutdown,
}

struct InteractiveDragTracker {
    window: WindowHandle,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl InteractiveDragTracker {
    fn start(window: WindowHandle, sender: Sender<DaemonEvent>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let handle = thread::Builder::new()
            .name("winland-native-drag-tracker".to_owned())
            .spawn(move || {
                let mut last_cursor = None;
                while !worker_stop.load(Ordering::Relaxed) {
                    if !winland_win32::left_mouse_button_is_down() {
                        let _ = sender.send(DaemonEvent::InteractiveDragEnded { window });
                        break;
                    }

                    match winland_win32::cursor_position() {
                        Ok(cursor) if last_cursor != Some(cursor) => {
                            last_cursor = Some(cursor);
                            if sender
                                .send(DaemonEvent::InteractiveDragTick { window, cursor })
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            debug!(window = %window, %error, "stopping native drag tracker");
                            break;
                        }
                    }

                    thread::sleep(INTERACTIVE_DRAG_POLL_INTERVAL);
                }
            })
            .context("spawn native drag tracker")?;

        Ok(Self {
            window,
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for InteractiveDragTracker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            warn!(
                window = %self.window,
                "native drag tracker thread panicked while stopping"
            );
        }
    }
}

fn sync_interactive_drag_tracker(
    tracker: &mut Option<InteractiveDragTracker>,
    sender: &Sender<DaemonEvent>,
    active_drag: Option<WindowHandle>,
    batch: &[WindowEvent],
) {
    let tracker_window = tracker.as_ref().map(|tracker| tracker.window);
    let ended = batch.iter().any(|event| {
        event.kind == WindowEventKind::MoveSizeEnd && Some(event.window) == tracker_window
    });

    if ended || tracker_window.is_some() && tracker_window != active_drag {
        *tracker = None;
    }

    let Some(active_drag) = active_drag else {
        return;
    };

    if tracker
        .as_ref()
        .is_some_and(|tracker| tracker.window == active_drag)
    {
        return;
    }

    match InteractiveDragTracker::start(active_drag, sender.clone()) {
        Ok(next) => {
            *tracker = Some(next);
        }
        Err(error) => {
            warn!(window = %active_drag, %error, "failed to start native drag tracker");
        }
    }
}

struct DaemonState {
    windows: BTreeMap<WindowHandle, WindowInfo>,
    foreground: Option<WindowHandle>,
    tile_order: Vec<WindowHandle>,
    participation: BTreeMap<WindowHandle, WindowParticipation>,
    dwindle_splits: BTreeMap<(WorkspaceId, MonitorId), Vec<DwindleSplit>>,
    previous_rects: BTreeMap<WindowHandle, Rect>,
    learned_size_constraints: BTreeMap<WindowHandle, WindowSizeConstraints>,
    window_monitor_overrides: BTreeMap<WindowHandle, MonitorId>,
    overflow_promoted_floating: BTreeSet<WindowHandle>,
    workspaces: WorkspaceManager,
    active_modifier_drag: Option<ActiveModifierDrag>,
    active_interactive_drag: Option<ActiveInteractiveDrag>,
    suppressed_modifier_drag_events: BTreeSet<WindowHandle>,
    config: RuntimeConfig,
    source_config: Config,
    config_path: Option<PathBuf>,
    config_version: u64,
    config_loaded_at: SystemTime,
    hotkey_commands: HotkeyCommandMap,
    hotkey_backend: Option<HotkeyBackend>,
    hotkey_sender: Option<Sender<HotkeyEvent>>,
    modifier_drag: Option<winland_win32::ModifierDragRegistration>,
    mouse_drag_sender: Option<Sender<MouseDragEvent>>,
    border_manager: Option<BorderManager>,
    game_mode: GameModeRuntimeState,
    perf: DaemonPerformance,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct GameModeRuntimeState {
    active: Option<GameModeActivation>,
    fullscreen_deactivate_misses: u8,
}

#[derive(Debug, Clone, Default)]
struct DaemonPerformance {
    relayout_count: u64,
    skipped_relayout_count: u64,
    last_relayout_duration: Duration,
    last_relayout_move_count: usize,
    border_window_count: usize,
    config_reload_count: u64,
}

impl DaemonPerformance {
    fn snapshot(
        &self,
        managed_window_count: usize,
        game_mode_active: bool,
    ) -> DaemonPerformanceSnapshot {
        DaemonPerformanceSnapshot {
            relayout_count: self.relayout_count,
            skipped_relayout_count: self.skipped_relayout_count,
            last_relayout_duration_ms: saturating_duration_millis(self.last_relayout_duration),
            last_relayout_move_count: self.last_relayout_move_count,
            managed_window_count,
            border_window_count: self.border_window_count,
            game_mode_active,
            config_reload_count: self.config_reload_count,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GameModeActivation {
    window: WindowHandle,
    title: String,
    executable_path: Option<String>,
    monitor: Option<MonitorId>,
    reason: GameModeReason,
    actions: GameModeActions,
    fullscreen: FullscreenDetection,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct GameModeActions {
    pause_layouts: bool,
    pause_focused_monitor_only: bool,
    hide_borders: bool,
    disable_animations: bool,
    disable_keyboard_hooks: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct GameModeTransition {
    activated: bool,
    deactivated: bool,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveInteractiveDrag {
    window: WindowHandle,
    start_cursor: Point,
    start_rect: Rect,
    monitors: Vec<MonitorInfo>,
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
            overflow_promoted_floating: BTreeSet::new(),
            workspaces: WorkspaceManager::new(config.workspace_count),
            active_modifier_drag: None,
            active_interactive_drag: None,
            suppressed_modifier_drag_events: BTreeSet::new(),
            config,
            source_config: Config::default(),
            config_path: None,
            config_version: 1,
            config_loaded_at: UNIX_EPOCH,
            hotkey_commands,
            hotkey_backend: None,
            hotkey_sender: None,
            modifier_drag: None,
            mouse_drag_sender: None,
            border_manager: None,
            game_mode: GameModeRuntimeState::default(),
            perf: DaemonPerformance::default(),
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
        self.update_game_mode(&monitors, "startup retile")?;
        if self.game_mode_pauses_layouts() {
            info!("startup retile skipped because game mode is active");
            return Ok(());
        }
        let assignments = self.tile_assignments(&monitors);
        self.apply_tile_assignments_with_feedback(&assignments, &monitors, "startup retile");
        info!(
            move_count = assignments.len(),
            "completed startup retile request"
        );
        Ok(())
    }

    fn update_game_mode(
        &mut self,
        monitors: &[MonitorInfo],
        operation: &'static str,
    ) -> Result<GameModeTransition> {
        self.sync_workspace_state(monitors);
        let detection = self.current_game_mode_detection(monitors);
        let previous = self.game_mode.active.clone();
        let mut next = self.activation_from_detection(detection, monitors);
        let mut retained_fullscreen_misses = 0;
        if next.is_none()
            && let Some(previous_active) = previous.as_ref()
            && self.should_confirm_fullscreen_game_mode_deactivation(previous_active)
        {
            let misses = self
                .game_mode
                .fullscreen_deactivate_misses
                .saturating_add(1);
            if misses < GAME_MODE_FULLSCREEN_DEACTIVATE_CONFIRMATIONS {
                retained_fullscreen_misses = misses;
                next = Some(previous_active.clone());
                debug!(
                    focused = %previous_active.window,
                    misses,
                    required = GAME_MODE_FULLSCREEN_DEACTIVATE_CONFIRMATIONS,
                    operation,
                    "retained fullscreen game mode pending deactivation confirmation"
                );
            }
        }
        let transition = GameModeTransition {
            activated: previous.is_none() && next.is_some(),
            deactivated: previous.is_some() && next.is_none(),
        };

        if previous.as_ref() != next.as_ref() {
            match &next {
                Some(active) => {
                    info!(
                        focused = %active.window,
                        title = %active.title,
                        exe = %active.executable_path.as_deref().unwrap_or("-"),
                        monitor = ?active.monitor,
                        reason = %game_mode_reason_label(&active.reason),
                        pause_layouts = active.actions.pause_layouts,
                        pause_focused_monitor_only = active.actions.pause_focused_monitor_only,
                        layout_pause_scope = game_mode_layout_pause_scope_for(active),
                        hide_borders = active.actions.hide_borders,
                        disable_animations = active.actions.disable_animations,
                        disable_keyboard_hooks = active.actions.disable_keyboard_hooks,
                        operation,
                        "game mode activated"
                    );
                }
                None => {
                    if let Some(previous) = previous {
                        info!(
                            focused = %previous.window,
                            title = %previous.title,
                            exe = %previous.executable_path.as_deref().unwrap_or("-"),
                            monitor = ?previous.monitor,
                            reason = %game_mode_reason_label(&previous.reason),
                            operation,
                            "game mode deactivated"
                        );
                    }
                }
            }
        }

        self.game_mode.active = next;
        self.game_mode.fullscreen_deactivate_misses = if retained_fullscreen_misses > 0 {
            retained_fullscreen_misses
        } else {
            0
        };

        winland_win32::set_input_hooks_paused(
            self.game_mode
                .active
                .as_ref()
                .is_some_and(|active| active.actions.disable_keyboard_hooks),
        );

        if self.game_mode_hides_borders()
            && let Some(manager) = &self.border_manager
            && let Err(error) = manager.clear()
        {
            warn!(%error, operation, "failed to hide border overlays for game mode");
        }

        Ok(transition)
    }

    fn should_confirm_fullscreen_game_mode_deactivation(
        &self,
        active: &GameModeActivation,
    ) -> bool {
        self.foreground == Some(active.window)
            && matches!(active.reason, GameModeReason::Fullscreen { .. })
    }

    fn current_game_mode_detection(&self, monitors: &[MonitorInfo]) -> GameModeDetection {
        let focused_window = self.foreground.and_then(|handle| self.windows.get(&handle));
        detect_game_mode(
            focused_window,
            monitors,
            &self.config.window_rules,
            &self.config.game_mode.policy,
        )
    }

    fn activation_from_detection(
        &self,
        detection: GameModeDetection,
        monitors: &[MonitorInfo],
    ) -> Option<GameModeActivation> {
        if !detection.active {
            return None;
        }
        let window = self.foreground?;
        let info = self.windows.get(&window)?;
        let reason = detection.reason?;
        let monitor = detection
            .fullscreen
            .monitor
            .or_else(|| monitor_for_rect(info.rect, monitors));
        let actions = GameModeActions {
            pause_layouts: self.config.game_mode.pause_all_layouts_when_game_focused,
            pause_focused_monitor_only: self.config.game_mode.pause_focused_monitor_only,
            hide_borders: self.config.game_mode.disable_borders,
            disable_animations: self.config.game_mode.disable_animations,
            disable_keyboard_hooks: self.config.game_mode.disable_keyboard_hooks,
        };

        Some(GameModeActivation {
            window,
            title: info.title.clone(),
            executable_path: info.executable_path.clone(),
            monitor,
            reason,
            actions,
            fullscreen: detection.fullscreen,
        })
    }

    fn game_mode_pauses_layouts(&self) -> bool {
        self.game_mode.active.as_ref().is_some_and(|active| {
            active.actions.pause_layouts
                && (!active.actions.pause_focused_monitor_only || active.monitor.is_none())
        })
    }

    fn game_mode_pauses_monitor(&self, monitor: MonitorId) -> bool {
        self.game_mode.active.as_ref().is_some_and(|active| {
            active.actions.pause_layouts
                && active.actions.pause_focused_monitor_only
                && active.monitor == Some(monitor)
        })
    }

    fn game_mode_hides_borders(&self) -> bool {
        self.game_mode
            .active
            .as_ref()
            .is_some_and(|active| active.actions.hide_borders)
    }

    fn reconcile_after_events(&mut self, batch: &[WindowEvent]) -> Result<()> {
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
        let coalesced_batch = coalesce_window_events(&filtered_batch);
        if coalesced_batch.len() < filtered_batch.len() {
            debug!(
                original_event_count = filtered_batch.len(),
                coalesced_event_count = coalesced_batch.len(),
                "coalesced duplicate window events"
            );
        }
        let batch = coalesced_batch.as_slice();

        if self.reconcile_low_latency_window_events(batch)? {
            return Ok(());
        }

        let mut refreshed = Self::discover(self.config.clone(), self.hotkey_commands.clone())
            .context("refresh window snapshot after event batch")?;
        let monitors = winland_win32::enumerate_monitors()
            .context("enumerate monitors while preserving daemon state")?;
        let diff = self.diff(&refreshed, &monitors);
        self.preserve_keyboard_state(&mut refreshed, &monitors);
        refreshed.game_mode = self.game_mode.clone();
        refreshed.source_config = self.source_config.clone();
        refreshed.config_path = self.config_path.clone();
        refreshed.config_version = self.config_version;
        refreshed.config_loaded_at = self.config_loaded_at;
        let border_manager = self.border_manager.take();
        let hotkey_backend = self.hotkey_backend.take();
        let hotkey_sender = self.hotkey_sender.clone();
        let modifier_drag = self.modifier_drag.take();
        let mouse_drag_sender = self.mouse_drag_sender.clone();
        let perf = self.perf.clone();
        *self = refreshed;
        self.border_manager = border_manager;
        self.hotkey_backend = hotkey_backend;
        self.hotkey_sender = hotkey_sender;
        self.modifier_drag = modifier_drag;
        self.mouse_drag_sender = mouse_drag_sender;
        self.perf = perf;
        let transition = self.update_game_mode(&monitors, "event reconciliation")?;
        self.refresh_active_interactive_drag_rect_from_cursor(&monitors);
        let mut event_plan = self.plan_after_window_events(batch, &diff, &monitors);
        if transition.deactivated && self.config.dynamic_retile {
            event_plan.moves = self.tile_assignments(&monitors);
        }
        if self.game_mode_pauses_layouts() {
            event_plan.moves.clear();
        }
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

    fn handle_hotkey_event(&mut self, event: HotkeyEvent) {
        if let Err(error) = self.handle_hotkey(event) {
            warn!(%error, "daemon hotkey command failed");
        }
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

    fn should_process_moved_event_immediately(&self, event: WindowEvent) -> bool {
        event.kind == WindowEventKind::Moved
            && (self.active_interactive_drag_window() == Some(event.window)
                || self
                    .active_modifier_drag
                    .is_some_and(|drag| drag.window == event.window))
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
                let monitors = winland_win32::enumerate_monitors()
                    .context("enumerate monitors for foreground game-mode update")?;
                if let Some(window) = self.foreground {
                    self.focus_monitor_for_window(window, &monitors);
                }
                let transition = self.update_game_mode(&monitors, "foreground update")?;
                if transition.deactivated && self.config.dynamic_retile {
                    let assignments = self.tile_assignments(&monitors);
                    self.apply_tile_assignments_with_feedback(
                        &assignments,
                        &monitors,
                        "game mode exit retile",
                    );
                }
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
        let active_drag_move = handles
            .iter()
            .any(|window| self.active_interactive_drag_window() == Some(*window));

        let monitors = winland_win32::enumerate_monitors()
            .context("enumerate monitors for low-latency move event")?;
        let mut moved_between_monitors = false;
        let mut updated = 0usize;

        for window in handles {
            let Some(old_rect) = self.windows.get(&window).map(|info| info.rect) else {
                return Ok(false);
            };
            let old_monitor = monitor_for_rect(old_rect, &monitors);
            let rect = self
                .current_move_event_rect(window, active_drag_move)
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
                if let Some(monitor) = new_monitor {
                    self.window_monitor_overrides.insert(window, monitor);
                }
            }
        }

        let transition = self.update_game_mode(&monitors, "low-latency moved update")?;
        if transition.deactivated && self.config.dynamic_retile {
            let assignments = self.tile_assignments(&monitors);
            self.apply_tile_assignments_with_feedback(
                &assignments,
                &monitors,
                "game mode exit retile",
            );
        }

        if moved_between_monitors && self.config.dynamic_retile {
            if self.game_mode_pauses_layouts() {
                self.sync_drag_border_geometry_with_monitors(
                    &monitors,
                    active_drag_move,
                    "low-latency moved borders",
                )?;
                return Ok(true);
            }
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

        self.sync_drag_border_geometry_with_monitors(
            &monitors,
            active_drag_move,
            "low-latency moved borders",
        )?;
        Ok(true)
    }

    fn current_move_event_rect(
        &self,
        window: WindowHandle,
        active_drag_move: bool,
    ) -> winland_win32::Result<Rect> {
        if active_drag_move && self.active_interactive_drag_window() == Some(window) {
            if let Ok(cursor) = winland_win32::cursor_position()
                && let Some(rect) = self.interactive_drag_rect_from_cursor(window, cursor)
            {
                return Ok(rect);
            }

            if let Some(info) = self.windows.get(&window) {
                return Ok(info.rect);
            }
        }

        match winland_win32::window_rect_for_handle(window) {
            Ok(rect) => Ok(rect),
            Err(error)
                if active_drag_move && self.active_interactive_drag_window() == Some(window) =>
            {
                if let Some(info) = self.windows.get(&window) {
                    Ok(info.rect)
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    }

    fn handle_interactive_drag_tick(&mut self, window: WindowHandle, cursor: Point) -> Result<()> {
        let Some(monitors) = self.active_interactive_drag_monitors(window) else {
            return Ok(());
        };
        let Some(rect) = self.interactive_drag_rect_from_cursor(window, cursor) else {
            return Ok(());
        };
        if self.apply_interactive_drag_rect(window, rect, &monitors) {
            self.sync_border_geometry_with_monitors(&monitors, "native drag tracker borders")?;
        }
        Ok(())
    }

    fn apply_interactive_drag_rect(
        &mut self,
        window: WindowHandle,
        rect: Rect,
        monitors: &[MonitorInfo],
    ) -> bool {
        if self.active_interactive_drag_window() != Some(window)
            || !self.windows.contains_key(&window)
        {
            return false;
        }

        if let Some(info) = self.windows.get_mut(&window) {
            if info.rect == rect {
                return false;
            }
            info.rect = rect;
        }
        self.workspaces.update_window_rect(window, rect);
        if let Some(monitor) = monitor_for_rect(rect, monitors) {
            self.window_monitor_overrides.insert(window, monitor);
        }
        true
    }

    fn reconcile_movesize_event(&mut self, event: WindowEvent) -> Result<bool> {
        if !self.windows.contains_key(&event.window) {
            return Ok(false);
        }

        let monitors = winland_win32::enumerate_monitors()
            .context("enumerate monitors for low-latency movesize event")?;

        let transition = self.update_game_mode(&monitors, "low-latency movesize update")?;
        let sync_drag_start_borders = event.kind == WindowEventKind::MoveSizeStart;
        let plan = if sync_drag_start_borders {
            let plan = self.plan_after_window_events(&[event], &SnapshotDiff::default(), &monitors);
            self.refresh_movesize_event_rect(event.window, &monitors);
            self.sync_border_geometry_with_monitors(&monitors, "native drag start borders")?;
            plan
        } else {
            self.refresh_movesize_event_rect(event.window, &monitors);
            self.plan_after_window_events(&[event], &SnapshotDiff::default(), &monitors)
        };
        let moves = if transition.deactivated && self.config.dynamic_retile {
            self.tile_assignments(&monitors)
        } else if self.game_mode_pauses_layouts() {
            Vec::new()
        } else {
            plan.moves
        };
        self.apply_tile_assignments_with_feedback(&moves, &monitors, "low-latency movesize retile");
        self.sync_borders_with_monitors(&monitors, "low-latency movesize borders")?;
        debug!(
            kind = ?event.kind,
            window = %event.window,
            retile_moves = moves.len(),
            "handled movesize event without full snapshot rebuild"
        );
        Ok(true)
    }

    fn refresh_movesize_event_rect(&mut self, window: WindowHandle, monitors: &[MonitorInfo]) {
        if self.active_interactive_drag_window() == Some(window) {
            self.refresh_active_interactive_drag_rect_from_cursor(monitors);
            return;
        }

        if let Ok(rect) = winland_win32::window_rect_for_handle(window) {
            self.update_cached_window_rect(window, rect, monitors);
        }
    }

    fn update_cached_window_rect(
        &mut self,
        window: WindowHandle,
        rect: Rect,
        monitors: &[MonitorInfo],
    ) {
        if let Some(info) = self.windows.get_mut(&window) {
            info.rect = rect;
        }
        self.workspaces.update_window_rect(window, rect);
        if let Some(monitor) = monitor_for_rect(rect, monitors) {
            self.window_monitor_overrides.insert(window, monitor);
        }
    }

    fn refresh_active_interactive_drag_rect_from_cursor(
        &mut self,
        monitors: &[MonitorInfo],
    ) -> bool {
        let Some(window) = self.active_interactive_drag_window() else {
            return false;
        };
        let Some(rect) = winland_win32::cursor_position()
            .ok()
            .and_then(|cursor| self.interactive_drag_rect_from_cursor(window, cursor))
        else {
            return false;
        };
        self.apply_interactive_drag_rect(window, rect, monitors)
    }

    fn handle_mouse_drag(&mut self, event: MouseDragEvent) -> Result<()> {
        if self.game_mode_pauses_layouts() {
            debug!(
                window = %event.window,
                kind = ?event.kind,
                "ignored modifier drag event while game mode is active"
            );
            return Ok(());
        }

        match event.kind {
            MouseDragEventKind::Started => self.start_modifier_drag(event),
            MouseDragEventKind::Moved => self.move_modifier_drag(event),
            MouseDragEventKind::Ended => self.end_modifier_drag(event),
            MouseDragEventKind::TitlebarStarted => self.start_titlebar_drag(event),
            MouseDragEventKind::TitlebarMoved => {
                self.handle_interactive_drag_tick(event.window, event.cursor)
            }
            MouseDragEventKind::TitlebarEnded => {
                self.finish_interactive_drag(event.window);
                Ok(())
            }
        }
    }

    fn start_titlebar_drag(&mut self, event: MouseDragEvent) -> Result<()> {
        if self.active_modifier_drag.is_some()
            || self.active_interactive_drag_window() == Some(event.window)
        {
            return Ok(());
        }

        let monitors = winland_win32::enumerate_monitors()
            .context("enumerate monitors before native titlebar drag start")?;
        self.start_interactive_drag_with_cursor(event.window, event.cursor, &monitors);
        if self.active_interactive_drag_window() == Some(event.window) {
            if let Some(drag) = self.active_interactive_drag.as_ref() {
                let (title, class_name) = self
                    .windows
                    .get(&event.window)
                    .map(|window| (window.title.as_str(), window.class_name.as_str()))
                    .unwrap_or(("", ""));
                info!(
                    window = %event.window,
                    title,
                    class_name,
                    start_cursor_x = drag.start_cursor.x,
                    start_cursor_y = drag.start_cursor.y,
                    start_rect = %drag.start_rect,
                    "started native titlebar drag"
                );
            }
            self.sync_border_geometry_with_monitors(&monitors, "native titlebar down borders")?;
        }
        Ok(())
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
        let monitors = winland_win32::enumerate_monitors()
            .context("enumerate monitors before modifier drag start")?;
        let started_temporary_float = self.handle_movesize_start(event.window, &monitors);

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

        if drag.started_temporary_float || self.should_try_overflow_float_drop(event.window) {
            let monitors = winland_win32::enumerate_monitors()
                .context("enumerate monitors after modifier drag end")?;
            let drop_context = DropContext {
                rect: dropped_rect,
                cursor: Some(event.cursor),
            };
            if self.handle_movesize_end(event.window, &monitors, Some(drop_context)) {
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
        self.finish_interactive_drag(event.window);
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
        if let Ok(monitors) = winland_win32::enumerate_monitors()
            && let Some(monitor) = monitor_for_rect(accepted_rect, &monitors)
        {
            self.window_monitor_overrides.insert(window, monitor);
        }
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

    fn handle_ipc_request(&mut self, request: IpcRequest) -> IpcResponse {
        match request.command {
            IpcCommand::State => IpcResponse::state(self.state_snapshot()),
            IpcCommand::ReloadConfig => match self.reload_config("ipc reload-config") {
                Ok(report) => IpcResponse::reload_config(report),
                Err(error) => {
                    warn!(%error, "config reload failed");
                    IpcResponse::error(error.to_string())
                }
            },
        }
    }

    fn state_snapshot(&self) -> DaemonStateSnapshot {
        let monitors = winland_win32::enumerate_monitors().unwrap_or_default();
        self.state_snapshot_with_monitors(&monitors)
    }

    fn state_snapshot_with_monitors(&self, monitors: &[MonitorInfo]) -> DaemonStateSnapshot {
        let manageable_windows = self.manageable_window_count();
        DaemonStateSnapshot {
            config_path: self
                .config_path
                .as_ref()
                .map(|path| path.display().to_string()),
            config_version: self.config_version,
            config_loaded_at_unix_ms: system_time_unix_ms(self.config_loaded_at),
            total_windows: self.windows.len(),
            manageable_windows,
            floating_windows: self.floating_window_count(),
            temporary_floating_windows: self.temporary_floating_window_count(),
            active_workspace: self.workspaces.active_workspace().0,
            foreground_window: self.foreground.map(|handle| handle.0),
            monitors: self.monitor_snapshots(monitors),
            windows: self.window_snapshots(monitors),
            performance: self
                .perf
                .snapshot(manageable_windows, self.game_mode.active.is_some()),
        }
    }

    fn monitor_snapshots(&self, monitors: &[MonitorInfo]) -> Vec<MonitorStateSnapshot> {
        monitors
            .iter()
            .map(|monitor| MonitorStateSnapshot {
                monitor_id: monitor.id.0,
                workspace_id: self.workspaces.active_workspace_for_monitor(monitor.id).0,
                focused: self.workspaces.focused_monitor() == Some(monitor.id),
            })
            .collect()
    }

    fn window_snapshots(&self, monitors: &[MonitorInfo]) -> Vec<WindowStateSnapshot> {
        self.windows
            .iter()
            .map(|(handle, window)| {
                let monitor = self.window_owner_monitor(*handle, monitors);
                let workspace = self
                    .workspaces
                    .window_state(*handle)
                    .map(|state| state.workspace);
                WindowStateSnapshot {
                    handle: handle.0,
                    title: window.title.clone(),
                    monitor_id: monitor.map(|monitor| monitor.0),
                    workspace_id: workspace.map(|workspace| workspace.0),
                    focused: self.foreground == Some(*handle),
                    participation: self.window_snapshot_participation(*handle),
                    constrained: !merge_size_constraints(
                        window.size_constraints,
                        self.learned_size_constraints
                            .get(handle)
                            .copied()
                            .unwrap_or_default(),
                    )
                    .is_unconstrained(),
                    visible_on_active_workspace: self
                        .is_window_visible_on_owned_monitor(*handle, monitors),
                }
            })
            .collect()
    }

    fn window_snapshot_participation(&self, window: WindowHandle) -> WindowParticipationSnapshot {
        match self.window_participation(window) {
            WindowParticipation::Tiled => WindowParticipationSnapshot::Tiled,
            WindowParticipation::Floating => WindowParticipationSnapshot::Floating,
            WindowParticipation::TemporarilyFloating => {
                WindowParticipationSnapshot::TemporarilyFloating
            }
        }
    }

    fn reload_config(&mut self, operation: &'static str) -> Result<ReloadConfigReport> {
        let loaded_config =
            winland_config::load_or_default(None).context("reload Winland config from disk")?;
        loaded_config
            .config
            .validate()
            .context("validate reloaded Winland config")?;
        let diff = ConfigDiff::between(&self.source_config, &loaded_config.config);
        self.perf.config_reload_count = self.perf.config_reload_count.saturating_add(1);

        if diff.is_noop() {
            let monitors = winland_win32::enumerate_monitors()
                .context("enumerate monitors for unchanged config reload report")?;
            info!("config reload skipped; config is unchanged");
            return Ok(ReloadConfigReport {
                config_path: self
                    .config_path
                    .as_ref()
                    .map(|path| path.display().to_string()),
                config_version: self.config_version,
                reloaded_at_unix_ms: system_time_unix_ms(SystemTime::now()),
                changed_sections: diff.changed_sections,
                state: self.state_snapshot_with_monitors(&monitors),
            });
        }
        let runtime_config =
            RuntimeConfig::from_config(&loaded_config.config).context("prepare runtime config")?;
        let (new_hotkey_bindings, new_hotkey_commands) =
            hotkey_bindings_from_config(&loaded_config.config)
                .context("prepare hotkey bindings for reloaded config")?;

        let mut refreshed = Self::discover(runtime_config, new_hotkey_commands.clone())
            .context("build reloaded daemon window snapshot")?;
        let monitors = winland_win32::enumerate_monitors()
            .context("enumerate monitors while reloading daemon state")?;
        self.preserve_keyboard_state(&mut refreshed, &monitors);
        refreshed.dwindle_splits.clear();
        refreshed
            .workspaces
            .set_workspace_count(refreshed.config.workspace_count);
        refreshed.sync_workspace_state(&monitors);

        let previous_visibility = self.window_visibility_map(&monitors);

        self.replace_hotkey_backend_for_reload(&loaded_config.config, new_hotkey_bindings)
            .context("activate reloaded hotkeys")?;
        if let Err(error) = self.replace_modifier_drag_for_reload(&loaded_config.config) {
            let old_config = self.source_config.clone();
            if let Ok((old_bindings, _)) = hotkey_bindings_from_config(&old_config)
                && let Err(restore_error) =
                    self.replace_hotkey_backend_for_reload(&old_config, old_bindings)
            {
                warn!(
                    %restore_error,
                    "failed to restore previous hotkeys after modifier-drag reload failure"
                );
            }
            return Err(error).context("activate reloaded modifier-drag settings");
        }

        let border_manager = self.border_manager.take();
        let hotkey_backend = self.hotkey_backend.take();
        let hotkey_sender = self.hotkey_sender.clone();
        let modifier_drag = self.modifier_drag.take();
        let mouse_drag_sender = self.mouse_drag_sender.clone();
        let perf = self.perf.clone();

        refreshed.source_config = loaded_config.config.clone();
        refreshed.config_path = loaded_config.path.clone();
        refreshed.config_version = self.config_version.saturating_add(1);
        refreshed.config_loaded_at = SystemTime::now();
        refreshed.border_manager = border_manager;
        refreshed.hotkey_backend = hotkey_backend;
        refreshed.hotkey_sender = hotkey_sender;
        refreshed.modifier_drag = modifier_drag;
        refreshed.mouse_drag_sender = mouse_drag_sender;
        refreshed.game_mode = self.game_mode.clone();
        refreshed.perf = perf;

        *self = refreshed;
        let rule_stats = self.reapply_window_rules_after_reload(&monitors);
        let visibility_changes =
            self.visibility_changes_after_reload(previous_visibility, &monitors);
        let transition = self.update_game_mode(&monitors, operation)?;

        for window in &visibility_changes.hide {
            if let Err(error) = winland_win32::hide_window(*window) {
                warn!(window = %window, %error, "failed to hide window after config reload");
            }
        }
        for change in &visibility_changes.show {
            if let Some(rect) = change.restore_rect
                && let Err(error) = winland_win32::move_resize_window(change.window, rect)
            {
                warn!(
                    window = %change.window,
                    rect = %rect,
                    %error,
                    "failed to restore window placement after config reload"
                );
            }
            if let Err(error) = winland_win32::show_window_without_activate(change.window) {
                warn!(window = %change.window, %error, "failed to show window after config reload");
            }
        }

        if !self.game_mode_pauses_layouts() {
            let assignments = self.tile_assignments(&monitors);
            self.apply_tile_assignments_with_feedback(&assignments, &monitors, operation);
        }
        if let Err(error) = self.sync_borders_with_monitors(&monitors, "reload-config borders") {
            warn!(%error, "config reload applied but border sync failed");
        }

        self.log_snapshot("reloaded daemon config");
        info!(
            version = self.config_version,
            path = self
                .config_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<defaults>".to_owned()),
            changed_sections = ?diff.changed_sections,
            rules_added = rule_stats.added_to_tile_order,
            rules_removed = rule_stats.removed_from_tile_order,
            rules_floated = rule_stats.set_floating,
            rules_tiled = rule_stats.set_tiled,
            hidden = visibility_changes.hide.len(),
            shown = visibility_changes.show.len(),
            game_mode_activated = transition.activated,
            game_mode_deactivated = transition.deactivated,
            "config reload applied"
        );

        Ok(ReloadConfigReport {
            config_path: self
                .config_path
                .as_ref()
                .map(|path| path.display().to_string()),
            config_version: self.config_version,
            reloaded_at_unix_ms: system_time_unix_ms(self.config_loaded_at),
            changed_sections: diff.changed_sections,
            state: self.state_snapshot_with_monitors(&monitors),
        })
    }

    fn execute_command(&mut self, command: DaemonCommand) -> Result<()> {
        if let DaemonCommand::Launch(command_line) = &command {
            winland_win32::launch_app(command_line)
                .with_context(|| format!("launch app from hotkey '{command_line}'"))?;
            return Ok(());
        }

        if command == DaemonCommand::Reload {
            self.reload_config("hotkey reload")?;
            return Ok(());
        }

        if self.game_mode_pauses_layouts() && command.is_suppressed_by_game_mode() {
            info!(?command, "ignored daemon command while game mode is active");
            return Ok(());
        }

        let monitors = if command.needs_monitors() {
            winland_win32::enumerate_monitors().context("enumerate monitors for daemon command")?
        } else {
            Vec::new()
        };
        let plan = self.plan_command(command, &monitors);

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

        if let Some(target) = plan.focus
            && let Err(error) = winland_win32::focus_window(target)
        {
            warn!(window = %target, %error, "failed to focus window from hotkey command");
        }

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
                let focus = self.focus_target(direction, monitors);
                if let Some(target) = focus {
                    self.foreground = Some(target);
                    self.focus_monitor_for_window(target, monitors);
                }

                CommandPlan {
                    focus,
                    ..CommandPlan::default()
                }
            }
            DaemonCommand::FocusMonitor(selector) => {
                let focus = self.focus_monitor_command(selector, monitors);
                CommandPlan {
                    focus,
                    ..CommandPlan::default()
                }
            }
            DaemonCommand::Swap(direction) => {
                self.swap_focused_with(direction, monitors);
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
                self.toggle_focused_float(monitors);
                CommandPlan {
                    moves: self.tile_assignments(monitors),
                    ..CommandPlan::default()
                }
            }
            DaemonCommand::SwitchWorkspace(workspace) => {
                let workspace_plan = self.switch_workspace(workspace, monitors);
                let focus = workspace_plan
                    .focus
                    .or_else(|| self.focus_candidate_for_focused_monitor(monitors));
                self.foreground = focus;
                CommandPlan {
                    focus,
                    hide: workspace_plan.hide,
                    show: workspace_plan.show,
                    moves: self.tile_assignments(monitors),
                    ..CommandPlan::default()
                }
            }
            DaemonCommand::SwitchWorkspaceRelative(direction) => {
                self.sync_workspace_state(monitors);
                let reference_monitor = self
                    .active_drag_window()
                    .filter(|window| self.is_command_movable_window(*window, monitors))
                    .and_then(|window| self.window_owner_monitor(window, monitors))
                    .or_else(|| self.command_monitor(monitors));
                let workspace = self.relative_workspace_for_monitor(direction, reference_monitor);
                let workspace_plan = self.switch_workspace(workspace, monitors);
                let focus = workspace_plan
                    .focus
                    .or_else(|| self.focus_candidate_for_focused_monitor(monitors));
                self.foreground = focus;
                CommandPlan {
                    focus,
                    hide: workspace_plan.hide,
                    show: workspace_plan.show,
                    moves: self.tile_assignments(monitors),
                    ..CommandPlan::default()
                }
            }
            DaemonCommand::MoveFocusedToWorkspace(workspace) => {
                self.move_focused_to_workspace(workspace, false, monitors)
            }
            DaemonCommand::MoveFocusedToWorkspaceAndFollow(workspace) => {
                self.move_focused_to_workspace(workspace, true, monitors)
            }
            DaemonCommand::MoveFocusedToMonitor(selector) => {
                self.move_focused_to_monitor(selector, monitors)
            }
            DaemonCommand::SendWorkspaceToMonitor { workspace, monitor } => {
                self.send_workspace_to_monitor(workspace, monitor, monitors)
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

    fn focus_target(
        &self,
        direction: FocusDirection,
        monitors: &[MonitorInfo],
    ) -> Option<WindowHandle> {
        let current = self
            .foreground
            .filter(|handle| self.is_manageable_window(*handle, monitors))
            .or_else(|| self.focusable_handles(monitors).into_iter().next())?;

        let current_center = self.windows.get(&current)?.rect.center();
        self.focusable_handles(monitors)
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
            .or_else(|| self.wrapping_focus_target(current, direction, monitors))
    }

    fn wrapping_focus_target(
        &self,
        current: WindowHandle,
        direction: FocusDirection,
        monitors: &[MonitorInfo],
    ) -> Option<WindowHandle> {
        let handles = self.focusable_handles(monitors);
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

    fn swap_focused_with(&mut self, direction: FocusDirection, monitors: &[MonitorInfo]) {
        let Some(current) = self
            .foreground
            .filter(|handle| self.is_manageable_window(*handle, monitors))
        else {
            return;
        };
        let Some(target) = self.focus_target(direction, monitors) else {
            return;
        };

        let current_index = self.tile_order.iter().position(|handle| *handle == current);
        let target_index = self.tile_order.iter().position(|handle| *handle == target);
        if let (Some(current_index), Some(target_index)) = (current_index, target_index) {
            self.tile_order.swap(current_index, target_index);
        }
    }

    fn toggle_focused_float(&mut self, monitors: &[MonitorInfo]) {
        let Some(current) = self
            .foreground
            .filter(|handle| self.is_manageable_window(*handle, monitors))
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

        for event in batch {
            match event.kind {
                WindowEventKind::MoveSizeStart => {
                    if self.handle_movesize_start(event.window, monitors) {
                        should_retile = should_retile || self.config.dynamic_retile;
                    }
                }
                WindowEventKind::MoveSizeEnd => {
                    if self.handle_movesize_end(event.window, monitors, None) {
                        should_retile = true;
                    }
                }
                _ => {}
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

    fn handle_movesize_start(&mut self, window: WindowHandle, monitors: &[MonitorInfo]) -> bool {
        self.forget_learned_size_constraint(window);
        self.start_interactive_drag(window, monitors);
        self.config.drag_to_float && self.start_temporary_float(window, monitors)
    }

    fn handle_movesize_end(
        &mut self,
        window: WindowHandle,
        monitors: &[MonitorInfo],
        drop_context: Option<DropContext>,
    ) -> bool {
        self.finish_interactive_drag(window);

        let mut temporary_float_cleared = false;
        if self.window_participation(window) == WindowParticipation::TemporarilyFloating {
            if let Some(drop_context) = drop_context {
                self.reorder_window_by_drop_at(
                    window,
                    monitors,
                    drop_context.rect,
                    drop_context.cursor,
                );
            } else {
                self.reorder_window_by_drop(window, monitors);
            }
            temporary_float_cleared = self.clear_temporary_float(window);
        }

        let overflow_float_reabsorbed =
            self.reabsorb_overflow_float_by_drop(window, monitors, drop_context);

        overflow_float_reabsorbed || temporary_float_cleared && self.config.retile_on_drag_end
    }

    fn should_try_overflow_float_drop(&self, window: WindowHandle) -> bool {
        self.config.overflow_float_persistence == OverflowFloatPersistence::RetileOnDragEnd
            && self.overflow_promoted_floating.contains(&window)
            && self.window_participation(window) == WindowParticipation::Floating
    }

    fn reabsorb_overflow_float_by_drop(
        &mut self,
        window: WindowHandle,
        monitors: &[MonitorInfo],
        drop_context: Option<DropContext>,
    ) -> bool {
        if !self.should_try_overflow_float_drop(window) {
            return false;
        }

        let Some(drop_context) = drop_context.or_else(|| {
            self.windows.get(&window).map(|window| DropContext {
                rect: window.rect,
                cursor: None,
            })
        }) else {
            return false;
        };

        if !self.overflow_float_drop_would_fit(
            window,
            monitors,
            drop_context.rect,
            drop_context.cursor,
        ) {
            debug!(
                window = %window,
                "kept overflow-promoted floating window floating after drop because layout still would not fit"
            );
            return false;
        }

        self.set_window_participation(window, WindowParticipation::Tiled);
        self.reorder_window_by_drop_at(window, monitors, drop_context.rect, drop_context.cursor);
        info!(
            window = %window,
            "reabsorbed overflow-promoted floating window after drop"
        );
        true
    }

    fn overflow_float_drop_would_fit(
        &self,
        window: WindowHandle,
        monitors: &[MonitorInfo],
        dropped_rect: Rect,
        cursor_position: Option<Point>,
    ) -> bool {
        let Some(monitor) = self.drop_target_monitor(dropped_rect, cursor_position, monitors)
        else {
            return false;
        };
        let mut handles = self.drop_target_handles(window, monitor, monitors);
        let layout = self.config.layout_for_monitor(
            monitor,
            self.workspaces.active_workspace_for_monitor(monitor.id),
        );

        if layout.kind == LayoutKind::Dwindle && !handles.is_empty() {
            return self.dwindle_drop_would_fit(
                window,
                dropped_rect,
                cursor_position,
                &handles,
                monitor,
                layout,
            );
        }

        let drop_point = cursor_position.unwrap_or_else(|| dropped_rect.center());
        let local_index = self.drop_insert_index_at_point(window, drop_point, &handles, monitor);
        handles.insert(local_index, window);
        let assignments = self.preview_tile_assignments(monitor, &handles, layout, cursor_position);
        tile_assignments_fit_work_area(monitor.work_area, &assignments)
    }

    fn dwindle_drop_would_fit(
        &self,
        window: WindowHandle,
        dropped_rect: Rect,
        cursor_position: Option<Point>,
        target_handles: &[WindowHandle],
        monitor: &MonitorInfo,
        layout: LayoutConfig,
    ) -> bool {
        let drop_point = cursor_position.unwrap_or_else(|| dropped_rect.center());
        let workspace = self.workspaces.active_workspace_for_monitor(monitor.id);
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
        let mut handles = target_handles.to_vec();
        let insert_at = handles
            .iter()
            .position(|handle| *handle == target_assignment.window)
            .map(|index| index + 1)
            .unwrap_or(handles.len());
        handles.insert(insert_at, window);
        preview_splits.push(DwindleSplit {
            target: target_assignment.window,
            new_window: window,
            direction,
        });
        let layout_windows = self.layout_windows_for_handles(&handles);
        let assignments = tile_layout_windows_with_state(
            monitor.work_area,
            &layout_windows,
            layout,
            None,
            Some(&mut preview_splits),
        );
        tile_assignments_fit_work_area(monitor.work_area, &assignments)
    }

    fn start_interactive_drag_from_moved_events(
        &mut self,
        batch: &[WindowEvent],
    ) -> Result<Option<Vec<MonitorInfo>>> {
        if self.active_interactive_drag_window().is_some()
            || self.active_modifier_drag.is_some()
            || !winland_win32::left_mouse_button_is_down()
            || !batch
                .iter()
                .all(|event| event.kind == WindowEventKind::Moved)
        {
            return Ok(None);
        }

        let Some(window) = self.inferred_native_drag_window(batch) else {
            return Ok(None);
        };
        let monitors = winland_win32::enumerate_monitors()
            .context("enumerate monitors for inferred native drag start")?;
        self.start_interactive_drag(window, &monitors);
        Ok((self.active_interactive_drag_window() == Some(window)).then_some(monitors))
    }

    fn inferred_native_drag_window(&self, batch: &[WindowEvent]) -> Option<WindowHandle> {
        if let Some(foreground) = self.foreground
            && batch
                .iter()
                .any(|event| event.window == foreground && self.windows.contains_key(&foreground))
        {
            return Some(foreground);
        }

        batch.iter().find_map(|event| {
            self.windows
                .contains_key(&event.window)
                .then_some(event.window)
        })
    }

    fn start_interactive_drag(&mut self, window: WindowHandle, monitors: &[MonitorInfo]) {
        let cursor = winland_win32::cursor_position().ok();
        self.start_interactive_drag_with_optional_cursor(window, cursor, monitors);
    }

    fn start_interactive_drag_with_cursor(
        &mut self,
        window: WindowHandle,
        cursor: Point,
        monitors: &[MonitorInfo],
    ) {
        self.start_interactive_drag_with_optional_cursor(window, Some(cursor), monitors);
    }

    fn start_interactive_drag_with_optional_cursor(
        &mut self,
        window: WindowHandle,
        start_cursor: Option<Point>,
        monitors: &[MonitorInfo],
    ) {
        if self.active_interactive_drag_window() == Some(window) {
            return;
        }

        if !self.is_command_movable_window(window, monitors) {
            return;
        }

        let Some(cached_rect) = self.windows.get(&window).map(|info| info.rect) else {
            return;
        };
        let visible_rect = winland_win32::window_rect_for_handle(window).unwrap_or_else(|error| {
            debug!(
                window = %window,
                %error,
                cached_rect = %cached_rect,
                "failed to read native drag start rect; using cached visible rect"
            );
            cached_rect
        });
        let start_cursor = start_cursor.unwrap_or_else(|| visible_rect.center());
        self.update_cached_window_rect(window, visible_rect, monitors);
        self.active_interactive_drag = Some(ActiveInteractiveDrag {
            window,
            start_cursor,
            start_rect: visible_rect,
            monitors: monitors.to_vec(),
        });
    }

    fn finish_interactive_drag(&mut self, window: WindowHandle) {
        if self.active_interactive_drag_window() == Some(window) {
            self.active_interactive_drag = None;
        }
    }

    fn active_interactive_drag_window(&self) -> Option<WindowHandle> {
        self.active_interactive_drag
            .as_ref()
            .map(|drag| drag.window)
    }

    fn active_interactive_drag_monitors(&self, window: WindowHandle) -> Option<Vec<MonitorInfo>> {
        self.active_interactive_drag
            .as_ref()
            .filter(|drag| drag.window == window)
            .map(|drag| drag.monitors.clone())
    }

    fn interactive_drag_rect_from_cursor(
        &self,
        window: WindowHandle,
        cursor: Point,
    ) -> Option<Rect> {
        let drag = self.active_interactive_drag.as_ref()?;
        if drag.window != window {
            return None;
        }

        Some(offset_rect_by_cursor_delta(
            drag.start_rect,
            drag.start_cursor,
            cursor,
        ))
    }

    fn active_drag_window(&self) -> Option<WindowHandle> {
        self.active_modifier_drag
            .map(|drag| drag.window)
            .or_else(|| self.active_interactive_drag_window())
    }

    fn start_temporary_float(&mut self, window: WindowHandle, monitors: &[MonitorInfo]) -> bool {
        if self.window_participation(window) == WindowParticipation::Floating
            || !self
                .window_owner_monitor(window, monitors)
                .is_some_and(|monitor| self.is_tilable_window_on_monitor(window, monitor))
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

    fn reorder_window_by_drop(&mut self, window: WindowHandle, monitors: &[MonitorInfo]) -> bool {
        let Some(window_info) = self.windows.get(&window) else {
            return false;
        };
        self.reorder_window_by_drop_at(window, monitors, window_info.rect, None)
    }

    fn reorder_window_by_drop_at(
        &mut self,
        window: WindowHandle,
        monitors: &[MonitorInfo],
        dropped_rect: Rect,
        cursor_position: Option<Point>,
    ) -> bool {
        let Some(monitor) = self.drop_target_monitor(dropped_rect, cursor_position, monitors)
        else {
            return false;
        };
        let target_monitor = monitor.id;
        self.window_monitor_overrides.insert(window, target_monitor);

        let target_handles = self.drop_target_handles(window, monitor, monitors);
        let layout = self.config.layout_for_monitor(
            monitor,
            self.workspaces.active_workspace_for_monitor(monitor.id),
        );

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

    fn drop_target_monitor<'a>(
        &self,
        dropped_rect: Rect,
        cursor_position: Option<Point>,
        monitors: &'a [MonitorInfo],
    ) -> Option<&'a MonitorInfo> {
        let target_monitor = cursor_position
            .and_then(|point| monitor_for_point(point, monitors))
            .or_else(|| monitor_for_rect(dropped_rect, monitors))?;
        monitors.iter().find(|monitor| monitor.id == target_monitor)
    }

    fn drop_target_handles(
        &self,
        window: WindowHandle,
        monitor: &MonitorInfo,
        monitors: &[MonitorInfo],
    ) -> Vec<WindowHandle> {
        self.tile_order
            .iter()
            .copied()
            .filter(|handle| *handle != window)
            .filter(|handle| self.window_participation(*handle).is_tiled())
            .filter(|handle| {
                self.windows.get(handle).is_some_and(|candidate| {
                    self.is_tilable_window_on_monitor(*handle, monitor.id)
                        && self.monitor_owns_window_rect(
                            *handle,
                            monitor,
                            self.window_layout_rect(*handle, candidate),
                            monitors,
                        )
                })
            })
            .collect()
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
        let workspace = self.workspaces.active_workspace_for_monitor(monitor.id);
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
                let layout = self.config.layout_for_monitor(
                    monitor,
                    self.workspaces.active_workspace_for_monitor(monitor.id),
                );
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
        let drag_monitor = self
            .active_drag_window()
            .filter(|window| self.is_command_movable_window(*window, monitors))
            .and_then(|window| self.window_owner_monitor(window, monitors));
        let monitor = drag_monitor
            .or_else(|| self.command_monitor(monitors))
            .or_else(|| monitors.first().map(|monitor| monitor.id));
        let dragged_window = self.workspace_switch_drag_window(monitor, monitors);
        let dragged_window_moved = dragged_window.and_then(|window| {
            if let Some(info) = self.windows.get(&window) {
                self.workspaces.update_window_rect(window, info.rect);
            }

            self.workspaces
                .move_window_to_workspace(window, workspace)
                .then_some(window)
        });

        let mut plan = match monitor {
            Some(monitor) => self.workspaces.switch_monitor_to(monitor, workspace),
            None => self.workspaces.switch_to(workspace),
        };
        plan.hide.retain(|window| {
            dragged_window_moved != Some(*window)
                && self.should_hide_for_workspace(*window, monitors)
                && monitor.is_none_or(|monitor| {
                    self.window_belongs_to_monitor(*window, monitor, monitors)
                })
        });

        let mut show: Vec<_> = plan
            .show
            .into_iter()
            .filter(|change| {
                monitor.is_none_or(|monitor| {
                    self.window_belongs_to_monitor(change.window, monitor, monitors)
                })
            })
            .collect();
        if let Some(window) = dragged_window_moved
            && !show.iter().any(|change| change.window == window)
            && monitor
                .is_none_or(|monitor| self.window_belongs_to_monitor(window, monitor, monitors))
        {
            show.push(WorkspaceVisibilityChange {
                window,
                restore_rect: self
                    .workspaces
                    .window_state(window)
                    .and_then(|state| state.last_rect),
            });
        }

        if self
            .foreground
            .is_some_and(|window| !self.is_window_visible_on_owned_monitor(window, monitors))
        {
            self.foreground = None;
        }

        info!(
            workspace = %workspace,
            monitor = ?monitor,
            dragged_window = ?dragged_window_moved,
            hide_count = plan.hide.len(),
            show_count = show.len(),
            "switched workspace"
        );

        WorkspaceCommandPlan {
            focus: dragged_window_moved,
            hide: plan.hide,
            show,
        }
    }

    fn workspace_switch_drag_window(
        &self,
        target_monitor: Option<MonitorId>,
        monitors: &[MonitorInfo],
    ) -> Option<WindowHandle> {
        let window = self.active_drag_window()?;
        if !self.is_command_movable_window(window, monitors) {
            return None;
        }

        if let Some(monitor) = target_monitor
            && !self.window_belongs_to_monitor(window, monitor, monitors)
        {
            return None;
        }

        Some(window)
    }

    fn move_focused_to_workspace(
        &mut self,
        workspace: WorkspaceId,
        follow: bool,
        monitors: &[MonitorInfo],
    ) -> CommandPlan {
        let Some(current) = self
            .foreground
            .filter(|handle| self.is_command_movable_window(*handle, monitors))
        else {
            return CommandPlan::default();
        };

        self.sync_workspace_state(monitors);
        if let Some(window) = self.windows.get(&current) {
            self.workspaces.update_window_rect(current, window.rect);
        }

        let owner_monitor = self
            .window_owner_monitor(current, monitors)
            .or_else(|| self.command_monitor(monitors));
        let was_visible = self.is_window_visible_on_owned_monitor(current, monitors);
        if !self.workspaces.move_window_to_workspace(current, workspace) {
            return CommandPlan::default();
        }

        info!(
            window = %current,
            workspace = %workspace,
            follow,
            monitor = ?owner_monitor,
            participation = ?self.window_participation(current),
            "moved window to workspace"
        );

        if follow {
            let workspace_plan = if let Some(monitor) = owner_monitor {
                self.workspaces.switch_monitor_to(monitor, workspace)
            } else {
                self.workspaces.switch_to(workspace)
            };
            let hide = workspace_plan
                .hide
                .into_iter()
                .filter(|window| *window != current)
                .filter(|window| {
                    self.should_hide_for_workspace(*window, monitors)
                        && owner_monitor.is_none_or(|monitor| {
                            self.window_belongs_to_monitor(*window, monitor, monitors)
                        })
                })
                .collect();
            let show = workspace_plan
                .show
                .into_iter()
                .filter(|change| {
                    owner_monitor.is_none_or(|monitor| {
                        self.window_belongs_to_monitor(change.window, monitor, monitors)
                    })
                })
                .collect();
            self.foreground = Some(current);
            return CommandPlan {
                focus: Some(current),
                hide,
                show,
                moves: self.tile_assignments(monitors),
                ..CommandPlan::default()
            };
        }

        let mut hide = Vec::new();
        if was_visible && !self.is_window_visible_on_owned_monitor(current, monitors) {
            self.foreground = None;
            if self.should_hide_for_workspace(current, monitors) {
                hide.push(current);
            }
        }

        let focus =
            owner_monitor.and_then(|monitor| self.focus_candidate_for_monitor(monitor, monitors));
        self.foreground = focus;

        CommandPlan {
            hide,
            focus,
            moves: self.tile_assignments(monitors),
            ..CommandPlan::default()
        }
    }

    fn tile_assignments(&mut self, monitors: &[MonitorInfo]) -> Vec<TileAssignment> {
        if self.game_mode_pauses_layouts() {
            return Vec::new();
        }

        let cursor_position = winland_win32::cursor_position().ok();
        let mut assignments = Vec::new();
        let mut promoted_overflow_windows = 0usize;

        for monitor in monitors {
            let active_workspace = self.workspaces.active_workspace_for_monitor(monitor.id);
            if self.game_mode_pauses_monitor(monitor.id) {
                debug!(
                    monitor = %monitor.id,
                    "skipped tile assignments for game-mode paused monitor"
                );
                continue;
            }

            let handles = self.tiled_handles_for_monitor(monitor, monitors);
            let layout = self.config.layout_for_monitor(monitor, active_workspace);
            let overflow_plan =
                self.resolve_overflow_for_monitor(monitor, &handles, layout, cursor_position);
            for window in &overflow_plan.overflow_windows {
                if self.window_participation(*window).is_tiled() {
                    self.promote_overflow_window_to_floating(*window);
                    promoted_overflow_windows += 1;
                }
            }
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

        if promoted_overflow_windows > 0 {
            debug!(
                promoted_overflow_windows,
                "promoted overflow windows to persistent floating"
            );
        }

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
                .get(&(
                    self.workspaces.active_workspace_for_monitor(monitor.id),
                    monitor.id,
                ))
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
        let started = Instant::now();
        let mut assignments = assignments.to_vec();
        let mut had_assignments = false;
        let mut applied_count = 0usize;
        let mut skipped_count = 0usize;

        for pass in 0..TILE_FEEDBACK_PASSES {
            if assignments.is_empty() {
                break;
            }
            had_assignments = true;

            let actionable = self.actionable_tile_assignments(&assignments, monitors);
            skipped_count =
                skipped_count.saturating_add(assignments.len().saturating_sub(actionable.len()));
            if actionable.is_empty() {
                break;
            }

            apply_tile_assignments_once(&actionable, operation);
            applied_count = applied_count.saturating_add(actionable.len());

            if !self.learn_constraints_from_actual_rects(&actionable, operation) {
                break;
            }

            if pass + 1 < TILE_FEEDBACK_PASSES {
                assignments = self.tile_assignments(monitors);
            }
        }

        self.record_relayout_result(
            had_assignments,
            applied_count,
            skipped_count,
            started.elapsed(),
            operation,
        );
        // Overflow resolution can make an existing tiled window floating without
        // otherwise touching its HWND, so promote floating windows after every
        // layout pass, independent of focus or border overlay sync.
        self.sync_floating_z_order_with_monitors(monitors, operation);
    }

    fn actionable_tile_assignments(
        &self,
        assignments: &[TileAssignment],
        monitors: &[MonitorInfo],
    ) -> Vec<TileAssignment> {
        assignments
            .iter()
            .copied()
            .filter(|assignment| self.is_safe_to_move_window(assignment.window, monitors))
            .filter(|assignment| {
                !self.windows.get(&assignment.window).is_some_and(|window| {
                    rect_within_tolerance(window.rect, assignment.rect, LAYOUT_APPLY_TOLERANCE_PX)
                })
            })
            .collect()
    }

    fn record_relayout_result(
        &mut self,
        had_assignments: bool,
        applied_count: usize,
        skipped_count: usize,
        elapsed: Duration,
        operation: &'static str,
    ) {
        if applied_count == 0 {
            if had_assignments {
                self.perf.skipped_relayout_count =
                    self.perf.skipped_relayout_count.saturating_add(1);
                self.perf.last_relayout_duration = elapsed;
                self.perf.last_relayout_move_count = 0;
                debug!(
                    skipped_assignments = skipped_count,
                    duration_ms = saturating_duration_millis(elapsed),
                    operation,
                    "skipped no-op relayout"
                );
            }
            return;
        }

        self.perf.relayout_count = self.perf.relayout_count.saturating_add(1);
        self.perf.last_relayout_duration = elapsed;
        self.perf.last_relayout_move_count = applied_count;
        debug!(
            applied_moves = applied_count,
            skipped_assignments = skipped_count,
            duration_ms = saturating_duration_millis(elapsed),
            operation,
            "applied relayout"
        );
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
            if let Some(info) = self.windows.get_mut(&assignment.window) {
                info.rect = actual;
            }
            self.workspaces
                .update_window_rect(assignment.window, actual);

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
                    self.is_tilable_window_on_monitor(*handle, monitor.id)
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
        self.workspaces
            .sync_monitors(monitors.iter().map(|monitor| monitor.id));
        if let Some(monitor) = self
            .foreground
            .and_then(|window| self.window_owner_monitor(window, monitors))
        {
            self.workspaces.focus_monitor(monitor);
        }

        let existing: BTreeSet<_> = self.windows.keys().copied().collect();
        self.workspaces.retain_windows(&existing);

        let windows: Vec<_> = self
            .windows
            .iter()
            .map(|(handle, window)| (*handle, window.clone()))
            .collect();

        for (handle, window) in windows {
            let decision = self.rule_decision(&window);
            let previous_owner = self.window_owner_monitor(handle, monitors);
            let rect_monitor = monitor_for_rect(window.rect, monitors);
            let owner_changed =
                window.is_visible && rect_monitor.is_some() && previous_owner != rect_monitor;
            if window.is_visible
                && let Some(monitor) = rect_monitor
            {
                self.window_monitor_overrides.insert(handle, monitor);
            }
            let owner_monitor = self.window_owner_monitor(handle, monitors).or(rect_monitor);
            if let Some(monitor) = owner_monitor {
                self.window_monitor_overrides
                    .entry(handle)
                    .or_insert(monitor);
            }
            if self.workspaces.window_state(handle).is_some() {
                if self.is_workspace_manageable_by_rules(&window, &decision) {
                    self.workspaces.update_window_rect(handle, window.rect);
                    if owner_changed
                        && let Some(monitor) = owner_monitor
                        && !self
                            .workspaces
                            .is_window_on_monitor_workspace(handle, monitor)
                    {
                        let workspace = self.workspaces.active_workspace_for_monitor(monitor);
                        self.workspaces.move_window_to_workspace(handle, workspace);
                    }
                }
            } else if self.is_manageable_by_rules(&window, &decision)
                && !is_fullscreen_window(
                    &window,
                    monitors,
                    self.config.game_mode.policy.fullscreen_tolerance_px,
                )
            {
                if let Some(workspace) = decision.target_workspace {
                    self.workspaces
                        .track_window_on_workspace(handle, workspace, window.rect);
                } else if let Some(monitor) = owner_monitor {
                    self.workspaces.track_window_on_workspace(
                        handle,
                        self.workspaces.active_workspace_for_monitor(monitor),
                        window.rect,
                    );
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
                && !is_fullscreen_window(
                    info,
                    monitors,
                    self.config.game_mode.policy.fullscreen_tolerance_px,
                )
        })
    }

    fn is_tilable_window_on_monitor(&self, handle: WindowHandle, monitor: MonitorId) -> bool {
        self.workspaces
            .is_window_on_monitor_workspace(handle, monitor)
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

    fn window_owner_monitor(
        &self,
        handle: WindowHandle,
        monitors: &[MonitorInfo],
    ) -> Option<MonitorId> {
        if let Some(monitor) = self.window_monitor_overrides.get(&handle).copied()
            && monitors.iter().any(|candidate| candidate.id == monitor)
        {
            return Some(monitor);
        }

        let window = self.windows.get(&handle)?;
        monitor_for_rect(self.window_layout_rect(handle, window), monitors)
    }

    fn window_belongs_to_monitor(
        &self,
        handle: WindowHandle,
        monitor: MonitorId,
        monitors: &[MonitorInfo],
    ) -> bool {
        self.window_owner_monitor(handle, monitors) == Some(monitor)
    }

    fn is_window_visible_on_owned_monitor(
        &self,
        handle: WindowHandle,
        monitors: &[MonitorInfo],
    ) -> bool {
        let Some(monitor) = self.window_owner_monitor(handle, monitors) else {
            return self.workspaces.is_window_on_active_workspace(handle);
        };

        self.workspaces
            .is_window_on_monitor_workspace(handle, monitor)
    }

    fn focus_monitor_for_window(&mut self, window: WindowHandle, monitors: &[MonitorInfo]) {
        if let Some(monitor) = self.window_owner_monitor(window, monitors) {
            self.workspaces.focus_monitor(monitor);
        }
    }

    fn command_monitor(&self, monitors: &[MonitorInfo]) -> Option<MonitorId> {
        self.foreground
            .and_then(|window| self.window_owner_monitor(window, monitors))
            .or_else(|| {
                self.workspaces
                    .focused_monitor()
                    .filter(|focused| monitors.iter().any(|monitor| monitor.id == *focused))
            })
            .or_else(|| sorted_monitor_ids(monitors).into_iter().next())
    }

    fn focus_candidate_for_focused_monitor(
        &self,
        monitors: &[MonitorInfo],
    ) -> Option<WindowHandle> {
        let monitor = self.workspaces.focused_monitor()?;
        self.focus_candidate_for_monitor(monitor, monitors)
    }

    fn focus_candidate_for_monitor(
        &self,
        monitor: MonitorId,
        monitors: &[MonitorInfo],
    ) -> Option<WindowHandle> {
        self.tile_order
            .iter()
            .copied()
            .filter(|handle| self.window_belongs_to_monitor(*handle, monitor, monitors))
            .find(|handle| self.is_manageable_window(*handle, monitors))
    }

    fn focus_monitor_command(
        &mut self,
        selector: MonitorSelector,
        monitors: &[MonitorInfo],
    ) -> Option<WindowHandle> {
        let monitor = self.resolve_monitor_selector(selector, monitors)?;

        self.workspaces.focus_monitor(monitor);
        let focus = self.focus_candidate_for_monitor(monitor, monitors);
        self.foreground = focus;
        info!(monitor = %monitor, focus = ?focus, "focused monitor");
        focus
    }

    fn relative_workspace_for_monitor(
        &self,
        direction: CycleDirection,
        monitor: Option<MonitorId>,
    ) -> WorkspaceId {
        let active = monitor
            .map(|monitor| self.workspaces.active_workspace_for_monitor(monitor))
            .unwrap_or_else(|| self.workspaces.active_workspace());
        self.relative_workspace_from(active, direction)
    }

    fn relative_workspace_from(
        &self,
        active: WorkspaceId,
        direction: CycleDirection,
    ) -> WorkspaceId {
        let workspaces: Vec<_> = self.workspaces.workspaces().collect();
        if workspaces.is_empty() {
            return WorkspaceId(1);
        }

        let index = workspaces
            .iter()
            .position(|workspace| *workspace == active)
            .unwrap_or(0);
        let next = match direction {
            CycleDirection::Next => (index + 1) % workspaces.len(),
            CycleDirection::Prev => {
                if index == 0 {
                    workspaces.len() - 1
                } else {
                    index - 1
                }
            }
        };
        workspaces[next]
    }

    fn resolve_monitor_selector(
        &self,
        selector: MonitorSelector,
        monitors: &[MonitorInfo],
    ) -> Option<MonitorId> {
        let ordered = sorted_monitor_ids(monitors);
        if ordered.is_empty() {
            return None;
        }

        match selector {
            MonitorSelector::Id(id) if ordered.contains(&id) => Some(id),
            MonitorSelector::Id(_) => None,
            MonitorSelector::Index(index) => index
                .checked_sub(1)
                .and_then(|index| ordered.get(index).copied()),
            MonitorSelector::Next | MonitorSelector::Prev => {
                let current = self
                    .command_monitor(monitors)
                    .and_then(|monitor| ordered.iter().position(|candidate| *candidate == monitor))
                    .unwrap_or(0);
                let next = match selector {
                    MonitorSelector::Next => (current + 1) % ordered.len(),
                    MonitorSelector::Prev => {
                        if current == 0 {
                            ordered.len() - 1
                        } else {
                            current - 1
                        }
                    }
                    MonitorSelector::Id(_) | MonitorSelector::Index(_) => unreachable!(),
                };
                ordered.get(next).copied()
            }
        }
    }

    fn move_focused_to_monitor(
        &mut self,
        selector: MonitorSelector,
        monitors: &[MonitorInfo],
    ) -> CommandPlan {
        let Some(current) = self
            .foreground
            .filter(|handle| self.is_command_movable_window(*handle, monitors))
        else {
            return CommandPlan::default();
        };
        let Some(target_monitor) = self.resolve_monitor_selector(selector, monitors) else {
            return CommandPlan::default();
        };
        let target_workspace = self.workspaces.active_workspace_for_monitor(target_monitor);
        let source_monitor = self.window_owner_monitor(current, monitors);

        if let Some(window) = self.windows.get(&current) {
            self.workspaces.update_window_rect(current, window.rect);
        }
        self.workspaces
            .move_window_to_workspace(current, target_workspace);
        self.window_monitor_overrides
            .insert(current, target_monitor);
        self.workspaces.focus_monitor(target_monitor);
        self.foreground = Some(current);

        let mut moves = self.tile_assignments(monitors);
        if self.window_participation(current).is_floating()
            && let Some(rect) =
                self.translated_window_rect(current, source_monitor, target_monitor, monitors)
        {
            moves.push(TileAssignment {
                window: current,
                rect,
            });
        }

        info!(
            window = %current,
            from_monitor = ?source_monitor,
            to_monitor = %target_monitor,
            workspace = %target_workspace,
            participation = ?self.window_participation(current),
            "moved window to monitor"
        );

        CommandPlan {
            focus: Some(current),
            moves,
            ..CommandPlan::default()
        }
    }

    fn send_workspace_to_monitor(
        &mut self,
        workspace: WorkspaceId,
        selector: MonitorSelector,
        monitors: &[MonitorInfo],
    ) -> CommandPlan {
        let Some(target_monitor) = self.resolve_monitor_selector(selector, monitors) else {
            return CommandPlan::default();
        };
        let windows: Vec<_> = self
            .workspaces
            .window_states()
            .filter_map(|(window, state)| (state.workspace == workspace).then_some(window))
            .filter(|window| self.is_safe_to_move_window(*window, monitors))
            .collect();
        let floating_moves: Vec<_> = windows
            .iter()
            .copied()
            .filter(|window| self.window_participation(*window).is_floating())
            .filter_map(|window| {
                let source_monitor = self.window_owner_monitor(window, monitors);
                self.translated_window_rect(window, source_monitor, target_monitor, monitors)
                    .map(|rect| TileAssignment { window, rect })
            })
            .collect();

        for window in &windows {
            self.window_monitor_overrides
                .insert(*window, target_monitor);
        }

        let mut workspace_plan = self.workspaces.switch_monitor_to(target_monitor, workspace);
        workspace_plan.hide.retain(|window| {
            self.should_hide_for_workspace(*window, monitors)
                && self.window_belongs_to_monitor(*window, target_monitor, monitors)
        });
        workspace_plan.show.retain(|change| {
            self.window_belongs_to_monitor(change.window, target_monitor, monitors)
        });
        let mut show = workspace_plan.show;
        for window in &windows {
            if show.iter().any(|change| change.window == *window) {
                continue;
            }
            show.push(WorkspaceVisibilityChange {
                window: *window,
                restore_rect: self
                    .workspaces
                    .window_state(*window)
                    .and_then(|state| state.last_rect),
            });
        }

        let focus = self.focus_candidate_for_monitor(target_monitor, monitors);
        self.foreground = focus;
        let mut moves = self.tile_assignments(monitors);
        moves.extend(floating_moves);

        info!(
            workspace = %workspace,
            monitor = %target_monitor,
            window_count = windows.len(),
            focus = ?focus,
            "sent workspace to monitor"
        );

        CommandPlan {
            focus,
            hide: workspace_plan.hide,
            show,
            moves,
            ..CommandPlan::default()
        }
    }

    fn translated_window_rect(
        &self,
        window: WindowHandle,
        source_monitor: Option<MonitorId>,
        target_monitor: MonitorId,
        monitors: &[MonitorInfo],
    ) -> Option<Rect> {
        let window_info = self.windows.get(&window)?;
        let target = monitors
            .iter()
            .find(|monitor| monitor.id == target_monitor)?;
        let source =
            source_monitor.and_then(|source| monitors.iter().find(|monitor| monitor.id == source));
        Some(translate_rect_to_monitor(window_info.rect, source, target))
    }

    fn preserve_keyboard_state(&self, refreshed: &mut Self, monitors: &[MonitorInfo]) {
        refreshed.workspaces = self.workspaces.clone();
        refreshed
            .workspaces
            .set_workspace_count(refreshed.config.workspace_count);
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
        refreshed.overflow_promoted_floating = self
            .overflow_promoted_floating
            .iter()
            .copied()
            .filter(|handle| known_workspace_windows.contains(handle))
            .collect();
        refreshed.active_modifier_drag = self
            .active_modifier_drag
            .filter(|drag| refreshed.windows.contains_key(&drag.window));
        refreshed.active_interactive_drag = self
            .active_interactive_drag
            .clone()
            .filter(|drag| refreshed.windows.contains_key(&drag.window));
        refreshed.suppressed_modifier_drag_events = self
            .suppressed_modifier_drag_events
            .iter()
            .copied()
            .filter(|handle| refreshed.windows.contains_key(handle))
            .collect();
    }

    fn window_visibility_map(&self, monitors: &[MonitorInfo]) -> BTreeMap<WindowHandle, bool> {
        self.windows
            .keys()
            .copied()
            .map(|handle| {
                (
                    handle,
                    self.is_window_visible_on_owned_monitor(handle, monitors),
                )
            })
            .collect()
    }

    fn visibility_changes_after_reload(
        &self,
        previous_visibility: BTreeMap<WindowHandle, bool>,
        monitors: &[MonitorInfo],
    ) -> ReloadVisibilityChanges {
        let mut changes = ReloadVisibilityChanges::default();

        for handle in self.windows.keys().copied() {
            let was_visible = previous_visibility.get(&handle).copied().unwrap_or(false);
            let is_visible = self.is_window_visible_on_owned_monitor(handle, monitors);
            if was_visible && !is_visible && self.should_hide_for_workspace(handle, monitors) {
                changes.hide.push(handle);
            } else if !was_visible && is_visible {
                changes.show.push(WorkspaceVisibilityChange {
                    window: handle,
                    restore_rect: self
                        .workspaces
                        .window_state(handle)
                        .and_then(|state| state.last_rect),
                });
            }
        }

        changes
    }

    fn reapply_window_rules_after_reload(&mut self, monitors: &[MonitorInfo]) -> RuleReloadStats {
        let mut stats = RuleReloadStats::default();
        let windows: Vec<_> = self
            .windows
            .iter()
            .map(|(handle, window)| (*handle, window.clone()))
            .collect();

        for (handle, window) in windows {
            let decision = self.rule_decision(&window);
            let manageable = self.is_workspace_manageable_by_rules(&window, &decision)
                && !is_fullscreen_window(
                    &window,
                    monitors,
                    self.config.game_mode.policy.fullscreen_tolerance_px,
                );

            if !manageable {
                if self.workspaces.remove_window(handle) {
                    stats.untracked += 1;
                }
                if self.participation.remove(&handle).is_some() {
                    stats.set_tiled += 1;
                }
                self.overflow_promoted_floating.remove(&handle);
                continue;
            }

            if self.workspaces.window_state(handle).is_none() {
                if let Some(workspace) = decision.target_workspace {
                    self.workspaces
                        .track_window_on_workspace(handle, workspace, window.rect);
                } else if let Some(monitor) = self.window_owner_monitor(handle, monitors) {
                    self.workspaces.track_window_on_workspace(
                        handle,
                        self.workspaces.active_workspace_for_monitor(monitor),
                        window.rect,
                    );
                } else {
                    self.workspaces.track_window(handle, window.rect);
                }
            }

            if let Some(workspace) = decision.target_workspace {
                self.workspaces.move_window_to_workspace(handle, workspace);
            }
            if let Some(always_on_workspace) = decision.always_on_workspace {
                self.workspaces
                    .set_visible_on_all_workspaces(handle, always_on_workspace);
            }
            match decision.float {
                Some(true) => {
                    self.set_window_participation(handle, WindowParticipation::Floating);
                    stats.set_floating += 1;
                }
                Some(false) => {
                    self.set_window_participation(handle, WindowParticipation::Tiled);
                    stats.set_tiled += 1;
                }
                None => {}
            }
        }

        let before = self.tile_order.clone();
        let manageable: BTreeSet<_> = self.manageable_handles_sorted().into_iter().collect();
        self.tile_order.retain(|handle| manageable.contains(handle));
        stats.removed_from_tile_order = before.len().saturating_sub(self.tile_order.len());

        for handle in manageable {
            if !self.tile_order.contains(&handle) {
                self.tile_order.push(handle);
                stats.added_to_tile_order += 1;
            }
        }

        stats
    }

    fn replace_hotkey_backend_for_reload(
        &mut self,
        config: &Config,
        bindings: Vec<HotkeyBinding>,
    ) -> Result<()> {
        let Some(sender) = self.hotkey_sender.clone() else {
            return Ok(());
        };
        let old_config = self.source_config.clone();
        let old_backend = self.hotkey_backend.take();
        drop(old_backend);

        match install_hotkey_backend_strict(config, bindings, sender.clone()) {
            Ok(backend) => {
                debug!(backend = backend.name(), "reloaded daemon hotkey backend");
                self.hotkey_backend = Some(backend);
                Ok(())
            }
            Err(error) => {
                let restore_result = hotkey_bindings_from_config(&old_config)
                    .context("rebuild previous hotkeys after reload failure")
                    .and_then(|(old_bindings, _)| {
                        install_hotkey_backend(&old_config, old_bindings, sender)
                            .context("restore previous hotkeys after reload failure")
                    });
                match restore_result {
                    Ok(backend) => {
                        self.hotkey_backend = Some(backend);
                        Err(error)
                            .context("new hotkey registration failed; previous hotkeys restored")
                    }
                    Err(restore_error) => Err(anyhow!(
                        "new hotkey registration failed ({error}); previous hotkeys could not be restored: {restore_error}"
                    )),
                }
            }
        }
    }

    fn replace_modifier_drag_for_reload(&mut self, config: &Config) -> Result<()> {
        let Some(sender) = self.mouse_drag_sender.clone() else {
            return Ok(());
        };
        let old_config = self.source_config.clone();
        let old_registration = self.modifier_drag.take();
        drop(old_registration);

        match install_modifier_drag(config, sender.clone()) {
            Ok(registration) => {
                self.modifier_drag = registration;
                Ok(())
            }
            Err(error) => {
                let restore_result = install_modifier_drag(&old_config, sender)
                    .context("restore previous modifier drag settings after reload failure");
                match restore_result {
                    Ok(registration) => {
                        self.modifier_drag = registration;
                        Err(error).context(
                            "new modifier drag settings failed; previous settings restored",
                        )
                    }
                    Err(restore_error) => Err(anyhow!(
                        "new modifier drag settings failed ({error}); previous settings could not be restored: {restore_error}"
                    )),
                }
            }
        }
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
        self.overflow_promoted_floating.remove(&window);
        match participation {
            WindowParticipation::Tiled => {
                self.participation.remove(&window);
            }
            WindowParticipation::Floating | WindowParticipation::TemporarilyFloating => {
                self.participation.insert(window, participation);
            }
        }
    }

    fn promote_overflow_window_to_floating(&mut self, window: WindowHandle) {
        self.remember_previous_rect(window);
        self.set_window_participation(window, WindowParticipation::Floating);
        self.overflow_promoted_floating.insert(window);
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

    fn focusable_handles(&self, monitors: &[MonitorInfo]) -> Vec<WindowHandle> {
        self.tile_order
            .iter()
            .copied()
            .filter(|handle| self.is_manageable_window(*handle, monitors))
            .collect()
    }

    fn is_manageable_window(&self, handle: WindowHandle, monitors: &[MonitorInfo]) -> bool {
        self.is_window_visible_on_owned_monitor(handle, monitors)
            && self.windows.get(&handle).is_some_and(|window| {
                self.is_manageable_by_rules(window, &self.rule_decision(window))
            })
    }

    fn is_command_movable_window(&self, handle: WindowHandle, monitors: &[MonitorInfo]) -> bool {
        self.is_manageable_window(handle, monitors)
            && self.windows.get(&handle).is_some_and(|window| {
                !is_fullscreen_window(
                    window,
                    monitors,
                    self.config.game_mode.policy.fullscreen_tolerance_px,
                )
            })
    }

    fn is_safe_to_move_window(&self, handle: WindowHandle, monitors: &[MonitorInfo]) -> bool {
        self.windows.get(&handle).is_some_and(|window| {
            self.is_workspace_manageable_by_rules(window, &self.rule_decision(window))
                && !is_fullscreen_window(
                    window,
                    monitors,
                    self.config.game_mode.policy.fullscreen_tolerance_px,
                )
        })
    }

    fn rule_decision(&self, window: &WindowInfo) -> WindowRuleDecision {
        evaluate_window_rules(window, &self.config.window_rules)
    }

    fn is_manageable_by_rules(&self, window: &WindowInfo, decision: &WindowRuleDecision) -> bool {
        window.is_manageable()
            && decision.manage != Some(false)
            && !matches!(
                decision.mode,
                Some(WindowRuleMode::Ignore | WindowRuleMode::Game | WindowRuleMode::Fullscreen)
            )
            && !game_mode_executable_matches(window, &self.config.game_mode.policy)
    }

    fn is_workspace_manageable_by_rules(
        &self,
        window: &WindowInfo,
        decision: &WindowRuleDecision,
    ) -> bool {
        window.is_workspace_manageable()
            && decision.manage != Some(false)
            && !matches!(
                decision.mode,
                Some(WindowRuleMode::Ignore | WindowRuleMode::Game | WindowRuleMode::Fullscreen)
            )
            && !game_mode_executable_matches(window, &self.config.game_mode.policy)
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
        self.sync_floating_z_order_with_monitors(monitors, operation);
        self.sync_border_geometry_with_monitors(monitors, operation)
    }

    fn sync_border_geometry_with_monitors(
        &mut self,
        monitors: &[MonitorInfo],
        operation: &'static str,
    ) -> Result<()> {
        let Some(manager) = &self.border_manager else {
            return Ok(());
        };

        let updates = self.border_updates(monitors);
        let visible_count = updates.iter().filter(|update| update.visible).count();
        manager
            .sync(updates, self.config.borders.width)
            .context("sync border overlays")?;
        self.perf.border_window_count = visible_count;
        debug!(
            operation,
            visible_border_windows = visible_count,
            "synced border overlays"
        );
        Ok(())
    }

    fn sync_drag_border_geometry_with_monitors(
        &mut self,
        monitors: &[MonitorInfo],
        active_drag_move: bool,
        operation: &'static str,
    ) -> Result<()> {
        if active_drag_move {
            self.sync_border_geometry_with_monitors(monitors, operation)
        } else {
            self.sync_borders_with_monitors(monitors, operation)
        }
    }

    fn sync_floating_z_order_with_monitors(
        &self,
        monitors: &[MonitorInfo],
        operation: &'static str,
    ) {
        let floating_windows = self.floating_z_order_windows(monitors);
        if floating_windows.is_empty() {
            return;
        }

        for window in &floating_windows {
            if let Err(error) = winland_win32::raise_window_no_activate(*window) {
                warn!(
                    window = %window,
                    %error,
                    operation,
                    "failed to raise floating window above tiled windows"
                );
            }
        }

        debug!(
            floating_window_count = floating_windows.len(),
            operation, "synced floating window z-order"
        );
    }

    fn floating_z_order_windows(&self, monitors: &[MonitorInfo]) -> Vec<WindowHandle> {
        if self.game_mode_pauses_layouts() {
            return Vec::new();
        }

        let mut windows: Vec<_> = self
            .tile_order
            .iter()
            .copied()
            .filter(|handle| self.is_floating_z_order_window(*handle, monitors))
            .collect();

        for handle in self.windows.keys().copied() {
            if !windows.contains(&handle) && self.is_floating_z_order_window(handle, monitors) {
                windows.push(handle);
            }
        }

        if let Some(foreground) = self.foreground
            && let Some(index) = windows.iter().position(|window| *window == foreground)
        {
            let foreground = windows.remove(index);
            windows.push(foreground);
        }

        windows
    }

    fn is_floating_z_order_window(&self, handle: WindowHandle, monitors: &[MonitorInfo]) -> bool {
        let participation = self.window_participation(handle);
        if !participation.is_floating() {
            return false;
        }

        let Some(monitor) = self.window_owner_monitor(handle, monitors) else {
            return false;
        };
        if self.game_mode_pauses_monitor(monitor) {
            return false;
        }

        self.is_window_visible_on_owned_monitor(handle, monitors)
            && self.windows.get(&handle).is_some_and(|window| {
                self.is_workspace_manageable_by_rules(window, &self.rule_decision(window))
                    && !is_fullscreen_window(
                        window,
                        monitors,
                        self.config.game_mode.policy.fullscreen_tolerance_px,
                    )
            })
    }

    fn border_candidates(&self, monitors: &[MonitorInfo]) -> Vec<BorderCandidate> {
        let config = self.config.borders;
        if !config.enabled {
            return Vec::new();
        }

        if self.game_mode_hides_borders() {
            return Vec::new();
        }

        if config.disable_when_fullscreen
            && self.foreground.is_some_and(|window| {
                self.windows.get(&window).is_some_and(|info| {
                    is_fullscreen_window(
                        info,
                        monitors,
                        self.config.game_mode.policy.fullscreen_tolerance_px,
                    ) || !self.is_manageable_by_rules(info, &self.rule_decision(info))
                })
            })
        {
            return Vec::new();
        }

        self.windows
            .iter()
            .filter(|(handle, window)| {
                let Some(monitor) = self.window_owner_monitor(**handle, monitors) else {
                    return false;
                };

                self.is_manageable_window(**handle, monitors)
                    && !self.game_mode_pauses_monitor(monitor)
                    && !is_fullscreen_window(
                        window,
                        monitors,
                        self.config.game_mode.policy.fullscreen_tolerance_px,
                    )
            })
            .filter_map(|(handle, window)| {
                let participation = self.window_participation(*handle);
                let focused = self.foreground == Some(*handle);

                let color = if focused {
                    config.active_color
                } else if participation.is_floating() {
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
                    focused,
                    floating: participation.is_floating(),
                })
            })
            .collect()
    }

    fn border_updates(&self, monitors: &[MonitorInfo]) -> Vec<BorderUpdate> {
        let visible_candidates: BTreeMap<_, _> = self
            .border_candidates(monitors)
            .into_iter()
            .map(|candidate| (candidate.window, candidate))
            .collect();

        let mut updates: Vec<_> = self
            .border_retention_candidates(monitors)
            .into_iter()
            .map(|candidate| {
                let visible = visible_candidates.contains_key(&candidate.window);
                let candidate = visible_candidates
                    .get(&candidate.window)
                    .copied()
                    .unwrap_or(candidate);
                let rect = if visible && self.active_drag_window() != Some(candidate.window) {
                    winland_win32::window_rect_for_handle(candidate.window)
                        .unwrap_or(candidate.rect)
                } else {
                    candidate.rect
                };
                BorderUpdate {
                    window: candidate.window,
                    rect,
                    color: candidate.color,
                    visible,
                }
            })
            .collect();
        updates.sort_by_key(|update| {
            let visible = visible_candidates.get(&update.window);
            let layer = visible
                .map(|candidate| match (candidate.floating, candidate.focused) {
                    (true, true) => 4,
                    (true, false) => 3,
                    (false, true) => 2,
                    (false, false) => 1,
                })
                .unwrap_or(0);
            (update.visible, layer)
        });
        updates
    }

    fn border_retention_candidates(&self, monitors: &[MonitorInfo]) -> Vec<BorderCandidate> {
        let config = self.config.borders;

        self.windows
            .iter()
            .filter(|(handle, window)| {
                let Some(monitor) = self.window_owner_monitor(**handle, monitors) else {
                    return false;
                };

                self.is_manageable_window(**handle, monitors)
                    && !self.game_mode_pauses_monitor(monitor)
                    && !is_fullscreen_window(
                        window,
                        monitors,
                        self.config.game_mode.policy.fullscreen_tolerance_px,
                    )
            })
            .map(|(handle, window)| {
                let participation = self.window_participation(*handle);
                let focused = self.foreground == Some(*handle);

                let color = if focused {
                    config.active_color
                } else if participation.is_floating() {
                    config.floating_color
                } else {
                    config.inactive_color
                };

                BorderCandidate {
                    window: *handle,
                    rect: window.rect,
                    color,
                    focused,
                    floating: participation.is_floating(),
                }
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
    FocusMonitor(MonitorSelector),
    Swap(FocusDirection),
    Retile,
    ToggleFloat,
    SwitchWorkspace(WorkspaceId),
    SwitchWorkspaceRelative(CycleDirection),
    MoveFocusedToWorkspace(WorkspaceId),
    MoveFocusedToWorkspaceAndFollow(WorkspaceId),
    MoveFocusedToMonitor(MonitorSelector),
    SendWorkspaceToMonitor {
        workspace: WorkspaceId,
        monitor: MonitorSelector,
    },
    Reload,
    Quit,
    Launch(String),
}

impl DaemonCommand {
    fn needs_monitors(&self) -> bool {
        self.needs_layout() || matches!(self, Self::Focus(_) | Self::FocusMonitor(_))
    }

    fn needs_layout(&self) -> bool {
        matches!(
            self,
            Self::FocusMonitor(_)
                | Self::Swap(_)
                | Self::Retile
                | Self::ToggleFloat
                | Self::SwitchWorkspace(_)
                | Self::SwitchWorkspaceRelative(_)
                | Self::MoveFocusedToWorkspace(_)
                | Self::MoveFocusedToWorkspaceAndFollow(_)
                | Self::MoveFocusedToMonitor(_)
                | Self::SendWorkspaceToMonitor { .. }
        )
    }

    fn is_suppressed_by_game_mode(&self) -> bool {
        matches!(
            self,
            Self::Focus(_)
                | Self::FocusMonitor(_)
                | Self::Swap(_)
                | Self::Retile
                | Self::ToggleFloat
                | Self::SwitchWorkspace(_)
                | Self::SwitchWorkspaceRelative(_)
                | Self::MoveFocusedToWorkspace(_)
                | Self::MoveFocusedToWorkspaceAndFollow(_)
                | Self::MoveFocusedToMonitor(_)
                | Self::SendWorkspaceToMonitor { .. }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CycleDirection {
    Next,
    Prev,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MonitorSelector {
    Next,
    Prev,
    Index(usize),
    Id(MonitorId),
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
struct ReloadVisibilityChanges {
    hide: Vec<WindowHandle>,
    show: Vec<WorkspaceVisibilityChange>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RuleReloadStats {
    untracked: usize,
    added_to_tile_order: usize,
    removed_from_tile_order: usize,
    set_floating: usize,
    set_tiled: usize,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ConfigDiff {
    changed_sections: Vec<String>,
}

impl ConfigDiff {
    fn between(old: &Config, new: &Config) -> Self {
        let mut changed_sections = Vec::new();

        if old.hotkeys != new.hotkeys {
            changed_sections.push("hotkeys".to_owned());
        }
        if old.layout.default != new.layout.default
            || old.layout.master_ratio_percent != new.layout.master_ratio_percent
            || old.layout.smart_split != new.layout.smart_split
            || old.layout.preserve_split != new.layout.preserve_split
            || old.layout.per_monitor != new.layout.per_monitor
            || old.layout.per_workspace != new.layout.per_workspace
        {
            changed_sections.push("layout".to_owned());
        }
        if old.layout.gap != new.layout.gap || old.layout.border != new.layout.border {
            changed_sections.push("gaps".to_owned());
        }
        if old.borders != new.borders {
            changed_sections.push("borders".to_owned());
        }
        if old.window_rules != new.window_rules {
            changed_sections.push("window-rules".to_owned());
        }
        if old.behavior != new.behavior {
            changed_sections.push("behavior".to_owned());
        }
        if old.game_mode != new.game_mode {
            changed_sections.push("game-mode".to_owned());
        }
        if old.workspaces != new.workspaces {
            changed_sections.push("workspaces".to_owned());
        }
        if old.general != new.general {
            changed_sections.push("general".to_owned());
        }
        if changed_sections.is_empty() {
            changed_sections.push("none".to_owned());
        }

        Self { changed_sections }
    }

    fn is_noop(&self) -> bool {
        self.changed_sections.len() == 1 && self.changed_sections[0] == "none"
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct WorkspaceCommandPlan {
    focus: Option<WindowHandle>,
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
    focused: bool,
    floating: bool,
}

fn count_events(batch: &[WindowEvent], kind: WindowEventKind) -> usize {
    batch.iter().filter(|event| event.kind == kind).count()
}

fn rect_within_tolerance(actual: Rect, expected: Rect, tolerance: i32) -> bool {
    let tolerance = tolerance.max(0);
    (actual.left - expected.left).abs() <= tolerance
        && (actual.top - expected.top).abs() <= tolerance
        && (actual.right - expected.right).abs() <= tolerance
        && (actual.bottom - expected.bottom).abs() <= tolerance
}

fn saturating_duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn system_time_unix_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
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

fn install_hotkey_backend_strict(
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
            if !registration.failures().is_empty() {
                let failures = format_hotkey_registration_failures(registration.failures());
                return Err(anyhow!("failed to register reloaded hotkeys: {failures}"));
            }
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
        bypass: hotkey_bypass_rules_from_config(config)?,
    };
    let registration = winland_win32::install_modifier_drag(options, mouse_drag_sender)
        .context("install documented low-level mouse hook for modifier drag")?;

    info!(
        modifiers = %config.hotkeys.modifier_drag.modifiers,
        "installed modifier drag hook"
    );
    Ok(Some(registration))
}

fn format_hotkey_registration_failures(
    failures: &[winland_win32::HotkeyRegistrationFailure],
) -> String {
    failures
        .iter()
        .map(|failure| {
            format!(
                "{} (id {}): {}",
                failure.description, failure.id.0, failure.error
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
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
        bypass: hotkey_bypass_rules_from_config(config)?,
        latency_budget: Duration::from_micros(config.hotkeys.override_latency_budget_micros),
    })
}

fn hotkey_bypass_rules_from_config(config: &Config) -> Result<HotkeyBypassRules> {
    let mut process_names = text_matchers_from_config(&config.hotkeys.bypass.process_name)?;
    if config.game_mode.enabled && config.game_mode.disable_keyboard_hooks {
        process_names.extend(
            config
                .game_mode
                .game_exes
                .iter()
                .chain(config.game_mode.ignored_exes.iter())
                .map(|exe| winland_core::TextMatcher::Exact(exe.clone())),
        );
    }

    Ok(HotkeyBypassRules {
        fullscreen: config.hotkeys.bypass.fullscreen
            || (config.game_mode.enabled
                && config.game_mode.disable_keyboard_hooks
                && config.game_mode.pause_on_fullscreen),
        class_names: text_matchers_from_config(&config.hotkeys.bypass.class)?,
        executable_paths: text_matchers_from_config(&config.hotkeys.bypass.executable_path)?,
        process_names,
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
    let command = command.trim();
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

    if let Some(command) = daemon_command_from_words(command) {
        return Some(command);
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

fn daemon_command_from_words(command: &str) -> Option<DaemonCommand> {
    let words: Vec<_> = command.split_whitespace().collect();
    match words.as_slice() {
        ["switch-workspace", "next"] => {
            Some(DaemonCommand::SwitchWorkspaceRelative(CycleDirection::Next))
        }
        ["switch-workspace", "prev" | "previous"] => {
            Some(DaemonCommand::SwitchWorkspaceRelative(CycleDirection::Prev))
        }
        ["switch-workspace", workspace] => parse_workspace(workspace)
            .map(|workspace| DaemonCommand::SwitchWorkspace(WorkspaceId(workspace))),
        ["move-window-to-workspace", workspace] => parse_workspace(workspace)
            .map(|workspace| DaemonCommand::MoveFocusedToWorkspace(WorkspaceId(workspace))),
        ["move-window-to-workspace-and-follow", workspace] => {
            parse_workspace(workspace).map(|workspace| {
                DaemonCommand::MoveFocusedToWorkspaceAndFollow(WorkspaceId(workspace))
            })
        }
        ["focus-monitor", selector] => {
            parse_monitor_selector(selector).map(DaemonCommand::FocusMonitor)
        }
        ["move-window-to-monitor", selector] => {
            parse_monitor_selector(selector).map(DaemonCommand::MoveFocusedToMonitor)
        }
        ["send-workspace-to-monitor", workspace, monitor] => {
            let workspace = WorkspaceId(parse_workspace(workspace)?);
            let monitor = parse_monitor_selector(monitor)?;
            Some(DaemonCommand::SendWorkspaceToMonitor { workspace, monitor })
        }
        _ => None,
    }
}

fn parse_workspace(input: &str) -> Option<u16> {
    input.parse::<u16>().ok().filter(|workspace| *workspace > 0)
}

fn parse_monitor_selector(input: &str) -> Option<MonitorSelector> {
    match input {
        "next" => Some(MonitorSelector::Next),
        "prev" | "previous" => Some(MonitorSelector::Prev),
        _ => {
            if let Some(hex) = input
                .strip_prefix("0x")
                .or_else(|| input.strip_prefix("0X"))
            {
                u64::from_str_radix(hex, 16)
                    .ok()
                    .map(|id| MonitorSelector::Id(MonitorId(id)))
            } else {
                input
                    .parse::<usize>()
                    .ok()
                    .filter(|index| *index > 0)
                    .map(MonitorSelector::Index)
            }
        }
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

fn game_mode_reason_label(reason: &GameModeReason) -> String {
    match reason {
        GameModeReason::ConfiguredExecutable(exe) => format!("configured executable {exe}"),
        GameModeReason::WindowRule {
            mode,
            matched_rules,
        } => {
            format!("window rule mode {mode:?} via {}", matched_rules.join(", "))
        }
        GameModeReason::Fullscreen { monitor, area } => {
            format!("fullscreen {area:?} on monitor {monitor}")
        }
    }
}

fn game_mode_layout_pause_scope_for(active: &GameModeActivation) -> &'static str {
    if !active.actions.pause_layouts {
        "none"
    } else if active.actions.pause_focused_monitor_only && active.monitor.is_some() {
        "focused-monitor"
    } else {
        "global"
    }
}

fn is_fullscreen_window(window: &WindowInfo, monitors: &[MonitorInfo], tolerance_px: i32) -> bool {
    detect_fullscreen_window(window, monitors, tolerance_px).is_fullscreen
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

fn sorted_monitor_ids(monitors: &[MonitorInfo]) -> Vec<MonitorId> {
    let mut ordered: Vec<_> = monitors
        .iter()
        .map(|monitor| (monitor.rect.left, monitor.rect.top, monitor.id))
        .collect();
    ordered.sort();
    ordered.into_iter().map(|(_, _, id)| id).collect()
}

fn translate_rect_to_monitor(
    rect: Rect,
    source: Option<&MonitorInfo>,
    target: &MonitorInfo,
) -> Rect {
    let translated = if let Some(source) = source {
        let dx = target.work_area.left.saturating_sub(source.work_area.left);
        let dy = target.work_area.top.saturating_sub(source.work_area.top);
        Rect {
            left: rect.left.saturating_add(dx),
            top: rect.top.saturating_add(dy),
            right: rect.right.saturating_add(dx),
            bottom: rect.bottom.saturating_add(dy),
        }
    } else {
        let center = target.work_area.center();
        Rect::from_size(
            center.x.saturating_sub(rect.width() / 2),
            center.y.saturating_sub(rect.height() / 2),
            rect.width(),
            rect.height(),
        )
    };

    clamp_rect_to_area(translated, target.work_area)
}

fn clamp_rect_to_area(rect: Rect, area: Rect) -> Rect {
    let width = rect.width();
    let height = rect.height();
    let min_left = area.left;
    let max_left = area.right.saturating_sub(width).max(min_left);
    let min_top = area.top;
    let max_top = area.bottom.saturating_sub(height).max(min_top);
    Rect::from_size(
        rect.left.clamp(min_left, max_left),
        rect.top.clamp(min_top, max_top),
        width,
        height,
    )
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
    use winland_core::{FullscreenArea, MonitorId, Rect, WindowSizeConstraints, WindowStyles};

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
        assert_eq!(
            daemon_command_from_name("switch-workspace next"),
            Some(DaemonCommand::SwitchWorkspaceRelative(CycleDirection::Next))
        );
        assert_eq!(
            daemon_command_from_name("focus-monitor prev"),
            Some(DaemonCommand::FocusMonitor(MonitorSelector::Prev))
        );
        assert_eq!(
            daemon_command_from_name("move-window-to-monitor 2"),
            Some(DaemonCommand::MoveFocusedToMonitor(MonitorSelector::Index(
                2
            )))
        );
        assert_eq!(
            daemon_command_from_name("move-window-to-workspace 3"),
            Some(DaemonCommand::MoveFocusedToWorkspace(WorkspaceId(3)))
        );
        assert_eq!(
            daemon_command_from_name("move-window-to-workspace-and-follow 3"),
            Some(DaemonCommand::MoveFocusedToWorkspaceAndFollow(WorkspaceId(
                3
            )))
        );
        assert_eq!(
            daemon_command_from_name("send-workspace-to-monitor 3 0x2"),
            Some(DaemonCommand::SendWorkspaceToMonitor {
                workspace: WorkspaceId(3),
                monitor: MonitorSelector::Id(MonitorId(2)),
            })
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

        let plan = state.plan_command(DaemonCommand::Retile, std::slice::from_ref(&monitor));

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

        let plan = state.plan_command(DaemonCommand::Retile, std::slice::from_ref(&monitor));

        assert_eq!(
            plan.moves,
            vec![TileAssignment {
                window: WindowHandle(1),
                rect: work_area,
            }]
        );
        assert_eq!(
            state.window_participation(WindowHandle(2)),
            WindowParticipation::Floating
        );
        assert_eq!(
            state.overflow_promoted_floating,
            BTreeSet::from([WindowHandle(2)])
        );
        assert_eq!(
            state.floating_z_order_windows(std::slice::from_ref(&monitor)),
            vec![WindowHandle(2)]
        );
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
        assert_eq!(
            state.window_participation(WindowHandle(1)),
            WindowParticipation::Floating
        );
        assert_eq!(
            state.overflow_promoted_floating,
            BTreeSet::from([WindowHandle(1)])
        );
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
            state.overflow_promoted_floating,
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
        assert_eq!(
            state.window_participation(WindowHandle(1)),
            WindowParticipation::Floating
        );
        assert_eq!(
            state.overflow_promoted_floating,
            BTreeSet::from([WindowHandle(1)])
        );
    }

    #[test]
    fn overflow_windows_stay_floating_after_they_can_fit_by_default() {
        let monitor = primary_test_monitor();
        let mut first = window(1, "One", Rect::from_size(0, 0, 100, 100));
        first.size_constraints = winland_core::WindowSizeConstraints::minimum(700, 0);
        let mut second = window(2, "Two", Rect::from_size(200, 0, 100, 100));
        second.size_constraints = winland_core::WindowSizeConstraints::minimum(700, 0);
        let mut state = daemon_state([first, second]);
        state.foreground = Some(WindowHandle(1));

        let _ = state.plan_command(DaemonCommand::Retile, std::slice::from_ref(&monitor));
        assert_eq!(
            state.overflow_promoted_floating,
            BTreeSet::from([WindowHandle(2)])
        );

        state
            .windows
            .get_mut(&WindowHandle(2))
            .unwrap()
            .size_constraints = winland_core::WindowSizeConstraints::NONE;
        let plan = state.plan_command(DaemonCommand::Retile, &[monitor]);

        assert_eq!(
            state.window_participation(WindowHandle(2)),
            WindowParticipation::Floating
        );
        assert_eq!(
            state.overflow_promoted_floating,
            BTreeSet::from([WindowHandle(2)])
        );
        assert_eq!(plan.moves.len(), 1);
    }

    #[test]
    fn overflow_promoted_window_retiles_after_explicit_drop_when_configured() {
        let monitor = primary_test_monitor();
        let mut first = window(1, "One", Rect::from_size(0, 0, 100, 100));
        first.size_constraints = winland_core::WindowSizeConstraints::minimum(700, 0);
        let mut second = window(2, "Two", Rect::from_size(200, 0, 100, 100));
        second.size_constraints = winland_core::WindowSizeConstraints::minimum(700, 0);
        let mut state = daemon_state([first, second]);
        state.config.overflow_float_persistence = OverflowFloatPersistence::RetileOnDragEnd;
        state.foreground = Some(WindowHandle(1));

        let _ = state.plan_command(DaemonCommand::Retile, std::slice::from_ref(&monitor));
        assert_eq!(
            state.window_participation(WindowHandle(2)),
            WindowParticipation::Floating
        );

        state
            .windows
            .get_mut(&WindowHandle(2))
            .unwrap()
            .size_constraints = winland_core::WindowSizeConstraints::NONE;

        assert!(state.handle_movesize_end(
            WindowHandle(2),
            std::slice::from_ref(&monitor),
            Some(DropContext {
                rect: Rect::from_size(800, 0, 100, 100),
                cursor: Some(Point { x: 850, y: 50 }),
            }),
        ));
        assert_eq!(
            state.window_participation(WindowHandle(2)),
            WindowParticipation::Tiled
        );
        assert_eq!(state.overflow_promoted_floating, BTreeSet::new());
    }

    #[test]
    fn overflow_promoted_window_stays_floating_after_drop_when_layout_still_will_not_fit() {
        let monitor = primary_test_monitor();
        let mut first = window(1, "One", Rect::from_size(0, 0, 100, 100));
        first.size_constraints = winland_core::WindowSizeConstraints::minimum(700, 0);
        let mut second = window(2, "Two", Rect::from_size(200, 0, 100, 100));
        second.size_constraints = winland_core::WindowSizeConstraints::minimum(700, 0);
        let mut state = daemon_state([first, second]);
        state.config.overflow_float_persistence = OverflowFloatPersistence::RetileOnDragEnd;
        state.foreground = Some(WindowHandle(1));

        let _ = state.plan_command(DaemonCommand::Retile, std::slice::from_ref(&monitor));

        assert!(!state.handle_movesize_end(
            WindowHandle(2),
            std::slice::from_ref(&monitor),
            Some(DropContext {
                rect: Rect::from_size(800, 0, 100, 100),
                cursor: Some(Point { x: 850, y: 50 }),
            }),
        ));
        assert_eq!(
            state.window_participation(WindowHandle(2)),
            WindowParticipation::Floating
        );
        assert_eq!(
            state.overflow_promoted_floating,
            BTreeSet::from([WindowHandle(2)])
        );
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

        assert!(state.handle_movesize_start(WindowHandle(1), &[primary_test_monitor()]));

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

        assert!(state.reorder_window_by_drop_at(
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

        assert!(state.reorder_window_by_drop_at(
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
    fn native_drag_moved_events_are_processed_without_debounce() {
        let mut state = daemon_state([window(1, "Dragged", Rect::from_size(0, 0, 100, 100))]);
        state.active_interactive_drag = Some(active_interactive_drag(WindowHandle(1)));

        assert!(state.should_process_moved_event_immediately(event(WindowEventKind::Moved, 1)));
        assert!(!state.should_process_moved_event_immediately(event(WindowEventKind::Moved, 2)));
        assert!(!state.should_process_moved_event_immediately(event(WindowEventKind::Shown, 1)));
    }

    #[test]
    fn native_drag_tick_updates_cached_rect_and_monitor_override() {
        let mut state = daemon_state([window(1, "Dragged", Rect::from_size(0, 0, 100, 100))]);
        let monitors = [primary_test_monitor(), secondary_test_monitor()];
        let dragged = WindowHandle(1);
        state.active_interactive_drag = Some(ActiveInteractiveDrag {
            window: dragged,
            start_cursor: Point { x: 10, y: 10 },
            start_rect: Rect::from_size(1000, 0, 240, 180),
            monitors: monitors.to_vec(),
        });
        let dragged_rect = state
            .interactive_drag_rect_from_cursor(dragged, Point { x: 210, y: 30 })
            .unwrap();

        assert!(state.apply_interactive_drag_rect(dragged, dragged_rect, &monitors));
        assert_eq!(dragged_rect, Rect::from_size(1200, 20, 240, 180));
        assert_eq!(state.windows.get(&dragged).unwrap().rect, dragged_rect);
        assert_eq!(
            state.window_monitor_overrides.get(&dragged),
            Some(&MonitorId(2))
        );
    }

    #[test]
    fn native_drag_delta_uses_initial_cursor_and_visible_rect() {
        let state = {
            let mut state =
                daemon_state([window(1, "Dragged", Rect::from_size(100, 100, 200, 120))]);
            state.active_interactive_drag = Some(ActiveInteractiveDrag {
                window: WindowHandle(1),
                start_cursor: Point { x: 124, y: 118 },
                start_rect: Rect::from_size(100, 100, 200, 120),
                monitors: vec![primary_test_monitor()],
            });
            state
        };

        assert_eq!(
            state.interactive_drag_rect_from_cursor(WindowHandle(1), Point { x: 324, y: 218 }),
            Some(Rect::from_size(300, 200, 200, 120))
        );
    }

    #[test]
    fn native_drag_tick_ignores_stale_windows() {
        let mut state = daemon_state([window(1, "Dragged", Rect::from_size(0, 0, 100, 100))]);
        let monitors = [primary_test_monitor()];

        assert!(!state.apply_interactive_drag_rect(
            WindowHandle(1),
            Rect::from_size(10, 10, 100, 100),
            &monitors
        ));
        assert_eq!(
            state.windows.get(&WindowHandle(1)).unwrap().rect,
            Rect::from_size(0, 0, 100, 100)
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
    fn switching_workspace_while_dragging_moves_dragged_window_to_target_workspace() {
        let mut state = daemon_state([
            window(1, "Dragged", Rect::from_size(0, 0, 100, 100)),
            window(2, "Stays Behind", Rect::from_size(200, 0, 100, 100)),
            window(3, "Already There", Rect::from_size(400, 0, 100, 100)),
        ]);
        state
            .workspaces
            .move_window_to_workspace(WindowHandle(3), WorkspaceId(2));

        assert!(state.handle_movesize_start(WindowHandle(1), &[primary_test_monitor()]));

        let plan = state.plan_command(
            DaemonCommand::SwitchWorkspace(WorkspaceId(2)),
            &[primary_test_monitor()],
        );

        assert_eq!(
            state
                .workspaces
                .window_state(WindowHandle(1))
                .map(|state| state.workspace),
            Some(WorkspaceId(2))
        );
        assert_eq!(
            state.window_participation(WindowHandle(1)),
            WindowParticipation::TemporarilyFloating
        );
        assert_eq!(state.foreground, Some(WindowHandle(1)));
        assert_eq!(plan.focus, Some(WindowHandle(1)));
        assert_eq!(plan.hide, vec![WindowHandle(2)]);
        assert_eq!(
            plan.show,
            vec![
                WorkspaceVisibilityChange {
                    window: WindowHandle(1),
                    restore_rect: Some(Rect::from_size(0, 0, 100, 100)),
                },
                WorkspaceVisibilityChange {
                    window: WindowHandle(3),
                    restore_rect: Some(Rect::from_size(400, 0, 100, 100)),
                },
            ]
        );
        assert_eq!(
            plan.moves,
            vec![TileAssignment {
                window: WindowHandle(3),
                rect: primary_test_monitor().work_area,
            }]
        );
    }

    #[test]
    fn switching_workspace_while_dragging_floating_window_preserves_floating_state() {
        let mut state = daemon_state([
            window(1, "Floating", Rect::from_size(0, 0, 100, 100)),
            window(2, "Stays Behind", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.set_window_participation(WindowHandle(1), WindowParticipation::Floating);

        assert!(!state.handle_movesize_start(WindowHandle(1), &[primary_test_monitor()]));
        assert_eq!(
            state.active_interactive_drag_window(),
            Some(WindowHandle(1))
        );

        let plan = state.plan_command(
            DaemonCommand::SwitchWorkspace(WorkspaceId(2)),
            &[primary_test_monitor()],
        );

        assert_eq!(
            state
                .workspaces
                .window_state(WindowHandle(1))
                .map(|state| state.workspace),
            Some(WorkspaceId(2))
        );
        assert_eq!(
            state.window_participation(WindowHandle(1)),
            WindowParticipation::Floating
        );
        assert_eq!(plan.focus, Some(WindowHandle(1)));
        assert_eq!(plan.hide, vec![WindowHandle(2)]);
        assert_eq!(
            plan.show,
            vec![WorkspaceVisibilityChange {
                window: WindowHandle(1),
                restore_rect: Some(Rect::from_size(0, 0, 100, 100)),
            }]
        );
        assert!(plan.moves.is_empty());

        assert!(!state.handle_movesize_end(WindowHandle(1), &[primary_test_monitor()], None));
        assert_eq!(state.active_interactive_drag_window(), None);
    }

    #[test]
    fn switching_workspace_while_modifier_dragging_uses_modifier_drag_window() {
        let mut state = daemon_state([
            window(1, "Dragged", Rect::from_size(0, 0, 100, 100)),
            window(2, "Stays Behind", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.active_modifier_drag = Some(ActiveModifierDrag {
            window: WindowHandle(1),
            start_cursor: Point { x: 10, y: 10 },
            last_cursor: Point { x: 10, y: 10 },
            start_rect: Rect::from_size(0, 0, 100, 100),
            move_count: 0,
            started_temporary_float: false,
        });

        let plan = state.plan_command(
            DaemonCommand::SwitchWorkspace(WorkspaceId(2)),
            &[primary_test_monitor()],
        );

        assert_eq!(
            state
                .workspaces
                .window_state(WindowHandle(1))
                .map(|state| state.workspace),
            Some(WorkspaceId(2))
        );
        assert_eq!(plan.focus, Some(WindowHandle(1)));
        assert_eq!(plan.hide, vec![WindowHandle(2)]);
        assert_eq!(
            plan.show,
            vec![WorkspaceVisibilityChange {
                window: WindowHandle(1),
                restore_rect: Some(Rect::from_size(0, 0, 100, 100)),
            }]
        );
    }

    #[test]
    fn switching_relative_workspace_while_dragging_uses_dragged_monitor_workspace() {
        let monitors = [primary_test_monitor(), secondary_test_monitor()];
        let mut state = daemon_state([
            window(1, "Primary", Rect::from_size(0, 0, 100, 100)),
            window(2, "Dragged Secondary", Rect::from_size(1100, 0, 100, 100)),
        ]);
        state.sync_workspace_state(&monitors);

        assert!(state.handle_movesize_start(WindowHandle(2), &monitors));

        let plan = state.plan_command(
            DaemonCommand::SwitchWorkspaceRelative(CycleDirection::Next),
            &monitors,
        );

        assert_eq!(
            state
                .workspaces
                .window_state(WindowHandle(2))
                .map(|state| state.workspace),
            Some(WorkspaceId(3))
        );
        assert_eq!(
            state.workspaces.active_workspace_for_monitor(MonitorId(2)),
            WorkspaceId(3)
        );
        assert_eq!(plan.focus, Some(WindowHandle(2)));
        assert!(
            plan.show
                .iter()
                .any(|change| change.window == WindowHandle(2))
        );
    }

    #[test]
    fn switching_workspace_while_dragging_fullscreen_window_does_not_move_it() {
        let mut state = daemon_state([
            window(1, "Fullscreen", primary_test_monitor().rect),
            window(2, "Stays Behind", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.active_interactive_drag = Some(active_interactive_drag(WindowHandle(1)));

        let plan = state.plan_command(
            DaemonCommand::SwitchWorkspace(WorkspaceId(2)),
            &[primary_test_monitor()],
        );

        assert_ne!(
            state
                .workspaces
                .window_state(WindowHandle(1))
                .map(|state| state.workspace),
            Some(WorkspaceId(2))
        );
        assert_ne!(plan.focus, Some(WindowHandle(1)));
        assert!(
            !plan
                .show
                .iter()
                .any(|change| change.window == WindowHandle(1))
        );
    }

    #[test]
    fn focus_monitor_next_focuses_visible_window_on_target_monitor() {
        let monitors = [primary_test_monitor(), secondary_test_monitor()];
        let mut state = daemon_state([
            window(1, "Primary", Rect::from_size(0, 0, 100, 100)),
            window(2, "Secondary", Rect::from_size(1100, 0, 100, 100)),
        ]);
        state.sync_workspace_state(&monitors);
        state.foreground = Some(WindowHandle(1));
        state.focus_monitor_for_window(WindowHandle(1), &monitors);

        let plan = state.plan_command(
            DaemonCommand::FocusMonitor(MonitorSelector::Next),
            &monitors,
        );

        assert_eq!(state.workspaces.focused_monitor(), Some(MonitorId(2)));
        assert_eq!(plan.focus, Some(WindowHandle(2)));
        assert_eq!(state.foreground, Some(WindowHandle(2)));
        assert!(plan.moves.is_empty());
    }

    #[test]
    fn move_window_to_monitor_uses_target_monitor_active_workspace() {
        let monitors = [primary_test_monitor(), secondary_test_monitor()];
        let mut state = daemon_state([
            window(1, "Move Me", Rect::from_size(0, 0, 100, 100)),
            window(2, "Stay Primary", Rect::from_size(200, 0, 100, 100)),
            window(3, "Stay Secondary", Rect::from_size(1100, 0, 100, 100)),
        ]);
        state.sync_workspace_state(&monitors);
        state.foreground = Some(WindowHandle(1));
        state.focus_monitor_for_window(WindowHandle(1), &monitors);

        let plan = state.plan_command(
            DaemonCommand::MoveFocusedToMonitor(MonitorSelector::Next),
            &monitors,
        );

        assert_eq!(
            state
                .workspaces
                .window_state(WindowHandle(1))
                .map(|state| state.workspace),
            Some(WorkspaceId(2))
        );
        assert_eq!(
            state
                .window_monitor_overrides
                .get(&WindowHandle(1))
                .copied(),
            Some(MonitorId(2))
        );
        assert_eq!(plan.focus, Some(WindowHandle(1)));
        assert_eq!(
            plan.moves,
            vec![
                TileAssignment {
                    window: WindowHandle(2),
                    rect: primary_test_monitor().work_area,
                },
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(1000, 0, 400, 560),
                },
                TileAssignment {
                    window: WindowHandle(3),
                    rect: Rect::from_size(1400, 0, 400, 560),
                },
            ]
        );
    }

    #[test]
    fn send_workspace_to_monitor_updates_window_ownership_and_focus() {
        let monitors = [primary_test_monitor(), secondary_test_monitor()];
        let mut state = daemon_state([
            window(1, "Workspace Two", Rect::from_size(0, 0, 100, 100)),
            window(2, "Workspace One", Rect::from_size(200, 0, 100, 100)),
        ]);
        state
            .workspaces
            .move_window_to_workspace(WindowHandle(1), WorkspaceId(2));
        state.set_window_participation(WindowHandle(1), WindowParticipation::Floating);

        let plan = state.plan_command(
            DaemonCommand::SendWorkspaceToMonitor {
                workspace: WorkspaceId(2),
                monitor: MonitorSelector::Index(2),
            },
            &monitors,
        );

        assert_eq!(
            state
                .window_monitor_overrides
                .get(&WindowHandle(1))
                .copied(),
            Some(MonitorId(2))
        );
        assert_eq!(
            state.workspaces.active_workspace_for_monitor(MonitorId(2)),
            WorkspaceId(2)
        );
        assert_eq!(state.workspaces.focused_monitor(), Some(MonitorId(2)));
        assert_eq!(plan.focus, Some(WindowHandle(1)));
        assert_eq!(
            plan.show,
            vec![WorkspaceVisibilityChange {
                window: WindowHandle(1),
                restore_rect: Some(Rect::from_size(0, 0, 100, 100)),
            }]
        );
        assert_eq!(
            plan.moves,
            vec![
                TileAssignment {
                    window: WindowHandle(2),
                    rect: primary_test_monitor().work_area,
                },
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(1000, 0, 100, 100),
                },
            ]
        );
        assert_eq!(
            state.window_participation(WindowHandle(1)),
            WindowParticipation::Floating
        );
    }

    #[test]
    fn move_window_to_workspace_and_follow_keeps_floating_state_and_focuses_window() {
        let mut state = daemon_state([
            window(1, "Floating", Rect::from_size(0, 0, 100, 100)),
            window(2, "Other", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.foreground = Some(WindowHandle(1));
        state.set_window_participation(WindowHandle(1), WindowParticipation::Floating);

        let plan = state.plan_command(
            DaemonCommand::MoveFocusedToWorkspaceAndFollow(WorkspaceId(2)),
            &[primary_test_monitor()],
        );

        assert_eq!(state.workspaces.active_workspace(), WorkspaceId(2));
        assert_eq!(state.foreground, Some(WindowHandle(1)));
        assert_eq!(plan.focus, Some(WindowHandle(1)));
        assert_eq!(plan.hide, vec![WindowHandle(2)]);
        assert_eq!(
            plan.show,
            vec![WorkspaceVisibilityChange {
                window: WindowHandle(1),
                restore_rect: Some(Rect::from_size(0, 0, 100, 100)),
            }]
        );
        assert_eq!(
            state.window_participation(WindowHandle(1)),
            WindowParticipation::Floating
        );
        assert!(plan.moves.is_empty());
    }

    #[test]
    fn inactive_workspace_windows_are_not_bordered() {
        let mut state = daemon_state([
            window(1, "Active", Rect::from_size(0, 0, 100, 100)),
            window(2, "Hidden", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.config.borders.enabled = true;
        state
            .workspaces
            .move_window_to_workspace(WindowHandle(2), WorkspaceId(2));

        let candidates = state.border_candidates(&[primary_test_monitor()]);

        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.window)
                .collect::<Vec<_>>(),
            vec![WindowHandle(1)]
        );
    }

    #[test]
    fn floating_z_order_raises_floating_temporary_and_overflow_windows() {
        let mut state = daemon_state([
            window(1, "Tiled", Rect::from_size(0, 0, 100, 100)),
            window(2, "Floating", Rect::from_size(200, 0, 100, 100)),
            window(3, "Temporary", Rect::from_size(400, 0, 100, 100)),
            window(4, "Overflow", Rect::from_size(600, 0, 100, 100)),
        ]);
        state.set_window_participation(WindowHandle(2), WindowParticipation::Floating);
        state.set_window_participation(WindowHandle(3), WindowParticipation::TemporarilyFloating);
        state.promote_overflow_window_to_floating(WindowHandle(4));
        state.foreground = Some(WindowHandle(3));

        let windows = state.floating_z_order_windows(&[primary_test_monitor()]);

        assert_eq!(
            windows,
            vec![WindowHandle(2), WindowHandle(4), WindowHandle(3)]
        );
    }

    #[test]
    fn floating_z_order_stays_above_focused_tiled_window() {
        let mut state = daemon_state([
            window(1, "Focused Tiled", Rect::from_size(0, 0, 100, 100)),
            window(2, "Floating", Rect::from_size(200, 0, 100, 100)),
            window(3, "Overflow", Rect::from_size(400, 0, 100, 100)),
        ]);
        state.foreground = Some(WindowHandle(1));
        state.set_window_participation(WindowHandle(2), WindowParticipation::Floating);
        state.promote_overflow_window_to_floating(WindowHandle(3));

        let windows = state.floating_z_order_windows(&[primary_test_monitor()]);

        assert_eq!(windows, vec![WindowHandle(2), WindowHandle(3)]);
    }

    #[test]
    fn floating_z_order_excludes_inactive_workspace_windows() {
        let mut state = daemon_state([window(1, "Hidden", Rect::from_size(0, 0, 100, 100))]);
        state.set_window_participation(WindowHandle(1), WindowParticipation::Floating);
        state
            .workspaces
            .move_window_to_workspace(WindowHandle(1), WorkspaceId(2));

        let hidden = state.floating_z_order_windows(&[primary_test_monitor()]);
        state.workspaces.switch_to(WorkspaceId(2));
        let shown = state.floating_z_order_windows(&[primary_test_monitor()]);

        assert!(hidden.is_empty());
        assert_eq!(shown, vec![WindowHandle(1)]);
    }

    #[test]
    fn floating_z_order_respects_focused_monitor_game_mode_pause() {
        let monitors = [primary_test_monitor(), secondary_test_monitor()];
        let mut state = daemon_state([
            window(1, "Fullscreen Game", monitors[0].rect),
            window(2, "Primary Floating", Rect::from_size(200, 0, 100, 100)),
            window(3, "Secondary Floating", Rect::from_size(1100, 0, 100, 100)),
        ]);
        state.foreground = Some(WindowHandle(1));
        state.config.game_mode.pause_focused_monitor_only = true;
        state.set_window_participation(WindowHandle(2), WindowParticipation::Floating);
        state.set_window_participation(WindowHandle(3), WindowParticipation::Floating);

        state.update_game_mode(&monitors, "test").unwrap();
        let windows = state.floating_z_order_windows(&monitors);

        assert_eq!(windows, vec![WindowHandle(3)]);
    }

    #[test]
    fn state_snapshot_reports_monitor_workspace_and_window_visibility() {
        let monitors = [primary_test_monitor(), secondary_test_monitor()];
        let mut state = daemon_state([
            window(1, "Primary", Rect::from_size(0, 0, 100, 100)),
            window(2, "Secondary", Rect::from_size(1100, 0, 100, 100)),
        ]);
        state.sync_workspace_state(&monitors);
        state.foreground = Some(WindowHandle(2));
        state.focus_monitor_for_window(WindowHandle(2), &monitors);

        let snapshot = state.state_snapshot_with_monitors(&monitors);

        assert_eq!(
            snapshot.monitors,
            vec![
                MonitorStateSnapshot {
                    monitor_id: 1,
                    workspace_id: 1,
                    focused: false,
                },
                MonitorStateSnapshot {
                    monitor_id: 2,
                    workspace_id: 2,
                    focused: true,
                },
            ]
        );
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.windows[0].monitor_id, Some(1));
        assert_eq!(snapshot.windows[0].workspace_id, Some(1));
        assert!(snapshot.windows[0].visible_on_active_workspace);
        assert_eq!(snapshot.windows[1].monitor_id, Some(2));
        assert_eq!(snapshot.windows[1].workspace_id, Some(2));
        assert!(snapshot.windows[1].focused);
        assert!(snapshot.windows[1].visible_on_active_workspace);
    }

    #[test]
    fn work_area_sized_window_on_inactive_workspace_is_not_hidden() {
        let mut state = daemon_state([window(1, "One", primary_test_monitor().work_area)]);
        state
            .workspaces
            .move_window_to_workspace(WindowHandle(1), WorkspaceId(2));
        state.workspaces.switch_to(WorkspaceId(2));

        let plan = state.plan_command(
            DaemonCommand::SwitchWorkspace(WorkspaceId(1)),
            &[primary_test_monitor()],
        );

        assert_eq!(plan.hide, Vec::<WindowHandle>::new());
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
    fn duplicate_window_events_are_coalesced_to_last_observation() {
        let batch = [
            event(WindowEventKind::Shown, 1),
            event(WindowEventKind::Hidden, 2),
            event(WindowEventKind::Shown, 1),
            event(WindowEventKind::Moved, 1),
            event(WindowEventKind::Moved, 1),
        ];

        let coalesced = coalesce_window_events(&batch);

        assert_eq!(
            coalesced,
            vec![
                event(WindowEventKind::Hidden, 2),
                event(WindowEventKind::Shown, 1),
                event(WindowEventKind::Moved, 1),
            ]
        );
    }

    #[test]
    fn no_op_relayout_is_counted_without_calling_win32_move() {
        let monitor = primary_test_monitor();
        let mut state = daemon_state([window(1, "Editor", monitor.work_area)]);
        let assignments = vec![TileAssignment {
            window: WindowHandle(1),
            rect: monitor.work_area,
        }];

        state.apply_tile_assignments_with_feedback(
            &assignments,
            std::slice::from_ref(&monitor),
            "test no-op relayout",
        );

        assert_eq!(state.perf.relayout_count, 0);
        assert_eq!(state.perf.skipped_relayout_count, 1);
        assert_eq!(state.perf.last_relayout_move_count, 0);
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

        let IpcResponse {
            result: winland_ipc::IpcResponseResult::State(snapshot),
            ..
        } = response
        else {
            panic!("expected state response");
        };

        assert_eq!(snapshot.total_windows, 2);
        assert_eq!(snapshot.manageable_windows, 2);
        assert_eq!(snapshot.floating_windows, 1);
        assert_eq!(snapshot.temporary_floating_windows, 0);
        assert_eq!(snapshot.active_workspace, 2);
        assert_eq!(snapshot.foreground_window, Some(2));
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(
            snapshot.windows[0].participation,
            WindowParticipationSnapshot::Floating
        );
        assert!(snapshot.windows[1].focused);
    }

    #[test]
    fn state_snapshot_reports_constrained_floating_and_hidden_workspace_status() {
        let mut constrained = window(1, "Constrained", Rect::from_size(0, 0, 100, 100));
        constrained.size_constraints = WindowSizeConstraints::minimum(500, 300);
        let mut state = daemon_state([
            constrained,
            window(2, "Overflow", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.promote_overflow_window_to_floating(WindowHandle(2));
        state
            .workspaces
            .move_window_to_workspace(WindowHandle(2), WorkspaceId(2));

        let snapshot = state.state_snapshot_with_monitors(&[primary_test_monitor()]);

        assert_eq!(snapshot.windows.len(), 2);
        assert!(snapshot.windows[0].constrained);
        assert_eq!(
            snapshot.windows[1].participation,
            WindowParticipationSnapshot::Floating
        );
        assert!(!snapshot.windows[1].visible_on_active_workspace);
    }

    #[test]
    fn config_diff_reports_reload_sections() {
        let old = Config::default();
        let mut new = old.clone();
        new.layout.gap = 8;
        new.borders.enabled = true;
        new.game_mode.game_exes.push("game.exe".to_owned());

        let diff = ConfigDiff::between(&old, &new);

        assert_eq!(diff.changed_sections, vec!["gaps", "borders", "game-mode"]);
    }

    #[test]
    fn config_diff_reports_rule_only_reload_section() {
        let old = Config::default();
        let mut new = old.clone();
        new.window_rules.push(winland_config::WindowRuleConfig {
            name: Some("game".to_owned()),
            matcher: winland_config::WindowRuleMatchConfig {
                process_name: Some(winland_config::TextMatcherConfig::Exact(
                    "game.exe".to_owned(),
                )),
                ..winland_config::WindowRuleMatchConfig::default()
            },
            action: winland_config::WindowRuleActionConfig {
                mode: Some(winland_config::WindowRuleModeConfig::Game),
                ..winland_config::WindowRuleActionConfig::default()
            },
        });

        let diff = ConfigDiff::between(&old, &new);

        assert_eq!(diff.changed_sections, vec!["window-rules"]);
    }

    #[test]
    fn rule_reload_updates_workspace_and_respects_explicit_float_state() {
        let mut state = daemon_state([
            window(1, "Editor", Rect::from_size(0, 0, 100, 100)),
            window(2, "Settings", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.set_window_participation(WindowHandle(2), WindowParticipation::Floating);
        state.config.window_rules = vec![WindowRule {
            name: "settings to workspace two".to_owned(),
            matcher: winland_core::WindowRuleMatch {
                title: Some(winland_core::TextMatcher::Exact("Settings".to_owned())),
                ..winland_core::WindowRuleMatch::default()
            },
            action: winland_core::WindowRuleAction {
                target_workspace: Some(WorkspaceId(2)),
                ..winland_core::WindowRuleAction::default()
            },
        }];

        state.reapply_window_rules_after_reload(&[primary_test_monitor()]);

        assert_eq!(
            state
                .workspaces
                .window_state(WindowHandle(2))
                .unwrap()
                .workspace,
            WorkspaceId(2)
        );
        assert_eq!(
            state.window_participation(WindowHandle(2)),
            WindowParticipation::Floating
        );
    }

    #[test]
    fn rule_reload_only_tiles_floating_window_when_rule_says_so() {
        let mut state = daemon_state([window(1, "Settings", Rect::from_size(0, 0, 100, 100))]);
        state.set_window_participation(WindowHandle(1), WindowParticipation::Floating);
        state.config.window_rules = vec![WindowRule {
            name: "force settings tiled".to_owned(),
            matcher: winland_core::WindowRuleMatch {
                title: Some(winland_core::TextMatcher::Exact("Settings".to_owned())),
                ..winland_core::WindowRuleMatch::default()
            },
            action: winland_core::WindowRuleAction {
                float: Some(false),
                ..winland_core::WindowRuleAction::default()
            },
        }];

        state.reapply_window_rules_after_reload(&[primary_test_monitor()]);

        assert_eq!(
            state.window_participation(WindowHandle(1)),
            WindowParticipation::Tiled
        );
    }

    #[test]
    fn rule_reload_removes_ignored_windows_from_tiling_order() {
        let mut state = daemon_state([
            window(1, "Editor", Rect::from_size(0, 0, 100, 100)),
            window(2, "Tool", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.config.window_rules = vec![WindowRule {
            name: "ignore tool".to_owned(),
            matcher: winland_core::WindowRuleMatch {
                title: Some(winland_core::TextMatcher::Exact("Tool".to_owned())),
                ..winland_core::WindowRuleMatch::default()
            },
            action: winland_core::WindowRuleAction {
                manage: Some(false),
                ..winland_core::WindowRuleAction::default()
            },
        }];

        let stats = state.reapply_window_rules_after_reload(&[primary_test_monitor()]);

        assert_eq!(stats.removed_from_tile_order, 1);
        assert_eq!(state.tile_order, vec![WindowHandle(1)]);
        assert!(state.workspaces.window_state(WindowHandle(2)).is_none());
    }

    #[test]
    fn rule_reload_removes_game_mode_windows_from_tiling_order() {
        let mut state = daemon_state([
            window(1, "Editor", Rect::from_size(0, 0, 100, 100)),
            window(2, "Game", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.config.window_rules = vec![WindowRule {
            name: "game mode".to_owned(),
            matcher: winland_core::WindowRuleMatch {
                title: Some(winland_core::TextMatcher::Exact("Game".to_owned())),
                ..winland_core::WindowRuleMatch::default()
            },
            action: winland_core::WindowRuleAction {
                mode: Some(WindowRuleMode::Game),
                ..winland_core::WindowRuleAction::default()
            },
        }];

        let stats = state.reapply_window_rules_after_reload(&[primary_test_monitor()]);

        assert_eq!(stats.removed_from_tile_order, 1);
        assert_eq!(state.tile_order, vec![WindowHandle(1)]);
        assert!(state.workspaces.window_state(WindowHandle(2)).is_none());
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
    fn border_updates_layer_floating_borders_above_focused_tiled_borders() {
        let mut state = daemon_state([
            window(1, "Focused Tiled", Rect::from_size(0, 0, 300, 300)),
            window(2, "Floating", Rect::from_size(200, 0, 300, 300)),
        ]);
        state.config.borders.enabled = true;
        state.foreground = Some(WindowHandle(1));
        state.set_window_participation(WindowHandle(2), WindowParticipation::Floating);

        let updates = state.border_updates(&[primary_test_monitor()]);

        assert_eq!(
            updates
                .iter()
                .map(|update| update.window)
                .collect::<Vec<_>>(),
            vec![WindowHandle(1), WindowHandle(2)]
        );
    }

    #[test]
    fn border_updates_layer_focused_floating_border_above_other_floating_borders() {
        let mut state = daemon_state([
            window(1, "Floating", Rect::from_size(0, 0, 300, 300)),
            window(2, "Focused Floating", Rect::from_size(200, 0, 300, 300)),
            window(3, "Tiled", Rect::from_size(400, 0, 300, 300)),
        ]);
        state.config.borders.enabled = true;
        state.foreground = Some(WindowHandle(2));
        state.set_window_participation(WindowHandle(1), WindowParticipation::Floating);
        state.set_window_participation(WindowHandle(2), WindowParticipation::Floating);

        let updates = state.border_updates(&[primary_test_monitor()]);

        assert_eq!(
            updates
                .iter()
                .map(|update| update.window)
                .collect::<Vec<_>>(),
            vec![WindowHandle(3), WindowHandle(1), WindowHandle(2)]
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

    #[test]
    fn border_candidates_hide_all_while_game_mode_disables_borders() {
        let mut game = window(1, "Configured Game", Rect::from_size(0, 0, 100, 100));
        game.executable_path = Some(r"C:\Games\game.exe".to_owned());
        let mut state =
            daemon_state([game, window(2, "Editor", Rect::from_size(200, 0, 100, 100))]);
        state.config.borders.enabled = true;
        state.config.game_mode.policy.game_exes = vec!["game.exe".to_owned()];
        state.config.game_mode.disable_borders = true;
        state.foreground = Some(WindowHandle(1));

        state
            .update_game_mode(&[primary_test_monitor()], "test")
            .unwrap();
        let candidates = state.border_candidates(&[primary_test_monitor()]);

        assert!(candidates.is_empty());
    }

    #[test]
    fn game_mode_activates_for_focused_fullscreen_window_and_pauses_layout() {
        let monitor = primary_test_monitor();
        let mut state = daemon_state([
            window(1, "Fullscreen Game", monitor.rect),
            window(2, "Editor", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.foreground = Some(WindowHandle(1));

        let transition = state
            .update_game_mode(std::slice::from_ref(&monitor), "test")
            .unwrap();
        let assignments = state.tile_assignments(std::slice::from_ref(&monitor));

        assert!(transition.activated);
        assert!(state.game_mode.active.is_some());
        assert!(assignments.is_empty());
    }

    #[test]
    fn focused_monitor_game_mode_keeps_other_monitors_tiling() {
        let monitors = [primary_test_monitor(), secondary_test_monitor()];
        let mut state = daemon_state([
            window(1, "Fullscreen Game", monitors[0].rect),
            window(2, "Secondary Editor", Rect::from_size(1100, 0, 100, 100)),
        ]);
        state.foreground = Some(WindowHandle(1));
        state.config.game_mode.pause_focused_monitor_only = true;

        let transition = state.update_game_mode(&monitors, "test").unwrap();
        let assignments = state.tile_assignments(&monitors);

        assert!(transition.activated);
        assert_eq!(
            state
                .game_mode
                .active
                .as_ref()
                .and_then(|active| active.monitor),
            Some(monitors[0].id)
        );
        assert_eq!(
            assignments,
            vec![TileAssignment {
                window: WindowHandle(2),
                rect: monitors[1].work_area,
            }]
        );
    }

    #[test]
    fn configured_game_executable_is_not_manageable_or_tiled() {
        let monitor = primary_test_monitor();
        let mut game = window(1, "Configured Game", Rect::from_size(0, 0, 100, 100));
        game.executable_path = Some(r"C:\Games\cs2.exe".to_owned());
        let mut state =
            daemon_state([game, window(2, "Editor", Rect::from_size(200, 0, 100, 100))]);
        state.config.game_mode.policy.game_exes = vec!["cs2.exe".to_owned()];
        state.tile_order = state.manageable_handles_sorted();
        state.sync_workspace_state(std::slice::from_ref(&monitor));

        let assignments = state
            .plan_command(DaemonCommand::Retile, std::slice::from_ref(&monitor))
            .moves;

        assert!(!state.is_manageable_window(WindowHandle(1), std::slice::from_ref(&monitor)));
        assert_eq!(
            assignments,
            vec![TileAssignment {
                window: WindowHandle(2),
                rect: monitor.work_area,
            }]
        );
    }

    #[test]
    fn fullscreen_game_mode_exit_waits_for_confirmation_then_retiles() {
        let monitor = primary_test_monitor();
        let mut state = daemon_state([
            window(1, "Former Game", Rect::from_size(0, 0, 100, 100)),
            window(2, "Editor", Rect::from_size(200, 0, 100, 100)),
        ]);
        state.foreground = Some(WindowHandle(1));
        state.game_mode.active = Some(GameModeActivation {
            window: WindowHandle(1),
            title: "Former Game".to_owned(),
            executable_path: Some(r"C:\Games\game.exe".to_owned()),
            monitor: Some(monitor.id),
            reason: GameModeReason::Fullscreen {
                monitor: monitor.id,
                area: FullscreenArea::MonitorBounds,
            },
            actions: GameModeActions {
                pause_layouts: true,
                hide_borders: true,
                ..GameModeActions::default()
            },
            fullscreen: FullscreenDetection {
                is_fullscreen: true,
                monitor: Some(monitor.id),
                area: Some(FullscreenArea::MonitorBounds),
            },
        });

        let first_transition = state
            .update_game_mode(std::slice::from_ref(&monitor), "test")
            .unwrap();
        assert!(!first_transition.deactivated);
        assert!(state.game_mode.active.is_some());

        let transition = state
            .update_game_mode(std::slice::from_ref(&monitor), "test")
            .unwrap();
        let assignments = state.tile_assignments(std::slice::from_ref(&monitor));

        assert!(transition.deactivated);
        assert_eq!(assignments.len(), 2);
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
            overflow_promoted_floating: BTreeSet::new(),
            workspaces: WorkspaceManager::new(9),
            active_modifier_drag: None,
            active_interactive_drag: None,
            suppressed_modifier_drag_events: BTreeSet::new(),
            config: RuntimeConfig::default(),
            source_config: Config::default(),
            config_path: None,
            config_version: 1,
            config_loaded_at: UNIX_EPOCH,
            hotkey_commands: HotkeyCommandMap::default(),
            hotkey_backend: None,
            hotkey_sender: None,
            modifier_drag: None,
            mouse_drag_sender: None,
            border_manager: None,
            game_mode: GameModeRuntimeState::default(),
            perf: DaemonPerformance::default(),
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

    fn active_interactive_drag(window: WindowHandle) -> ActiveInteractiveDrag {
        ActiveInteractiveDrag {
            window,
            start_cursor: Point { x: 0, y: 0 },
            start_rect: Rect::from_size(0, 0, 100, 100),
            monitors: vec![primary_test_monitor()],
        }
    }
}
