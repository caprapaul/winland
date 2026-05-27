use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;
use winland_config::{Config, HotkeyKey, HotkeyMode, HotkeyModifier, TextMatcherConfig};
use winland_core::{
    LayoutConfig, MonitorInfo, Rect, TileAssignment, WindowHandle, WindowInfo, WindowParticipation,
    WindowRule, WindowRuleDecision, WorkspaceId, WorkspaceManager, WorkspaceVisibilityChange,
    evaluate_window_rules, tile_windows_with_config,
};
use winland_ipc::{
    DaemonStateSnapshot, IpcCommand, IpcRequest, IpcResponse, decode_request, encode_response,
};
use winland_win32::{
    HotkeyBinding, HotkeyBypassRules, HotkeyEvent, HotkeyId, HotkeyLowLevelEvent,
    HotkeyModifierSet, HotkeyOverrideOptions, IpcTransportRequest, VirtualKey, WindowEvent,
    WindowEventKind,
};

const RECONCILE_DEBOUNCE: Duration = Duration::from_millis(50);
const MAX_BATCH_SIZE: usize = 512;
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
    drop(daemon_sender);

    let mut state = DaemonState::discover(runtime_config, hotkey_commands)
        .context("build initial window snapshot")?;
    state.apply_startup_retile()?;
    let processor = thread::Builder::new()
        .name("winland-daemon-events".to_owned())
        .spawn(move || process_daemon_events(daemon_receiver, state))
        .context("spawn daemon event processor")?;

    info!("winland daemon started; entering Win32 message loop");
    let message_loop_result =
        winland_win32::run_message_loop().context("run Win32 daemon message loop");

    drop(hotkey_backend);
    drop(subscription);
    join_bridge(window_bridge, "window event bridge")?;
    join_bridge(hotkey_bridge, "hotkey bridge")?;

    match processor.join() {
        Ok(Ok(())) => message_loop_result,
        Ok(Err(error)) => Err(error).context("process daemon events"),
        Err(_) => Err(anyhow!("daemon event processor thread panicked")),
    }
}

fn init_tracing(default_level: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
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
    workspace_count: u16,
    window_rules: Vec<WindowRule>,
    startup_retile: bool,
    dynamic_retile: bool,
    drag_to_float: bool,
    retile_on_drag_end: bool,
}

impl RuntimeConfig {
    fn from_config(config: &Config) -> Result<Self> {
        Ok(Self {
            layout: config.layout_config(),
            workspace_count: config.workspace_count(),
            window_rules: config.window_rules().context("convert window rules")?,
            startup_retile: config.behavior.startup_retile,
            dynamic_retile: config.behavior.dynamic_retile,
            drag_to_float: config.behavior.drag_to_float,
            retile_on_drag_end: config.behavior.retile_on_drag_end,
        })
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self::from_config(&Config::default()).expect("built-in config defaults are valid")
    }
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
                let batch = receive_window_batch(&receiver, &mut state, first_event)?;
                state.reconcile_after_events(&batch)?;
            }
            DaemonEvent::Hotkey(event) => state.handle_hotkey(event)?,
            DaemonEvent::Ipc(request) => state.handle_ipc(request),
        }
    }

    info!("daemon event channel closed; event processor stopping");
    Ok(())
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
    Ipc(IpcTransportRequest),
}

#[derive(Debug)]
struct DaemonState {
    windows: BTreeMap<WindowHandle, WindowInfo>,
    foreground: Option<WindowHandle>,
    tile_order: Vec<WindowHandle>,
    participation: BTreeMap<WindowHandle, WindowParticipation>,
    previous_rects: BTreeMap<WindowHandle, Rect>,
    workspaces: WorkspaceManager,
    config: RuntimeConfig,
    hotkey_commands: HotkeyCommandMap,
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
            previous_rects: BTreeMap::new(),
            workspaces: WorkspaceManager::new(config.workspace_count),
            config,
            hotkey_commands,
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
        apply_tile_assignments(&assignments, "startup retile");
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

        let mut refreshed = Self::discover(self.config.clone(), self.hotkey_commands.clone())
            .context("refresh window snapshot after event batch")?;
        let monitors = winland_win32::enumerate_monitors()
            .context("enumerate monitors while preserving daemon state")?;
        let diff = self.diff(&refreshed, &monitors);
        self.preserve_keyboard_state(&mut refreshed, &monitors);
        *self = refreshed;
        let event_plan = self.plan_after_window_events(batch, &diff, &monitors);
        apply_tile_assignments(&event_plan.moves, "dynamic retile");

        info!(
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
            *self = refreshed;
            self.log_snapshot("reloaded daemon state");
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

        for assignment in &plan.moves {
            if let Err(error) =
                winland_win32::move_resize_window(assignment.window, assignment.rect)
            {
                warn!(
                    window = %assignment.window,
                    rect = %assignment.rect,
                    %error,
                    "failed to move window during hotkey command"
                );
            }
        }

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
                        if self.start_temporary_float(event.window) {
                            should_retile = should_retile || self.config.dynamic_retile;
                        }
                    }
                    WindowEventKind::MoveSizeEnd => {
                        if self.window_participation(event.window)
                            == WindowParticipation::TemporarilyFloating
                        {
                            self.reorder_temporary_float_by_drop(event.window, monitors);
                        }

                        if self.clear_temporary_float(event.window)
                            && self.config.retile_on_drag_end
                        {
                            should_retile = true;
                        }
                    }
                    _ => {}
                }
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
            )
        }) || diff.moved_between_monitors > 0
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
        let Some(target_monitor) = monitor_for_rect(window_info.rect, monitors) else {
            return false;
        };
        let Some(monitor) = monitors.iter().find(|monitor| monitor.id == target_monitor) else {
            return false;
        };

        let target_handles: Vec<_> = self
            .tile_order
            .iter()
            .copied()
            .filter(|handle| *handle != window)
            .filter(|handle| self.window_participation(*handle).is_tiled())
            .filter(|handle| {
                self.windows.get(handle).is_some_and(|candidate| {
                    self.is_tilable_window(*handle)
                        && monitor
                            .rect
                            .contains(self.window_layout_rect(*handle, candidate).center())
                })
            })
            .collect();
        let local_index =
            self.drop_insert_index(window, window_info.rect, &target_handles, monitor);

        self.reinsert_window_at_local_index(window, &target_handles, local_index)
    }

    fn drop_insert_index(
        &self,
        window: WindowHandle,
        dropped_rect: Rect,
        target_handles: &[WindowHandle],
        monitor: &MonitorInfo,
    ) -> usize {
        let dropped_center = dropped_rect.center();

        (0..=target_handles.len())
            .filter_map(|index| {
                let mut handles = target_handles.to_vec();
                handles.insert(index, window);
                let assignment =
                    tile_windows_with_config(monitor.work_area, &handles, self.config.layout)
                        .into_iter()
                        .find(|assignment| assignment.window == window)?;
                let center = assignment.rect.center();
                let dx = i64::from(center.x - dropped_center.x);
                let dy = i64::from(center.y - dropped_center.y);
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

    fn tile_assignments(&self, monitors: &[MonitorInfo]) -> Vec<TileAssignment> {
        monitors
            .iter()
            .flat_map(|monitor| {
                let handles: Vec<_> = self
                    .tile_order
                    .iter()
                    .copied()
                    .filter(|handle| self.window_participation(*handle).is_tiled())
                    .filter(|handle| {
                        self.windows.get(handle).is_some_and(|window| {
                            self.is_tilable_window(*handle)
                                && monitor
                                    .rect
                                    .contains(self.window_layout_rect(*handle, window).center())
                        })
                    })
                    .collect();

                tile_windows_with_config(monitor.work_area, &handles, self.config.layout)
            })
            .collect()
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
        refreshed.previous_rects = self
            .previous_rects
            .iter()
            .filter(|(handle, _)| known_workspace_windows.contains(handle))
            .map(|(handle, rect)| (*handle, *rect))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonCommand {
    Focus(FocusDirection),
    Swap(FocusDirection),
    Retile,
    ToggleFloat,
    SwitchWorkspace(WorkspaceId),
    MoveFocusedToWorkspace(WorkspaceId),
    Reload,
    Quit,
}

impl DaemonCommand {
    fn needs_layout(self) -> bool {
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

fn count_events(batch: &[WindowEvent], kind: WindowEventKind) -> usize {
    batch.iter().filter(|event| event.kind == kind).count()
}

#[derive(Debug, Clone, Default)]
struct HotkeyCommandMap {
    commands: BTreeMap<HotkeyId, DaemonCommand>,
}

impl HotkeyCommandMap {
    fn command(&self, id: HotkeyId) -> Option<DaemonCommand> {
        self.commands.get(&id).copied()
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

fn hotkey_bindings_from_config(config: &Config) -> Result<(Vec<HotkeyBinding>, HotkeyCommandMap)> {
    let mut bindings = Vec::with_capacity(config.hotkeys.bindings.len());
    let mut commands = BTreeMap::new();

    for (index, binding_config) in config.hotkeys.bindings.iter().enumerate() {
        let id = HotkeyId((index + 1) as i32);
        let command = daemon_command_from_name(&binding_config.command)
            .with_context(|| format!("map hotkey command '{}'", binding_config.command))?;
        let chord = binding_config
            .chord()
            .with_context(|| format!("parse hotkey '{}'", binding_config.keys))?;
        bindings.push(
            HotkeyBinding::new(
                id,
                hotkey_modifiers_from_config(&chord),
                virtual_key_from_config(&chord.key),
                binding_config.command.clone(),
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
    let mut modifiers = HotkeyModifierSet::new();
    for modifier in &chord.modifiers {
        modifiers = match modifier {
            HotkeyModifier::Alt => modifiers.alt(),
            HotkeyModifier::Control => modifiers.control(),
            HotkeyModifier::Shift => modifiers.shift(),
            HotkeyModifier::Super => modifiers.super_key(),
        };
    }
    modifiers
}

fn virtual_key_from_config(key: &HotkeyKey) -> VirtualKey {
    match key {
        HotkeyKey::Character(ch) => VirtualKey::ascii_uppercase(*ch as u8),
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
    if key == VirtualKey::ESCAPE {
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

fn apply_tile_assignments(assignments: &[TileAssignment], operation: &'static str) {
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
    monitors
        .iter()
        .any(|monitor| window.rect == monitor.rect || window.rect == monitor.work_area)
}

fn monitor_for_rect(rect: Rect, monitors: &[MonitorInfo]) -> Option<winland_core::MonitorId> {
    let center = rect.center();
    monitors
        .iter()
        .find(|monitor| monitor.rect.contains(center))
        .map(|monitor| monitor.id)
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
            previous_rects: BTreeMap::new(),
            workspaces: WorkspaceManager::new(9),
            config: RuntimeConfig::default(),
            hotkey_commands: HotkeyCommandMap::default(),
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
            rect,
        }
    }
}
