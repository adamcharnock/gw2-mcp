//! Real-world clock adapter.

use chrono::{DateTime, Utc};

use crate::ports::Clock;

/// Returns wall-clock time. The default implementation in production.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}
