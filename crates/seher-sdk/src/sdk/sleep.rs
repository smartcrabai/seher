//! Thin wrapper around [`zzsleep::sleep_until`].

use chrono::{DateTime, Local, Utc};

/// Sleep until the given UTC instant. If the instant is in the past, returns immediately.
pub async fn sleep_until(reset: DateTime<Utc>, quiet: bool) {
    let now = Utc::now();
    if reset <= now {
        return;
    }
    let local: DateTime<Local> = reset.into();
    zzsleep::sleep_until(local, quiet).await;
}
