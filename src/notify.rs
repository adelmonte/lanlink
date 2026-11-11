//! Desktop notifications (swaync, mako, dunst... anything on the freedesktop spec).
//! notify-rust's API is blocking, so everything here runs on plain threads.

use notify_rust::{Notification, NotificationHandle, Timeout};

fn base(summary: &str, body: &str) -> Notification {
    let mut n = Notification::new();
    n.appname("lanlink").summary(summary).body(body).icon("network-wireless");
    n
}

pub fn show(summary: impl Into<String>, body: impl Into<String>) {
    let (s, b) = (summary.into(), body.into());
    std::thread::spawn(move || {
        let _ = base(&s, &b).show();
    });
}

/// Show a notification with buttons. `on` gets the chosen action id, or None
/// if it was dismissed, timed out, or no notification daemon is running.
pub fn ask(
    summary: impl Into<String>,
    body: impl Into<String>,
    actions: &[(&str, &str)],
    timeout_ms: u32,
    on: impl FnOnce(Option<String>) + Send + 'static,
) {
    let (s, b) = (summary.into(), body.into());
    let actions: Vec<(String, String)> =
        actions.iter().map(|(a, l)| (a.to_string(), l.to_string())).collect();
    std::thread::spawn(move || {
        let mut n = base(&s, &b);
        for (a, l) in &actions {
            n.action(a, l);
        }
        n.timeout(Timeout::Milliseconds(timeout_ms));
        match n.show() {
            Ok(h) => {
                let mut chosen = None;
                h.wait_for_action(|a| {
                    if a != "__closed" {
                        chosen = Some(a.to_string());
                    }
                });
                on(chosen)
            }
            Err(_) => on(None),
        }
    });
}

/// A notification that stays until closed. Blocking; call from a thread.
pub fn persistent(summary: &str, body: &str) -> Option<NotificationHandle> {
    let mut n = base(summary, body);
    n.timeout(Timeout::Never);
    n.show().ok()
}
