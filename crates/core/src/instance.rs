//! One live dashboard per user: a second one would only repeat the same scans.
//!
//! The claim is an OS object the process holds until it exits, so a crash never leaves it
//! behind: a named mutex on Windows, an `flock` on a file in the state folder elsewhere.

/// Held while this process runs the dashboard; dropping it lets another one start. Empty
/// when the claim could not be made at all.
pub struct Claim {
    #[cfg(windows)]
    _held: Option<windows::Win32::Foundation::HANDLE>,
    #[cfg(unix)]
    _held: Option<std::fs::File>,
}

/// `None` when another dashboard of this user already runs. When the claim cannot be made
/// at all, the dashboard still starts rather than refusing over a missing folder.
pub fn claim() -> Option<Claim> {
    imp::claim()
}

/// Brings the running dashboard's window to the front, where the OS allows it (Windows).
/// `false` when it could not, so the caller says so instead.
pub fn activate_running() -> bool {
    imp::activate_running()
}

/// Whether the console window closes as soon as this process exits, as when it was opened
/// by double-clicking the program: a message printed just before would never be read.
pub fn console_closes_on_exit() -> bool {
    imp::console_closes_on_exit()
}

#[cfg(windows)]
mod imp {
    use std::path::PathBuf;

    use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HWND};
    use windows::Win32::System::Console::{GetConsoleProcessList, GetConsoleWindow};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::Win32::UI::WindowsAndMessaging::{
        GA_ROOTOWNER, GetAncestor, IsIconic, IsWindow, SW_RESTORE, SetForegroundWindow, ShowWindow,
    };
    use windows::core::w;

    use super::Claim;

    /// Where the running dashboard leaves its console window for a later launch to find.
    fn window_file() -> Option<PathBuf> {
        Some(crate::hooks::data_dir()?.join("dashboard.window"))
    }

    pub fn claim() -> Option<Claim> {
        // `Local\` is this sign-in session, so another user's dashboard does not count.
        let Ok(mutex) = (unsafe { CreateMutexW(None, false, w!(r"Local\agentlens-dashboard")) }) else {
            return Some(Claim { _held: None });
        };
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            // The mutex lives while any handle to it is open, this one included.
            let _ = unsafe { CloseHandle(mutex) };
            return None;
        }
        if let Some(path) = window_file() {
            let window = unsafe { GetConsoleWindow() };
            let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
            let _ = std::fs::write(&path, (window.0 as isize).to_string());
        }
        Some(Claim { _held: Some(mutex) })
    }

    impl Drop for Claim {
        fn drop(&mut self) {
            if let Some(mutex) = self._held {
                // Removed first, so the window is never offered once the claim is gone.
                if let Some(path) = window_file() {
                    let _ = std::fs::remove_file(path);
                }
                let _ = unsafe { CloseHandle(mutex) };
            }
        }
    }

    pub fn activate_running() -> bool {
        let Some(raw) = window_file()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| text.trim().parse::<isize>().ok())
            .filter(|raw| *raw != 0)
        else {
            return false;
        };
        let console = HWND(raw as *mut _);
        unsafe {
            if !IsWindow(Some(console)).as_bool() {
                return false;
            }
            // In Windows Terminal the console window is a hidden stand-in that the terminal's
            // own window owns; in a classic console it is the window itself.
            let owner = GetAncestor(console, GA_ROOTOWNER);
            let window = if owner.is_invalid() { console } else { owner };
            if IsIconic(window).as_bool() {
                let _ = ShowWindow(window, SW_RESTORE);
            }
            SetForegroundWindow(window).as_bool()
        }
    }

    pub fn console_closes_on_exit() -> bool {
        let mut pids = [0u32; 2];
        unsafe { GetConsoleProcessList(&mut pids) == 1 }
    }
}

#[cfg(unix)]
mod imp {
    use std::os::fd::AsRawFd;

    use super::Claim;

    pub fn claim() -> Option<Claim> {
        let Some(file) = crate::hooks::data_dir().and_then(|dir| {
            std::fs::create_dir_all(&dir).ok()?;
            std::fs::File::create(dir.join("dashboard.lock")).ok()
        }) else {
            return Some(Claim { _held: None });
        };
        let held = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        held.then_some(Claim { _held: Some(file) })
    }

    /// A terminal's windows and tabs have no common interface to reach the dashboard through.
    pub fn activate_running() -> bool {
        false
    }

    pub fn console_closes_on_exit() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn second_claim_waits_for_the_first() {
        let first = super::claim().expect("no dashboard runs during tests");
        assert!(super::claim().is_none());
        drop(first);
        assert!(super::claim().is_some());
    }
}
