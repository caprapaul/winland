use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowHandle(pub u64);

impl fmt::Display for WindowHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:X}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn from_size(left: i32, top: i32, width: i32, height: i32) -> Self {
        Self {
            left,
            top,
            right: left.saturating_add(width),
            bottom: top.saturating_add(height),
        }
    }

    pub fn width(self) -> i32 {
        self.right.saturating_sub(self.left)
    }

    pub fn height(self) -> i32 {
        self.bottom.saturating_sub(self.top)
    }

    pub fn is_empty(self) -> bool {
        self.width() <= 0 || self.height() <= 0
    }

    pub fn center(self) -> Point {
        Point {
            x: self.left.saturating_add(self.width() / 2),
            y: self.top.saturating_add(self.height() / 2),
        }
    }

    pub fn contains(self, point: Point) -> bool {
        point.x >= self.left && point.x < self.right && point.y >= self.top && point.y < self.bottom
    }

    pub fn inset(self, amount: i32) -> Self {
        let amount = amount.max(0);
        let horizontal = amount.min(self.width().saturating_sub(1).max(0) / 2);
        let vertical = amount.min(self.height().saturating_sub(1).max(0) / 2);

        Self {
            left: self.left.saturating_add(horizontal),
            top: self.top.saturating_add(vertical),
            right: self.right.saturating_sub(horizontal),
            bottom: self.bottom.saturating_sub(vertical),
        }
    }
}

impl fmt::Display for Rect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{},{} {}x{}",
            self.left,
            self.top,
            self.width(),
            self.height()
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub width: i32,
    pub height: i32,
}

impl Size {
    pub const ZERO: Self = Self {
        width: 0,
        height: 0,
    };

    pub fn new(width: i32, height: i32) -> Self {
        Self {
            width: width.max(0),
            height: height.max(0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowSizeConstraints {
    pub min: Size,
    pub max: Option<Size>,
}

impl WindowSizeConstraints {
    pub const NONE: Self = Self {
        min: Size::ZERO,
        max: None,
    };

    pub fn minimum(width: i32, height: i32) -> Self {
        Self {
            min: Size::new(width, height),
            max: None,
        }
    }

    pub fn fixed(width: i32, height: i32) -> Self {
        let size = Size::new(width, height);
        Self {
            min: size,
            max: Some(size),
        }
    }

    pub fn normalized(self) -> Self {
        let min = Size::new(self.min.width, self.min.height);
        let max = self
            .max
            .map(|max| Size::new(max.width.max(min.width), max.height.max(min.height)));

        Self { min, max }
    }

    pub fn is_unconstrained(self) -> bool {
        self.normalized() == Self::NONE
    }
}

impl Default for WindowSizeConstraints {
    fn default() -> Self {
        Self::NONE
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowLayoutInfo {
    pub handle: WindowHandle,
    pub size_constraints: WindowSizeConstraints,
}

impl WindowLayoutInfo {
    pub fn unconstrained(handle: WindowHandle) -> Self {
        Self {
            handle,
            size_constraints: WindowSizeConstraints::NONE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MonitorId(pub u64);

impl fmt::Display for MonitorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:X}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    pub id: MonitorId,
    pub is_primary: bool,
    pub rect: Rect,
    pub work_area: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileAssignment {
    pub window: WindowHandle,
    pub rect: Rect,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WindowParticipation {
    #[default]
    Tiled,
    Floating,
    TemporarilyFloating,
}

impl WindowParticipation {
    pub fn is_tiled(self) -> bool {
        matches!(self, Self::Tiled)
    }

    pub fn is_floating(self) -> bool {
        matches!(self, Self::Floating | Self::TemporarilyFloating)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutDirection {
    Left,
    Down,
    Up,
    Right,
}

impl LayoutDirection {
    fn is_backward(self) -> bool {
        matches!(self, Self::Left | Self::Up)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LayoutKind {
    #[default]
    MasterStack,
    Dwindle,
    VerticalStack,
    HorizontalStack,
}

impl LayoutKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::MasterStack => "master-stack",
            Self::Dwindle => "dwindle",
            Self::VerticalStack => "vertical-stack",
            Self::HorizontalStack => "horizontal-stack",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "master-stack" => Some(Self::MasterStack),
            "dwindle" => Some(Self::Dwindle),
            "vertical-stack" => Some(Self::VerticalStack),
            "horizontal-stack" => Some(Self::HorizontalStack),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Left,
    Down,
    Up,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DwindleSplit {
    pub target: WindowHandle,
    pub new_window: WindowHandle,
    pub direction: SplitDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutConfig {
    pub kind: LayoutKind,
    pub gap: i32,
    pub border: i32,
    pub master_ratio_percent: u8,
    pub smart_split: bool,
    pub preserve_split: bool,
}

impl LayoutConfig {
    pub const MIN_MASTER_RATIO_PERCENT: u8 = 10;
    pub const MAX_MASTER_RATIO_PERCENT: u8 = 90;

    pub fn normalized(self) -> Self {
        Self {
            kind: self.kind,
            gap: self.gap.max(0),
            border: self.border.max(0),
            master_ratio_percent: self.master_ratio_percent.clamp(
                Self::MIN_MASTER_RATIO_PERCENT,
                Self::MAX_MASTER_RATIO_PERCENT,
            ),
            smart_split: self.smart_split,
            preserve_split: self.preserve_split || self.smart_split,
        }
    }
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            kind: LayoutKind::MasterStack,
            gap: 0,
            border: 0,
            master_ratio_percent: 50,
            smart_split: false,
            preserve_split: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorLayoutState {
    monitor: MonitorId,
    config: LayoutConfig,
    windows: Vec<WindowHandle>,
    focused: Option<WindowHandle>,
    participation: BTreeMap<WindowHandle, WindowParticipation>,
    dwindle_splits: Vec<DwindleSplit>,
}

impl MonitorLayoutState {
    pub fn new(monitor: MonitorId) -> Self {
        Self::with_config(monitor, LayoutConfig::default())
    }

    pub fn with_config(monitor: MonitorId, config: LayoutConfig) -> Self {
        Self {
            monitor,
            config: config.normalized(),
            windows: Vec::new(),
            focused: None,
            participation: BTreeMap::new(),
            dwindle_splits: Vec::new(),
        }
    }

    pub fn monitor(&self) -> MonitorId {
        self.monitor
    }

    pub fn config(&self) -> LayoutConfig {
        self.config
    }

    pub fn focused(&self) -> Option<WindowHandle> {
        self.focused
    }

    pub fn windows(&self) -> &[WindowHandle] {
        &self.windows
    }

    pub fn is_floating(&self, window: WindowHandle) -> bool {
        self.participation(window).is_floating()
    }

    pub fn participation(&self, window: WindowHandle) -> WindowParticipation {
        self.participation.get(&window).copied().unwrap_or_default()
    }

    pub fn set_participation(
        &mut self,
        window: WindowHandle,
        participation: WindowParticipation,
    ) -> bool {
        if !self.windows.contains(&window) {
            return false;
        }

        match participation {
            WindowParticipation::Tiled => {
                self.participation.remove(&window);
            }
            WindowParticipation::Floating | WindowParticipation::TemporarilyFloating => {
                self.participation.insert(window, participation);
            }
        }

        true
    }

    pub fn insert_window(&mut self, window: WindowHandle) -> bool {
        if self.windows.contains(&window) {
            return false;
        }

        self.windows.push(window);
        if self.focused.is_none() {
            self.focused = Some(window);
        }

        true
    }

    pub fn remove_window(&mut self, window: WindowHandle) -> bool {
        let Some(index) = self
            .windows
            .iter()
            .position(|candidate| *candidate == window)
        else {
            return false;
        };

        self.windows.remove(index);
        self.participation.remove(&window);

        if self.focused == Some(window) {
            self.focused = if self.windows.is_empty() {
                None
            } else {
                Some(self.windows[index.min(self.windows.len() - 1)])
            };
        }

        true
    }

    pub fn focus_window(&mut self, window: WindowHandle) -> bool {
        if !self.windows.contains(&window) {
            return false;
        }

        self.focused = Some(window);
        true
    }

    pub fn move_focus(&mut self, direction: LayoutDirection) -> Option<WindowHandle> {
        let target = self.neighbor(self.focused, direction)?;
        self.focused = Some(target);
        Some(target)
    }

    pub fn swap_focused(&mut self, direction: LayoutDirection) -> Option<WindowHandle> {
        let focused = self.focused?;
        let focused_index = self
            .windows
            .iter()
            .position(|candidate| *candidate == focused)?;
        let target_index = adjacent_index(focused_index, self.windows.len(), direction)?;

        self.windows.swap(focused_index, target_index);
        Some(self.windows[focused_index])
    }

    pub fn toggle_floating(&mut self, window: WindowHandle) -> Option<bool> {
        if !self.windows.contains(&window) {
            return None;
        }

        let next = match self.participation(window) {
            WindowParticipation::Tiled => WindowParticipation::Floating,
            WindowParticipation::Floating | WindowParticipation::TemporarilyFloating => {
                WindowParticipation::Tiled
            }
        };
        self.set_participation(window, next);
        Some(next.is_floating())
    }

    pub fn set_temporarily_floating(&mut self, window: WindowHandle) -> bool {
        if self.participation(window) == WindowParticipation::Floating {
            return false;
        }

        self.set_participation(window, WindowParticipation::TemporarilyFloating)
    }

    pub fn clear_temporary_floating(&mut self, window: WindowHandle) -> bool {
        if self.participation(window) != WindowParticipation::TemporarilyFloating {
            return false;
        }

        self.set_participation(window, WindowParticipation::Tiled)
    }

    pub fn adjust_master_ratio(&mut self, delta_percentage_points: i8) -> u8 {
        let current = i16::from(self.config.master_ratio_percent);
        let adjusted = current + i16::from(delta_percentage_points);
        self.config.master_ratio_percent = adjusted.clamp(
            i16::from(LayoutConfig::MIN_MASTER_RATIO_PERCENT),
            i16::from(LayoutConfig::MAX_MASTER_RATIO_PERCENT),
        ) as u8;
        self.config.master_ratio_percent
    }

    pub fn reset_layout(&mut self) {
        self.config = LayoutConfig::default();
        self.participation.clear();
        self.dwindle_splits.clear();
        self.focused = self.windows.first().copied();
    }

    pub fn assignments(&mut self, work_area: Rect) -> Vec<TileAssignment> {
        self.assignments_with_cursor(work_area, None)
    }

    pub fn assignments_with_cursor(
        &mut self,
        work_area: Rect,
        cursor_position: Option<Point>,
    ) -> Vec<TileAssignment> {
        let tiled_windows: Vec<_> = self
            .windows
            .iter()
            .copied()
            .filter(|window| self.participation(*window).is_tiled())
            .collect();

        tile_windows_with_state(
            work_area,
            &tiled_windows,
            self.config,
            cursor_position,
            Some(&mut self.dwindle_splits),
        )
    }

    fn neighbor(
        &self,
        current: Option<WindowHandle>,
        direction: LayoutDirection,
    ) -> Option<WindowHandle> {
        if self.windows.is_empty() {
            return None;
        }

        let current_index = current
            .and_then(|window| {
                self.windows
                    .iter()
                    .position(|candidate| *candidate == window)
            })
            .unwrap_or(0);
        let target_index = adjacent_index(current_index, self.windows.len(), direction)?;
        Some(self.windows[target_index])
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutEngine {
    default_config: LayoutConfig,
    monitors: BTreeMap<MonitorId, MonitorLayoutState>,
}

impl LayoutEngine {
    pub fn new() -> Self {
        Self::with_default_config(LayoutConfig::default())
    }

    pub fn with_default_config(default_config: LayoutConfig) -> Self {
        Self {
            default_config: default_config.normalized(),
            monitors: BTreeMap::new(),
        }
    }

    pub fn ensure_monitor(&mut self, monitor: MonitorId) -> &mut MonitorLayoutState {
        self.monitors
            .entry(monitor)
            .or_insert_with(|| MonitorLayoutState::with_config(monitor, self.default_config))
    }

    pub fn remove_monitor(&mut self, monitor: MonitorId) -> Option<MonitorLayoutState> {
        self.monitors.remove(&monitor)
    }

    pub fn monitor(&self, monitor: MonitorId) -> Option<&MonitorLayoutState> {
        self.monitors.get(&monitor)
    }

    pub fn monitor_mut(&mut self, monitor: MonitorId) -> Option<&mut MonitorLayoutState> {
        self.monitors.get_mut(&monitor)
    }

    pub fn insert_window(&mut self, monitor: MonitorId, window: WindowHandle) -> bool {
        if self
            .monitors
            .get(&monitor)
            .is_some_and(|state| state.windows.contains(&window))
        {
            return false;
        }

        self.remove_window(window);
        self.ensure_monitor(monitor).insert_window(window)
    }

    pub fn remove_window(&mut self, window: WindowHandle) -> bool {
        self.monitors
            .values_mut()
            .any(|monitor| monitor.remove_window(window))
    }

    pub fn move_window_to_monitor(&mut self, monitor: MonitorId, window: WindowHandle) -> bool {
        self.insert_window(monitor, window)
    }

    pub fn reset_monitor(&mut self, monitor: MonitorId) -> bool {
        let Some(state) = self.monitors.get_mut(&monitor) else {
            return false;
        };

        state.reset_layout();
        true
    }

    pub fn assignments(&mut self, monitors: &[MonitorInfo]) -> Vec<TileAssignment> {
        monitors
            .iter()
            .flat_map(|monitor| {
                self.monitors
                    .get_mut(&monitor.id)
                    .map(|state| state.assignments(monitor.work_area))
                    .unwrap_or_default()
            })
            .collect()
    }
}

impl Default for LayoutEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceId(pub u16);

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowRuleMode {
    Ignore,
    Game,
    Fullscreen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceWindowState {
    pub workspace: WorkspaceId,
    pub last_rect: Option<Rect>,
    pub visible_on_all_workspaces: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceVisibilityChange {
    pub window: WindowHandle,
    pub restore_rect: Option<Rect>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSwitchPlan {
    pub from: WorkspaceId,
    pub to: WorkspaceId,
    pub hide: Vec<WindowHandle>,
    pub show: Vec<WorkspaceVisibilityChange>,
}

impl WorkspaceSwitchPlan {
    pub fn is_empty(&self) -> bool {
        self.hide.is_empty() && self.show.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceManager {
    workspaces: BTreeSet<WorkspaceId>,
    active: WorkspaceId,
    windows: BTreeMap<WindowHandle, WorkspaceWindowState>,
}

impl WorkspaceManager {
    pub fn new(workspace_count: u16) -> Self {
        let workspace_count = workspace_count.max(1);
        let workspaces = (1..=workspace_count).map(WorkspaceId).collect();

        Self {
            workspaces,
            active: WorkspaceId(1),
            windows: BTreeMap::new(),
        }
    }

    pub fn active_workspace(&self) -> WorkspaceId {
        self.active
    }

    pub fn workspaces(&self) -> impl Iterator<Item = WorkspaceId> + '_ {
        self.workspaces.iter().copied()
    }

    pub fn window_state(&self, window: WindowHandle) -> Option<WorkspaceWindowState> {
        self.windows.get(&window).copied()
    }

    pub fn track_window(&mut self, window: WindowHandle, rect: Rect) -> bool {
        self.track_window_on_workspace(window, self.active, rect)
    }

    pub fn track_window_on_workspace(
        &mut self,
        window: WindowHandle,
        workspace: WorkspaceId,
        rect: Rect,
    ) -> bool {
        self.ensure_workspace(workspace);
        match self.windows.get_mut(&window) {
            Some(state) => {
                state.last_rect = Some(rect);
                false
            }
            None => {
                self.windows.insert(
                    window,
                    WorkspaceWindowState {
                        workspace,
                        last_rect: Some(rect),
                        visible_on_all_workspaces: false,
                    },
                );
                true
            }
        }
    }

    pub fn remove_window(&mut self, window: WindowHandle) -> bool {
        self.windows.remove(&window).is_some()
    }

    pub fn retain_windows(&mut self, existing: &BTreeSet<WindowHandle>) {
        self.windows.retain(|window, _| existing.contains(window));
    }

    pub fn update_window_rect(&mut self, window: WindowHandle, rect: Rect) -> bool {
        let Some(state) = self.windows.get_mut(&window) else {
            return false;
        };

        state.last_rect = Some(rect);
        true
    }

    pub fn move_window_to_workspace(
        &mut self,
        window: WindowHandle,
        workspace: WorkspaceId,
    ) -> bool {
        self.ensure_workspace(workspace);
        let Some(state) = self.windows.get_mut(&window) else {
            return false;
        };

        if state.workspace == workspace {
            return false;
        }

        state.workspace = workspace;
        true
    }

    pub fn set_visible_on_all_workspaces(
        &mut self,
        window: WindowHandle,
        visible_on_all_workspaces: bool,
    ) -> bool {
        let Some(state) = self.windows.get_mut(&window) else {
            return false;
        };

        state.visible_on_all_workspaces = visible_on_all_workspaces;
        true
    }

    pub fn is_window_on_active_workspace(&self, window: WindowHandle) -> bool {
        self.windows
            .get(&window)
            .is_some_and(|state| state.visible_on_all_workspaces || state.workspace == self.active)
    }

    pub fn visible_windows(&self) -> impl Iterator<Item = WindowHandle> + '_ {
        self.windows
            .iter()
            .filter(|(_, state)| state.visible_on_all_workspaces || state.workspace == self.active)
            .map(|(window, _)| *window)
    }

    pub fn switch_to(&mut self, target: WorkspaceId) -> WorkspaceSwitchPlan {
        self.ensure_workspace(target);

        let from = self.active;
        let mut plan = WorkspaceSwitchPlan {
            from,
            to: target,
            hide: Vec::new(),
            show: Vec::new(),
        };

        if from == target {
            return plan;
        }

        for (window, state) in &self.windows {
            if state.visible_on_all_workspaces {
                continue;
            }

            if state.workspace == from {
                plan.hide.push(*window);
            } else if state.workspace == target {
                plan.show.push(WorkspaceVisibilityChange {
                    window: *window,
                    restore_rect: state.last_rect,
                });
            }
        }

        self.active = target;
        plan
    }

    fn ensure_workspace(&mut self, workspace: WorkspaceId) {
        self.workspaces.insert(workspace);
    }
}

pub fn tile_windows(work_area: Rect, windows: &[WindowHandle]) -> Vec<TileAssignment> {
    master_stack_assignments(work_area, windows, LayoutConfig::default())
}

pub fn tile_windows_with_config(
    work_area: Rect,
    windows: &[WindowHandle],
    config: LayoutConfig,
) -> Vec<TileAssignment> {
    tile_windows_with_state(work_area, windows, config, None, None)
}

pub fn tile_layout_windows_with_config(
    work_area: Rect,
    windows: &[WindowLayoutInfo],
    config: LayoutConfig,
) -> Vec<TileAssignment> {
    tile_layout_windows_with_state(work_area, windows, config, None, None)
}

pub fn tile_windows_with_state(
    work_area: Rect,
    windows: &[WindowHandle],
    config: LayoutConfig,
    cursor_position: Option<Point>,
    dwindle_splits: Option<&mut Vec<DwindleSplit>>,
) -> Vec<TileAssignment> {
    let windows: Vec<_> = windows
        .iter()
        .copied()
        .map(WindowLayoutInfo::unconstrained)
        .collect();
    tile_layout_windows_with_state(work_area, &windows, config, cursor_position, dwindle_splits)
}

pub fn tile_layout_windows_with_state(
    work_area: Rect,
    windows: &[WindowLayoutInfo],
    config: LayoutConfig,
    cursor_position: Option<Point>,
    dwindle_splits: Option<&mut Vec<DwindleSplit>>,
) -> Vec<TileAssignment> {
    let config = config.normalized();
    match config.kind {
        LayoutKind::MasterStack => master_stack_layout_assignments(work_area, windows, config),
        LayoutKind::Dwindle => {
            dwindle_layout_assignments(work_area, windows, config, cursor_position, dwindle_splits)
        }
        LayoutKind::VerticalStack => {
            stack_layout_assignments(work_area, windows, config, StackAxis::Rows)
        }
        LayoutKind::HorizontalStack => {
            stack_layout_assignments(work_area, windows, config, StackAxis::Columns)
        }
    }
}

pub fn tile_assignments_fit_work_area(work_area: Rect, assignments: &[TileAssignment]) -> bool {
    if work_area.is_empty() {
        return assignments.is_empty();
    }

    for assignment in assignments {
        if assignment.rect.is_empty() || !rect_contains_rect(work_area, assignment.rect) {
            return false;
        }
    }

    for (index, first) in assignments.iter().enumerate() {
        if assignments
            .iter()
            .skip(index + 1)
            .any(|second| rects_overlap(first.rect, second.rect))
        {
            return false;
        }
    }

    true
}

fn rect_contains_rect(outer: Rect, inner: Rect) -> bool {
    inner.left >= outer.left
        && inner.top >= outer.top
        && inner.right <= outer.right
        && inner.bottom <= outer.bottom
}

fn rects_overlap(first: Rect, second: Rect) -> bool {
    first.left < second.right
        && second.left < first.right
        && first.top < second.bottom
        && second.top < first.bottom
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextMatcher {
    Exact(String),
    Contains(String),
    Prefix(String),
    Suffix(String),
}

impl TextMatcher {
    pub fn matches(&self, value: &str) -> bool {
        let value = value.to_lowercase();
        match self {
            Self::Exact(expected) => value == expected.to_lowercase(),
            Self::Contains(needle) => value.contains(&needle.to_lowercase()),
            Self::Prefix(prefix) => value.starts_with(&prefix.to_lowercase()),
            Self::Suffix(suffix) => value.ends_with(&suffix.to_lowercase()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowRuleMatch {
    pub class_name: Option<TextMatcher>,
    pub title: Option<TextMatcher>,
    pub executable_path: Option<TextMatcher>,
    pub process_name: Option<TextMatcher>,
}

impl WindowRuleMatch {
    pub fn matches(&self, window: &WindowInfo) -> bool {
        if let Some(matcher) = &self.class_name
            && !matcher.matches(&window.class_name)
        {
            return false;
        }

        if let Some(matcher) = &self.title
            && !matcher.matches(&window.title)
        {
            return false;
        }

        if let Some(matcher) = &self.executable_path
            && !window
                .executable_path
                .as_deref()
                .is_some_and(|path| matcher.matches(path))
        {
            return false;
        }

        if let Some(matcher) = &self.process_name
            && !window
                .executable_path
                .as_deref()
                .and_then(process_name)
                .is_some_and(|name| matcher.matches(&name))
        {
            return false;
        }

        true
    }

    pub fn is_empty(&self) -> bool {
        self.class_name.is_none()
            && self.title.is_none()
            && self.executable_path.is_none()
            && self.process_name.is_none()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowRuleAction {
    pub manage: Option<bool>,
    pub float: Option<bool>,
    pub target_workspace: Option<WorkspaceId>,
    pub always_on_workspace: Option<bool>,
    pub layout: Option<String>,
    pub mode: Option<WindowRuleMode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowRule {
    pub name: String,
    pub matcher: WindowRuleMatch,
    pub action: WindowRuleAction,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowRuleDecision {
    pub manage: Option<bool>,
    pub float: Option<bool>,
    pub target_workspace: Option<WorkspaceId>,
    pub always_on_workspace: Option<bool>,
    pub layout: Option<String>,
    pub mode: Option<WindowRuleMode>,
    pub matched_rules: Vec<String>,
}

pub fn evaluate_window_rules(window: &WindowInfo, rules: &[WindowRule]) -> WindowRuleDecision {
    let mut decision = WindowRuleDecision::default();

    for rule in rules {
        if !rule.matcher.matches(window) {
            continue;
        }

        decision.matched_rules.push(rule.name.clone());
        merge_rule_action(&mut decision, &rule.action);
    }

    decision
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowStyles {
    pub style: u32,
    pub extended_style: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub handle: WindowHandle,
    pub title: String,
    pub class_name: String,
    pub process_id: u32,
    pub executable_path: Option<String>,
    pub is_visible: bool,
    pub is_minimized: bool,
    pub is_dwm_cloaked: bool,
    pub has_owner: bool,
    pub is_tool_window: bool,
    pub styles: WindowStyles,
    pub size_constraints: WindowSizeConstraints,
    pub rect: Rect,
}

impl WindowInfo {
    /// Explains whether a discovered top-level window is safe to include in
    /// early Winland operations.
    ///
    /// This filter is intentionally conservative. Phase 1 should prefer
    /// skipping questionable shell, owned, invisible, cloaked, tool, minimized,
    /// or placeholder windows over accidentally treating desktop infrastructure
    /// as an application window.
    pub fn manageable_reason(&self) -> Manageability {
        if !self.is_visible {
            return Manageability::Unmanageable("not visible");
        }

        if self.is_minimized {
            return Manageability::Unmanageable("minimized");
        }

        if self.is_dwm_cloaked {
            return Manageability::Unmanageable("DWM cloaked");
        }

        if self.title.trim().is_empty() {
            return Manageability::Unmanageable("empty title");
        }

        if self.class_name.trim().is_empty() {
            return Manageability::Unmanageable("empty class name");
        }

        if self.has_owner {
            return Manageability::Unmanageable("owned window");
        }

        if self.is_tool_window {
            return Manageability::Unmanageable("tool window");
        }

        if self.rect.is_empty() {
            return Manageability::Unmanageable("empty rectangle");
        }

        if is_shell_class(&self.class_name) {
            return Manageability::Unmanageable("shell window class");
        }

        Manageability::Manageable
    }

    pub fn is_manageable(&self) -> bool {
        self.manageable_reason().is_manageable()
    }

    /// Like `manageable_reason`, but allows a window to be hidden by Winland's
    /// fake workspace mechanism while still rejecting risky window classes.
    pub fn workspace_manageable_reason(&self) -> Manageability {
        if self.is_visible {
            return self.manageable_reason();
        }

        self.manageable_reason_after_visibility_check()
    }

    pub fn is_workspace_manageable(&self) -> bool {
        self.workspace_manageable_reason().is_manageable()
    }

    fn manageable_reason_after_visibility_check(&self) -> Manageability {
        if self.is_minimized {
            return Manageability::Unmanageable("minimized");
        }

        if self.is_dwm_cloaked {
            return Manageability::Unmanageable("DWM cloaked");
        }

        if self.title.trim().is_empty() {
            return Manageability::Unmanageable("empty title");
        }

        if self.class_name.trim().is_empty() {
            return Manageability::Unmanageable("empty class name");
        }

        if self.has_owner {
            return Manageability::Unmanageable("owned window");
        }

        if self.is_tool_window {
            return Manageability::Unmanageable("tool window");
        }

        if self.rect.is_empty() {
            return Manageability::Unmanageable("empty rectangle");
        }

        if is_shell_class(&self.class_name) {
            return Manageability::Unmanageable("shell window class");
        }

        Manageability::Manageable
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manageability {
    Manageable,
    Unmanageable(&'static str),
}

impl Manageability {
    pub fn is_manageable(self) -> bool {
        matches!(self, Self::Manageable)
    }
}

pub fn manageable_windows(windows: &[WindowInfo]) -> impl Iterator<Item = &WindowInfo> {
    windows.iter().filter(|window| window.is_manageable())
}

pub fn windows_in_monitor<'a>(
    windows: &'a [WindowInfo],
    monitor: &'a MonitorInfo,
) -> impl Iterator<Item = &'a WindowInfo> + 'a {
    windows
        .iter()
        .filter(|window| monitor.rect.contains(window.rect.center()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullscreenArea {
    MonitorBounds,
    WorkArea,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FullscreenDetection {
    pub is_fullscreen: bool,
    pub monitor: Option<MonitorId>,
    pub area: Option<FullscreenArea>,
}

impl FullscreenDetection {
    pub const fn inactive() -> Self {
        Self {
            is_fullscreen: false,
            monitor: None,
            area: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameModePolicy {
    pub enabled: bool,
    pub pause_on_fullscreen: bool,
    pub fullscreen_tolerance_px: i32,
    pub game_exes: Vec<String>,
    pub ignored_exes: Vec<String>,
}

impl Default for GameModePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            pause_on_fullscreen: true,
            fullscreen_tolerance_px: 4,
            game_exes: Vec::new(),
            ignored_exes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameModeReason {
    ConfiguredExecutable(String),
    WindowRule {
        mode: WindowRuleMode,
        matched_rules: Vec<String>,
    },
    Fullscreen {
        monitor: MonitorId,
        area: FullscreenArea,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameModeDetection {
    pub active: bool,
    pub reason: Option<GameModeReason>,
    pub fullscreen: FullscreenDetection,
    pub matched_executable: Option<String>,
    pub matched_rule_mode: Option<WindowRuleMode>,
    pub matched_rules: Vec<String>,
}

impl GameModeDetection {
    pub fn inactive(fullscreen: FullscreenDetection) -> Self {
        Self {
            active: false,
            reason: None,
            fullscreen,
            matched_executable: None,
            matched_rule_mode: None,
            matched_rules: Vec::new(),
        }
    }
}

pub fn detect_fullscreen_rect(
    rect: Rect,
    monitors: &[MonitorInfo],
    tolerance_px: i32,
) -> FullscreenDetection {
    let tolerance = tolerance_px.max(0);
    for monitor in monitors {
        if rect_matches_with_tolerance(rect, monitor.rect, tolerance) {
            return FullscreenDetection {
                is_fullscreen: true,
                monitor: Some(monitor.id),
                area: Some(FullscreenArea::MonitorBounds),
            };
        }

        if rect_matches_with_tolerance(rect, monitor.work_area, tolerance) {
            return FullscreenDetection {
                is_fullscreen: true,
                monitor: Some(monitor.id),
                area: Some(FullscreenArea::WorkArea),
            };
        }
    }

    FullscreenDetection::inactive()
}

pub fn detect_fullscreen_window(
    window: &WindowInfo,
    monitors: &[MonitorInfo],
    tolerance_px: i32,
) -> FullscreenDetection {
    detect_fullscreen_rect(window.rect, monitors, tolerance_px)
}

pub fn detect_game_mode(
    focused_window: Option<&WindowInfo>,
    monitors: &[MonitorInfo],
    rules: &[WindowRule],
    policy: &GameModePolicy,
) -> GameModeDetection {
    let Some(window) = focused_window else {
        return GameModeDetection::inactive(FullscreenDetection::inactive());
    };
    let fullscreen = detect_fullscreen_window(window, monitors, policy.fullscreen_tolerance_px);

    if !policy.enabled {
        return GameModeDetection::inactive(fullscreen);
    }

    if let Some(executable) = matched_game_executable(window, policy) {
        return GameModeDetection {
            active: true,
            reason: Some(GameModeReason::ConfiguredExecutable(executable.clone())),
            fullscreen,
            matched_executable: Some(executable),
            matched_rule_mode: None,
            matched_rules: Vec::new(),
        };
    }

    let decision = evaluate_window_rules(window, rules);
    if let Some(
        mode @ (WindowRuleMode::Ignore | WindowRuleMode::Game | WindowRuleMode::Fullscreen),
    ) = decision.mode
    {
        return GameModeDetection {
            active: true,
            reason: Some(GameModeReason::WindowRule {
                mode,
                matched_rules: decision.matched_rules.clone(),
            }),
            fullscreen,
            matched_executable: None,
            matched_rule_mode: Some(mode),
            matched_rules: decision.matched_rules,
        };
    }

    if policy.pause_on_fullscreen
        && fullscreen.is_fullscreen
        && let (Some(monitor), Some(area)) = (fullscreen.monitor, fullscreen.area)
    {
        return GameModeDetection {
            active: true,
            reason: Some(GameModeReason::Fullscreen { monitor, area }),
            fullscreen,
            matched_executable: None,
            matched_rule_mode: None,
            matched_rules: Vec::new(),
        };
    }

    GameModeDetection::inactive(fullscreen)
}

pub fn game_mode_executable_matches(window: &WindowInfo, policy: &GameModePolicy) -> bool {
    matched_game_executable(window, policy).is_some()
}

fn matched_game_executable(window: &WindowInfo, policy: &GameModePolicy) -> Option<String> {
    let executable_path = window.executable_path.as_deref()?;
    let process_name = process_name(executable_path)?;

    policy
        .game_exes
        .iter()
        .chain(policy.ignored_exes.iter())
        .find(|configured| process_name.eq_ignore_ascii_case(configured.trim()))
        .cloned()
}

fn rect_matches_with_tolerance(actual: Rect, expected: Rect, tolerance: i32) -> bool {
    (actual.left - expected.left).abs() <= tolerance
        && (actual.top - expected.top).abs() <= tolerance
        && (actual.right - expected.right).abs() <= tolerance
        && (actual.bottom - expected.bottom).abs() <= tolerance
}

fn is_shell_class(class_name: &str) -> bool {
    matches!(
        class_name,
        "Progman"
            | "WorkerW"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "Button"
            | "DV2ControlHost"
            | "MsgrIMEWindowClass"
            | "IME"
    )
}

fn merge_rule_action(decision: &mut WindowRuleDecision, action: &WindowRuleAction) {
    if action.manage.is_some() {
        decision.manage = action.manage;
    }
    if action.float.is_some() {
        decision.float = action.float;
    }
    if action.target_workspace.is_some() {
        decision.target_workspace = action.target_workspace;
    }
    if action.always_on_workspace.is_some() {
        decision.always_on_workspace = action.always_on_workspace;
    }
    if action.layout.is_some() {
        decision.layout = action.layout.clone();
    }
    if action.mode.is_some() {
        decision.mode = action.mode;
    }
}

fn process_name(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

fn master_stack_assignments(
    work_area: Rect,
    windows: &[WindowHandle],
    config: LayoutConfig,
) -> Vec<TileAssignment> {
    let windows: Vec<_> = windows
        .iter()
        .copied()
        .map(WindowLayoutInfo::unconstrained)
        .collect();
    master_stack_layout_assignments(work_area, &windows, config)
}

fn master_stack_layout_assignments(
    work_area: Rect,
    windows: &[WindowLayoutInfo],
    config: LayoutConfig,
) -> Vec<TileAssignment> {
    if work_area.is_empty() || windows.is_empty() {
        return Vec::new();
    }

    let config = config.normalized();
    match windows {
        [] => Vec::new(),
        [window] => vec![TileAssignment {
            window: window.handle,
            rect: reserve_constrained_window_rect(
                work_area.inset(config.gap),
                config,
                window.size_constraints,
            ),
        }],
        [master, stack @ ..] => {
            let gap = config.gap;
            let outer_area = work_area.inset(gap);
            if outer_area.is_empty() {
                return Vec::new();
            }

            let available_width = outer_area.width().saturating_sub(gap);
            if available_width <= 0 {
                return Vec::new();
            }

            let (master_width, stack_width) = allocate_pair_with_constraints(
                outer_area.width(),
                gap,
                config.master_ratio_percent,
                effective_axis_constraint(master.size_constraints, config, Axis::Horizontal),
                stack_cross_axis_constraint(stack, config, Axis::Horizontal),
            );
            let master_rect = Rect::from_size(
                outer_area.left,
                outer_area.top,
                master_width,
                outer_area.height(),
            );
            let stack_area = Rect {
                left: master_rect.right.saturating_add(gap),
                top: outer_area.top,
                right: master_rect
                    .right
                    .saturating_add(gap)
                    .saturating_add(stack_width),
                bottom: outer_area.bottom,
            };

            let mut assignments = Vec::with_capacity(windows.len());
            assignments.push(TileAssignment {
                window: master.handle,
                rect: reserve_constrained_window_rect(master_rect, config, master.size_constraints),
            });
            assignments.extend(
                split_rows_for_layout_windows(stack_area, stack, config)
                    .zip(stack)
                    .map(|(rect, window)| TileAssignment {
                        window: window.handle,
                        rect: reserve_constrained_window_rect(
                            rect,
                            config,
                            window.size_constraints,
                        ),
                    }),
            );
            assignments
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StackAxis {
    Rows,
    Columns,
}

fn stack_layout_assignments(
    work_area: Rect,
    windows: &[WindowLayoutInfo],
    config: LayoutConfig,
    axis: StackAxis,
) -> Vec<TileAssignment> {
    if work_area.is_empty() || windows.is_empty() {
        return Vec::new();
    }

    let config = config.normalized();
    let outer_area = work_area.inset(config.gap);
    if outer_area.is_empty() {
        return Vec::new();
    }

    let rects: Vec<_> = match axis {
        StackAxis::Rows => split_rows_for_layout_windows(outer_area, windows, config).collect(),
        StackAxis::Columns => {
            split_columns_for_layout_windows(outer_area, windows, config).collect()
        }
    };

    rects
        .into_iter()
        .zip(windows)
        .map(|(rect, window)| TileAssignment {
            window: window.handle,
            rect: reserve_constrained_window_rect(rect, config, window.size_constraints),
        })
        .collect()
}

fn dwindle_layout_assignments(
    work_area: Rect,
    windows: &[WindowLayoutInfo],
    config: LayoutConfig,
    cursor_position: Option<Point>,
    dwindle_splits: Option<&mut Vec<DwindleSplit>>,
) -> Vec<TileAssignment> {
    if work_area.is_empty() || windows.is_empty() {
        if let Some(splits) = dwindle_splits {
            splits.clear();
        }
        return Vec::new();
    }

    let config = config.normalized();
    let outer_area = work_area.inset(config.gap);
    if outer_area.is_empty() {
        return Vec::new();
    }

    if windows.len() == 1 {
        if let Some(splits) = dwindle_splits {
            splits.clear();
        }
        return vec![TileAssignment {
            window: windows[0].handle,
            rect: reserve_constrained_window_rect(outer_area, config, windows[0].size_constraints),
        }];
    }

    let handles: Vec<_> = windows.iter().map(|window| window.handle).collect();
    let constraints: BTreeMap<_, _> = windows
        .iter()
        .map(|window| (window.handle, window.size_constraints))
        .collect();
    let current_windows: BTreeSet<_> = handles.iter().copied().collect();
    let split_state = dwindle_splits;
    let historical_splits = split_state
        .as_ref()
        .map(|splits| splits.as_slice())
        .unwrap_or(&[]);
    let root = dwindle_root_window(&handles, historical_splits);
    let mut tree = prune_dwindle_tree(
        build_dwindle_tree(root, historical_splits),
        &current_windows,
    )
    .unwrap_or(DwindleTree::Leaf(windows[0].handle));

    let mut assignments = Vec::new();
    collect_dwindle_assignments(&tree, outer_area, config, &constraints, &mut assignments);

    let initial_missing_count = handles
        .iter()
        .filter(|window| {
            !assignments
                .iter()
                .any(|assignment| assignment.window == **window)
        })
        .count();
    let use_smart_split_for_new_window = config.smart_split && initial_missing_count == 1;

    for window in handles.iter().copied() {
        if assignments
            .iter()
            .any(|assignment| assignment.window == window)
        {
            continue;
        }

        let target_assignment = new_dwindle_split_target(
            &assignments,
            use_smart_split_for_new_window,
            cursor_position,
        );
        let split_cursor = use_smart_split_for_new_window
            .then_some(cursor_position)
            .flatten();
        let direction = dwindle_split_direction(target_assignment.rect, config, split_cursor);
        let split = DwindleSplit {
            target: target_assignment.window,
            new_window: window,
            direction,
        };

        insert_dwindle_split(&mut tree, split);
        assignments.clear();
        collect_dwindle_assignments(&tree, outer_area, config, &constraints, &mut assignments);
    }

    if let Some(splits) = split_state {
        splits.clear();
        serialize_dwindle_tree(&tree, splits);
    }

    sort_assignments_by_window_order(&mut assignments, &handles);

    assignments
        .into_iter()
        .map(|assignment| TileAssignment {
            window: assignment.window,
            rect: reserve_constrained_window_rect(
                assignment.rect,
                config,
                constraints
                    .get(&assignment.window)
                    .copied()
                    .unwrap_or_default(),
            ),
        })
        .collect()
}

fn sort_assignments_by_window_order(assignments: &mut [TileAssignment], windows: &[WindowHandle]) {
    assignments.sort_by_key(|assignment| {
        windows
            .iter()
            .position(|window| *window == assignment.window)
            .unwrap_or(usize::MAX)
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DwindleTree {
    Leaf(WindowHandle),
    Split {
        direction: SplitDirection,
        existing: Box<DwindleTree>,
        new: Box<DwindleTree>,
    },
}

fn dwindle_root_window(windows: &[WindowHandle], splits: &[DwindleSplit]) -> WindowHandle {
    let new_windows: BTreeSet<_> = splits.iter().map(|split| split.new_window).collect();
    splits
        .iter()
        .map(|split| split.target)
        .find(|target| !new_windows.contains(target))
        .unwrap_or(windows[0])
}

fn build_dwindle_tree(root: WindowHandle, splits: &[DwindleSplit]) -> DwindleTree {
    let mut tree = DwindleTree::Leaf(root);

    for split in splits {
        insert_dwindle_split(&mut tree, *split);
    }

    tree
}

fn prune_dwindle_tree(tree: DwindleTree, windows: &BTreeSet<WindowHandle>) -> Option<DwindleTree> {
    match tree {
        DwindleTree::Leaf(window) if windows.contains(&window) => Some(DwindleTree::Leaf(window)),
        DwindleTree::Leaf(_) => None,
        DwindleTree::Split {
            direction,
            existing,
            new,
        } => match (
            prune_dwindle_tree(*existing, windows),
            prune_dwindle_tree(*new, windows),
        ) {
            (Some(existing), Some(new)) => Some(DwindleTree::Split {
                direction,
                existing: Box::new(existing),
                new: Box::new(new),
            }),
            (Some(remaining), None) | (None, Some(remaining)) => Some(remaining),
            (None, None) => None,
        },
    }
}

fn insert_dwindle_split(tree: &mut DwindleTree, split: DwindleSplit) -> bool {
    if dwindle_tree_contains(tree, split.new_window) {
        return false;
    }

    match tree {
        DwindleTree::Leaf(window) if *window == split.target => {
            *tree = DwindleTree::Split {
                direction: split.direction,
                existing: Box::new(DwindleTree::Leaf(split.target)),
                new: Box::new(DwindleTree::Leaf(split.new_window)),
            };
            true
        }
        DwindleTree::Leaf(_) => false,
        DwindleTree::Split { existing, new, .. } => {
            insert_dwindle_split(existing, split) || insert_dwindle_split(new, split)
        }
    }
}

fn dwindle_tree_contains(tree: &DwindleTree, needle: WindowHandle) -> bool {
    match tree {
        DwindleTree::Leaf(window) => *window == needle,
        DwindleTree::Split { existing, new, .. } => {
            dwindle_tree_contains(existing, needle) || dwindle_tree_contains(new, needle)
        }
    }
}

fn collect_dwindle_assignments(
    tree: &DwindleTree,
    rect: Rect,
    config: LayoutConfig,
    constraints: &BTreeMap<WindowHandle, WindowSizeConstraints>,
    assignments: &mut Vec<TileAssignment>,
) {
    match tree {
        DwindleTree::Leaf(window) => assignments.push(TileAssignment {
            window: *window,
            rect,
        }),
        DwindleTree::Split {
            direction,
            existing,
            new,
        } => {
            let (existing_rect, new_rect) =
                split_for_direction(rect, *direction, config, constraints, existing, new);
            collect_dwindle_assignments(existing, existing_rect, config, constraints, assignments);
            collect_dwindle_assignments(new, new_rect, config, constraints, assignments);
        }
    }
}

fn serialize_dwindle_tree(tree: &DwindleTree, splits: &mut Vec<DwindleSplit>) {
    if let DwindleTree::Split {
        direction,
        existing,
        new,
    } = tree
    {
        splits.push(DwindleSplit {
            target: dwindle_tree_root_leaf(existing),
            new_window: dwindle_tree_root_leaf(new),
            direction: *direction,
        });
        serialize_dwindle_tree(existing, splits);
        serialize_dwindle_tree(new, splits);
    }
}

fn dwindle_tree_root_leaf(tree: &DwindleTree) -> WindowHandle {
    match tree {
        DwindleTree::Leaf(window) => *window,
        DwindleTree::Split { existing, .. } => dwindle_tree_root_leaf(existing),
    }
}

fn new_dwindle_split_target(
    assignments: &[TileAssignment],
    use_cursor_target: bool,
    cursor_position: Option<Point>,
) -> TileAssignment {
    if use_cursor_target
        && let Some(cursor_position) = cursor_position
        && let Some(assignment) = assignments
            .iter()
            .find(|assignment| assignment.rect.contains(cursor_position))
    {
        return *assignment;
    }

    *assignments.last().unwrap_or(&TileAssignment {
        window: WindowHandle(0),
        rect: Rect::from_size(0, 0, 1, 1),
    })
}

fn dwindle_split_direction(
    target_rect: Rect,
    config: LayoutConfig,
    cursor_position: Option<Point>,
) -> SplitDirection {
    if config.smart_split
        && let Some(cursor_position) = cursor_position
        && target_rect.contains(cursor_position)
    {
        return split_direction_for_point(target_rect, cursor_position);
    }

    if target_rect.width() >= target_rect.height() {
        SplitDirection::Right
    } else {
        SplitDirection::Down
    }
}

pub fn split_direction_for_point(rect: Rect, point: Point) -> SplitDirection {
    let width = f64::from(rect.width().max(1));
    let height = f64::from(rect.height().max(1));
    let center_x = f64::from(rect.left) + width / 2.0;
    let center_y = f64::from(rect.top) + height / 2.0;
    let normalized_x = (f64::from(point.x) - center_x) / (width / 2.0);
    let normalized_y = (f64::from(point.y) - center_y) / (height / 2.0);

    if normalized_x.abs() >= normalized_y.abs() {
        if normalized_x < 0.0 {
            SplitDirection::Left
        } else {
            SplitDirection::Right
        }
    } else if normalized_y < 0.0 {
        SplitDirection::Up
    } else {
        SplitDirection::Down
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AxisConstraint {
    min: i32,
    max: Option<i32>,
}

impl AxisConstraint {
    const NONE: Self = Self { min: 0, max: None };

    fn normalized(self) -> Self {
        Self {
            min: self.min.max(0),
            max: self.max.map(|max| max.max(self.min.max(0))),
        }
    }
}

fn split_for_direction(
    rect: Rect,
    direction: SplitDirection,
    config: LayoutConfig,
    constraints: &BTreeMap<WindowHandle, WindowSizeConstraints>,
    existing: &DwindleTree,
    new: &DwindleTree,
) -> (Rect, Rect) {
    let gap = config.gap;
    match direction {
        SplitDirection::Left | SplitDirection::Right => {
            let available_width = rect.width().saturating_sub(gap);
            if available_width <= 0 {
                return (rect, rect);
            }

            let (left_constraint, right_constraint) = match direction {
                SplitDirection::Left => (
                    tree_axis_constraint(new, constraints, config, Axis::Horizontal),
                    tree_axis_constraint(existing, constraints, config, Axis::Horizontal),
                ),
                SplitDirection::Right => (
                    tree_axis_constraint(existing, constraints, config, Axis::Horizontal),
                    tree_axis_constraint(new, constraints, config, Axis::Horizontal),
                ),
                SplitDirection::Down | SplitDirection::Up => unreachable!(),
            };
            let (first_width, second_width) = allocate_pair_with_constraints(
                rect.width(),
                gap,
                config.master_ratio_percent,
                left_constraint,
                right_constraint,
            );
            let left = Rect::from_size(rect.left, rect.top, first_width, rect.height());
            let right = Rect::from_size(
                left.right.saturating_add(gap),
                rect.top,
                second_width,
                rect.height(),
            );

            match direction {
                SplitDirection::Left => (right, left),
                SplitDirection::Right => (left, right),
                SplitDirection::Down | SplitDirection::Up => unreachable!(),
            }
        }
        SplitDirection::Up | SplitDirection::Down => {
            let available_height = rect.height().saturating_sub(gap);
            if available_height <= 0 {
                return (rect, rect);
            }

            let (top_constraint, bottom_constraint) = match direction {
                SplitDirection::Up => (
                    tree_axis_constraint(new, constraints, config, Axis::Vertical),
                    tree_axis_constraint(existing, constraints, config, Axis::Vertical),
                ),
                SplitDirection::Down => (
                    tree_axis_constraint(existing, constraints, config, Axis::Vertical),
                    tree_axis_constraint(new, constraints, config, Axis::Vertical),
                ),
                SplitDirection::Left | SplitDirection::Right => unreachable!(),
            };
            let (first_height, second_height) = allocate_pair_with_constraints(
                rect.height(),
                gap,
                config.master_ratio_percent,
                top_constraint,
                bottom_constraint,
            );
            let top = Rect::from_size(rect.left, rect.top, rect.width(), first_height);
            let bottom = Rect::from_size(
                rect.left,
                top.bottom.saturating_add(gap),
                rect.width(),
                second_height,
            );

            match direction {
                SplitDirection::Up => (bottom, top),
                SplitDirection::Down => (top, bottom),
                SplitDirection::Left | SplitDirection::Right => unreachable!(),
            }
        }
    }
}

fn reserve_window_rect(rect: Rect, config: LayoutConfig) -> Rect {
    rect.inset(config.border)
}

fn reserve_constrained_window_rect(
    rect: Rect,
    config: LayoutConfig,
    constraints: WindowSizeConstraints,
) -> Rect {
    let constraints = constraints.normalized();
    let border = config.border.saturating_mul(2);
    let min_width = constraints.min.width.saturating_add(border);
    let min_height = constraints.min.height.saturating_add(border);
    let max_width = constraints
        .max
        .map(|max| max.width.saturating_add(border).max(min_width));
    let max_height = constraints
        .max
        .map(|max| max.height.saturating_add(border).max(min_height));

    let width = clamp_length(rect.width(), min_width, max_width);
    let height = clamp_length(rect.height(), min_height, max_height);

    reserve_window_rect(Rect::from_size(rect.left, rect.top, width, height), config)
}

fn clamp_length(length: i32, min: i32, max: Option<i32>) -> i32 {
    let length = length.max(min);
    max.map(|max| length.min(max)).unwrap_or(length)
}

fn effective_axis_constraint(
    constraints: WindowSizeConstraints,
    config: LayoutConfig,
    axis: Axis,
) -> AxisConstraint {
    let constraints = constraints.normalized();
    let border = config.border.saturating_mul(2);
    match axis {
        Axis::Horizontal => AxisConstraint {
            min: constraints.min.width.saturating_add(border),
            max: constraints.max.map(|max| max.width.saturating_add(border)),
        },
        Axis::Vertical => AxisConstraint {
            min: constraints.min.height.saturating_add(border),
            max: constraints.max.map(|max| max.height.saturating_add(border)),
        },
    }
    .normalized()
}

fn stack_cross_axis_constraint(
    windows: &[WindowLayoutInfo],
    config: LayoutConfig,
    axis: Axis,
) -> AxisConstraint {
    let mut min = 0;
    let mut max = Some(i32::MAX);

    for window in windows {
        let constraint = effective_axis_constraint(window.size_constraints, config, axis);
        min = min.max(constraint.min);
        max = match (max, constraint.max) {
            (Some(current), Some(candidate)) => Some(current.min(candidate)),
            _ => None,
        };
    }

    AxisConstraint { min, max }.normalized()
}

fn tree_axis_constraint(
    tree: &DwindleTree,
    constraints: &BTreeMap<WindowHandle, WindowSizeConstraints>,
    config: LayoutConfig,
    axis: Axis,
) -> AxisConstraint {
    match tree {
        DwindleTree::Leaf(window) => effective_axis_constraint(
            constraints.get(window).copied().unwrap_or_default(),
            config,
            axis,
        ),
        DwindleTree::Split {
            direction,
            existing,
            new,
        } => {
            let existing = tree_axis_constraint(existing, constraints, config, axis);
            let new = tree_axis_constraint(new, constraints, config, axis);
            match (*direction, axis) {
                (SplitDirection::Left | SplitDirection::Right, Axis::Horizontal)
                | (SplitDirection::Up | SplitDirection::Down, Axis::Vertical) => {
                    combine_series_constraints(existing, new, config.gap)
                }
                _ => combine_parallel_constraints(existing, new),
            }
        }
    }
}

fn combine_series_constraints(
    first: AxisConstraint,
    second: AxisConstraint,
    gap: i32,
) -> AxisConstraint {
    let gap = gap.max(0);
    AxisConstraint {
        min: first.min.saturating_add(gap).saturating_add(second.min),
        max: first
            .max
            .zip(second.max)
            .map(|(first, second)| first.saturating_add(gap).saturating_add(second)),
    }
    .normalized()
}

fn combine_parallel_constraints(first: AxisConstraint, second: AxisConstraint) -> AxisConstraint {
    AxisConstraint {
        min: first.min.max(second.min),
        max: first
            .max
            .zip(second.max)
            .map(|(first, second)| first.min(second)),
    }
    .normalized()
}

fn allocate_pair_with_constraints(
    length: i32,
    gap: i32,
    percent: u8,
    first: AxisConstraint,
    second: AxisConstraint,
) -> (i32, i32) {
    let first = first.normalized();
    let second = second.normalized();
    let available = length.saturating_sub(gap.max(0)).max(0);

    if first.min.saturating_add(second.min) >= available {
        return (first.min, second.min);
    }

    let mut first_len = scale_length(available, percent);
    let mut second_len = available.saturating_sub(first_len);

    satisfy_pair_minimums(&mut first_len, &mut second_len, first.min, second.min);
    apply_pair_maximums(&mut first_len, &mut second_len, first.max, second.max);
    satisfy_pair_minimums(&mut first_len, &mut second_len, first.min, second.min);

    (first_len.max(0), second_len.max(0))
}

fn satisfy_pair_minimums(
    first_len: &mut i32,
    second_len: &mut i32,
    first_min: i32,
    second_min: i32,
) {
    if *first_len < first_min {
        let needed = first_min.saturating_sub(*first_len);
        let available = second_len.saturating_sub(second_min).max(0);
        let take = needed.min(available);
        *first_len = first_len.saturating_add(take);
        *second_len = second_len.saturating_sub(take);
        if *first_len < first_min {
            *first_len = first_min;
        }
    }

    if *second_len < second_min {
        let needed = second_min.saturating_sub(*second_len);
        let available = first_len.saturating_sub(first_min).max(0);
        let take = needed.min(available);
        *second_len = second_len.saturating_add(take);
        *first_len = first_len.saturating_sub(take);
        if *second_len < second_min {
            *second_len = second_min;
        }
    }
}

fn apply_pair_maximums(
    first_len: &mut i32,
    second_len: &mut i32,
    first_max: Option<i32>,
    second_max: Option<i32>,
) {
    if let Some(max) = first_max
        && *first_len > max
    {
        let spare = first_len.saturating_sub(max);
        *first_len = max;
        let capacity = second_max
            .map(|max| max.saturating_sub(*second_len).max(0))
            .unwrap_or(spare);
        *second_len = second_len.saturating_add(spare.min(capacity));
    }

    if let Some(max) = second_max
        && *second_len > max
    {
        let spare = second_len.saturating_sub(max);
        *second_len = max;
        let capacity = first_max
            .map(|max| max.saturating_sub(*first_len).max(0))
            .unwrap_or(spare);
        *first_len = first_len.saturating_add(spare.min(capacity));
    }
}

fn split_rows_for_layout_windows<'a>(
    area: Rect,
    windows: &'a [WindowLayoutInfo],
    config: LayoutConfig,
) -> impl Iterator<Item = Rect> + 'a {
    let heights = allocate_segments_with_constraints(
        area.height(),
        windows.len(),
        config.gap,
        windows
            .iter()
            .map(move |window| {
                effective_axis_constraint(window.size_constraints, config, Axis::Vertical)
            })
            .collect(),
    );

    heights.into_iter().scan(area.top, move |top, height| {
        let rect = Rect::from_size(area.left, *top, area.width(), height);
        *top = top.saturating_add(height).saturating_add(config.gap);
        Some(rect)
    })
}

fn split_columns_for_layout_windows<'a>(
    area: Rect,
    windows: &'a [WindowLayoutInfo],
    config: LayoutConfig,
) -> impl Iterator<Item = Rect> + 'a {
    let widths = allocate_segments_with_constraints(
        area.width(),
        windows.len(),
        config.gap,
        windows
            .iter()
            .map(move |window| {
                effective_axis_constraint(window.size_constraints, config, Axis::Horizontal)
            })
            .collect(),
    );

    widths.into_iter().scan(area.left, move |left, width| {
        let rect = Rect::from_size(*left, area.top, width, area.height());
        *left = left.saturating_add(width).saturating_add(config.gap);
        Some(rect)
    })
}

fn allocate_segments_with_constraints(
    length: i32,
    count: usize,
    gap: i32,
    constraints: Vec<AxisConstraint>,
) -> Vec<i32> {
    if count == 0 {
        return Vec::new();
    }

    let gap = gap.max(0);
    let total_gap = gap.saturating_mul(count.saturating_sub(1).min(i32::MAX as usize) as i32);
    let available = length.saturating_sub(total_gap);
    let base = available / count as i32;
    let remainder = available % count as i32;
    let constraints: Vec<_> = constraints
        .into_iter()
        .map(AxisConstraint::normalized)
        .collect();
    let mut lengths: Vec<_> = (0..count)
        .map(|index| {
            let extra = if (index as i32) < remainder { 1 } else { 0 };
            base.saturating_add(extra)
        })
        .collect();

    satisfy_segment_minimums(&mut lengths, &constraints);
    apply_segment_maximums(&mut lengths, &constraints);
    satisfy_segment_minimums(&mut lengths, &constraints);

    lengths
}

fn satisfy_segment_minimums(lengths: &mut [i32], constraints: &[AxisConstraint]) {
    for index in 0..lengths.len() {
        let min = constraints
            .get(index)
            .copied()
            .unwrap_or(AxisConstraint::NONE)
            .min;
        if lengths[index] >= min {
            continue;
        }

        let needed = min.saturating_sub(lengths[index]);
        let taken = take_segment_slack(lengths, constraints, index, needed);
        lengths[index] = lengths[index].saturating_add(taken);
        if lengths[index] < min {
            lengths[index] = min;
        }
    }
}

fn apply_segment_maximums(lengths: &mut [i32], constraints: &[AxisConstraint]) {
    for index in 0..lengths.len() {
        let Some(max) = constraints
            .get(index)
            .copied()
            .unwrap_or(AxisConstraint::NONE)
            .max
        else {
            continue;
        };

        if lengths[index] <= max {
            continue;
        }

        let spare = lengths[index].saturating_sub(max);
        lengths[index] = max;
        distribute_segment_spare(lengths, constraints, index, spare);
    }
}

fn take_segment_slack(
    lengths: &mut [i32],
    constraints: &[AxisConstraint],
    excluded: usize,
    mut needed: i32,
) -> i32 {
    let mut taken: i32 = 0;
    for (donor, donor_len) in lengths.iter_mut().enumerate() {
        if donor == excluded || needed <= 0 {
            continue;
        }

        let min = constraints
            .get(donor)
            .copied()
            .unwrap_or(AxisConstraint::NONE)
            .min;
        let available = donor_len.saturating_sub(min).max(0);
        let amount = available.min(needed);
        *donor_len = donor_len.saturating_sub(amount);
        needed = needed.saturating_sub(amount);
        taken = taken.saturating_add(amount);
    }

    taken
}

fn distribute_segment_spare(
    lengths: &mut [i32],
    constraints: &[AxisConstraint],
    excluded: usize,
    mut spare: i32,
) {
    for (target, target_len) in lengths.iter_mut().enumerate() {
        if target == excluded || spare <= 0 {
            continue;
        }

        let max = constraints
            .get(target)
            .copied()
            .unwrap_or(AxisConstraint::NONE)
            .max;
        let capacity = max
            .map(|max| max.saturating_sub(*target_len).max(0))
            .unwrap_or(spare);
        let amount = capacity.min(spare);
        *target_len = target_len.saturating_add(amount);
        spare = spare.saturating_sub(amount);
    }
}

fn scale_length(length: i32, percent: u8) -> i32 {
    ((i64::from(length) * i64::from(percent)) / 100).clamp(i64::from(i32::MIN), i64::from(i32::MAX))
        as i32
}

fn adjacent_index(current_index: usize, len: usize, direction: LayoutDirection) -> Option<usize> {
    if len == 0 {
        return None;
    }

    if direction.is_backward() {
        Some(if current_index == 0 {
            len - 1
        } else {
            current_index - 1
        })
    } else {
        Some((current_index + 1) % len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> WindowInfo {
        WindowInfo {
            handle: WindowHandle(1),
            title: "Editor".to_owned(),
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
            size_constraints: WindowSizeConstraints::NONE,
            rect: Rect {
                left: 10,
                top: 20,
                right: 810,
                bottom: 620,
            },
        }
    }

    #[test]
    fn ordinary_visible_top_level_window_is_manageable() {
        assert_eq!(window().manageable_reason(), Manageability::Manageable);
    }

    #[test]
    fn invisible_windows_are_not_manageable() {
        let mut window = window();
        window.is_visible = false;

        assert_eq!(
            window.manageable_reason(),
            Manageability::Unmanageable("not visible")
        );
    }

    #[test]
    fn workspace_manageable_allows_hidden_windows_but_keeps_other_guards() {
        let mut hidden = window();
        hidden.is_visible = false;

        assert_eq!(
            hidden.workspace_manageable_reason(),
            Manageability::Manageable
        );

        hidden.is_minimized = true;
        assert_eq!(
            hidden.workspace_manageable_reason(),
            Manageability::Unmanageable("minimized")
        );
    }

    #[test]
    fn shell_windows_are_not_manageable() {
        let mut window = window();
        window.class_name = "Shell_TrayWnd".to_owned();

        assert_eq!(
            window.manageable_reason(),
            Manageability::Unmanageable("shell window class")
        );
    }

    #[test]
    fn zero_sized_windows_are_not_manageable() {
        let mut window = window();
        window.rect.right = window.rect.left;

        assert_eq!(
            window.manageable_reason(),
            Manageability::Unmanageable("empty rectangle")
        );
    }

    #[test]
    fn tiling_no_windows_produces_no_assignments() {
        let work_area = Rect::from_size(0, 0, 1920, 1080);

        assert!(tile_windows(work_area, &[]).is_empty());
    }

    #[test]
    fn tiling_one_window_fills_the_work_area() {
        let work_area = Rect::from_size(10, 20, 800, 600);
        let assignments = tile_windows(work_area, &[WindowHandle(1)]);

        assert_eq!(
            assignments,
            vec![TileAssignment {
                window: WindowHandle(1),
                rect: work_area,
            }]
        );
    }

    #[test]
    fn tile_assignment_fit_rejects_rects_outside_work_area() {
        let work_area = Rect::from_size(0, 0, 800, 600);

        assert!(!tile_assignments_fit_work_area(
            work_area,
            &[TileAssignment {
                window: WindowHandle(1),
                rect: Rect::from_size(0, 0, 900, 600),
            }],
        ));
    }

    #[test]
    fn tile_assignment_fit_rejects_overlapping_rects() {
        let work_area = Rect::from_size(0, 0, 800, 600);

        assert!(!tile_assignments_fit_work_area(
            work_area,
            &[
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 500, 600),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(400, 0, 400, 600),
                },
            ],
        ));
    }

    #[test]
    fn tiling_two_windows_splits_master_and_stack_evenly() {
        let work_area = Rect::from_size(0, 0, 1000, 800);
        let assignments = tile_windows(work_area, &[WindowHandle(1), WindowHandle(2)]);

        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 500, 800),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(500, 0, 500, 800),
                },
            ]
        );
    }

    #[test]
    fn tiling_three_windows_stacks_non_master_windows_vertically() {
        let work_area = Rect::from_size(0, 0, 1000, 801);
        let assignments = tile_windows(
            work_area,
            &[WindowHandle(1), WindowHandle(2), WindowHandle(3)],
        );

        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 500, 801),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(500, 0, 500, 401),
                },
                TileAssignment {
                    window: WindowHandle(3),
                    rect: Rect::from_size(500, 401, 500, 400),
                },
            ]
        );
    }

    #[test]
    fn vertical_stack_splits_windows_into_rows() {
        let work_area = Rect::from_size(0, 0, 100, 101);
        let assignments = tile_windows_with_config(
            work_area,
            &[WindowHandle(1), WindowHandle(2), WindowHandle(3)],
            LayoutConfig {
                kind: LayoutKind::VerticalStack,
                ..LayoutConfig::default()
            },
        );

        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 100, 34),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(0, 34, 100, 34),
                },
                TileAssignment {
                    window: WindowHandle(3),
                    rect: Rect::from_size(0, 68, 100, 33),
                },
            ]
        );
    }

    #[test]
    fn horizontal_stack_splits_windows_into_columns() {
        let work_area = Rect::from_size(0, 0, 101, 100);
        let assignments = tile_windows_with_config(
            work_area,
            &[WindowHandle(1), WindowHandle(2), WindowHandle(3)],
            LayoutConfig {
                kind: LayoutKind::HorizontalStack,
                ..LayoutConfig::default()
            },
        );

        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 34, 100),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(34, 0, 34, 100),
                },
                TileAssignment {
                    window: WindowHandle(3),
                    rect: Rect::from_size(68, 0, 33, 100),
                },
            ]
        );
    }

    #[test]
    fn vertical_stack_reserves_minimum_heights_without_overlap() {
        let work_area = Rect::from_size(0, 0, 100, 120);
        let windows = [
            WindowLayoutInfo {
                handle: WindowHandle(1),
                size_constraints: WindowSizeConstraints::minimum(0, 80),
            },
            WindowLayoutInfo::unconstrained(WindowHandle(2)),
        ];

        let assignments = tile_layout_windows_with_config(
            work_area,
            &windows,
            LayoutConfig {
                kind: LayoutKind::VerticalStack,
                ..LayoutConfig::default()
            },
        );

        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 100, 80),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(0, 80, 100, 40),
                },
            ]
        );
    }

    #[test]
    fn master_stack_reserves_fixed_master_width_for_stack() {
        let work_area = Rect::from_size(0, 0, 1000, 800);
        let windows = [
            WindowLayoutInfo {
                handle: WindowHandle(1),
                size_constraints: WindowSizeConstraints::fixed(300, 800),
            },
            WindowLayoutInfo::unconstrained(WindowHandle(2)),
        ];

        let assignments = tile_layout_windows_with_config(
            work_area,
            &windows,
            LayoutConfig {
                kind: LayoutKind::MasterStack,
                ..LayoutConfig::default()
            },
        );

        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 300, 800),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(300, 0, 700, 800),
                },
            ]
        );
    }

    #[test]
    fn dwindle_reserves_minimum_split_width_without_overlap() {
        let work_area = Rect::from_size(0, 0, 1000, 800);
        let windows = [
            WindowLayoutInfo::unconstrained(WindowHandle(1)),
            WindowLayoutInfo {
                handle: WindowHandle(2),
                size_constraints: WindowSizeConstraints::minimum(700, 0),
            },
        ];

        let assignments = tile_layout_windows_with_config(
            work_area,
            &windows,
            LayoutConfig {
                kind: LayoutKind::Dwindle,
                ..LayoutConfig::default()
            },
        );

        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 300, 800),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(300, 0, 700, 800),
                },
            ]
        );
    }

    #[test]
    fn dwindle_reserves_minimum_width_for_left_side_new_split() {
        let work_area = Rect::from_size(0, 0, 1000, 800);
        let windows = [
            WindowLayoutInfo::unconstrained(WindowHandle(1)),
            WindowLayoutInfo {
                handle: WindowHandle(2),
                size_constraints: WindowSizeConstraints::minimum(700, 0),
            },
        ];
        let mut splits = vec![DwindleSplit {
            target: WindowHandle(1),
            new_window: WindowHandle(2),
            direction: SplitDirection::Left,
        }];

        let assignments = tile_layout_windows_with_state(
            work_area,
            &windows,
            LayoutConfig {
                kind: LayoutKind::Dwindle,
                preserve_split: true,
                ..LayoutConfig::default()
            },
            None,
            Some(&mut splits),
        );

        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(700, 0, 300, 800),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(0, 0, 700, 800),
                },
            ]
        );
    }

    #[test]
    fn dwindle_reserves_minimum_height_for_upper_side_new_split() {
        let work_area = Rect::from_size(0, 0, 1000, 800);
        let windows = [
            WindowLayoutInfo::unconstrained(WindowHandle(1)),
            WindowLayoutInfo {
                handle: WindowHandle(2),
                size_constraints: WindowSizeConstraints::minimum(0, 600),
            },
        ];
        let mut splits = vec![DwindleSplit {
            target: WindowHandle(1),
            new_window: WindowHandle(2),
            direction: SplitDirection::Up,
        }];

        let assignments = tile_layout_windows_with_state(
            work_area,
            &windows,
            LayoutConfig {
                kind: LayoutKind::Dwindle,
                preserve_split: true,
                ..LayoutConfig::default()
            },
            None,
            Some(&mut splits),
        );

        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 600, 1000, 200),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(0, 0, 1000, 600),
                },
            ]
        );
    }

    #[test]
    fn dwindle_splits_newest_leaf_by_available_shape() {
        let work_area = Rect::from_size(0, 0, 1000, 800);
        let assignments = tile_windows_with_config(
            work_area,
            &[WindowHandle(1), WindowHandle(2), WindowHandle(3)],
            LayoutConfig {
                kind: LayoutKind::Dwindle,
                ..LayoutConfig::default()
            },
        );

        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 500, 800),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(500, 0, 500, 400),
                },
                TileAssignment {
                    window: WindowHandle(3),
                    rect: Rect::from_size(500, 400, 500, 400),
                },
            ]
        );
    }

    #[test]
    fn smart_split_uses_cursor_triangle_and_preserves_existing_splits() {
        let work_area = Rect::from_size(0, 0, 1000, 800);
        let mut splits = Vec::new();
        let config = LayoutConfig {
            kind: LayoutKind::Dwindle,
            smart_split: true,
            ..LayoutConfig::default()
        };

        let assignments = tile_windows_with_state(
            work_area,
            &[WindowHandle(1), WindowHandle(2)],
            config,
            Some(Point { x: 10, y: 400 }),
            Some(&mut splits),
        );

        assert_eq!(
            splits,
            vec![DwindleSplit {
                target: WindowHandle(1),
                new_window: WindowHandle(2),
                direction: SplitDirection::Left,
            }]
        );
        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(500, 0, 500, 800),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(0, 0, 500, 800),
                },
            ]
        );

        let preserved = tile_windows_with_state(
            work_area,
            &[WindowHandle(1), WindowHandle(2)],
            config,
            Some(Point { x: 990, y: 400 }),
            Some(&mut splits),
        );

        assert_eq!(
            splits,
            vec![DwindleSplit {
                target: WindowHandle(1),
                new_window: WindowHandle(2),
                direction: SplitDirection::Left,
            }]
        );
        assert_eq!(preserved, assignments);
    }

    #[test]
    fn smart_split_reconstructs_missing_history_from_shape_when_many_splits_are_unknown() {
        let work_area = Rect::from_size(0, 0, 1000, 800);
        let mut splits = Vec::new();
        let config = LayoutConfig {
            kind: LayoutKind::Dwindle,
            smart_split: true,
            ..LayoutConfig::default()
        };

        let assignments = tile_windows_with_state(
            work_area,
            &[
                WindowHandle(1),
                WindowHandle(2),
                WindowHandle(3),
                WindowHandle(4),
            ],
            config,
            Some(Point { x: 500, y: 799 }),
            Some(&mut splits),
        );

        assert_eq!(
            splits,
            vec![
                DwindleSplit {
                    target: WindowHandle(1),
                    new_window: WindowHandle(2),
                    direction: SplitDirection::Right,
                },
                DwindleSplit {
                    target: WindowHandle(2),
                    new_window: WindowHandle(3),
                    direction: SplitDirection::Down,
                },
                DwindleSplit {
                    target: WindowHandle(3),
                    new_window: WindowHandle(4),
                    direction: SplitDirection::Right,
                },
            ]
        );
        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 500, 800),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(500, 0, 500, 400),
                },
                TileAssignment {
                    window: WindowHandle(3),
                    rect: Rect::from_size(500, 400, 250, 400),
                },
                TileAssignment {
                    window: WindowHandle(4),
                    rect: Rect::from_size(750, 400, 250, 400),
                },
            ]
        );
    }

    #[test]
    fn smart_split_uses_cursor_for_single_new_split_only() {
        let work_area = Rect::from_size(0, 0, 1000, 800);
        let mut splits = vec![DwindleSplit {
            target: WindowHandle(1),
            new_window: WindowHandle(2),
            direction: SplitDirection::Right,
        }];
        let config = LayoutConfig {
            kind: LayoutKind::Dwindle,
            smart_split: true,
            ..LayoutConfig::default()
        };

        let assignments = tile_windows_with_state(
            work_area,
            &[WindowHandle(1), WindowHandle(2), WindowHandle(3)],
            config,
            Some(Point { x: 510, y: 400 }),
            Some(&mut splits),
        );

        assert_eq!(
            splits,
            vec![
                DwindleSplit {
                    target: WindowHandle(1),
                    new_window: WindowHandle(2),
                    direction: SplitDirection::Right,
                },
                DwindleSplit {
                    target: WindowHandle(2),
                    new_window: WindowHandle(3),
                    direction: SplitDirection::Left,
                },
            ]
        );
        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 500, 800),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(750, 0, 250, 800),
                },
                TileAssignment {
                    window: WindowHandle(3),
                    rect: Rect::from_size(500, 0, 250, 800),
                },
            ]
        );
    }

    #[test]
    fn smart_split_targets_leaf_under_cursor_instead_of_always_newest_leaf() {
        let work_area = Rect::from_size(0, 0, 1000, 800);
        let mut splits = vec![DwindleSplit {
            target: WindowHandle(1),
            new_window: WindowHandle(2),
            direction: SplitDirection::Right,
        }];
        let config = LayoutConfig {
            kind: LayoutKind::Dwindle,
            smart_split: true,
            ..LayoutConfig::default()
        };

        let assignments = tile_windows_with_state(
            work_area,
            &[WindowHandle(1), WindowHandle(2), WindowHandle(3)],
            config,
            Some(Point { x: 10, y: 400 }),
            Some(&mut splits),
        );

        assert_eq!(
            splits,
            vec![
                DwindleSplit {
                    target: WindowHandle(1),
                    new_window: WindowHandle(2),
                    direction: SplitDirection::Right,
                },
                DwindleSplit {
                    target: WindowHandle(1),
                    new_window: WindowHandle(3),
                    direction: SplitDirection::Left,
                },
            ]
        );
        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(250, 0, 250, 800),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(500, 0, 500, 800),
                },
                TileAssignment {
                    window: WindowHandle(3),
                    rect: Rect::from_size(0, 0, 250, 800),
                },
            ]
        );
    }

    #[test]
    fn dwindle_prunes_missing_leaf_and_expands_remaining_sibling_subtree() {
        let work_area = Rect::from_size(0, 0, 1000, 800);
        let mut splits = vec![
            DwindleSplit {
                target: WindowHandle(1),
                new_window: WindowHandle(2),
                direction: SplitDirection::Down,
            },
            DwindleSplit {
                target: WindowHandle(1),
                new_window: WindowHandle(3),
                direction: SplitDirection::Right,
            },
        ];
        let config = LayoutConfig {
            kind: LayoutKind::Dwindle,
            smart_split: true,
            ..LayoutConfig::default()
        };

        let assignments = tile_windows_with_state(
            work_area,
            &[WindowHandle(1), WindowHandle(3)],
            config,
            Some(Point { x: 500, y: 700 }),
            Some(&mut splits),
        );

        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 500, 800),
                },
                TileAssignment {
                    window: WindowHandle(3),
                    rect: Rect::from_size(500, 0, 500, 800),
                },
            ]
        );
    }

    #[test]
    fn dwindle_prunes_missing_root_leaf_and_expands_remaining_sibling_subtree() {
        let work_area = Rect::from_size(0, 0, 1000, 800);
        let mut splits = vec![
            DwindleSplit {
                target: WindowHandle(1),
                new_window: WindowHandle(2),
                direction: SplitDirection::Right,
            },
            DwindleSplit {
                target: WindowHandle(2),
                new_window: WindowHandle(3),
                direction: SplitDirection::Down,
            },
        ];
        let config = LayoutConfig {
            kind: LayoutKind::Dwindle,
            smart_split: true,
            ..LayoutConfig::default()
        };

        let assignments = tile_windows_with_state(
            work_area,
            &[WindowHandle(2), WindowHandle(3)],
            config,
            Some(Point { x: 100, y: 400 }),
            Some(&mut splits),
        );

        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(0, 0, 1000, 400),
                },
                TileAssignment {
                    window: WindowHandle(3),
                    rect: Rect::from_size(0, 400, 1000, 400),
                },
            ]
        );
    }

    #[test]
    fn dwindle_canonicalizes_split_history_after_pruning_dragged_leaf() {
        let work_area = Rect::from_size(0, 0, 1000, 800);
        let mut splits = vec![
            DwindleSplit {
                target: WindowHandle(1),
                new_window: WindowHandle(3),
                direction: SplitDirection::Right,
            },
            DwindleSplit {
                target: WindowHandle(3),
                new_window: WindowHandle(2),
                direction: SplitDirection::Down,
            },
        ];
        let config = LayoutConfig {
            kind: LayoutKind::Dwindle,
            smart_split: true,
            ..LayoutConfig::default()
        };

        let assignments = tile_windows_with_state(
            work_area,
            &[WindowHandle(1), WindowHandle(2)],
            config,
            Some(Point { x: 750, y: 10 }),
            Some(&mut splits),
        );

        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 500, 800),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(500, 0, 500, 800),
                },
            ]
        );
        assert_eq!(
            splits,
            vec![DwindleSplit {
                target: WindowHandle(1),
                new_window: WindowHandle(2),
                direction: SplitDirection::Right,
            }]
        );
    }

    #[test]
    fn smart_split_uses_diagonal_triangle_regions_in_wide_rects() {
        let rect = Rect::from_size(0, 0, 1000, 200);

        assert_eq!(
            split_direction_for_point(rect, Point { x: 250, y: 80 }),
            SplitDirection::Left
        );
        assert_eq!(
            split_direction_for_point(rect, Point { x: 750, y: 80 }),
            SplitDirection::Right
        );
        assert_eq!(
            split_direction_for_point(rect, Point { x: 500, y: 10 }),
            SplitDirection::Up
        );
        assert_eq!(
            split_direction_for_point(rect, Point { x: 500, y: 190 }),
            SplitDirection::Down
        );
    }

    #[test]
    fn smart_split_uses_diagonal_triangle_regions_in_tall_rects() {
        let rect = Rect::from_size(0, 0, 200, 1000);

        assert_eq!(
            split_direction_for_point(rect, Point { x: 80, y: 250 }),
            SplitDirection::Up
        );
        assert_eq!(
            split_direction_for_point(rect, Point { x: 80, y: 750 }),
            SplitDirection::Down
        );
        assert_eq!(
            split_direction_for_point(rect, Point { x: 10, y: 500 }),
            SplitDirection::Left
        );
        assert_eq!(
            split_direction_for_point(rect, Point { x: 190, y: 500 }),
            SplitDirection::Right
        );
    }

    #[test]
    fn smart_split_ignores_cursor_outside_split_target() {
        let work_area = Rect::from_size(0, 0, 1000, 800);
        let mut splits = vec![DwindleSplit {
            target: WindowHandle(1),
            new_window: WindowHandle(2),
            direction: SplitDirection::Right,
        }];
        let config = LayoutConfig {
            kind: LayoutKind::Dwindle,
            smart_split: true,
            ..LayoutConfig::default()
        };

        let assignments = tile_windows_with_state(
            work_area,
            &[WindowHandle(1), WindowHandle(2), WindowHandle(3)],
            config,
            Some(Point { x: 2000, y: 400 }),
            Some(&mut splits),
        );

        assert_eq!(
            splits,
            vec![
                DwindleSplit {
                    target: WindowHandle(1),
                    new_window: WindowHandle(2),
                    direction: SplitDirection::Right,
                },
                DwindleSplit {
                    target: WindowHandle(2),
                    new_window: WindowHandle(3),
                    direction: SplitDirection::Down,
                },
            ]
        );
        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 500, 800),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(500, 0, 500, 400),
                },
                TileAssignment {
                    window: WindowHandle(3),
                    rect: Rect::from_size(500, 400, 500, 400),
                },
            ]
        );
    }

    #[test]
    fn layout_state_inserts_windows_without_duplicates() {
        let mut layout = MonitorLayoutState::new(MonitorId(1));

        assert!(layout.insert_window(WindowHandle(1)));
        assert!(layout.insert_window(WindowHandle(2)));
        assert!(!layout.insert_window(WindowHandle(1)));

        assert_eq!(layout.windows(), &[WindowHandle(1), WindowHandle(2)]);
        assert_eq!(layout.focused(), Some(WindowHandle(1)));
    }

    #[test]
    fn removing_focused_window_selects_next_window() {
        let mut layout = MonitorLayoutState::new(MonitorId(1));
        layout.insert_window(WindowHandle(1));
        layout.insert_window(WindowHandle(2));
        layout.insert_window(WindowHandle(3));
        assert!(layout.focus_window(WindowHandle(2)));

        assert!(layout.remove_window(WindowHandle(2)));

        assert_eq!(layout.windows(), &[WindowHandle(1), WindowHandle(3)]);
        assert_eq!(layout.focused(), Some(WindowHandle(3)));
    }

    #[test]
    fn focus_movement_wraps_through_layout_order() {
        let mut layout = MonitorLayoutState::new(MonitorId(1));
        layout.insert_window(WindowHandle(1));
        layout.insert_window(WindowHandle(2));
        layout.insert_window(WindowHandle(3));

        assert_eq!(
            layout.move_focus(LayoutDirection::Left),
            Some(WindowHandle(3))
        );
        assert_eq!(
            layout.move_focus(LayoutDirection::Right),
            Some(WindowHandle(1))
        );
        assert_eq!(
            layout.move_focus(LayoutDirection::Down),
            Some(WindowHandle(2))
        );
    }

    #[test]
    fn swapping_focused_window_changes_layout_order() {
        let mut layout = MonitorLayoutState::new(MonitorId(1));
        layout.insert_window(WindowHandle(1));
        layout.insert_window(WindowHandle(2));
        layout.insert_window(WindowHandle(3));
        assert!(layout.focus_window(WindowHandle(2)));

        assert_eq!(
            layout.swap_focused(LayoutDirection::Right),
            Some(WindowHandle(3))
        );

        assert_eq!(
            layout.windows(),
            &[WindowHandle(1), WindowHandle(3), WindowHandle(2)]
        );
        assert_eq!(layout.focused(), Some(WindowHandle(2)));
    }

    #[test]
    fn master_ratio_changes_master_stack_geometry() {
        let mut layout = MonitorLayoutState::new(MonitorId(1));
        layout.insert_window(WindowHandle(1));
        layout.insert_window(WindowHandle(2));
        assert_eq!(layout.adjust_master_ratio(10), 60);

        let assignments = layout.assignments(Rect::from_size(0, 0, 1000, 800));

        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 600, 800),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(600, 0, 400, 800),
                },
            ]
        );
    }

    #[test]
    fn gaps_and_borders_are_geometry_reservations() {
        let mut layout = MonitorLayoutState::with_config(
            MonitorId(1),
            LayoutConfig {
                gap: 10,
                border: 2,
                master_ratio_percent: 50,
                ..LayoutConfig::default()
            },
        );
        layout.insert_window(WindowHandle(1));
        layout.insert_window(WindowHandle(2));

        let assignments = layout.assignments(Rect::from_size(0, 0, 100, 100));

        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(12, 12, 31, 76),
                },
                TileAssignment {
                    window: WindowHandle(2),
                    rect: Rect::from_size(57, 12, 31, 76),
                },
            ]
        );
    }

    #[test]
    fn floating_windows_are_excluded_from_tiling_assignments() {
        let mut layout = MonitorLayoutState::new(MonitorId(1));
        layout.insert_window(WindowHandle(1));
        layout.insert_window(WindowHandle(2));
        layout.insert_window(WindowHandle(3));

        assert_eq!(layout.toggle_floating(WindowHandle(2)), Some(true));

        let assignments = layout.assignments(Rect::from_size(0, 0, 1000, 800));

        assert_eq!(
            assignments,
            vec![
                TileAssignment {
                    window: WindowHandle(1),
                    rect: Rect::from_size(0, 0, 500, 800),
                },
                TileAssignment {
                    window: WindowHandle(3),
                    rect: Rect::from_size(500, 0, 500, 800),
                },
            ]
        );
    }

    #[test]
    fn temporary_floating_windows_are_reabsorbed_after_drag_end() {
        let mut layout = MonitorLayoutState::new(MonitorId(1));
        layout.insert_window(WindowHandle(1));
        layout.insert_window(WindowHandle(2));
        layout.insert_window(WindowHandle(3));

        assert!(layout.set_temporarily_floating(WindowHandle(2)));
        assert_eq!(
            layout.participation(WindowHandle(2)),
            WindowParticipation::TemporarilyFloating
        );

        let during_drag = layout.assignments(Rect::from_size(0, 0, 1000, 800));
        assert_eq!(
            during_drag
                .iter()
                .map(|assignment| assignment.window)
                .collect::<Vec<_>>(),
            vec![WindowHandle(1), WindowHandle(3)]
        );

        assert!(layout.clear_temporary_floating(WindowHandle(2)));
        let after_drag = layout.assignments(Rect::from_size(0, 0, 1000, 800));
        assert_eq!(
            after_drag
                .iter()
                .map(|assignment| assignment.window)
                .collect::<Vec<_>>(),
            vec![WindowHandle(1), WindowHandle(2), WindowHandle(3)]
        );
    }

    #[test]
    fn permanent_floating_is_not_replaced_by_temporary_drag_state() {
        let mut layout = MonitorLayoutState::new(MonitorId(1));
        layout.insert_window(WindowHandle(1));

        assert_eq!(layout.toggle_floating(WindowHandle(1)), Some(true));
        assert!(!layout.set_temporarily_floating(WindowHandle(1)));
        assert_eq!(
            layout.participation(WindowHandle(1)),
            WindowParticipation::Floating
        );
    }

    #[test]
    fn layout_reset_restores_default_geometry_state() {
        let mut layout = MonitorLayoutState::with_config(
            MonitorId(1),
            LayoutConfig {
                gap: 12,
                border: 4,
                master_ratio_percent: 70,
                ..LayoutConfig::default()
            },
        );
        layout.insert_window(WindowHandle(1));
        layout.insert_window(WindowHandle(2));
        assert!(layout.focus_window(WindowHandle(2)));
        layout.toggle_floating(WindowHandle(2));

        layout.reset_layout();

        assert_eq!(layout.config(), LayoutConfig::default());
        assert_eq!(layout.focused(), Some(WindowHandle(1)));
        assert!(!layout.is_floating(WindowHandle(2)));
    }

    #[test]
    fn layout_engine_assigns_multiple_monitors_independently() {
        let monitors = [
            MonitorInfo {
                id: MonitorId(1),
                is_primary: true,
                rect: Rect::from_size(0, 0, 1000, 800),
                work_area: Rect::from_size(0, 0, 1000, 760),
            },
            MonitorInfo {
                id: MonitorId(2),
                is_primary: false,
                rect: Rect::from_size(1000, 0, 800, 600),
                work_area: Rect::from_size(1000, 0, 800, 560),
            },
        ];
        let mut engine = LayoutEngine::new();
        engine.insert_window(MonitorId(1), WindowHandle(1));
        engine.insert_window(MonitorId(1), WindowHandle(2));
        engine.insert_window(MonitorId(2), WindowHandle(3));

        let assignments = engine.assignments(&monitors);

        assert_eq!(
            assignments,
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
    fn layout_engine_does_not_reorder_duplicate_monitor_insert() {
        let mut engine = LayoutEngine::new();
        assert!(engine.insert_window(MonitorId(1), WindowHandle(1)));
        assert!(engine.insert_window(MonitorId(1), WindowHandle(2)));

        assert!(!engine.insert_window(MonitorId(1), WindowHandle(1)));

        assert_eq!(
            engine
                .monitor(MonitorId(1))
                .map(MonitorLayoutState::windows),
            Some([WindowHandle(1), WindowHandle(2)].as_slice())
        );
    }

    #[test]
    fn workspace_switch_hides_old_workspace_and_shows_target_workspace() {
        let mut workspaces = WorkspaceManager::new(2);
        workspaces.track_window(WindowHandle(1), Rect::from_size(0, 0, 100, 100));
        workspaces.track_window_on_workspace(
            WindowHandle(2),
            WorkspaceId(2),
            Rect::from_size(200, 0, 100, 100),
        );

        let plan = workspaces.switch_to(WorkspaceId(2));

        assert_eq!(workspaces.active_workspace(), WorkspaceId(2));
        assert_eq!(plan.from, WorkspaceId(1));
        assert_eq!(plan.to, WorkspaceId(2));
        assert_eq!(plan.hide, vec![WindowHandle(1)]);
        assert_eq!(
            plan.show,
            vec![WorkspaceVisibilityChange {
                window: WindowHandle(2),
                restore_rect: Some(Rect::from_size(200, 0, 100, 100)),
            }]
        );
    }

    #[test]
    fn workspace_state_survives_create_destroy_style_updates() {
        let mut workspaces = WorkspaceManager::new(2);
        assert!(workspaces.track_window(WindowHandle(1), Rect::from_size(0, 0, 100, 100)));
        assert!(!workspaces.track_window(WindowHandle(1), Rect::from_size(10, 0, 100, 100)));
        workspaces.move_window_to_workspace(WindowHandle(1), WorkspaceId(2));
        workspaces.track_window(WindowHandle(2), Rect::from_size(0, 0, 100, 100));

        workspaces.retain_windows(&BTreeSet::from([WindowHandle(1)]));

        assert_eq!(
            workspaces.window_state(WindowHandle(1)),
            Some(WorkspaceWindowState {
                workspace: WorkspaceId(2),
                last_rect: Some(Rect::from_size(10, 0, 100, 100)),
                visible_on_all_workspaces: false,
            })
        );
        assert_eq!(workspaces.window_state(WindowHandle(2)), None);
    }

    #[test]
    fn visible_on_all_workspace_windows_are_not_hidden_or_shown() {
        let mut workspaces = WorkspaceManager::new(2);
        workspaces.track_window(WindowHandle(1), Rect::from_size(0, 0, 100, 100));
        workspaces.set_visible_on_all_workspaces(WindowHandle(1), true);
        workspaces.track_window_on_workspace(
            WindowHandle(2),
            WorkspaceId(2),
            Rect::from_size(200, 0, 100, 100),
        );

        let plan = workspaces.switch_to(WorkspaceId(2));

        assert_eq!(plan.hide, Vec::<WindowHandle>::new());
        assert_eq!(
            plan.show,
            vec![WorkspaceVisibilityChange {
                window: WindowHandle(2),
                restore_rect: Some(Rect::from_size(200, 0, 100, 100)),
            }]
        );
        assert!(workspaces.is_window_on_active_workspace(WindowHandle(1)));
        assert_eq!(
            workspaces.visible_windows().collect::<Vec<_>>(),
            vec![WindowHandle(1), WindowHandle(2)]
        );
    }

    #[test]
    fn monitor_selection_uses_window_center() {
        let monitor = MonitorInfo {
            id: MonitorId(1),
            is_primary: true,
            rect: Rect::from_size(0, 0, 100, 100),
            work_area: Rect::from_size(0, 0, 100, 90),
        };
        let mut inside = window();
        inside.handle = WindowHandle(1);
        inside.rect = Rect::from_size(70, 70, 40, 40);
        let mut outside = window();
        outside.handle = WindowHandle(2);
        outside.rect = Rect::from_size(100, 100, 40, 40);
        let windows = [inside, outside];

        let handles: Vec<_> = windows_in_monitor(&windows, &monitor)
            .map(|window| window.handle)
            .collect();

        assert_eq!(handles, vec![WindowHandle(1)]);
    }

    #[test]
    fn window_rules_match_stable_metadata() {
        let rule = WindowRule {
            name: "pin editor".to_owned(),
            matcher: WindowRuleMatch {
                class_name: Some(TextMatcher::Exact("ApplicationFrameWindow".to_owned())),
                title: Some(TextMatcher::Contains("edit".to_owned())),
                executable_path: Some(TextMatcher::Suffix("notepad.exe".to_owned())),
                process_name: Some(TextMatcher::Exact("notepad.exe".to_owned())),
            },
            action: WindowRuleAction {
                target_workspace: Some(WorkspaceId(2)),
                ..WindowRuleAction::default()
            },
        };

        let decision = evaluate_window_rules(&window(), &[rule]);

        assert_eq!(decision.target_workspace, Some(WorkspaceId(2)));
        assert_eq!(decision.matched_rules, vec!["pin editor"]);
    }

    #[test]
    fn later_matching_window_rules_override_earlier_actions() {
        let rules = [
            WindowRule {
                name: "float all app frames".to_owned(),
                matcher: WindowRuleMatch {
                    class_name: Some(TextMatcher::Exact("ApplicationFrameWindow".to_owned())),
                    ..WindowRuleMatch::default()
                },
                action: WindowRuleAction {
                    float: Some(true),
                    target_workspace: Some(WorkspaceId(2)),
                    ..WindowRuleAction::default()
                },
            },
            WindowRule {
                name: "keep notepad tiled".to_owned(),
                matcher: WindowRuleMatch {
                    process_name: Some(TextMatcher::Exact("notepad.exe".to_owned())),
                    ..WindowRuleMatch::default()
                },
                action: WindowRuleAction {
                    float: Some(false),
                    ..WindowRuleAction::default()
                },
            },
        ];

        let decision = evaluate_window_rules(&window(), &rules);

        assert_eq!(decision.float, Some(false));
        assert_eq!(decision.target_workspace, Some(WorkspaceId(2)));
        assert_eq!(
            decision.matched_rules,
            vec!["float all app frames", "keep notepad tiled"]
        );
    }

    #[test]
    fn fullscreen_detection_accepts_monitor_bounds_with_tolerance() {
        let monitors = [MonitorInfo {
            id: MonitorId(7),
            is_primary: true,
            rect: Rect::from_size(0, 0, 1920, 1080),
            work_area: Rect::from_size(0, 0, 1920, 1040),
        }];

        let detection = detect_fullscreen_rect(Rect::from_size(-3, 2, 1924, 1077), &monitors, 4);

        assert_eq!(
            detection,
            FullscreenDetection {
                is_fullscreen: true,
                monitor: Some(MonitorId(7)),
                area: Some(FullscreenArea::MonitorBounds),
            }
        );
    }

    #[test]
    fn fullscreen_detection_accepts_work_area_with_tolerance() {
        let monitors = [MonitorInfo {
            id: MonitorId(7),
            is_primary: true,
            rect: Rect::from_size(0, 0, 1920, 1080),
            work_area: Rect::from_size(0, 0, 1920, 1040),
        }];

        let detection = detect_fullscreen_rect(Rect::from_size(0, 0, 1920, 1038), &monitors, 4);

        assert_eq!(detection.monitor, Some(MonitorId(7)));
        assert_eq!(detection.area, Some(FullscreenArea::WorkArea));
    }

    #[test]
    fn game_mode_detects_configured_executables_before_fullscreen() {
        let monitors = [MonitorInfo {
            id: MonitorId(7),
            is_primary: true,
            rect: Rect::from_size(0, 0, 1920, 1080),
            work_area: Rect::from_size(0, 0, 1920, 1040),
        }];
        let mut game = window();
        game.executable_path = Some(r"C:\Games\CS2.EXE".to_owned());
        game.rect = Rect::from_size(100, 100, 1280, 720);
        let policy = GameModePolicy {
            game_exes: vec!["cs2.exe".to_owned()],
            ..GameModePolicy::default()
        };

        let detection = detect_game_mode(Some(&game), &monitors, &[], &policy);

        assert!(detection.active);
        assert_eq!(detection.matched_executable.as_deref(), Some("cs2.exe"));
        assert!(matches!(
            detection.reason,
            Some(GameModeReason::ConfiguredExecutable(_))
        ));
    }

    #[test]
    fn game_mode_detects_rule_modes() {
        let monitors = [MonitorInfo {
            id: MonitorId(7),
            is_primary: true,
            rect: Rect::from_size(0, 0, 1920, 1080),
            work_area: Rect::from_size(0, 0, 1920, 1040),
        }];
        let rule = WindowRule {
            name: "steam game".to_owned(),
            matcher: WindowRuleMatch {
                process_name: Some(TextMatcher::Exact("notepad.exe".to_owned())),
                ..WindowRuleMatch::default()
            },
            action: WindowRuleAction {
                mode: Some(WindowRuleMode::Game),
                ..WindowRuleAction::default()
            },
        };

        let detection = detect_game_mode(
            Some(&window()),
            &monitors,
            &[rule],
            &GameModePolicy::default(),
        );

        assert!(detection.active);
        assert_eq!(detection.matched_rule_mode, Some(WindowRuleMode::Game));
        assert_eq!(detection.matched_rules, vec!["steam game"]);
    }
}
