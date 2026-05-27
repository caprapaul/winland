#[cfg(windows)]
mod platform {
    use std::ffi::OsString;
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStringExt;
    use std::sync::mpsc::Sender;
    use std::sync::{Mutex, OnceLock};

    use tracing::{debug, warn};
    use windows::Win32::Foundation::{
        BOOL, CloseHandle, HANDLE, HMODULE, HWND, LPARAM, RECT, TRUE, WPARAM,
    };
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
    };
    use windows::Win32::System::Threading::{
        GetCurrentThreadId, OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
        UnregisterHotKey,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE,
        EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND,
        EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND,
        EVENT_SYSTEM_MOVESIZESTART, EnumWindows, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetClassNameW,
        GetForegroundWindow, GetMessageW, GetWindow, GetWindowLongPtrW, GetWindowRect,
        GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
        MONITORINFOF_PRIMARY, MSG, OBJID_WINDOW, PostThreadMessageW, SW_HIDE, SW_SHOWNOACTIVATE,
        SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER, SetForegroundWindow, SetWindowPos,
        ShowWindow, TranslateMessage, WINEVENT_OUTOFCONTEXT, WM_HOTKEY, WM_QUIT, WS_EX_TOOLWINDOW,
    };
    use windows::core::PWSTR;
    use winland_core::{
        MonitorId, MonitorInfo as CoreMonitorInfo, Rect, WindowHandle, WindowInfo, WindowStyles,
    };

    use crate::{
        HotkeyBinding, HotkeyEvent, HotkeyModifierSet, HotkeyRegistrationFailure, Result,
        Win32Error, WindowEvent, WindowEventKind,
    };

    static EVENT_SENDER: OnceLock<Mutex<Option<Sender<WindowEvent>>>> = OnceLock::new();
    static HOTKEY_SENDER: OnceLock<Mutex<Option<HotkeySenderState>>> = OnceLock::new();

    pub fn enumerate_windows() -> Result<Vec<WindowInfo>> {
        let mut windows = Vec::new();
        let state = EnumState {
            windows: &mut windows,
        };
        let state_ptr = &state as *const EnumState<'_> as isize;

        // SAFETY: The callback receives the state pointer only for the duration of
        // EnumWindows. The pointed-to Vec outlives the call and is not moved.
        unsafe {
            EnumWindows(Some(enum_windows_proc), LPARAM(state_ptr)).map_err(Win32Error::from)?;
        }

        debug!(count = windows.len(), "enumerated top-level windows");
        Ok(windows)
    }

    pub fn enumerate_monitors() -> Result<Vec<CoreMonitorInfo>> {
        let mut monitors = Vec::new();
        let state = MonitorEnumState {
            monitors: &mut monitors,
        };
        let state_ptr = &state as *const MonitorEnumState<'_> as isize;

        // SAFETY: The callback receives the state pointer only for the duration of
        // EnumDisplayMonitors. The pointed-to Vec outlives the call and is not moved.
        let ok = unsafe {
            EnumDisplayMonitors(
                HDC::default(),
                None,
                Some(enum_monitor_proc),
                LPARAM(state_ptr),
            )
        };
        if !ok.as_bool() {
            return Err(Win32Error::last_error("EnumDisplayMonitors"));
        }

        debug!(count = monitors.len(), "enumerated display monitors");
        Ok(monitors)
    }

    pub fn move_resize_window(handle: WindowHandle, rect: Rect) -> Result<()> {
        let hwnd = hwnd_from_handle(handle);

        // SAFETY: hwnd comes from earlier documented window enumeration, and the
        // rectangle is ordinary screen coordinates. SWP flags avoid activation and
        // z-order changes so this is a geometry-only request.
        unsafe {
            SetWindowPos(
                hwnd,
                HWND::default(),
                rect.left,
                rect.top,
                rect.width(),
                rect.height(),
                SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOZORDER,
            )
            .map_err(|source| Win32Error::Windows {
                context: "SetWindowPos",
                source,
            })?;
        }

        Ok(())
    }

    pub fn foreground_window() -> Result<Option<WindowHandle>> {
        // SAFETY: GetForegroundWindow reads the current foreground HWND and does
        // not require ownership of that window handle.
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.0.is_null() {
            Ok(None)
        } else {
            Ok(Some(handle_from_hwnd(hwnd)))
        }
    }

    pub fn focus_window(handle: WindowHandle) -> Result<()> {
        let hwnd = hwnd_from_handle(handle);

        // SAFETY: hwnd is a top-level window handle tracked from documented
        // enumeration or foreground APIs. SetForegroundWindow may still be
        // denied by Windows focus rules; the BOOL return is checked below.
        let ok = unsafe { SetForegroundWindow(hwnd) };
        if ok.as_bool() {
            Ok(())
        } else {
            Err(Win32Error::last_error("SetForegroundWindow"))
        }
    }

    pub fn hide_window(handle: WindowHandle) -> Result<()> {
        let hwnd = hwnd_from_handle(handle);

        // SAFETY: hwnd is a top-level window handle tracked from documented
        // enumeration. SW_HIDE asks Windows to hide the window without changing
        // ownership, styles, or DWM behavior.
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }

        Ok(())
    }

    pub fn show_window_without_activate(handle: WindowHandle) -> Result<()> {
        let hwnd = hwnd_from_handle(handle);

        // SAFETY: hwnd is a top-level window handle tracked from documented
        // enumeration. SW_SHOWNOACTIVATE makes a hidden workspace window visible
        // without stealing focus from the user's current foreground window.
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }

        Ok(())
    }

    pub fn subscribe_window_events(sender: Sender<WindowEvent>) -> Result<WindowEventSubscription> {
        {
            let mut guard = event_sender_slot()
                .lock()
                .map_err(|_| Win32Error::EventSenderLockPoisoned)?;
            if guard.is_some() {
                return Err(Win32Error::EventHooksAlreadyInstalled);
            }

            *guard = Some(sender);
        }

        match install_window_event_hooks() {
            Ok(subscription) => Ok(subscription),
            Err(error) => {
                clear_event_sender();
                Err(error)
            }
        }
    }

    pub fn register_hotkeys(
        bindings: Vec<HotkeyBinding>,
        sender: Sender<HotkeyEvent>,
    ) -> Result<HotkeyRegistration> {
        {
            let mut guard = hotkey_sender_slot()
                .lock()
                .map_err(|_| Win32Error::HotkeySenderLockPoisoned)?;
            if guard.is_some() {
                return Err(Win32Error::HotkeysAlreadyRegistered);
            }

            // SAFETY: This reads the id of the current thread. Hotkeys are
            // registered against this same thread below, and the message loop is
            // expected to run on this thread.
            let message_thread_id = unsafe { GetCurrentThreadId() };
            *guard = Some(HotkeySenderState {
                sender,
                message_thread_id,
            });
        }

        let mut registered = Vec::with_capacity(bindings.len());
        let mut failures = Vec::new();
        for binding in &bindings {
            match register_hotkey(binding) {
                Ok(()) => registered.push(binding.clone()),
                Err(error) => {
                    failures.push(HotkeyRegistrationFailure {
                        id: binding.id,
                        description: binding.description.clone(),
                        error: error.to_string(),
                    });
                }
            }
        }

        if registered.is_empty() {
            clear_hotkey_sender();
            return Err(Win32Error::NoHotkeysRegistered { failures });
        }

        debug!(
            registered = registered.len(),
            failed = failures.len(),
            "registered daemon hotkeys"
        );
        Ok(HotkeyRegistration {
            bindings: registered,
            failures,
        })
    }

    pub fn request_message_loop_stop() -> Result<()> {
        let thread_id = {
            let guard = hotkey_sender_slot()
                .lock()
                .map_err(|_| Win32Error::HotkeySenderLockPoisoned)?;
            guard
                .as_ref()
                .map(|state| state.message_thread_id)
                .ok_or(Win32Error::HotkeysNotRegistered)?
        };

        // SAFETY: thread_id is the thread where hotkeys were registered and
        // where the daemon message loop runs. Posting WM_QUIT requests a clean
        // GetMessageW exit without touching other thread state.
        unsafe {
            PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)).map_err(|source| {
                Win32Error::Windows {
                    context: "PostThreadMessageW(WM_QUIT)",
                    source,
                }
            })?;
        }

        Ok(())
    }

    pub fn run_message_loop() -> Result<()> {
        let mut message = MSG::default();

        loop {
            // SAFETY: message points to valid writable storage. A null HWND
            // receives thread messages, which is what WinEvent delivery needs.
            let result = unsafe { GetMessageW(&mut message, HWND::default(), 0, 0) };
            match result.0 {
                -1 => return Err(Win32Error::last_error("GetMessageW")),
                0 => return Ok(()),
                _ => {
                    if message.message == WM_HOTKEY {
                        dispatch_hotkey_message(message.wParam.0 as i32);
                        continue;
                    }

                    // SAFETY: message was just filled by GetMessageW.
                    unsafe {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
            }
        }
    }

    pub struct WindowEventSubscription {
        hooks: Vec<HWINEVENTHOOK>,
    }

    impl Drop for WindowEventSubscription {
        fn drop(&mut self) {
            for hook in self.hooks.drain(..) {
                // SAFETY: These hook handles were returned by SetWinEventHook
                // for this subscription and are unhooked at most once here.
                let ok = unsafe { UnhookWinEvent(hook) };
                if !ok.as_bool() {
                    warn!(?hook, "failed to unhook WinEvent hook");
                }
            }

            clear_event_sender();
        }
    }

    pub struct HotkeyRegistration {
        bindings: Vec<HotkeyBinding>,
        failures: Vec<HotkeyRegistrationFailure>,
    }

    impl HotkeyRegistration {
        pub fn failures(&self) -> &[HotkeyRegistrationFailure] {
            &self.failures
        }
    }

    impl Drop for HotkeyRegistration {
        fn drop(&mut self) {
            for binding in self.bindings.drain(..) {
                unregister_hotkey(binding);
            }

            clear_hotkey_sender();
        }
    }

    struct HotkeySenderState {
        sender: Sender<HotkeyEvent>,
        message_thread_id: u32,
    }

    struct EnumState<'a> {
        windows: &'a mut Vec<WindowInfo>,
    }

    unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: lparam is the EnumState pointer supplied to EnumWindows above,
        // and the callback is invoked synchronously during that call.
        let state = unsafe { &mut *(lparam.0 as *mut EnumState<'_>) };

        match window_info(hwnd) {
            Ok(info) => state.windows.push(info),
            Err(error) => debug!(?hwnd, %error, "skipping window after metadata read failure"),
        }

        TRUE
    }

    unsafe extern "system" fn win_event_proc(
        _hook: HWINEVENTHOOK,
        event: u32,
        hwnd: HWND,
        id_object: i32,
        id_child: i32,
        _event_thread: u32,
        event_time: u32,
    ) {
        let Some(event) = window_event(event, hwnd, id_object, id_child, event_time) else {
            return;
        };

        let Some(slot) = EVENT_SENDER.get() else {
            return;
        };

        match slot.lock() {
            Ok(guard) => {
                if let Some(sender) = guard.as_ref() {
                    let _ = sender.send(event);
                }
            }
            Err(_) => warn!("window event sender lock is poisoned"),
        }
    }

    fn register_hotkey(binding: &HotkeyBinding) -> Result<()> {
        let modifiers = hotkey_modifiers(binding.modifiers);

        // SAFETY: A null HWND registers the hotkey for the current thread, which
        // owns the daemon message loop. The id is supplied by daemon-owned
        // bindings and unregistered by HotkeyRegistration::drop.
        unsafe {
            RegisterHotKey(
                HWND::default(),
                binding.id.0,
                modifiers,
                binding.virtual_key.0,
            )
            .map_err(|source| Win32Error::HotkeyRegistration {
                id: binding.id.0,
                description: binding.description.clone(),
                source,
            })?;
        }

        Ok(())
    }

    fn unregister_hotkey(binding: HotkeyBinding) {
        // SAFETY: The id belongs to a binding that was successfully registered
        // for this thread by register_hotkeys and is unregistered at most once
        // during HotkeyRegistration::drop or rollback.
        let result = unsafe { UnregisterHotKey(HWND::default(), binding.id.0) };
        if let Err(error) = result {
            warn!(
                id = binding.id.0,
                description = %binding.description,
                %error,
                "failed to unregister hotkey"
            );
        }
    }

    fn dispatch_hotkey_message(id: i32) {
        let Some(slot) = HOTKEY_SENDER.get() else {
            return;
        };

        match slot.lock() {
            Ok(guard) => {
                if let Some(state) = guard.as_ref() {
                    let _ = state.sender.send(HotkeyEvent { id: id.into() });
                }
            }
            Err(_) => warn!("hotkey sender lock is poisoned"),
        }
    }

    fn hotkey_modifiers(modifiers: HotkeyModifierSet) -> HOT_KEY_MODIFIERS {
        let mut value = HOT_KEY_MODIFIERS(0);

        if modifiers.alt {
            value |= MOD_ALT;
        }
        if modifiers.control {
            value |= MOD_CONTROL;
        }
        if modifiers.shift {
            value |= MOD_SHIFT;
        }
        if modifiers.super_key {
            value |= MOD_WIN;
        }
        if modifiers.no_repeat {
            value |= MOD_NOREPEAT;
        }

        value
    }

    struct MonitorEnumState<'a> {
        monitors: &'a mut Vec<CoreMonitorInfo>,
    }

    unsafe extern "system" fn enum_monitor_proc(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        // SAFETY: lparam is the MonitorEnumState pointer supplied to
        // EnumDisplayMonitors above, and the callback is synchronous.
        let state = unsafe { &mut *(lparam.0 as *mut MonitorEnumState<'_>) };

        match monitor_info(monitor) {
            Ok(info) => state.monitors.push(info),
            Err(error) => debug!(?monitor, %error, "skipping monitor after metadata read failure"),
        }

        TRUE
    }

    fn monitor_info(monitor: HMONITOR) -> Result<CoreMonitorInfo> {
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..MONITORINFO::default()
        };

        // SAFETY: info is initialized with the required cbSize and points to valid
        // writable memory for the duration of the call.
        let ok = unsafe { GetMonitorInfoW(monitor, &mut info) };
        if !ok.as_bool() {
            return Err(Win32Error::last_error("GetMonitorInfoW"));
        }

        Ok(CoreMonitorInfo {
            id: MonitorId(monitor.0 as usize as u64),
            is_primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
            rect: rect_from_win32(info.rcMonitor),
            work_area: rect_from_win32(info.rcWork),
        })
    }

    fn install_window_event_hooks() -> Result<WindowEventSubscription> {
        let mut hooks = Vec::with_capacity(3);

        for (event_min, event_max) in [
            (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
            (EVENT_SYSTEM_MOVESIZESTART, EVENT_SYSTEM_MOVESIZEEND),
            (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
            (EVENT_OBJECT_CREATE, EVENT_OBJECT_LOCATIONCHANGE),
        ] {
            match install_window_event_hook(event_min, event_max) {
                Ok(hook) => hooks.push(hook),
                Err(error) => {
                    for hook in hooks.drain(..) {
                        // SAFETY: The handle came from SetWinEventHook in this
                        // function and has not been unhooked yet.
                        let _ = unsafe { UnhookWinEvent(hook) };
                    }
                    return Err(error);
                }
            }
        }

        Ok(WindowEventSubscription { hooks })
    }

    fn install_window_event_hook(event_min: u32, event_max: u32) -> Result<HWINEVENTHOOK> {
        // SAFETY: The callback is a process-static function pointer. Passing
        // process/thread id 0 subscribes to all processes on the current desktop,
        // and WINEVENT_OUTOFCONTEXT avoids injecting code into other processes.
        let hook = unsafe {
            SetWinEventHook(
                event_min,
                event_max,
                HMODULE::default(),
                Some(win_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            )
        };

        if hook.0.is_null() {
            return Err(Win32Error::last_error("SetWinEventHook"));
        }

        Ok(hook)
    }

    fn window_event(
        event: u32,
        hwnd: HWND,
        id_object: i32,
        id_child: i32,
        event_time: u32,
    ) -> Option<WindowEvent> {
        if hwnd.0.is_null() || id_object != OBJID_WINDOW.0 || id_child != 0 {
            return None;
        }

        let kind = match event {
            EVENT_OBJECT_CREATE => WindowEventKind::Created,
            EVENT_OBJECT_DESTROY => WindowEventKind::Destroyed,
            EVENT_OBJECT_SHOW => WindowEventKind::Shown,
            EVENT_OBJECT_HIDE => WindowEventKind::Hidden,
            EVENT_OBJECT_LOCATIONCHANGE => WindowEventKind::Moved,
            EVENT_SYSTEM_MOVESIZESTART => WindowEventKind::MoveSizeStart,
            EVENT_SYSTEM_MOVESIZEEND => WindowEventKind::MoveSizeEnd,
            EVENT_SYSTEM_MINIMIZESTART => WindowEventKind::Minimized,
            EVENT_SYSTEM_MINIMIZEEND => WindowEventKind::Restored,
            EVENT_SYSTEM_FOREGROUND => WindowEventKind::ForegroundChanged,
            _ => return None,
        };

        Some(WindowEvent {
            kind,
            window: handle_from_hwnd(hwnd),
            event_time,
        })
    }

    fn window_info(hwnd: HWND) -> Result<WindowInfo> {
        let mut process_id = 0;

        // SAFETY: hwnd is provided by EnumWindows, and process_id points to valid
        // writable memory for the duration of the call.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut process_id));
        }

        // SAFETY: hwnd is provided by EnumWindows and remains valid for this
        // metadata query. The call does not mutate application state.
        let is_visible = unsafe { IsWindowVisible(hwnd).as_bool() };
        // SAFETY: hwnd is provided by EnumWindows and remains valid for this
        // metadata query. The call does not mutate application state.
        let is_minimized = unsafe { IsIconic(hwnd).as_bool() };
        // SAFETY: hwnd is provided by EnumWindows and remains valid for this
        // metadata query. The owner handle is only inspected for nullness.
        let has_owner = unsafe { GetWindow(hwnd, GW_OWNER).is_ok_and(|owner| !owner.0.is_null()) };

        Ok(WindowInfo {
            handle: WindowHandle(hwnd.0 as usize as u64),
            title: window_title(hwnd)?,
            class_name: class_name(hwnd)?,
            process_id,
            executable_path: executable_path(process_id),
            is_visible,
            is_minimized,
            is_dwm_cloaked: dwm_cloaked(hwnd),
            has_owner,
            is_tool_window: is_tool_window(hwnd),
            styles: window_styles(hwnd),
            rect: window_rect(hwnd)?,
        })
    }

    fn window_title(hwnd: HWND) -> Result<String> {
        // SAFETY: hwnd is a top-level window handle from EnumWindows. This reads
        // metadata only and does not retain pointers after the call.
        let len = unsafe { GetWindowTextLengthW(hwnd) };
        if len == 0 {
            return Ok(String::new());
        }

        let mut buffer = vec![0u16; len as usize + 1];
        // SAFETY: buffer is writable UTF-16 storage with capacity len + 1 for the
        // terminating NUL expected by GetWindowTextW.
        let copied = unsafe { GetWindowTextW(hwnd, &mut buffer) };
        if copied == 0 {
            return Ok(String::new());
        }

        Ok(wide_to_string(&buffer[..copied as usize]))
    }

    fn class_name(hwnd: HWND) -> Result<String> {
        let mut buffer = vec![0u16; 256];
        // SAFETY: buffer is writable UTF-16 storage passed only for the duration
        // of the call, and hwnd is supplied by EnumWindows.
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

        // SAFETY: process_id was obtained from Win32. Requesting limited query
        // access does not mutate the process, and the returned handle is owned.
        let handle =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }.ok()?;
        let process = OwnedHandle(handle);

        let mut buffer = vec![0u16; 32768];
        let mut len = buffer.len() as u32;
        // SAFETY: process.0 is a live handle owned by OwnedHandle, buffer points
        // to writable UTF-16 storage, and len points to its current capacity.
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
        // SAFETY: cloaked points to valid writable storage sized as passed, and
        // hwnd is only queried for a documented DWM attribute.
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
        // SAFETY: hwnd is queried for its extended style only.
        let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
        ex_style & WS_EX_TOOLWINDOW.0 != 0
    }

    fn window_styles(hwnd: HWND) -> WindowStyles {
        // SAFETY: hwnd is queried for style bits only.
        let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
        // SAFETY: hwnd is queried for extended style bits only.
        let extended_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;

        WindowStyles {
            style,
            extended_style,
        }
    }

    fn window_rect(hwnd: HWND) -> Result<Rect> {
        let mut rect = RECT::default();
        // SAFETY: rect points to valid writable storage for the duration of the
        // call, and hwnd is a top-level window handle from EnumWindows.
        unsafe {
            GetWindowRect(hwnd, &mut rect).map_err(Win32Error::from)?;
        }

        Ok(rect_from_win32(rect))
    }

    fn rect_from_win32(rect: RECT) -> Rect {
        Rect {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        }
    }

    fn hwnd_from_handle(handle: WindowHandle) -> HWND {
        HWND(handle.0 as usize as *mut c_void)
    }

    fn handle_from_hwnd(hwnd: HWND) -> WindowHandle {
        WindowHandle(hwnd.0 as usize as u64)
    }

    fn event_sender_slot() -> &'static Mutex<Option<Sender<WindowEvent>>> {
        EVENT_SENDER.get_or_init(|| Mutex::new(None))
    }

    fn hotkey_sender_slot() -> &'static Mutex<Option<HotkeySenderState>> {
        HOTKEY_SENDER.get_or_init(|| Mutex::new(None))
    }

    fn clear_event_sender() {
        match event_sender_slot().lock() {
            Ok(mut guard) => *guard = None,
            Err(_) => warn!("window event sender lock is poisoned while clearing"),
        }
    }

    fn clear_hotkey_sender() {
        match hotkey_sender_slot().lock() {
            Ok(mut guard) => *guard = None,
            Err(_) => warn!("hotkey sender lock is poisoned while clearing"),
        }
    }

    fn wide_to_string(slice: &[u16]) -> String {
        OsString::from_wide(slice).to_string_lossy().into_owned()
    }

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: OwnedHandle only wraps handles returned by OpenProcess in
            // this module, and Drop runs at most once for the owned value.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use std::sync::mpsc::Sender;

    use crate::{HotkeyBinding, HotkeyEvent, Result, Win32Error};
    use winland_core::{MonitorInfo, Rect, WindowHandle, WindowInfo};

    use crate::WindowEvent;

    pub fn enumerate_windows() -> Result<Vec<WindowInfo>> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn enumerate_monitors() -> Result<Vec<MonitorInfo>> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn move_resize_window(_handle: WindowHandle, _rect: Rect) -> Result<()> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn foreground_window() -> Result<Option<WindowHandle>> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn focus_window(_handle: WindowHandle) -> Result<()> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn hide_window(_handle: WindowHandle) -> Result<()> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn show_window_without_activate(_handle: WindowHandle) -> Result<()> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn subscribe_window_events(
        _sender: Sender<WindowEvent>,
    ) -> Result<WindowEventSubscription> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn register_hotkeys(
        _bindings: Vec<HotkeyBinding>,
        _sender: Sender<HotkeyEvent>,
    ) -> Result<HotkeyRegistration> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn request_message_loop_stop() -> Result<()> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn run_message_loop() -> Result<()> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub struct WindowEventSubscription;
    pub struct HotkeyRegistration;

    impl HotkeyRegistration {
        pub fn failures(&self) -> &[HotkeyRegistrationFailure] {
            &[]
        }
    }
}

pub use platform::HotkeyRegistration;
pub use platform::WindowEventSubscription;
pub use platform::enumerate_monitors;
pub use platform::enumerate_windows;
pub use platform::focus_window;
pub use platform::foreground_window;
pub use platform::hide_window;
pub use platform::move_resize_window;
pub use platform::register_hotkeys;
pub use platform::request_message_loop_stop;
pub use platform::run_message_loop;
pub use platform::show_window_without_activate;
pub use platform::subscribe_window_events;
use winland_core::WindowHandle;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyBinding {
    pub id: HotkeyId,
    pub modifiers: HotkeyModifierSet,
    pub virtual_key: VirtualKey,
    pub description: String,
}

impl HotkeyBinding {
    pub fn new(
        id: HotkeyId,
        modifiers: HotkeyModifierSet,
        virtual_key: VirtualKey,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id,
            modifiers,
            virtual_key,
            description: description.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyRegistrationFailure {
    pub id: HotkeyId,
    pub description: String,
    pub error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HotkeyId(pub i32);

impl From<i32> for HotkeyId {
    fn from(value: i32) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotkeyModifierSet {
    pub alt: bool,
    pub control: bool,
    pub shift: bool,
    pub super_key: bool,
    pub no_repeat: bool,
}

impl HotkeyModifierSet {
    pub const fn new() -> Self {
        Self {
            alt: false,
            control: false,
            shift: false,
            super_key: false,
            no_repeat: true,
        }
    }

    pub const fn alt(mut self) -> Self {
        self.alt = true;
        self
    }

    pub const fn control(mut self) -> Self {
        self.control = true;
        self
    }

    pub const fn shift(mut self) -> Self {
        self.shift = true;
        self
    }

    pub const fn super_key(mut self) -> Self {
        self.super_key = true;
        self
    }
}

impl Default for HotkeyModifierSet {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualKey(pub u32);

impl VirtualKey {
    pub const fn ascii_uppercase(byte: u8) -> Self {
        Self(byte as u32)
    }

    pub const SPACE: Self = Self(0x20);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotkeyEvent {
    pub id: HotkeyId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowEvent {
    pub kind: WindowEventKind,
    pub window: WindowHandle,
    pub event_time: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WindowEventKind {
    Created,
    Destroyed,
    Shown,
    Hidden,
    Moved,
    MoveSizeStart,
    MoveSizeEnd,
    Minimized,
    Restored,
    ForegroundChanged,
}

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
    #[error("window event hooks are already installed")]
    EventHooksAlreadyInstalled,
    #[error("window event sender lock is poisoned")]
    EventSenderLockPoisoned,
    #[error("hotkeys are already registered")]
    HotkeysAlreadyRegistered,
    #[error("hotkeys are not registered")]
    HotkeysNotRegistered,
    #[cfg(windows)]
    #[error("failed to register hotkey {id} ({description}): {source}")]
    HotkeyRegistration {
        id: i32,
        description: String,
        #[source]
        source: windows::core::Error,
    },
    #[error("no daemon hotkeys could be registered: {failures:?}")]
    NoHotkeysRegistered {
        failures: Vec<HotkeyRegistrationFailure>,
    },
    #[error("hotkey sender lock is poisoned")]
    HotkeySenderLockPoisoned,
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
