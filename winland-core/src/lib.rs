use std::fmt;

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

pub fn tile_windows(work_area: Rect, windows: &[WindowHandle]) -> Vec<TileAssignment> {
    if work_area.is_empty() || windows.is_empty() {
        return Vec::new();
    }

    match windows {
        [] => Vec::new(),
        [window] => vec![TileAssignment {
            window: *window,
            rect: work_area,
        }],
        [master, stack @ ..] => {
            let master_width = work_area.width() / 2;
            let master_rect = Rect {
                left: work_area.left,
                top: work_area.top,
                right: work_area.left.saturating_add(master_width),
                bottom: work_area.bottom,
            };
            let stack_area = Rect {
                left: master_rect.right,
                top: work_area.top,
                right: work_area.right,
                bottom: work_area.bottom,
            };

            let mut assignments = Vec::with_capacity(windows.len());
            assignments.push(TileAssignment {
                window: *master,
                rect: master_rect,
            });
            assignments.extend(split_rows(stack_area, stack.len()).zip(stack).map(
                |(rect, window)| TileAssignment {
                    window: *window,
                    rect,
                },
            ));
            assignments
        }
    }
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

fn split_rows(area: Rect, rows: usize) -> impl Iterator<Item = Rect> {
    let base_height = area.height() / rows as i32;
    let remainder = area.height() % rows as i32;

    (0..rows).scan(area.top, move |top, index| {
        let extra = if (index as i32) < remainder { 1 } else { 0 };
        let height = base_height.saturating_add(extra);
        let rect = Rect::from_size(area.left, *top, area.width(), height);
        *top = top.saturating_add(height);
        Some(rect)
    })
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
}
