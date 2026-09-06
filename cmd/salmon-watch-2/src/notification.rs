use anyhow::{Context, Result};
use notify_rust::{Notification, Urgency};

pub trait NotificationSink {
    fn push(&self, title: &str, body: &str) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DesktopNotificationSink;

impl NotificationSink for DesktopNotificationSink {
    fn push(&self, title: &str, body: &str) -> Result<()> {
        desktop_notification(title, body)
            .show()
            .context("desktop notification delivery failed")?;
        Ok(())
    }
}

fn desktop_notification(title: &str, body: &str) -> Notification {
    let mut notification = Notification::new();
    notification
        .appname("Salmon Watch")
        .summary(title)
        .body(body)
        .urgency(Urgency::Normal);
    notification
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_notification_has_application_and_message() {
        let notification = desktop_notification("Example title", "Example body");

        assert_eq!(notification.appname, "Salmon Watch");
        assert_eq!(notification.summary, "Example title");
        assert_eq!(notification.body, "Example body");
    }
}
