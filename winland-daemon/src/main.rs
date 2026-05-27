use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;
use winland_core::{
    MonitorInfo, TileAssignment, WindowHandle, WindowInfo, WorkspaceId, WorkspaceManager,
    WorkspaceVisibilityChange, tile_windows,
};
use winland_win32::{
    HotkeyBinding, HotkeyEvent, HotkeyId, HotkeyModifierSet, VirtualKey, WindowEvent,
    WindowEventKind,
};

const RECONCILE_DEBOUNCE: Duration = Duration::from_millis(50);
const MAX_BATCH_SIZE: usize = 512;
const DEFAULT_WORKSPACE_COUNT: u16 = 9;

const HOTKEY_FOCUS_LEFT: HotkeyId = HotkeyId(1);
const HOTKEY_FOCUS_DOWN: HotkeyId = HotkeyId(2);
const HOTKEY_FOCUS_UP: HotkeyId = HotkeyId(3);
const HOTKEY_FOCUS_RIGHT: HotkeyId = HotkeyId(4);
const HOTKEY_SWAP_LEFT: HotkeyId = HotkeyId(5);
const HOTKEY_SWAP_DOWN: HotkeyId = HotkeyId(6);
const HOTKEY_SWAP_UP: HotkeyId = HotkeyId(7);
const HOTKEY_SWAP_RIGHT: HotkeyId = HotkeyId(8);
const HOTKEY_RETILE: HotkeyId = HotkeyId(9);
const HOTKEY_TOGGLE_FLOAT: HotkeyId = HotkeyId(10);
const HOTKEY_RELOAD: HotkeyId = HotkeyId(11);
const HOTKEY_QUIT: HotkeyId = HotkeyId(12);
const HOTKEY_SWITCH_WORKSPACE_BASE: i32 = 20;
const HOTKEY_MOVE_TO_WORKSPACE_BASE: i32 = 40;

fn main() -> Result<()> {
    init_tracing();

    let (daemon_sender, daemon_receiver) = mpsc::channel();
    let (window_sender, window_receiver) = mpsc::channel();
    let subscription = winland_win32::subscribe_window_events(window_sender)
        .context("install documented Win32 window event hooks")?;
    let window_bridge = spawn_window_bridge(window_receiver, daemon_sender.clone())
        .context("spawn window event bridge")?;

    let (hotkey_sender, hotkey_receiver) = mpsc::channel();
    let hotkey_bindings = default_hotkey_bindings();
    let hotkey_registration =
        winland_win32::register_hotkeys(hotkey_bindings.clone(), hotkey_sender)
            .context("register documented Win32 daemon hotkeys")?;
    log_hotkey_registration(&hotkey_bindings, &hotkey_registration);
    let hotkey_bridge = spawn_hotkey_bridge(hotkey_receiver, daemon_sender.clone())
        .context("spawn hotkey bridge")?;
    drop(daemon_sender);

    let state = DaemonState::discover().context("build initial window snapshot")?;
    let processor = thread::Builder::new()
        .name("winland-daemon-events".to_owned())
        .spawn(move || process_daemon_events(daemon_receiver, state))
        .context("spawn daemon event processor")?;

    info!("winland daemon started; entering Win32 message loop");
    let message_loop_result =
        winland_win32::run_message_loop().context("run Win32 daemon message loop");

    drop(hotkey_registration);
    drop(subscription);
    join_bridge(window_bridge, "window event bridge")?;
    join_bridge(hotkey_bridge, "hotkey bridge")?;

    match processor.join() {
        Ok(Ok(())) => message_loop_result,
        Ok(Err(error)) => Err(error).context("process daemon events"),
        Err(_) => Err(anyhow!("daemon event processor thread panicked")),
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
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
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(batch)
}

#[derive(Debug)]
enum DaemonEvent {
    Window(WindowEvent),
    Hotkey(HotkeyEvent),
}

#[derive(Debug)]
struct DaemonState {
    windows: BTreeMap<WindowHandle, WindowInfo>,
    foreground: Option<WindowHandle>,
    tile_order: Vec<WindowHandle>,
    floating: BTreeSet<WindowHandle>,
    workspaces: WorkspaceManager,
}

impl DaemonState {
    fn discover() -> Result<Self> {
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
            floating: BTreeSet::new(),
            workspaces: WorkspaceManager::new(DEFAULT_WORKSPACE_COUNT),
        };
        state.tile_order = state.manageable_handles_sorted();
        state.sync_workspace_state(&monitors);

        Ok(state)
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

        let mut refreshed =
            Self::discover().context("refresh window snapshot after event batch")?;
        let diff = self.diff(&refreshed);
        let monitors = winland_win32::enumerate_monitors()
            .context("enumerate monitors while preserving daemon state")?;
        self.preserve_keyboard_state(&mut refreshed, &monitors);
        *self = refreshed;

        info!(
            event_count = batch.len(),
            created_events = count_events(batch, WindowEventKind::Created),
            destroyed_events = count_events(batch, WindowEventKind::Destroyed),
            shown_events = count_events(batch, WindowEventKind::Shown),
            hidden_events = count_events(batch, WindowEventKind::Hidden),
            moved_events = count_events(batch, WindowEventKind::Moved),
            minimized_events = count_events(batch, WindowEventKind::Minimized),
            restored_events = count_events(batch, WindowEventKind::Restored),
            foreground_events = count_events(batch, WindowEventKind::ForegroundChanged),
            total_windows = self.windows.len(),
            manageable_windows = self.manageable_window_count(),
            floating_windows = self.floating.len(),
            active_workspace = %self.workspaces.active_workspace(),
            added = diff.added.len(),
            removed = diff.removed.len(),
            changed = diff.changed,
            foreground_changed = diff.foreground_changed,
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
        let Some(command) = command_for_hotkey(event.id) else {
            warn!(id = event.id.0, "ignoring unrecognized daemon hotkey");
            return Ok(());
        };

        info!(?command, "routing daemon hotkey command");
        self.execute_command(command)
    }

    fn execute_command(&mut self, command: DaemonCommand) -> Result<()> {
        if command == DaemonCommand::Reload {
            let mut refreshed = Self::discover().context("reload daemon window snapshot")?;
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

        if !self.floating.remove(&current) {
            self.floating.insert(current);
        }
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
            self.floating.remove(&current);
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
                    .filter(|handle| !self.floating.contains(handle))
                    .filter(|handle| {
                        self.windows.get(handle).is_some_and(|window| {
                            self.is_tilable_window(*handle)
                                && monitor
                                    .rect
                                    .contains(self.window_layout_rect(*handle, window).center())
                        })
                    })
                    .collect();

                tile_windows(monitor.work_area, &handles)
            })
            .collect()
    }

    fn sync_workspace_state(&mut self, monitors: &[MonitorInfo]) {
        let existing: BTreeSet<_> = self.windows.keys().copied().collect();
        self.workspaces.retain_windows(&existing);

        for (handle, window) in &self.windows {
            if self.workspaces.window_state(*handle).is_some() {
                if window.is_workspace_manageable() {
                    self.workspaces.update_window_rect(*handle, window.rect);
                }
            } else if window.is_manageable() && !is_fullscreen_window(window, monitors) {
                self.workspaces.track_window(*handle, window.rect);
            }
        }
    }

    fn should_hide_for_workspace(&self, window: WindowHandle, monitors: &[MonitorInfo]) -> bool {
        self.windows.get(&window).is_some_and(|info| {
            info.is_workspace_manageable() && !is_fullscreen_window(info, monitors)
        })
    }

    fn is_tilable_window(&self, handle: WindowHandle) -> bool {
        self.workspaces.is_window_on_active_workspace(handle)
            && self
                .windows
                .get(&handle)
                .is_some_and(WindowInfo::is_workspace_manageable)
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
        refreshed.floating = self
            .floating
            .intersection(&known_workspace_windows)
            .copied()
            .collect();
    }

    fn diff(&self, refreshed: &Self) -> SnapshotDiff {
        let old_handles: BTreeSet<_> = self.windows.keys().copied().collect();
        let new_handles: BTreeSet<_> = refreshed.windows.keys().copied().collect();

        let added = new_handles.difference(&old_handles).copied().collect();
        let removed = old_handles.difference(&new_handles).copied().collect();
        let changed = new_handles
            .intersection(&old_handles)
            .filter(|handle| self.windows.get(handle) != refreshed.windows.get(handle))
            .count();

        SnapshotDiff {
            added,
            removed,
            changed,
            foreground_changed: self.foreground != refreshed.foreground,
        }
    }

    fn manageable_window_count(&self) -> usize {
        self.windows
            .values()
            .filter(|window| window.is_manageable())
            .count()
    }

    fn manageable_handles_sorted(&self) -> Vec<WindowHandle> {
        self.windows
            .iter()
            .filter(|(_, window)| window.is_manageable())
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
            && self
                .windows
                .get(&handle)
                .is_some_and(|window| window.is_manageable())
    }

    fn log_snapshot(&self, message: &'static str) {
        info!(
            total_windows = self.windows.len(),
            manageable_windows = self.manageable_window_count(),
            floating_windows = self.floating.len(),
            active_workspace = %self.workspaces.active_workspace(),
            foreground = ?self.foreground,
            message
        );
    }
}

#[derive(Debug)]
struct SnapshotDiff {
    added: Vec<WindowHandle>,
    removed: Vec<WindowHandle>,
    changed: usize,
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

fn workspace_from_hotkey(id: i32, base: i32) -> Option<WorkspaceId> {
    let offset = id.checked_sub(base)?;
    if !(0..i32::from(DEFAULT_WORKSPACE_COUNT)).contains(&offset) {
        return None;
    }

    Some(WorkspaceId((offset + 1) as u16))
}

fn command_for_hotkey(id: HotkeyId) -> Option<DaemonCommand> {
    if let Some(workspace) = workspace_from_hotkey(id.0, HOTKEY_SWITCH_WORKSPACE_BASE) {
        return Some(DaemonCommand::SwitchWorkspace(workspace));
    }

    if let Some(workspace) = workspace_from_hotkey(id.0, HOTKEY_MOVE_TO_WORKSPACE_BASE) {
        return Some(DaemonCommand::MoveFocusedToWorkspace(workspace));
    }

    match id {
        HOTKEY_FOCUS_LEFT => Some(DaemonCommand::Focus(FocusDirection::Left)),
        HOTKEY_FOCUS_DOWN => Some(DaemonCommand::Focus(FocusDirection::Down)),
        HOTKEY_FOCUS_UP => Some(DaemonCommand::Focus(FocusDirection::Up)),
        HOTKEY_FOCUS_RIGHT => Some(DaemonCommand::Focus(FocusDirection::Right)),
        HOTKEY_SWAP_LEFT => Some(DaemonCommand::Swap(FocusDirection::Left)),
        HOTKEY_SWAP_DOWN => Some(DaemonCommand::Swap(FocusDirection::Down)),
        HOTKEY_SWAP_UP => Some(DaemonCommand::Swap(FocusDirection::Up)),
        HOTKEY_SWAP_RIGHT => Some(DaemonCommand::Swap(FocusDirection::Right)),
        HOTKEY_RETILE => Some(DaemonCommand::Retile),
        HOTKEY_TOGGLE_FLOAT => Some(DaemonCommand::ToggleFloat),
        HOTKEY_RELOAD => Some(DaemonCommand::Reload),
        HOTKEY_QUIT => Some(DaemonCommand::Quit),
        _ => None,
    }
}

fn default_hotkey_bindings() -> Vec<HotkeyBinding> {
    let base = HotkeyModifierSet::new().control().alt();
    let shifted = base.shift();

    let mut bindings = vec![
        binding(HOTKEY_FOCUS_LEFT, base, b'H', "focus left"),
        binding(HOTKEY_FOCUS_DOWN, base, b'J', "focus down"),
        binding(HOTKEY_FOCUS_UP, base, b'K', "focus up"),
        binding(HOTKEY_FOCUS_RIGHT, base, b'L', "focus right"),
        binding(HOTKEY_SWAP_LEFT, shifted, b'H', "swap left"),
        binding(HOTKEY_SWAP_DOWN, shifted, b'J', "swap down"),
        binding(HOTKEY_SWAP_UP, shifted, b'K', "swap up"),
        binding(HOTKEY_SWAP_RIGHT, shifted, b'L', "swap right"),
        binding(HOTKEY_RETILE, base, b'R', "retile"),
        HotkeyBinding::new(HOTKEY_TOGGLE_FLOAT, base, VirtualKey::SPACE, "toggle float"),
        binding(HOTKEY_RELOAD, base, b'C', "reload"),
        binding(HOTKEY_QUIT, base, b'Q', "quit"),
    ];

    for workspace in 1..=DEFAULT_WORKSPACE_COUNT {
        let key = b'0' + workspace as u8;
        bindings.push(binding(
            HotkeyId(HOTKEY_SWITCH_WORKSPACE_BASE + i32::from(workspace) - 1),
            base,
            key,
            "switch workspace",
        ));
        bindings.push(binding(
            HotkeyId(HOTKEY_MOVE_TO_WORKSPACE_BASE + i32::from(workspace) - 1),
            shifted,
            key,
            "move focused window to workspace",
        ));
    }

    bindings
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

fn virtual_key_label(key: VirtualKey) -> String {
    if key.0 == 0x20 {
        "Space".to_owned()
    } else if (b'0' as u32..=b'9' as u32).contains(&key.0)
        || (b'A' as u32..=b'Z' as u32).contains(&key.0)
    {
        char::from_u32(key.0).unwrap_or('?').to_string()
    } else {
        format!("VK_{:X}", key.0)
    }
}

fn binding(
    id: HotkeyId,
    modifiers: HotkeyModifierSet,
    key: u8,
    description: &'static str,
) -> HotkeyBinding {
    HotkeyBinding::new(id, modifiers, VirtualKey::ascii_uppercase(key), description)
}

fn is_fullscreen_window(window: &WindowInfo, monitors: &[MonitorInfo]) -> bool {
    monitors
        .iter()
        .any(|monitor| window.rect == monitor.rect || window.rect == monitor.work_area)
}

#[cfg(test)]
mod tests {
    use super::*;
    use winland_core::{MonitorId, Rect, WindowStyles};

    #[test]
    fn hotkey_ids_route_to_commands_without_real_hotkeys() {
        assert_eq!(
            command_for_hotkey(HOTKEY_FOCUS_RIGHT),
            Some(DaemonCommand::Focus(FocusDirection::Right))
        );
        assert_eq!(
            command_for_hotkey(HOTKEY_RETILE),
            Some(DaemonCommand::Retile)
        );
        assert_eq!(
            command_for_hotkey(HOTKEY_TOGGLE_FLOAT),
            Some(DaemonCommand::ToggleFloat)
        );
        assert_eq!(
            command_for_hotkey(HotkeyId(HOTKEY_SWITCH_WORKSPACE_BASE + 1)),
            Some(DaemonCommand::SwitchWorkspace(WorkspaceId(2)))
        );
        assert_eq!(
            command_for_hotkey(HotkeyId(HOTKEY_MOVE_TO_WORKSPACE_BASE + 8)),
            Some(DaemonCommand::MoveFocusedToWorkspace(WorkspaceId(9)))
        );
        assert_eq!(command_for_hotkey(HOTKEY_QUIT), Some(DaemonCommand::Quit));
        assert_eq!(command_for_hotkey(HotkeyId(999)), None);
    }

    #[test]
    fn hotkey_label_is_human_readable() {
        let binding = HotkeyBinding::new(
            HOTKEY_TOGGLE_FLOAT,
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

        assert!(state.floating.contains(&WindowHandle(1)));
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

        let diff = old.diff(&refreshed);

        assert_eq!(diff.added, vec![WindowHandle(3)]);
        assert_eq!(diff.removed, vec![WindowHandle(2)]);
        assert_eq!(diff.changed, 1);
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

    fn daemon_state<const N: usize>(windows: [WindowInfo; N]) -> DaemonState {
        let windows: BTreeMap<_, _> = windows
            .into_iter()
            .map(|window| (window.handle, window))
            .collect();
        let mut state = DaemonState {
            windows,
            foreground: None,
            tile_order: Vec::new(),
            floating: BTreeSet::new(),
            workspaces: WorkspaceManager::new(DEFAULT_WORKSPACE_COUNT),
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
