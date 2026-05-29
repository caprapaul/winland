#[cfg(windows)]
mod platform {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStringExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Mutex, OnceLock};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use tracing::{debug, warn};
    use windows::Win32::Foundation::{
        BOOL, COLORREF, CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
        GENERIC_READ, GENERIC_WRITE, HANDLE, HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, TRUE,
        WPARAM,
    };
    use windows::Win32::Graphics::Dwm::{
        DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
    };
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, EnumDisplayMonitors, FillRect,
        GetMonitorInfoW, HDC, HMONITOR, InvalidateRect, MONITORINFO, PAINTSTRUCT,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_MODE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
        ReadFile, WriteFile,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
        PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows::Win32::System::Threading::{
        AttachThreadInput, GetCurrentThreadId, OpenProcess, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
        MOD_WIN, RegisterHotKey, SetFocus, UnregisterHotKey,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow,
        DispatchMessageW, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY,
        EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_SHOW,
        EVENT_OBJECT_STATECHANGE, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND,
        EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND,
        EVENT_SYSTEM_MOVESIZESTART, EnumWindows, GA_ROOT, GW_OWNER, GWL_EXSTYLE, GWL_STYLE,
        GWLP_USERDATA, GetAncestor, GetClassNameW, GetCursorPos, GetForegroundWindow, GetMessageW,
        GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
        GetWindowThreadProcessId, HC_ACTION, HHOOK, HTTRANSPARENT, IsIconic, IsWindowVisible,
        KBDLLHOOKSTRUCT, MINMAXINFO, MONITORINFOF_PRIMARY, MSG, MSLLHOOKSTRUCT, OBJID_WINDOW,
        PM_NOREMOVE, PeekMessageW, PostThreadMessageW, RegisterClassW, SMTO_ABORTIFHUNG, SW_HIDE,
        SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER, SWP_SHOWWINDOW,
        SendMessageTimeoutW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos,
        SetWindowsHookExW, ShowWindow, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL,
        WH_MOUSE_LL, WINEVENT_OUTOFCONTEXT, WM_APP, WM_GETMINMAXINFO, WM_HOTKEY, WM_KEYDOWN,
        WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCHITTEST, WM_PAINT, WM_QUIT, WM_SYSKEYDOWN,
        WNDCLASSW, WS_CAPTION, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
        WS_THICKFRAME, WindowFromPoint,
    };
    use windows::core::{PCWSTR, PWSTR};
    use winland_core::{
        MonitorId, MonitorInfo as CoreMonitorInfo, Point, Rect, Size, WindowHandle, WindowInfo,
        WindowSizeConstraints, WindowStyles,
    };

    use crate::{
        BorderUpdate, HotkeyBinding, HotkeyEvent, HotkeyInterceptionDecision, HotkeyLowLevelEvent,
        HotkeyModifierSet, HotkeyOverrideOptions, HotkeyRegistrationFailure, HotkeyWindowContext,
        IPC_BUFFER_SIZE, IpcTransportRequest, ModifierDragOptions, MouseDragEvent,
        MouseDragEventKind, Result, VirtualKey, Win32Error, WindowEvent, WindowEventKind,
        classify_intercepted_hotkey, modifiers_match, rect_covers_any_monitor,
    };

    static EVENT_SENDER: OnceLock<Mutex<Option<Sender<WindowEvent>>>> = OnceLock::new();
    static HOTKEY_SENDER: OnceLock<Mutex<Option<HotkeySenderState>>> = OnceLock::new();
    static HOTKEY_OVERRIDE: OnceLock<Mutex<Option<HotkeyOverrideState>>> = OnceLock::new();
    static MOUSE_DRAG: OnceLock<Mutex<Option<MouseDragState>>> = OnceLock::new();
    static INPUT_HOOKS_PAUSED: AtomicBool = AtomicBool::new(false);
    const BORDER_CLASS_NAME: &str = "WinlandBorderOverlay";
    const BORDER_COMMAND_MESSAGE: u32 = WM_APP + 0x57;
    const MINMAXINFO_TIMEOUT_MS: u32 = 50;
    const GEOMETRY_CORRECTION_PASSES: usize = 3;
    const MODIFIER_DRAG_MOVE_INTERVAL: Duration = Duration::from_millis(16);

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
        let target = outer_rect_for_desired_visible_rect(hwnd, rect).unwrap_or(rect);
        let mut target = target;

        for pass in 0..GEOMETRY_CORRECTION_PASSES {
            set_window_outer_rect(hwnd, target)?;

            let Some(actual) = visible_window_rect(hwnd) else {
                return Ok(());
            };

            if actual == rect {
                return Ok(());
            }

            if (actual.width() != rect.width() || actual.height() != rect.height())
                && pass + 1 == GEOMETRY_CORRECTION_PASSES
            {
                return Ok(());
            }

            target = Rect {
                left: target
                    .left
                    .saturating_add(rect.left.saturating_sub(actual.left)),
                top: target
                    .top
                    .saturating_add(rect.top.saturating_sub(actual.top)),
                right: target
                    .right
                    .saturating_add(rect.right.saturating_sub(actual.right)),
                bottom: target
                    .bottom
                    .saturating_add(rect.bottom.saturating_sub(actual.bottom)),
            };
        }

        Ok(())
    }

    fn set_window_outer_rect(hwnd: HWND, rect: Rect) -> Result<()> {
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
            })
        }
    }

    pub fn window_rect_for_handle(handle: WindowHandle) -> Result<Rect> {
        layout_window_rect(hwnd_from_handle(handle))
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

    pub fn cursor_position() -> Result<Point> {
        let mut point = POINT::default();

        // SAFETY: GetCursorPos writes to the initialized POINT provided by this
        // stack frame and does not retain the pointer after returning.
        unsafe {
            GetCursorPos(&mut point).map_err(|source| Win32Error::Windows {
                context: "GetCursorPos",
                source,
            })?;
        }

        Ok(Point {
            x: point.x,
            y: point.y,
        })
    }

    pub fn focus_window(handle: WindowHandle) -> Result<()> {
        let hwnd = hwnd_from_handle(handle);
        activate_window(hwnd)
    }

    pub fn set_input_hooks_paused(paused: bool) {
        INPUT_HOOKS_PAUSED.store(paused, Ordering::Relaxed);
        debug!(paused, "updated low-level input hook pause state");
    }

    fn activate_window(hwnd: HWND) -> Result<()> {
        let current_thread_id = unsafe { GetCurrentThreadId() };
        // SAFETY: hwnd is a live top-level window handle. Passing None asks
        // Windows to return the owning GUI thread id without writing a process
        // id.
        let target_thread_id = unsafe { GetWindowThreadProcessId(hwnd, None) };
        // SAFETY: GetForegroundWindow only returns the current foreground HWND.
        let foreground_hwnd = unsafe { GetForegroundWindow() };
        let foreground_thread_id = if foreground_hwnd.0.is_null() {
            0
        } else {
            // SAFETY: foreground_hwnd came from GetForegroundWindow and is used
            // only to query its owning GUI thread id.
            unsafe { GetWindowThreadProcessId(foreground_hwnd, None) }
        };

        let _target_input = ThreadInputAttachment::attach(current_thread_id, target_thread_id);
        let _foreground_input = (foreground_thread_id != target_thread_id)
            .then(|| ThreadInputAttachment::attach(current_thread_id, foreground_thread_id));

        // SAFETY: hwnd is a top-level window handle tracked from documented
        // enumeration, hit-testing, or foreground APIs. BringWindowToTop and
        // SetForegroundWindow request activation through the documented window
        // manager path; their return values are checked or treated as best
        // effort below.
        unsafe {
            let _ = BringWindowToTop(hwnd);
        }
        let ok = unsafe { SetForegroundWindow(hwnd) };
        // SAFETY: With thread input temporarily attached, SetFocus can complete
        // the activation that a swallowed mouse click would normally perform.
        unsafe {
            let _ = SetFocus(hwnd);
        }
        // SAFETY: GetForegroundWindow only returns the current foreground HWND.
        let foreground_after = unsafe { GetForegroundWindow() };

        if ok.as_bool() || foreground_after == hwnd {
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

    #[derive(Debug)]
    pub struct BorderManager {
        sender: Sender<BorderCommand>,
        worker: Option<JoinHandle<()>>,
        thread_id: u32,
    }

    impl BorderManager {
        pub fn new() -> Result<Self> {
            let (command_sender, command_receiver) = mpsc::channel();
            let (ready_sender, ready_receiver) = mpsc::channel();
            let worker = thread::Builder::new()
                .name("winland-border-overlays".to_owned())
                .spawn(move || border_worker(command_receiver, ready_sender))
                .map_err(|error| Win32Error::ThreadSpawn {
                    name: "winland-border-overlays",
                    message: error.to_string(),
                })?;
            let thread_id = ready_receiver
                .recv()
                .map_err(|_| Win32Error::BorderOverlayWorkerStopped)?;

            Ok(Self {
                sender: command_sender,
                worker: Some(worker),
                thread_id,
            })
        }

        pub fn sync(&self, updates: Vec<BorderUpdate>, width: i32) -> Result<()> {
            self.sender
                .send(BorderCommand::Sync { updates, width })
                .map_err(|_| Win32Error::BorderOverlayWorkerStopped)?;
            self.post_command_message()
        }

        pub fn clear(&self) -> Result<()> {
            self.sender
                .send(BorderCommand::Clear)
                .map_err(|_| Win32Error::BorderOverlayWorkerStopped)?;
            self.post_command_message()
        }

        fn post_command_message(&self) -> Result<()> {
            // SAFETY: thread_id belongs to the overlay worker thread, which
            // creates a message queue before reporting readiness.
            unsafe {
                PostThreadMessageW(self.thread_id, BORDER_COMMAND_MESSAGE, WPARAM(0), LPARAM(0))
                    .map_err(|source| Win32Error::Windows {
                        context: "PostThreadMessageW(border command)",
                        source,
                    })
            }
        }
    }

    impl Drop for BorderManager {
        fn drop(&mut self) {
            let _ = self.sender.send(BorderCommand::Shutdown);
            let _ = self.post_command_message();
            if let Some(worker) = self.worker.take()
                && worker.join().is_err()
            {
                warn!("border overlay worker thread panicked while stopping");
            }
        }
    }

    fn border_worker(receiver: Receiver<BorderCommand>, ready: Sender<u32>) {
        // SAFETY: GetCurrentThreadId reads the id of this worker thread.
        let thread_id = unsafe { GetCurrentThreadId() };
        let mut bootstrap_message = MSG::default();
        // SAFETY: PeekMessageW with PM_NOREMOVE forces creation of this
        // thread's message queue before other threads post commands to it.
        unsafe {
            let _ = PeekMessageW(&mut bootstrap_message, HWND::default(), 0, 0, PM_NOREMOVE);
        }
        if let Err(error) = register_border_window_class() {
            warn!(%error, "failed to register border overlay window class");
            return;
        }
        let _ = ready.send(thread_id);

        let mut state = BorderOverlayState::default();
        let mut message = MSG::default();

        loop {
            // SAFETY: message points to valid writable storage. This worker owns
            // the border overlay HWNDs and pumps only its thread message queue.
            let result = unsafe { GetMessageW(&mut message, HWND::default(), 0, 0) };
            match result.0 {
                -1 => {
                    let error = Win32Error::last_error("GetMessageW(border worker)");
                    warn!(%error, "border worker message loop failed");
                    break;
                }
                0 => break,
                _ if message.message == BORDER_COMMAND_MESSAGE => {
                    let mut shutdown = false;
                    while let Ok(command) = receiver.try_recv() {
                        match command {
                            BorderCommand::Sync { updates, width } => {
                                state.sync(updates, width);
                            }
                            BorderCommand::Clear => state.clear(),
                            BorderCommand::Shutdown => {
                                state.clear();
                                shutdown = true;
                            }
                        }
                    }
                    if shutdown {
                        break;
                    }
                }
                _ => {
                    // SAFETY: message was just filled by GetMessageW.
                    unsafe {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
            }
        }

        state.clear();
        debug!("border overlay worker stopped");
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

    pub fn install_hotkey_override(
        bindings: Vec<HotkeyBinding>,
        options: HotkeyOverrideOptions,
        sender: Sender<HotkeyEvent>,
    ) -> Result<HotkeyOverrideRegistration> {
        {
            let mut guard = hotkey_override_slot()
                .lock()
                .map_err(|_| Win32Error::HotkeyOverrideLockPoisoned)?;
            if guard.is_some() {
                return Err(Win32Error::HotkeyOverrideAlreadyInstalled);
            }

            *guard = Some(HotkeyOverrideState {
                sender,
                bindings,
                options,
            });
        }

        // SAFETY: The callback is a process-static low-level keyboard hook
        // procedure. Thread id 0 installs it for the current desktop without
        // injecting Winland code into other processes.
        let hook =
            unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), None, 0) };
        let hook = match hook {
            Ok(hook) => hook,
            Err(source) => {
                clear_hotkey_override();
                return Err(Win32Error::Windows {
                    context: "SetWindowsHookExW(WH_KEYBOARD_LL)",
                    source,
                });
            }
        };
        if hook.0.is_null() {
            clear_hotkey_override();
            return Err(Win32Error::last_error("SetWindowsHookExW(WH_KEYBOARD_LL)"));
        }

        Ok(HotkeyOverrideRegistration { hook })
    }

    pub fn install_modifier_drag(
        options: ModifierDragOptions,
        sender: Sender<MouseDragEvent>,
    ) -> Result<ModifierDragRegistration> {
        {
            let mut guard = mouse_drag_slot()
                .lock()
                .map_err(|_| Win32Error::ModifierDragLockPoisoned)?;
            if guard.is_some() {
                return Err(Win32Error::ModifierDragAlreadyInstalled);
            }

            *guard = Some(MouseDragState {
                sender,
                options,
                active_drag: None,
            });
        }

        // SAFETY: The callback is a process-static low-level mouse hook
        // procedure. Thread id 0 installs it for the current desktop without
        // injecting Winland code into other processes.
        let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(low_level_mouse_proc), None, 0) };
        let hook = match hook {
            Ok(hook) => hook,
            Err(source) => {
                clear_mouse_drag();
                return Err(Win32Error::Windows {
                    context: "SetWindowsHookExW(WH_MOUSE_LL)",
                    source,
                });
            }
        };
        if hook.0.is_null() {
            clear_mouse_drag();
            return Err(Win32Error::last_error("SetWindowsHookExW(WH_MOUSE_LL)"));
        }

        Ok(ModifierDragRegistration { hook })
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

    pub fn spawn_ipc_server(
        pipe_name: &'static str,
        sender: Sender<IpcTransportRequest>,
    ) -> Result<IpcServer> {
        let handle = thread::Builder::new()
            .name("winland-ipc-named-pipe".to_owned())
            .spawn(move || ipc_server_loop(pipe_name, sender))
            .map_err(|source| Win32Error::ThreadSpawn {
                name: "winland-ipc-named-pipe",
                message: source.to_string(),
            })?;

        Ok(IpcServer { _handle: handle })
    }

    pub fn send_ipc_request(pipe_name: &str, request: &[u8]) -> Result<Vec<u8>> {
        let pipe = open_ipc_client(pipe_name)?;
        write_pipe_message(pipe.0, request, "WriteFile(IPC request)")?;
        read_pipe_message(pipe.0, "ReadFile(IPC response)")
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

    pub struct HotkeyOverrideRegistration {
        hook: HHOOK,
    }

    pub struct ModifierDragRegistration {
        hook: HHOOK,
    }

    pub struct IpcServer {
        _handle: JoinHandle<()>,
    }

    impl Drop for HotkeyOverrideRegistration {
        fn drop(&mut self) {
            // SAFETY: This hook handle was returned by SetWindowsHookExW for
            // this registration and is unhooked at most once here.
            let ok = unsafe { UnhookWindowsHookEx(self.hook) };
            if let Err(error) = ok {
                warn!(?error, "failed to remove low-level keyboard hook");
            }

            clear_hotkey_override();
        }
    }

    impl Drop for ModifierDragRegistration {
        fn drop(&mut self) {
            // SAFETY: This hook handle was returned by SetWindowsHookExW for
            // this registration and is unhooked at most once here.
            let ok = unsafe { UnhookWindowsHookEx(self.hook) };
            if let Err(error) = ok {
                warn!(?error, "failed to remove low-level mouse hook");
            }

            clear_mouse_drag();
        }
    }

    struct HotkeySenderState {
        sender: Sender<HotkeyEvent>,
        message_thread_id: u32,
    }

    struct HotkeyOverrideState {
        sender: Sender<HotkeyEvent>,
        bindings: Vec<HotkeyBinding>,
        options: HotkeyOverrideOptions,
    }

    struct MouseDragState {
        sender: Sender<MouseDragEvent>,
        options: ModifierDragOptions,
        active_drag: Option<ActiveMouseDrag>,
    }

    struct ActiveMouseDrag {
        window: WindowHandle,
        last_cursor: Point,
        last_sent_move_at: Instant,
        sent_move: bool,
    }

    struct ThreadInputAttachment {
        from: u32,
        to: u32,
        attached: bool,
    }

    impl ThreadInputAttachment {
        fn attach(from: u32, to: u32) -> Self {
            if from == 0 || to == 0 || from == to {
                return Self {
                    from,
                    to,
                    attached: false,
                };
            }

            // SAFETY: The thread ids come from documented Win32 thread-query
            // APIs. The attachment is process-local state and is detached by
            // this RAII guard.
            let attached = unsafe { AttachThreadInput(from, to, TRUE).as_bool() };
            if !attached {
                debug!(
                    from_thread = from,
                    to_thread = to,
                    error = ?windows::core::Error::from_win32(),
                    "failed to attach thread input for window activation"
                );
            }

            Self { from, to, attached }
        }
    }

    impl Drop for ThreadInputAttachment {
        fn drop(&mut self) {
            if !self.attached {
                return;
            }

            // SAFETY: This reverses the successful AttachThreadInput call made
            // by ThreadInputAttachment::attach for the same thread pair.
            unsafe {
                let _ = AttachThreadInput(self.from, self.to, BOOL(0));
            }
        }
    }

    struct OwnedPipe(HANDLE);

    impl Drop for OwnedPipe {
        fn drop(&mut self) {
            // SAFETY: OwnedPipe only wraps pipe handles returned by Win32 create/open
            // calls in this module, and Drop runs at most once for the owned value.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    fn ipc_server_loop(pipe_name: &'static str, sender: Sender<IpcTransportRequest>) {
        loop {
            let pipe = match create_ipc_server_pipe(pipe_name) {
                Ok(pipe) => pipe,
                Err(error) => {
                    warn!(%error, "failed to create IPC named pipe");
                    thread::sleep(Duration::from_millis(250));
                    continue;
                }
            };

            if let Err(error) = connect_ipc_server_pipe(pipe.0) {
                warn!(%error, "failed to accept IPC named pipe client");
                continue;
            }

            let request = match read_pipe_message(pipe.0, "ReadFile(IPC request)") {
                Ok(request) => request,
                Err(error) => {
                    warn!(%error, "failed to read IPC request");
                    let _ = disconnect_pipe(pipe.0);
                    continue;
                }
            };

            let (response_sender, response_receiver) = mpsc::channel();
            if sender
                .send(IpcTransportRequest {
                    request,
                    response: response_sender,
                })
                .is_err()
            {
                let _ = disconnect_pipe(pipe.0);
                break;
            }

            match response_receiver.recv_timeout(Duration::from_secs(5)) {
                Ok(response) => {
                    if let Err(error) =
                        write_pipe_message(pipe.0, &response, "WriteFile(IPC response)")
                    {
                        warn!(%error, "failed to write IPC response");
                    }
                }
                Err(error) => warn!(%error, "timed out waiting for daemon IPC response"),
            }

            let _ = disconnect_pipe(pipe.0);
        }
    }

    fn create_ipc_server_pipe(pipe_name: &str) -> Result<OwnedPipe> {
        let name = wide_null(pipe_name);
        // SAFETY: name is a null-terminated UTF-16 string that lives for the
        // duration of the call. The pipe is local, message-oriented, blocking,
        // and closed by OwnedPipe.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                IPC_BUFFER_SIZE,
                IPC_BUFFER_SIZE,
                0,
                None,
            )
        };

        if handle.is_invalid() {
            Err(Win32Error::last_error("CreateNamedPipeW"))
        } else {
            Ok(OwnedPipe(handle))
        }
    }

    fn connect_ipc_server_pipe(handle: HANDLE) -> Result<()> {
        // SAFETY: handle is a server pipe handle returned by CreateNamedPipeW.
        let result = unsafe { ConnectNamedPipe(handle, None) };
        match result {
            Ok(()) => Ok(()),
            Err(source) if win32_error_code(&source) == ERROR_PIPE_CONNECTED.0 => Ok(()),
            Err(source) => Err(Win32Error::Windows {
                context: "ConnectNamedPipe",
                source,
            }),
        }
    }

    fn open_ipc_client(pipe_name: &str) -> Result<OwnedPipe> {
        let name = wide_null(pipe_name);
        // SAFETY: name is a null-terminated UTF-16 string that lives for the
        // duration of the call. The returned client handle is closed by OwnedPipe.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                HANDLE::default(),
            )
        };

        match handle {
            Ok(handle) => Ok(OwnedPipe(handle)),
            Err(source)
                if matches!(
                    win32_error_code(&source),
                    code if code == ERROR_FILE_NOT_FOUND.0 || code == ERROR_PIPE_BUSY.0
                ) =>
            {
                Err(Win32Error::DaemonNotRunning {
                    pipe_name: pipe_name.to_owned(),
                })
            }
            Err(source) => Err(Win32Error::Windows {
                context: "CreateFileW(IPC named pipe)",
                source,
            }),
        }
    }

    fn read_pipe_message(handle: HANDLE, context: &'static str) -> Result<Vec<u8>> {
        let mut buffer = vec![0; IPC_BUFFER_SIZE as usize];
        let mut read = 0u32;
        // SAFETY: handle is an open pipe handle, buffer points to writable
        // storage, and read points to valid storage for the byte count.
        unsafe {
            ReadFile(handle, Some(&mut buffer), Some(&mut read), None)
                .map_err(|source| Win32Error::Windows { context, source })?;
        }
        buffer.truncate(read as usize);
        Ok(buffer)
    }

    fn write_pipe_message(handle: HANDLE, message: &[u8], context: &'static str) -> Result<()> {
        let mut written = 0u32;
        // SAFETY: handle is an open pipe handle, message points to readable
        // storage for the duration of the call, and written receives the count.
        unsafe {
            WriteFile(handle, Some(message), Some(&mut written), None)
                .map_err(|source| Win32Error::Windows { context, source })?;
        }

        if written as usize == message.len() {
            Ok(())
        } else {
            Err(Win32Error::ShortIpcWrite {
                expected: message.len(),
                actual: written as usize,
            })
        }
    }

    fn disconnect_pipe(handle: HANDLE) -> Result<()> {
        // SAFETY: handle is a connected server pipe handle. DisconnectNamedPipe
        // ends only this client connection and leaves other process state alone.
        unsafe { DisconnectNamedPipe(handle).map_err(Win32Error::from) }
    }

    fn wide_null(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn win32_error_code(error: &windows::core::Error) -> u32 {
        error.code().0 as u32 & 0xFFFF
    }

    struct EnumState<'a> {
        windows: &'a mut Vec<WindowInfo>,
    }

    unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: lparam is the EnumState pointer supplied to EnumWindows above,
        // and the callback is invoked synchronously during that call.
        let state = unsafe { &mut *(lparam.0 as *mut EnumState<'_>) };

        if is_border_overlay_window(hwnd) {
            return TRUE;
        }

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

    unsafe extern "system" fn low_level_keyboard_proc(
        code: i32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if code != HC_ACTION as i32 || !is_key_down_message(wparam.0 as u32) {
            // SAFETY: Forwarding unhandled hook events preserves the documented
            // low-level hook chain contract.
            return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
        }

        if INPUT_HOOKS_PAUSED.load(Ordering::Relaxed) {
            // SAFETY: See the forwarding note above.
            return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
        }

        // SAFETY: For HC_ACTION keyboard events, lparam points to a
        // KBDLLHOOKSTRUCT owned by Windows for the duration of this callback.
        let keyboard = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let event = HotkeyLowLevelEvent {
            modifiers: current_modifier_state(),
            virtual_key: VirtualKey(keyboard.vkCode),
        };

        let Some(slot) = HOTKEY_OVERRIDE.get() else {
            // SAFETY: See the forwarding note above.
            return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
        };

        let Ok(guard) = slot.lock() else {
            warn!("hotkey override lock is poisoned");
            // SAFETY: See the forwarding note above.
            return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
        };
        let Some(state) = guard.as_ref() else {
            // SAFETY: See the forwarding note above.
            return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
        };

        let focused = foreground_hotkey_context(&state.options);
        let started = Instant::now();
        let decision =
            classify_intercepted_hotkey(&event, &state.bindings, &state.options, focused.as_ref());
        let elapsed = started.elapsed();
        if elapsed > state.options.latency_budget {
            warn!(
                elapsed_micros = elapsed.as_micros(),
                budget_micros = state.options.latency_budget.as_micros(),
                "low-level hotkey decision exceeded latency budget"
            );
        }

        match decision {
            HotkeyInterceptionDecision::Dispatch { id, suppress } => {
                let _ = state.sender.send(HotkeyEvent { id });
                if suppress {
                    return LRESULT(1);
                }
            }
            HotkeyInterceptionDecision::PassThrough { reason } => {
                debug!(reason, "hotkey override passed key through");
            }
        }

        // SAFETY: See the forwarding note above.
        unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
    }

    unsafe extern "system" fn low_level_mouse_proc(
        code: i32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if code != HC_ACTION as i32 {
            // SAFETY: Forwarding unhandled hook events preserves the documented
            // low-level hook chain contract.
            return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
        }

        let message = wparam.0 as u32;
        if !matches!(message, WM_LBUTTONDOWN | WM_MOUSEMOVE | WM_LBUTTONUP) {
            // SAFETY: See the forwarding note above.
            return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
        }

        if INPUT_HOOKS_PAUSED.load(Ordering::Relaxed) {
            // SAFETY: See the forwarding note above.
            return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
        }

        // SAFETY: For HC_ACTION mouse events, lparam points to an
        // MSLLHOOKSTRUCT owned by Windows for the duration of this callback.
        let mouse = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
        let cursor = Point {
            x: mouse.pt.x,
            y: mouse.pt.y,
        };

        let Some(slot) = MOUSE_DRAG.get() else {
            // SAFETY: See the forwarding note above.
            return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
        };
        let Ok(mut guard) = slot.lock() else {
            warn!("modifier drag lock is poisoned");
            // SAFETY: See the forwarding note above.
            return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
        };
        let Some(state) = guard.as_mut() else {
            // SAFETY: See the forwarding note above.
            return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
        };

        match message {
            WM_LBUTTONDOWN => {
                if !modifiers_match(current_modifier_state(), state.options.modifiers) {
                    // SAFETY: See the forwarding note above.
                    return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
                }

                let Some(window) = modifier_drag_window_at(cursor, &state.options) else {
                    // SAFETY: See the forwarding note above.
                    return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
                };

                let now = Instant::now();
                state.active_drag = Some(ActiveMouseDrag {
                    window,
                    last_cursor: cursor,
                    last_sent_move_at: now,
                    sent_move: false,
                });
                let _ = state.sender.send(MouseDragEvent {
                    kind: MouseDragEventKind::Started,
                    window,
                    cursor,
                });
                LRESULT(1)
            }
            WM_MOUSEMOVE => {
                if let Some(drag) = state.active_drag.as_mut() {
                    drag.last_cursor = cursor;
                    let now = Instant::now();
                    if !drag.sent_move
                        || now.duration_since(drag.last_sent_move_at) >= MODIFIER_DRAG_MOVE_INTERVAL
                    {
                        drag.last_sent_move_at = now;
                        drag.sent_move = true;
                        let _ = state.sender.send(MouseDragEvent {
                            kind: MouseDragEventKind::Moved,
                            window: drag.window,
                            cursor,
                        });
                    }
                    LRESULT(1)
                } else {
                    // SAFETY: See the forwarding note above.
                    unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
                }
            }
            WM_LBUTTONUP => {
                if let Some(drag) = state.active_drag.take() {
                    let _ = state.sender.send(MouseDragEvent {
                        kind: MouseDragEventKind::Ended,
                        window: drag.window,
                        cursor: drag.last_cursor,
                    });
                    LRESULT(1)
                } else {
                    // SAFETY: See the forwarding note above.
                    unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
                }
            }
            _ => {
                // SAFETY: See the forwarding note above.
                unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
            }
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

    fn is_key_down_message(message: u32) -> bool {
        message == WM_KEYDOWN || message == WM_SYSKEYDOWN
    }

    fn current_modifier_state() -> HotkeyModifierSet {
        let mut modifiers = HotkeyModifierSet::new();
        modifiers.no_repeat = false;

        if key_is_down(0x12) {
            modifiers.alt = true;
        }
        if key_is_down(0x11) {
            modifiers.control = true;
        }
        if key_is_down(0x10) {
            modifiers.shift = true;
        }
        if key_is_down(0x5B) || key_is_down(0x5C) {
            modifiers.super_key = true;
        }

        modifiers
    }

    fn key_is_down(virtual_key: i32) -> bool {
        // SAFETY: GetAsyncKeyState reads process-global keyboard state for the
        // supplied documented virtual-key code and does not retain pointers.
        unsafe { GetAsyncKeyState(virtual_key) < 0 }
    }

    fn foreground_hotkey_context(options: &HotkeyOverrideOptions) -> Option<HotkeyWindowContext> {
        // SAFETY: GetForegroundWindow reads the current foreground HWND and does
        // not require ownership of that window handle.
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.0.is_null() {
            return None;
        }

        hotkey_context_for_window(hwnd, options.bypass.needs_process_metadata())
    }

    fn hotkey_context_for_window(
        hwnd: HWND,
        include_process_metadata: bool,
    ) -> Option<HotkeyWindowContext> {
        let rect = visible_window_rect(hwnd).or_else(|| window_rect(hwnd).ok());
        let styles = window_styles(hwnd);
        let executable_path = if include_process_metadata {
            window_executable_path(hwnd)
        } else {
            None
        };

        Some(HotkeyWindowContext {
            class_name: class_name(hwnd).unwrap_or_default(),
            executable_path,
            is_fullscreen: rect.is_some_and(|rect| {
                rect_matches_monitor(rect) && window_style_is_borderless(styles)
            }),
        })
    }

    fn window_executable_path(hwnd: HWND) -> Option<String> {
        let mut process_id = 0;
        // SAFETY: hwnd is a live window handle. process_id points to
        // valid writable storage for the duration of the call.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut process_id));
        }

        executable_path(process_id)
    }

    fn modifier_drag_window_at(
        cursor: Point,
        options: &ModifierDragOptions,
    ) -> Option<WindowHandle> {
        let point = POINT {
            x: cursor.x,
            y: cursor.y,
        };
        // SAFETY: WindowFromPoint only reads the window at the supplied screen
        // coordinate and does not retain pointers.
        let mut hwnd = unsafe { WindowFromPoint(point) };
        if hwnd.0.is_null() {
            return None;
        }

        // SAFETY: hwnd came from WindowFromPoint. GA_ROOT asks Windows for the
        // owning top-level window without mutating application state.
        hwnd = unsafe { GetAncestor(hwnd, GA_ROOT) };
        if hwnd.0.is_null() || !is_modifier_drag_candidate(hwnd) {
            return None;
        }

        let context = hotkey_context_for_window(hwnd, options.bypass.needs_process_metadata())?;
        if options.bypass.matches(&context) {
            return None;
        }

        Some(handle_from_hwnd(hwnd))
    }

    fn is_modifier_drag_candidate(hwnd: HWND) -> bool {
        // SAFETY: hwnd is queried for visibility only.
        if !unsafe { IsWindowVisible(hwnd).as_bool() } {
            return false;
        }
        if dwm_cloaked(hwnd) {
            return false;
        }
        if layout_window_rect(hwnd).is_ok_and(Rect::is_empty) {
            return false;
        }

        match class_name(hwnd) {
            Ok(class_name) => !matches!(
                class_name.as_str(),
                "Progman" | "WorkerW" | "Shell_TrayWnd" | "NotifyIconOverflowWindow"
            ),
            Err(_) => true,
        }
    }

    fn rect_matches_monitor(rect: Rect) -> bool {
        enumerate_monitors()
            .map(|monitors| rect_covers_any_monitor(rect, &monitors))
            .unwrap_or(false)
    }

    fn window_style_is_borderless(styles: WindowStyles) -> bool {
        styles.style & (WS_CAPTION.0 | WS_THICKFRAME.0) == 0
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

    #[derive(Debug)]
    enum BorderCommand {
        Sync {
            updates: Vec<crate::BorderUpdate>,
            width: i32,
        },
        Clear,
        Shutdown,
    }

    #[derive(Default)]
    struct BorderOverlayState {
        overlays: BTreeMap<WindowHandle, BorderOverlay>,
    }

    impl BorderOverlayState {
        fn sync(&mut self, updates: Vec<crate::BorderUpdate>, width: i32) {
            let width = width.max(1);
            let retained: std::collections::BTreeSet<_> =
                updates.iter().map(|update| update.window).collect();
            let stale: Vec<_> = self
                .overlays
                .keys()
                .copied()
                .filter(|window| !retained.contains(window))
                .collect();

            for window in stale {
                self.remove(window);
            }

            for update in updates {
                if update.rect.is_empty() {
                    self.remove(update.window);
                    continue;
                }

                if let std::collections::btree_map::Entry::Vacant(entry) =
                    self.overlays.entry(update.window)
                {
                    match BorderOverlay::create(update.window) {
                        Ok(overlay) => {
                            entry.insert(overlay);
                            debug!(window = %update.window, "created border overlay");
                        }
                        Err(error) => {
                            warn!(window = %update.window, %error, "failed to create border overlay");
                            continue;
                        }
                    }
                }

                if let Some(overlay) = self.overlays.get_mut(&update.window) {
                    overlay.update(update.rect, width, update.color);
                }
            }
        }

        fn clear(&mut self) {
            let windows: Vec<_> = self.overlays.keys().copied().collect();
            for window in windows {
                self.remove(window);
            }
        }

        fn remove(&mut self, window: WindowHandle) {
            if let Some(overlay) = self.overlays.remove(&window) {
                overlay.destroy();
                debug!(window = %window, "removed border overlay");
            }
        }
    }

    struct BorderOverlay {
        window: WindowHandle,
        sides: [HWND; 4],
        rect: Option<Rect>,
        width: i32,
        color: crate::BorderColor,
    }

    impl BorderOverlay {
        fn create(window: WindowHandle) -> Result<Self> {
            let sides = [
                create_border_side_window()?,
                create_border_side_window()?,
                create_border_side_window()?,
                create_border_side_window()?,
            ];

            Ok(Self {
                window,
                sides,
                rect: None,
                width: 0,
                color: crate::BorderColor::default(),
            })
        }

        fn update(&mut self, rect: Rect, width: i32, color: crate::BorderColor) {
            let color_changed = self.color != color;
            if color_changed {
                self.color = color;
                for side in self.sides {
                    set_border_side_color(side, color);
                }
                debug!(window = %self.window, ?color, "updated border focus state");
            }

            if self.rect != Some(rect) || self.width != width {
                let side_rects = border_side_rects(rect, width);
                for (hwnd, side_rect) in self.sides.into_iter().zip(side_rects) {
                    position_border_side(hwnd, hwnd_from_handle(self.window), side_rect);
                }
                debug!(window = %self.window, rect = %rect, width, "repositioned border overlay");
                self.rect = Some(rect);
                self.width = width;
            }
        }

        fn destroy(self) {
            for side in self.sides {
                // SAFETY: side is an HWND created by create_border_side_window
                // and owned by this BorderOverlay.
                unsafe {
                    let _ = DestroyWindow(side);
                }
            }
        }
    }

    fn register_border_window_class() -> Result<()> {
        let class_name = wide_null(BORDER_CLASS_NAME);
        // SAFETY: GetModuleHandleW(None) returns the current module handle.
        let instance = unsafe { GetModuleHandleW(None) }.map_err(|source| Win32Error::Windows {
            context: "GetModuleHandleW(border class)",
            source,
        })?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(border_window_proc),
            hInstance: instance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..WNDCLASSW::default()
        };

        // SAFETY: class points to a valid WNDCLASSW for the duration of the
        // call. The class name is process-local and static for the worker.
        let atom = unsafe { RegisterClassW(&class) };
        if atom == 0 {
            return Err(Win32Error::last_error("RegisterClassW(border class)"));
        }

        Ok(())
    }

    fn create_border_side_window() -> Result<HWND> {
        let class_name = wide_null(BORDER_CLASS_NAME);
        let title = wide_null("");
        // SAFETY: GetModuleHandleW(None) returns the current module handle.
        let instance = unsafe { GetModuleHandleW(None) }.map_err(|source| Win32Error::Windows {
            context: "GetModuleHandleW(border window)",
            source,
        })?;

        // SAFETY: The registered class has a static window procedure. The
        // overlay is a no-activate, tool, transparent popup with no parent.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                0,
                0,
                0,
                0,
                HWND::default(),
                None,
                instance,
                None,
            )
        }
        .map_err(|source| Win32Error::Windows {
            context: "CreateWindowExW(border side)",
            source,
        })?;

        set_border_side_color(hwnd, crate::BorderColor::default());
        Ok(hwnd)
    }

    fn set_border_side_color(hwnd: HWND, color: crate::BorderColor) {
        // SAFETY: hwnd is a border side HWND owned by the overlay worker. The
        // userdata stores a packed COLORREF used by the paint handler.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, color.colorref() as isize);
            let _ = InvalidateRect(hwnd, None, true);
        }
    }

    fn position_border_side(hwnd: HWND, target: HWND, rect: Rect) {
        // SAFETY: hwnd is a border side HWND owned by the overlay worker.
        // Inserting just behind the target keeps borders from drawing above a
        // window the user drags across them while still avoiding activation.
        if let Err(error) = unsafe {
            SetWindowPos(
                hwnd,
                target,
                rect.left,
                rect.top,
                rect.width(),
                rect.height(),
                SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
            )
        } {
            warn!(%error, rect = %rect, "failed to position border side");
        }
    }

    fn border_side_rects(rect: Rect, width: i32) -> [Rect; 4] {
        let width = width.max(1);
        [
            Rect {
                left: rect.left.saturating_sub(width),
                top: rect.top.saturating_sub(width),
                right: rect.right.saturating_add(width),
                bottom: rect.top,
            },
            Rect {
                left: rect.left.saturating_sub(width),
                top: rect.bottom,
                right: rect.right.saturating_add(width),
                bottom: rect.bottom.saturating_add(width),
            },
            Rect {
                left: rect.left.saturating_sub(width),
                top: rect.top,
                right: rect.left,
                bottom: rect.bottom,
            },
            Rect {
                left: rect.right,
                top: rect.top,
                right: rect.right.saturating_add(width),
                bottom: rect.bottom,
            },
        ]
    }

    unsafe extern "system" fn border_window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                // SAFETY: paint points to valid storage for the WM_PAINT cycle.
                let hdc = unsafe { BeginPaint(hwnd, &mut paint) };
                // SAFETY: The userdata is written by set_border_side_color.
                let color = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as u32;
                // SAFETY: CreateSolidBrush creates a GDI brush for this paint.
                let brush = unsafe { CreateSolidBrush(COLORREF(color)) };
                // SAFETY: hdc and paint.rcPaint are valid for the paint cycle.
                unsafe {
                    FillRect(hdc, &paint.rcPaint, brush);
                    let _ = DeleteObject(brush);
                    let _ = EndPaint(hwnd, &paint);
                }
                LRESULT(0)
            }
            _ => {
                // SAFETY: Delegating unhandled messages to DefWindowProcW
                // preserves the documented window procedure contract.
                unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
            }
        }
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
            (EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_NAMECHANGE),
            (EVENT_OBJECT_STATECHANGE, EVENT_OBJECT_STATECHANGE),
            (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
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
        if is_border_overlay_window(hwnd) {
            return None;
        }

        let kind = match event {
            EVENT_OBJECT_CREATE => WindowEventKind::Created,
            EVENT_OBJECT_DESTROY => WindowEventKind::Destroyed,
            EVENT_OBJECT_SHOW => WindowEventKind::Shown,
            EVENT_OBJECT_HIDE => WindowEventKind::Hidden,
            EVENT_OBJECT_LOCATIONCHANGE => WindowEventKind::Moved,
            EVENT_OBJECT_NAMECHANGE
            | EVENT_OBJECT_STATECHANGE
            | EVENT_OBJECT_CLOAKED
            | EVENT_OBJECT_UNCLOAKED => WindowEventKind::MetadataChanged,
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
        let styles = window_styles(hwnd);
        let rect = layout_window_rect(hwnd)?;

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
            styles,
            size_constraints: window_size_constraints(hwnd),
            rect,
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

    fn is_border_overlay_window(hwnd: HWND) -> bool {
        class_name(hwnd).is_ok_and(|class_name| class_name == BORDER_CLASS_NAME)
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

    fn window_size_constraints(hwnd: HWND) -> WindowSizeConstraints {
        let Some(minmax) = window_minmax_info(hwnd) else {
            return WindowSizeConstraints::NONE;
        };

        let min = Size::new(minmax.ptMinTrackSize.x, minmax.ptMinTrackSize.y);
        if !plausible_min_track_size(min) {
            return WindowSizeConstraints::NONE;
        }

        WindowSizeConstraints::minimum(min.width, min.height)
    }

    pub(super) fn plausible_min_track_size(size: Size) -> bool {
        (size.width > 0 || size.height > 0) && size.width < 10_000 && size.height < 10_000
    }

    fn window_minmax_info(hwnd: HWND) -> Option<MINMAXINFO> {
        let mut info = MINMAXINFO::default();
        let mut result = 0usize;

        // SAFETY: info points to writable MINMAXINFO storage for the duration of
        // this synchronous message. SendMessageTimeoutW avoids blocking the
        // daemon indefinitely if the target window is hung.
        let timeout_result = unsafe {
            SendMessageTimeoutW(
                hwnd,
                WM_GETMINMAXINFO,
                WPARAM(0),
                LPARAM(&mut info as *mut MINMAXINFO as isize),
                SMTO_ABORTIFHUNG,
                MINMAXINFO_TIMEOUT_MS,
                Some(&mut result),
            )
        };

        if timeout_result.0 == 0 {
            debug!(?hwnd, "WM_GETMINMAXINFO timed out or failed");
            None
        } else {
            Some(info)
        }
    }

    fn layout_window_rect(hwnd: HWND) -> Result<Rect> {
        visible_window_rect(hwnd).map_or_else(|| window_rect(hwnd), Ok)
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

    fn visible_window_rect(hwnd: HWND) -> Option<Rect> {
        let mut rect = RECT::default();
        // SAFETY: rect points to valid writable storage sized as passed, and
        // hwnd is only queried for a documented DWM frame-bounds attribute.
        let result = unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut rect as *mut RECT as *mut _,
                size_of::<RECT>() as u32,
            )
        };

        result.ok().map(|()| rect_from_win32(rect))
    }

    fn outer_rect_for_desired_visible_rect(hwnd: HWND, desired_visible: Rect) -> Option<Rect> {
        let outer = window_rect(hwnd).ok()?;
        let visible = visible_window_rect(hwnd)?;
        let left_margin = visible.left.saturating_sub(outer.left);
        let top_margin = visible.top.saturating_sub(outer.top);
        let right_margin = outer.right.saturating_sub(visible.right);
        let bottom_margin = outer.bottom.saturating_sub(visible.bottom);

        Some(Rect {
            left: desired_visible.left.saturating_sub(left_margin),
            top: desired_visible.top.saturating_sub(top_margin),
            right: desired_visible.right.saturating_add(right_margin),
            bottom: desired_visible.bottom.saturating_add(bottom_margin),
        })
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

    fn hotkey_override_slot() -> &'static Mutex<Option<HotkeyOverrideState>> {
        HOTKEY_OVERRIDE.get_or_init(|| Mutex::new(None))
    }

    fn mouse_drag_slot() -> &'static Mutex<Option<MouseDragState>> {
        MOUSE_DRAG.get_or_init(|| Mutex::new(None))
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

    fn clear_hotkey_override() {
        match hotkey_override_slot().lock() {
            Ok(mut guard) => *guard = None,
            Err(_) => warn!("hotkey override lock is poisoned while clearing"),
        }
    }

    fn clear_mouse_drag() {
        match mouse_drag_slot().lock() {
            Ok(mut guard) => *guard = None,
            Err(_) => warn!("modifier drag lock is poisoned while clearing"),
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

mod shell;

#[cfg(not(windows))]
mod platform {
    use std::sync::mpsc::Sender;

    use crate::{
        HotkeyBinding, HotkeyEvent, IpcTransportRequest, ModifierDragOptions, MouseDragEvent,
        Result, Win32Error,
    };
    use winland_core::{MonitorInfo, Point, Rect, WindowHandle, WindowInfo};

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

    pub fn window_rect_for_handle(_handle: WindowHandle) -> Result<Rect> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn foreground_window() -> Result<Option<WindowHandle>> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn cursor_position() -> Result<Point> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn focus_window(_handle: WindowHandle) -> Result<()> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn set_input_hooks_paused(_paused: bool) {}

    pub fn hide_window(_handle: WindowHandle) -> Result<()> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn show_window_without_activate(_handle: WindowHandle) -> Result<()> {
        Err(Win32Error::UnsupportedPlatform)
    }

    #[derive(Debug)]
    pub struct BorderManager;

    impl BorderManager {
        pub fn new() -> Result<Self> {
            Err(Win32Error::UnsupportedPlatform)
        }

        pub fn sync(&self, _updates: Vec<crate::BorderUpdate>, _width: i32) -> Result<()> {
            Err(Win32Error::UnsupportedPlatform)
        }

        pub fn clear(&self) -> Result<()> {
            Err(Win32Error::UnsupportedPlatform)
        }
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

    pub fn install_hotkey_override(
        _bindings: Vec<HotkeyBinding>,
        _options: crate::HotkeyOverrideOptions,
        _sender: Sender<HotkeyEvent>,
    ) -> Result<HotkeyOverrideRegistration> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn install_modifier_drag(
        _options: ModifierDragOptions,
        _sender: Sender<MouseDragEvent>,
    ) -> Result<ModifierDragRegistration> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn request_message_loop_stop() -> Result<()> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn run_message_loop() -> Result<()> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn spawn_ipc_server(
        _pipe_name: &'static str,
        _sender: Sender<IpcTransportRequest>,
    ) -> Result<IpcServer> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn send_ipc_request(_pipe_name: &str, _request: &[u8]) -> Result<Vec<u8>> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub struct WindowEventSubscription;
    pub struct HotkeyRegistration;
    pub struct HotkeyOverrideRegistration;
    pub struct ModifierDragRegistration;
    pub struct IpcServer;

    impl HotkeyRegistration {
        pub fn failures(&self) -> &[HotkeyRegistrationFailure] {
            &[]
        }
    }
}

pub use platform::BorderManager;
pub use platform::HotkeyOverrideRegistration;
pub use platform::HotkeyRegistration;
pub use platform::IpcServer;
pub use platform::ModifierDragRegistration;
pub use platform::WindowEventSubscription;
pub use platform::cursor_position;
pub use platform::enumerate_monitors;
pub use platform::enumerate_windows;
pub use platform::focus_window;
pub use platform::foreground_window;
pub use platform::hide_window;
pub use platform::install_hotkey_override;
pub use platform::install_modifier_drag;
pub use platform::move_resize_window;
pub use platform::register_hotkeys;
pub use platform::request_message_loop_stop;
pub use platform::run_message_loop;
pub use platform::send_ipc_request;
pub use platform::set_input_hooks_paused;
pub use platform::show_window_without_activate;
pub use platform::spawn_ipc_server;
pub use platform::subscribe_window_events;
pub use platform::window_rect_for_handle;
pub use shell::ShellReplacementChange;
pub use shell::ShellReplacementStatus;
pub use shell::USER_WINLOGON_KEY;
pub use shell::elevated_daemon_task_installed;
pub use shell::install_elevated_daemon_task;
pub use shell::install_shell_replacement;
pub use shell::launch_app;
pub use shell::launch_elevated_process_and_wait;
pub use shell::launch_explorer;
pub use shell::launch_shell_test;
pub use shell::quote_windows_arg;
pub use shell::restore_shell_replacement;
pub use shell::run_elevated_daemon_task;
pub use shell::shell_command_for_executable;
pub use shell::shell_command_with_daemon;
pub use shell::shell_replacement_status;
pub use shell::uninstall_elevated_daemon_task;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use winland_core::{MonitorInfo, Rect, TextMatcher, WindowHandle, detect_fullscreen_rect};

pub const DEFAULT_IPC_PIPE_NAME: &str = r"\\.\pipe\winland-ipc";
const IPC_BUFFER_SIZE: u32 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyBinding {
    pub id: HotkeyId,
    pub modifiers: HotkeyModifierSet,
    pub virtual_key: VirtualKey,
    pub description: String,
    pub suppress_app: bool,
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
            suppress_app: false,
        }
    }

    pub fn with_suppression(mut self, suppress_app: bool) -> Self {
        self.suppress_app = suppress_app;
        self
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

    pub const ARROW_LEFT: Self = Self(0x25);
    pub const ARROW_UP: Self = Self(0x26);
    pub const ARROW_RIGHT: Self = Self(0x27);
    pub const ARROW_DOWN: Self = Self(0x28);
    pub const ESCAPE: Self = Self(0x1B);
    pub const SPACE: Self = Self(0x20);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotkeyEvent {
    pub id: HotkeyId,
}

#[derive(Debug, Clone)]
pub struct HotkeyOverrideOptions {
    pub panic_hotkey: HotkeyLowLevelEvent,
    pub bypass: HotkeyBypassRules,
    pub latency_budget: Duration,
}

#[derive(Debug, Clone)]
pub struct ModifierDragOptions {
    pub modifiers: HotkeyModifierSet,
    pub bypass: HotkeyBypassRules,
}

#[derive(Debug, Clone, Default)]
pub struct HotkeyBypassRules {
    pub fullscreen: bool,
    pub class_names: Vec<TextMatcher>,
    pub executable_paths: Vec<TextMatcher>,
    pub process_names: Vec<TextMatcher>,
}

impl HotkeyBypassRules {
    pub fn needs_process_metadata(&self) -> bool {
        !self.executable_paths.is_empty() || !self.process_names.is_empty()
    }

    fn matches(&self, focused: &HotkeyWindowContext) -> bool {
        if self.fullscreen && focused.is_fullscreen {
            return true;
        }

        if self
            .class_names
            .iter()
            .any(|matcher| matcher.matches(&focused.class_name))
        {
            return true;
        }

        if let Some(path) = &focused.executable_path {
            if self
                .executable_paths
                .iter()
                .any(|matcher| matcher.matches(path))
            {
                return true;
            }

            if let Some(name) = process_name(path)
                && self
                    .process_names
                    .iter()
                    .any(|matcher| matcher.matches(&name))
            {
                return true;
            }
        }

        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotkeyLowLevelEvent {
    pub modifiers: HotkeyModifierSet,
    pub virtual_key: VirtualKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyWindowContext {
    pub class_name: String,
    pub executable_path: Option<String>,
    pub is_fullscreen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyInterceptionDecision {
    Dispatch { id: HotkeyId, suppress: bool },
    PassThrough { reason: &'static str },
}

pub fn classify_intercepted_hotkey(
    event: &HotkeyLowLevelEvent,
    bindings: &[HotkeyBinding],
    options: &HotkeyOverrideOptions,
    focused: Option<&HotkeyWindowContext>,
) -> HotkeyInterceptionDecision {
    if event.matches(&options.panic_hotkey) {
        return HotkeyInterceptionDecision::PassThrough {
            reason: "panic hotkey",
        };
    }

    if focused.is_some_and(|window| options.bypass.matches(window)) {
        return HotkeyInterceptionDecision::PassThrough {
            reason: "game-safe bypass",
        };
    }

    let Some(binding) = bindings
        .iter()
        .find(|binding| event.matches_binding(binding))
    else {
        return HotkeyInterceptionDecision::PassThrough {
            reason: "unbound key",
        };
    };

    HotkeyInterceptionDecision::Dispatch {
        id: binding.id,
        suppress: binding.suppress_app,
    }
}

pub fn benchmark_hotkey_decision_path(
    event: &HotkeyLowLevelEvent,
    bindings: &[HotkeyBinding],
    options: &HotkeyOverrideOptions,
    focused: Option<&HotkeyWindowContext>,
    iterations: usize,
) -> HotkeyDecisionBenchmark {
    let iterations = iterations.max(1);
    let mut max = Duration::ZERO;
    let started_all = Instant::now();

    for _ in 0..iterations {
        let started = Instant::now();
        let _ = classify_intercepted_hotkey(event, bindings, options, focused);
        max = max.max(started.elapsed());
    }

    let total = started_all.elapsed();
    HotkeyDecisionBenchmark {
        iterations,
        total,
        average: total / iterations as u32,
        max,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotkeyDecisionBenchmark {
    pub iterations: usize,
    pub total: Duration,
    pub average: Duration,
    pub max: Duration,
}

impl HotkeyLowLevelEvent {
    fn matches(&self, other: &Self) -> bool {
        self.virtual_key == other.virtual_key && modifiers_match(self.modifiers, other.modifiers)
    }

    fn matches_binding(&self, binding: &HotkeyBinding) -> bool {
        self.virtual_key == binding.virtual_key
            && modifiers_match(self.modifiers, binding.modifiers)
    }
}

pub(crate) fn modifiers_match(actual: HotkeyModifierSet, expected: HotkeyModifierSet) -> bool {
    actual.alt == expected.alt
        && actual.control == expected.control
        && actual.shift == expected.shift
        && actual.super_key == expected.super_key
}

fn process_name(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

fn rect_covers_any_monitor(rect: Rect, monitors: &[MonitorInfo]) -> bool {
    detect_fullscreen_rect(rect, monitors, 4).is_fullscreen
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowEvent {
    pub kind: WindowEventKind,
    pub window: WindowHandle,
    pub event_time: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseDragEvent {
    pub kind: MouseDragEventKind,
    pub window: WindowHandle,
    pub cursor: winland_core::Point,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseDragEventKind {
    Started,
    Moved,
    Ended,
}

#[derive(Debug)]
pub struct IpcTransportRequest {
    pub request: Vec<u8>,
    pub response: Sender<Vec<u8>>,
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
    MetadataChanged,
}

pub type Result<T> = std::result::Result<T, Win32Error>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BorderColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl BorderColor {
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    pub fn colorref(self) -> u32 {
        u32::from(self.red) | (u32::from(self.green) << 8) | (u32::from(self.blue) << 16)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorderUpdate {
    pub window: WindowHandle,
    pub rect: Rect,
    pub color: BorderColor,
}

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
    #[error("hotkey override hook is already installed")]
    HotkeyOverrideAlreadyInstalled,
    #[error("hotkey override hook state lock is poisoned")]
    HotkeyOverrideLockPoisoned,
    #[error("modifier drag hook is already installed")]
    ModifierDragAlreadyInstalled,
    #[error("modifier drag hook state lock is poisoned")]
    ModifierDragLockPoisoned,
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
    #[error("border overlay worker is not running")]
    BorderOverlayWorkerStopped,
    #[error("Winland daemon is not running on {pipe_name}")]
    DaemonNotRunning { pipe_name: String },
    #[error("{context} failed with Win32 error {code}")]
    Registry { context: &'static str, code: u32 },
    #[error("registry value {name} has type {actual}; expected {expected}")]
    UnexpectedRegistryValueType {
        name: String,
        expected: &'static str,
        actual: u32,
    },
    #[error("no Winland shell replacement backup is present")]
    MissingShellBackup,
    #[error("shell command must not be empty")]
    InvalidShellCommand,
    #[cfg(windows)]
    #[error("{context} returned an invalid process handle")]
    InvalidProcessHandle { context: &'static str },
    #[error("{operation} failed: {message}")]
    ShellOperation {
        operation: &'static str,
        message: String,
    },
    #[cfg(windows)]
    #[error("failed to spawn thread {name}: {message}")]
    ThreadSpawn { name: &'static str, message: String },
    #[cfg(windows)]
    #[error("short IPC write: wrote {actual} of {expected} bytes")]
    ShortIpcWrite { expected: usize, actual: usize },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_decision_dispatches_and_suppresses_matching_binding() {
        let binding = HotkeyBinding::new(
            HotkeyId(7),
            HotkeyModifierSet::new().control().alt(),
            VirtualKey::ascii_uppercase(b'H'),
            "focus-left",
        )
        .with_suppression(true);
        let options = test_options();
        let event = HotkeyLowLevelEvent {
            modifiers: HotkeyModifierSet::new().control().alt(),
            virtual_key: VirtualKey::ascii_uppercase(b'H'),
        };

        assert_eq!(
            classify_intercepted_hotkey(&event, &[binding], &options, None),
            HotkeyInterceptionDecision::Dispatch {
                id: HotkeyId(7),
                suppress: true,
            }
        );
    }

    #[test]
    fn override_decision_never_dispatches_panic_hotkey() {
        let binding = HotkeyBinding::new(
            HotkeyId(1),
            HotkeyModifierSet::new().control().alt().shift(),
            VirtualKey::ESCAPE,
            "quit",
        )
        .with_suppression(true);
        let options = test_options();

        assert_eq!(
            classify_intercepted_hotkey(&options.panic_hotkey, &[binding], &options, None),
            HotkeyInterceptionDecision::PassThrough {
                reason: "panic hotkey",
            }
        );
    }

    #[test]
    fn override_decision_bypasses_fullscreen_and_configured_games() {
        let binding = HotkeyBinding::new(
            HotkeyId(1),
            HotkeyModifierSet::new().control().alt(),
            VirtualKey::ascii_uppercase(b'H'),
            "focus-left",
        )
        .with_suppression(true);
        let event = HotkeyLowLevelEvent {
            modifiers: HotkeyModifierSet::new().control().alt(),
            virtual_key: VirtualKey::ascii_uppercase(b'H'),
        };
        let mut options = test_options();
        options.bypass.process_names = vec![TextMatcher::Exact("game.exe".to_owned())];

        let fullscreen = HotkeyWindowContext {
            class_name: "GameWindow".to_owned(),
            executable_path: Some(r"C:\Games\other.exe".to_owned()),
            is_fullscreen: true,
        };
        let configured_game = HotkeyWindowContext {
            class_name: "Window".to_owned(),
            executable_path: Some(r"C:\Games\game.exe".to_owned()),
            is_fullscreen: false,
        };

        assert_eq!(
            classify_intercepted_hotkey(
                &event,
                std::slice::from_ref(&binding),
                &options,
                Some(&fullscreen)
            ),
            HotkeyInterceptionDecision::PassThrough {
                reason: "game-safe bypass",
            }
        );
        assert_eq!(
            classify_intercepted_hotkey(&event, &[binding], &options, Some(&configured_game)),
            HotkeyInterceptionDecision::PassThrough {
                reason: "game-safe bypass",
            }
        );
    }

    #[test]
    fn work_area_rect_is_fullscreen_for_hotkey_bypass() {
        let monitors = [MonitorInfo {
            id: winland_core::MonitorId(1),
            is_primary: true,
            rect: Rect::from_size(0, 0, 1000, 800),
            work_area: Rect::from_size(0, 0, 1000, 760),
        }];

        assert!(rect_covers_any_monitor(monitors[0].rect, &monitors));
        assert!(rect_covers_any_monitor(monitors[0].work_area, &monitors));
    }

    #[cfg(windows)]
    #[test]
    fn minmax_constraints_only_trust_plausible_minimums() {
        assert!(platform::plausible_min_track_size(winland_core::Size::new(
            320, 240
        )));
        assert!(!platform::plausible_min_track_size(
            winland_core::Size::new(0, 0)
        ));
        assert!(!platform::plausible_min_track_size(
            winland_core::Size::new(30_000, 240)
        ));
    }

    #[test]
    fn hotkey_decision_benchmark_records_iterations() {
        let options = test_options();
        let event = HotkeyLowLevelEvent {
            modifiers: HotkeyModifierSet::new(),
            virtual_key: VirtualKey::SPACE,
        };

        let benchmark = benchmark_hotkey_decision_path(&event, &[], &options, None, 8);

        assert_eq!(benchmark.iterations, 8);
        assert!(benchmark.total >= benchmark.average);
    }

    fn test_options() -> HotkeyOverrideOptions {
        HotkeyOverrideOptions {
            panic_hotkey: HotkeyLowLevelEvent {
                modifiers: HotkeyModifierSet::new().control().alt().shift(),
                virtual_key: VirtualKey::ESCAPE,
            },
            bypass: HotkeyBypassRules {
                fullscreen: true,
                ..HotkeyBypassRules::default()
            },
            latency_budget: Duration::from_micros(250),
        }
    }
}
