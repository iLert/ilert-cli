//! Helpers shared between the unit tests of several modules.

use std::sync::{Mutex, MutexGuard, OnceLock};

/// `colored`'s override is a process-global, and libtest runs tests in threads.
/// Any test that pins colour on or off has to hold this lock for as long as it
/// depends on the setting, or a concurrent test flips it mid-assertion and the
/// failure looks like a bug in the code under test.
static COLOR_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Pins `colored`'s output on or off for the lifetime of the returned guard, and
/// hands the setting back to automatic detection when it drops.
#[must_use]
pub fn colors(on: bool) -> ColorGuard {
    let guard = COLOR_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    colored::control::set_override(on);
    ColorGuard { _guard: guard }
}

pub struct ColorGuard {
    _guard: MutexGuard<'static, ()>,
}

impl Drop for ColorGuard {
    fn drop(&mut self) {
        colored::control::unset_override();
    }
}
