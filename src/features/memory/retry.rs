//! Waiting before trying a failed reflection or diary again.
//!
//! A failed run isn't saved as done, so it's tried again later instead of skipping a day
//! (reflection) or a week (diary). To not ask a broken provider every hour, a failure
//! waits [`RETRY_SECS`] first. Kept in memory only: after a restart it just tries sooner.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// How long a failed run waits before the next try.
pub const RETRY_SECS: i64 = 3 * 60 * 60;

/// When each failed job (a scope or a server) may be tried again.
pub struct RetryLater(Mutex<BTreeMap<String, i64>>);

impl RetryLater {
    pub const fn new() -> Self {
        Self(Mutex::new(BTreeMap::new()))
    }

    /// Whether `key` failed recently and should wait.
    pub fn waiting(&self, key: &str, now: i64) -> bool {
        let retries = self.0.lock().unwrap();
        retries.get(key).is_some_and(|&at| now < at)
    }

    /// `key` failed at `now`: try again in [`RETRY_SECS`].
    pub fn failed(&self, key: &str, now: i64) {
        let mut retries = self.0.lock().unwrap();
        retries.insert(key.to_string(), now + RETRY_SECS);
    }

    /// `key` worked: no more waiting.
    pub fn done(&self, key: &str) {
        self.0.lock().unwrap().remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_after_a_failure_until_it_works() {
        let retry = RetryLater::new();
        assert!(!retry.waiting("server:1", 100));
        retry.failed("server:1", 100);
        assert!(retry.waiting("server:1", 100 + 3600));
        assert!(!retry.waiting("server:2", 100 + 3600));
        assert!(!retry.waiting("server:1", 100 + RETRY_SECS));
        retry.failed("server:1", 200);
        retry.done("server:1");
        assert!(!retry.waiting("server:1", 300));
    }
}
