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
    pub fn width(self) -> i32 {
        self.right.saturating_sub(self.left)
    }

    pub fn height(self) -> i32 {
        self.bottom.saturating_sub(self.top)
    }

    pub fn is_empty(self) -> bool {
        self.width() <= 0 || self.height() <= 0
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
}
