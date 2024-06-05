//! An abstraction over well-known dirs like AppData/Local on Windows and $HOME/.config on Linux
//!
//! On Linux it uses `dirs` which is a convenience wrapper for getting XDG environment vars
//!
//! On Windows it uses `known_folders` which calls into Windows for forwards-compatibility
//! We can't use `dirs` on Windows because we need to match connlib for when it opens wintun.dll.
//!
//! I wanted the ProgramData folder on Windows, which `dirs` alone doesn't provide.

pub use platform::{ipc_service_logs, logs, runtime, session, settings};

#[cfg(target_os = "linux")]
#[path = "known_dirs/linux.rs"]
pub mod platform;

#[cfg(target_os = "macos")]
#[path = "known_dirs/macos.rs"]
pub mod platform;

#[cfg(target_os = "windows")]
#[path = "known_dirs/windows.rs"]
pub mod platform;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke() {
        for dir in [ipc_service_logs(), logs(), runtime(), session(), settings()] {
            let dir = dir.expect("should have gotten Some(path)");
            assert!(dir
                .components()
                .any(|x| x == std::path::Component::Normal("dev.firezone.client".as_ref())));
        }
    }
}
