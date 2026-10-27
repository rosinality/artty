//! Secure keyboard entry: while it's on and an artty window is active,
//! macOS keeps other apps from reading keystrokes (as Terminal and iTerm2
//! offer for typing passwords). The system counts enables and disables, so
//! this keeps track and calls each only on a change.

use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn EnableSecureEventInput() -> i32;
    fn DisableSecureEventInput() -> i32;
}

/// Turn it on or off; nothing happens if it already is.
pub fn set(on: bool) {
    if ENABLED.swap(on, Ordering::SeqCst) != on {
        // SAFETY: plain Carbon calls with no arguments, balanced by the swap.
        unsafe {
            if on {
                EnableSecureEventInput();
            } else {
                DisableSecureEventInput();
            }
        }
    }
}

/// Toggle the setting in the config file (windows pick it up on reload).
pub fn toggle_setting() -> std::io::Result<()> {
    let (config, _) = artty_core::Config::load();
    let mut file = artty_core::config_file::ConfigFile::open()?;
    file.set_bool("secure-keyboard-entry", !config.secure_keyboard_entry);
    file.save()
}
