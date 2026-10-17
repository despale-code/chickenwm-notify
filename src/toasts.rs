//! Windows app notifications (toasts).
//!
//! Apps hand toasts to the Windows notification platform; normally Explorer's shell pops them
//! up. With Explorer suppressed they just queue (and all appear at once when Explorer comes
//! back). This reads them with `UserNotificationListener` and forwards new ones to the UI —
//! only while Explorer isn't running, so nothing is shown twice.
//!
//! Windows asks the user once for permission ("Let apps access your notifications"); if it's
//! refused, a notification explains where to allow it.

use crate::Incoming;
use chickenwm_core::ipc::Notification;
use std::collections::HashSet;
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use windows::UI::Notifications::Management::{UserNotificationListener, UserNotificationListenerAccessStatus};
use windows::UI::Notifications::{KnownNotificationBindings, NotificationKinds, UserNotification};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

/// Windows `DateTime` (100 ns ticks since 1601) → Unix seconds.
fn unix_secs(ticks: i64) -> u64 {
    ((ticks - 116_444_736_000_000_000).max(0) / 10_000_000) as u64
}

fn read(n: &UserNotification) -> windows::core::Result<(u32, Notification, u64)> {
    let id = n.Id()?;
    let app = n
        .AppInfo()
        .and_then(|a| a.DisplayInfo())
        .and_then(|d| d.DisplayName())
        .map(|s| s.to_string())
        .unwrap_or_else(|_| "App".to_string());
    let texts: Vec<String> = n
        .Notification()?
        .Visual()?
        .GetBinding(&KnownNotificationBindings::ToastGeneric()?)?
        .GetTextElements()?
        .into_iter()
        .filter_map(|t| t.Text().ok().map(|s| s.to_string()))
        .filter(|s| !s.trim().is_empty())
        .collect();
    let title = texts.first().cloned().unwrap_or_else(|| app.clone());
    let body = texts.iter().skip(1).cloned().collect::<Vec<_>>().join("\n");
    let created = unix_secs(n.CreationTime()?.UniversalTime);
    Ok((id, Notification { app, title, body, timeout_secs: None, urgency: None }, created))
}

fn explorer_running() -> bool {
    !chickenwm_core::failsafe::find_process_ids(&["explorer.exe"]).is_empty()
}

/// Polls the notification platform once a second. Runs on its own thread for the process'
/// lifetime.
pub fn run(tx: Sender<Incoming>, enabled: impl Fn() -> bool) {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    let listener = match UserNotificationListener::Current() {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("Windows notifications unavailable: {e}");
            return;
        }
    };
    let status = listener
        .RequestAccessAsync()
        .and_then(|op| op.get())
        .unwrap_or(UserNotificationListenerAccessStatus::Unspecified);
    if status != UserNotificationListenerAccessStatus::Allowed {
        tracing::warn!("notification access not granted ({status:?})");
        let _ = tx.send(Incoming::Note {
            note: Notification {
                app: "ChickenWM".into(),
                title: "Allow notification access".into(),
                body: "To show your apps' notifications, turn on Settings → Privacy & security → Notifications → \"Let apps access your notifications\", then restart chickenwm-notify.".into(),
                timeout_secs: Some(0),
                urgency: None,
            },
            toast_id: None,
            created: None,
        });
        return;
    }
    tracing::info!("reading Windows app notifications");

    let mut seen: HashSet<u32> = HashSet::new();
    // Notifications already waiting at startup: show the ones from the last two minutes (they
    // probably arrived while nothing could display them); older ones are just history.
    let started = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut first = true;
    loop {
        if let Ok(list) = listener.GetNotificationsAsync(NotificationKinds::Toast).and_then(|op| op.get()) {
            let mut present = HashSet::new();
            let display = enabled() && !explorer_running();
            for n in list {
                let Ok((id, note, created)) = read(&n) else { continue };
                present.insert(id);
                if !seen.insert(id) {
                    continue;
                }
                let fresh = !first || started.saturating_sub(created) < 120;
                if display {
                    let _ = tx.send(if fresh {
                        Incoming::Note { note, toast_id: Some(id), created: Some(created) }
                    } else {
                        Incoming::History { note, created }
                    });
                }
            }
            seen.retain(|id| present.contains(id));
            first = false;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Removes a toast from Windows' notification list (after the user dismissed it here).
pub fn remove(id: u32) {
    if let Ok(listener) = UserNotificationListener::Current() {
        let _ = listener.RemoveNotification(id);
    }
}
