use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;
use winland_core::{WindowHandle, WindowInfo};
use winland_win32::{WindowEvent, WindowEventKind};

const RECONCILE_DEBOUNCE: Duration = Duration::from_millis(50);
const MAX_BATCH_SIZE: usize = 512;

fn main() -> Result<()> {
    init_tracing();

    let (sender, receiver) = mpsc::channel();
    let subscription = winland_win32::subscribe_window_events(sender)
        .context("install documented Win32 window event hooks")?;

    let state = DaemonState::discover().context("build initial window snapshot")?;
    let processor = thread::Builder::new()
        .name("winland-event-reconcile".to_owned())
        .spawn(move || process_window_events(receiver, state))
        .context("spawn window event reconciliation thread")?;

    info!("winland daemon started; entering Win32 message loop");
    let message_loop_result =
        winland_win32::run_message_loop().context("run Win32 daemon message loop");

    drop(subscription);
    match processor.join() {
        Ok(Ok(())) => message_loop_result,
        Ok(Err(error)) => Err(error).context("process window event batches"),
        Err(_) => Err(anyhow!("window event reconciliation thread panicked")),
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

fn process_window_events(receiver: Receiver<WindowEvent>, mut state: DaemonState) -> Result<()> {
    state.log_snapshot("initial window snapshot");

    while let Ok(first_event) = receiver.recv() {
        let batch = receive_event_batch(&receiver, first_event);
        state.reconcile_after_events(&batch)?;
    }

    info!("window event channel closed; reconciliation thread stopping");
    Ok(())
}

fn receive_event_batch(
    receiver: &Receiver<WindowEvent>,
    first_event: WindowEvent,
) -> Vec<WindowEvent> {
    let mut batch = vec![first_event];

    while batch.len() < MAX_BATCH_SIZE {
        match receiver.recv_timeout(RECONCILE_DEBOUNCE) {
            Ok(event) => batch.push(event),
            Err(_) => break,
        }
    }

    batch
}

#[derive(Debug)]
struct DaemonState {
    windows: BTreeMap<WindowHandle, WindowInfo>,
    foreground: Option<WindowHandle>,
}

impl DaemonState {
    fn discover() -> Result<Self> {
        let windows = winland_win32::enumerate_windows()
            .context("enumerate windows for daemon snapshot")?
            .into_iter()
            .map(|window| (window.handle, window))
            .collect();
        let foreground = winland_win32::foreground_window().context("read foreground window")?;

        Ok(Self {
            windows,
            foreground,
        })
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

        let refreshed = Self::discover().context("refresh window snapshot after event batch")?;
        let diff = self.diff(&refreshed);
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

    fn log_snapshot(&self, message: &'static str) {
        info!(
            total_windows = self.windows.len(),
            manageable_windows = self.manageable_window_count(),
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

fn count_events(batch: &[WindowEvent], kind: WindowEventKind) -> usize {
    batch.iter().filter(|event| event.kind == kind).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use winland_core::{Rect, WindowStyles};

    #[test]
    fn snapshot_diff_reports_added_removed_changed_and_foreground_changes() {
        let mut old = DaemonState {
            windows: BTreeMap::new(),
            foreground: Some(WindowHandle(1)),
        };
        old.windows.insert(WindowHandle(1), window(1, "Editor"));
        old.windows.insert(WindowHandle(2), window(2, "Terminal"));

        let mut refreshed = DaemonState {
            windows: BTreeMap::new(),
            foreground: Some(WindowHandle(3)),
        };
        refreshed
            .windows
            .insert(WindowHandle(1), window(1, "Editor - changed"));
        refreshed
            .windows
            .insert(WindowHandle(3), window(3, "Browser"));

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

    fn event(kind: WindowEventKind, handle: u64) -> WindowEvent {
        WindowEvent {
            kind,
            window: WindowHandle(handle),
            event_time: 0,
        }
    }

    fn window(handle: u64, title: &str) -> WindowInfo {
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
            rect: Rect::from_size(10, 20, 800, 600),
        }
    }
}
