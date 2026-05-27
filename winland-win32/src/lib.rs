#[cfg(windows)]
mod platform {
    use std::ffi::OsString;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStringExt;

    use tracing::debug;
    use windows::Win32::Foundation::{BOOL, CloseHandle, HANDLE, HWND, LPARAM, RECT, TRUE};
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetClassNameW, GetWindow, GetWindowLongPtrW,
        GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
        IsWindowVisible, WS_EX_TOOLWINDOW,
    };
    use windows::core::PWSTR;
    use winland_core::{Rect, WindowHandle, WindowInfo, WindowStyles};

    use crate::{Result, Win32Error};

    pub fn enumerate_windows() -> Result<Vec<WindowInfo>> {
        let mut windows = Vec::new();
        let state = EnumState {
            windows: &mut windows,
        };
        let state_ptr = &state as *const EnumState<'_> as isize;

        unsafe {
            EnumWindows(Some(enum_windows_proc), LPARAM(state_ptr)).map_err(Win32Error::from)?;
        }

        debug!(count = windows.len(), "enumerated top-level windows");
        Ok(windows)
    }

    struct EnumState<'a> {
        windows: &'a mut Vec<WindowInfo>,
    }

    unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let state = unsafe { &mut *(lparam.0 as *mut EnumState<'_>) };

        match window_info(hwnd) {
            Ok(info) => state.windows.push(info),
            Err(error) => debug!(?hwnd, %error, "skipping window after metadata read failure"),
        }

        TRUE
    }

    fn window_info(hwnd: HWND) -> Result<WindowInfo> {
        let mut process_id = 0;

        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut process_id));
        }

        Ok(WindowInfo {
            handle: WindowHandle(hwnd.0 as usize as u64),
            title: window_title(hwnd)?,
            class_name: class_name(hwnd)?,
            process_id,
            executable_path: executable_path(process_id),
            is_visible: unsafe { IsWindowVisible(hwnd).as_bool() },
            is_minimized: unsafe { IsIconic(hwnd).as_bool() },
            is_dwm_cloaked: dwm_cloaked(hwnd),
            has_owner: unsafe { GetWindow(hwnd, GW_OWNER).is_ok_and(|owner| !owner.0.is_null()) },
            is_tool_window: is_tool_window(hwnd),
            styles: window_styles(hwnd),
            rect: window_rect(hwnd)?,
        })
    }

    fn window_title(hwnd: HWND) -> Result<String> {
        let len = unsafe { GetWindowTextLengthW(hwnd) };
        if len == 0 {
            return Ok(String::new());
        }

        let mut buffer = vec![0u16; len as usize + 1];
        let copied = unsafe { GetWindowTextW(hwnd, &mut buffer) };
        if copied == 0 {
            return Ok(String::new());
        }

        Ok(wide_to_string(&buffer[..copied as usize]))
    }

    fn class_name(hwnd: HWND) -> Result<String> {
        let mut buffer = vec![0u16; 256];
        let copied = unsafe { GetClassNameW(hwnd, &mut buffer) };
        if copied == 0 {
            return Err(Win32Error::last_error("GetClassNameW"));
        }

        Ok(wide_to_string(&buffer[..copied as usize]))
    }

    fn executable_path(process_id: u32) -> Option<String> {
        if process_id == 0 {
            return None;
        }

        let handle =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }.ok()?;
        let process = OwnedHandle(handle);

        let mut buffer = vec![0u16; 32768];
        let mut len = buffer.len() as u32;
        let ok = unsafe {
            QueryFullProcessImageNameW(
                process.0,
                PROCESS_NAME_FORMAT(0),
                PWSTR(buffer.as_mut_ptr()),
                &mut len,
            )
        };

        if ok.is_err() || len == 0 {
            return None;
        }

        Some(wide_to_string(&buffer[..len as usize]))
    }

    fn dwm_cloaked(hwnd: HWND) -> bool {
        let mut cloaked = 0u32;
        let result = unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut u32 as *mut _,
                size_of::<u32>() as u32,
            )
        };

        if let Err(error) = result {
            debug!(?hwnd, %error, "DwmGetWindowAttribute(DWMWA_CLOAKED) failed");
            false
        } else {
            cloaked != 0
        }
    }

    fn is_tool_window(hwnd: HWND) -> bool {
        let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
        ex_style & WS_EX_TOOLWINDOW.0 != 0
    }

    fn window_styles(hwnd: HWND) -> WindowStyles {
        WindowStyles {
            style: unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32,
            extended_style: unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32,
        }
    }

    fn window_rect(hwnd: HWND) -> Result<Rect> {
        let mut rect = RECT::default();
        unsafe {
            GetWindowRect(hwnd, &mut rect).map_err(Win32Error::from)?;
        }

        Ok(Rect {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        })
    }

    fn wide_to_string(slice: &[u16]) -> String {
        OsString::from_wide(slice).to_string_lossy().into_owned()
    }

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use crate::{Result, Win32Error};
    use winland_core::WindowInfo;

    pub fn enumerate_windows() -> Result<Vec<WindowInfo>> {
        Err(Win32Error::UnsupportedPlatform)
    }
}

pub use platform::enumerate_windows;

pub type Result<T> = std::result::Result<T, Win32Error>;

#[derive(Debug, thiserror::Error)]
pub enum Win32Error {
    #[cfg(windows)]
    #[error("{context} failed: {source}")]
    Windows {
        context: &'static str,
        #[source]
        source: windows::core::Error,
    },
    #[cfg(windows)]
    #[error(transparent)]
    Api(#[from] windows::core::Error),
    #[error("winland-win32 is only supported on Windows")]
    UnsupportedPlatform,
}

#[cfg(windows)]
impl Win32Error {
    fn last_error(context: &'static str) -> Self {
        Self::Windows {
            context,
            source: windows::core::Error::from_win32(),
        }
    }
}
