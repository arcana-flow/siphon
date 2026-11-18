//! Cross-platform OS process identity capture, detachment, and liveness verification.

use std::io;
use std::process::Command;

use crate::jobs::ProcessOwner;

/// Result of checking whether a previously recorded [`ProcessOwner`] is still alive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessLiveness {
    /// The OS confirmed the process is active and matches its recorded start marker.
    Alive,
    /// The OS confirmed the recorded owner process is gone (exited, zombie, or PID reused).
    ConfirmedDead {
        /// Human-readable explanation stored when marking an interrupted job.
        reason: String,
    },
    /// A process with the recorded PID exists, but identity could not be conclusively verified.
    Ambiguous,
}

/// Captures the current OS process identity including a platform start marker when available.
#[must_use]
pub fn current_process_owner() -> ProcessOwner {
    let pid = std::process::id();
    let started_at = capture_process_started_at(pid);
    ProcessOwner::new(pid, started_at).expect("current OS process ID must be positive")
}

/// Checks whether `owner` still refers to the same live OS process.
#[must_use]
pub fn check_process_liveness(owner: &ProcessOwner) -> ProcessLiveness {
    check_platform_liveness(owner)
}

/// Configures a worker [`Command`] to execute detached from the submitting terminal/console.
pub fn configure_detached_worker_command(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe extern "C" {
            fn setsid() -> i32;
        }
        // SAFETY: `pre_exec` runs between `fork` and `exec` in a freshly forked child process
        // that is never a process-group leader, so `setsid()` safely creates a new session
        // detached from the controlling terminal.
        unsafe {
            cmd.pre_exec(|| {
                if setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
}

/// Detaches the worker's standard streams from parent pipes after sending the startup ack.
pub fn redirect_worker_stdio_to_null() {
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::os::unix::io::AsRawFd;

        unsafe extern "C" {
            fn dup2(oldfd: i32, newfd: i32) -> i32;
        }

        if let Ok(null_file) = OpenOptions::new().read(true).write(true).open("/dev/null") {
            let null_fd = null_file.as_raw_fd();
            for target_fd in [0i32, 1i32, 2i32] {
                if null_fd != target_fd {
                    // SAFETY: `null_fd` is a valid open file descriptor owned by `null_file` for
                    // the duration of this loop; `dup2` atomically replaces `target_fd`.
                    let _ = unsafe { dup2(null_fd, target_fd) };
                }
            }
        }
    }

    #[cfg(windows)]
    {
        use std::fs::OpenOptions;
        use std::os::windows::io::IntoRawHandle;

        const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6; // -10i32 as u32
        const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5; // -11i32 as u32
        const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4; // -12i32 as u32

        unsafe extern "system" {
            fn SetStdHandle(n_std_handle: u32, h_handle: *mut core::ffi::c_void) -> i32;
        }

        if let Ok(null_file) = OpenOptions::new().read(true).write(true).open("NUL") {
            let raw = null_file.into_raw_handle();
            // SAFETY: `raw` is a valid NUL handle intentionally kept open as the process's
            // standard handles for the remainder of the detached worker lifetime.
            unsafe {
                let _ = SetStdHandle(STD_INPUT_HANDLE, raw);
                let _ = SetStdHandle(STD_OUTPUT_HANDLE, raw);
                let _ = SetStdHandle(STD_ERROR_HANDLE, raw);
            }
        }
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillZeroOutcome {
    Exists,
    NoSuchProcess,
}

#[cfg(unix)]
fn unix_kill_zero(pid: u32) -> KillZeroOutcome {
    const ESRCH: i32 = 3;

    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    let Ok(raw_pid) = i32::try_from(pid) else {
        return KillZeroOutcome::Exists;
    };
    if raw_pid <= 0 {
        return KillZeroOutcome::NoSuchProcess;
    }
    // SAFETY: `kill(pid, 0)` performs existence checking without delivering a signal.
    let rc = unsafe { kill(raw_pid, 0) };
    if rc == 0 {
        return KillZeroOutcome::Exists;
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(ESRCH) => KillZeroOutcome::NoSuchProcess,
        _ => KillZeroOutcome::Exists,
    }
}

#[cfg(target_os = "linux")]
fn capture_process_started_at(pid: u32) -> Option<String> {
    let contents = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (state, marker) = parse_linux_proc_stat(&contents)?;
    if matches!(state, 'Z' | 'X') {
        return None;
    }
    Some(marker)
}

#[cfg(target_os = "linux")]
fn check_platform_liveness(owner: &ProcessOwner) -> ProcessLiveness {
    let stat_path = format!("/proc/{}/stat", owner.pid);
    match std::fs::read_to_string(&stat_path) {
        Ok(contents) => {
            let Some((state, actual_marker)) = parse_linux_proc_stat(&contents) else {
                return ProcessLiveness::Ambiguous;
            };
            if matches!(state, 'Z' | 'X') {
                return ProcessLiveness::ConfirmedDead {
                    reason: format!("process {} has terminated (state {state})", owner.pid),
                };
            }
            match owner.process_started_at.as_deref() {
                Some(expected) if expected == actual_marker => ProcessLiveness::Alive,
                Some(_) => ProcessLiveness::ConfirmedDead {
                    reason: format!("process {} exited and its PID was reused", owner.pid),
                },
                None => ProcessLiveness::Ambiguous,
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if std::path::Path::new("/proc/self/stat").exists()
                || unix_kill_zero(owner.pid) == KillZeroOutcome::NoSuchProcess
            {
                ProcessLiveness::ConfirmedDead {
                    reason: format!("process {} is no longer running", owner.pid),
                }
            } else {
                ProcessLiveness::Ambiguous
            }
        }
        Err(_) => match unix_kill_zero(owner.pid) {
            KillZeroOutcome::NoSuchProcess => ProcessLiveness::ConfirmedDead {
                reason: format!("process {} is no longer running", owner.pid),
            },
            KillZeroOutcome::Exists => ProcessLiveness::Ambiguous,
        },
    }
}

/// Parses `(state_char, "linux:<starttime_ticks>")` from `/proc/<pid>/stat`.
#[cfg(any(target_os = "linux", test))]
fn parse_linux_proc_stat(contents: &str) -> Option<(char, String)> {
    let end_comm = contents.rfind(')')?;
    let rest = contents.get(end_comm + 1..)?.trim_start();
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    // Field 22 (1-indexed) in `/proc/<pid>/stat` is index 19 after `(comm)`.
    // We already consumed index 0 (`state`), so index 19 is 18 skips later (`nth(18)`).
    let starttime = fields.nth(18)?;
    if starttime.is_empty() || !starttime.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((state, format!("linux:{starttime}")))
}

#[cfg(target_os = "macos")]
mod macos_proc {
    use core::ffi::{c_char, c_void};

    pub const PROC_PIDTBSDINFO: i32 = 3;
    pub const SZOMB: u32 = 5;

    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    pub struct ProcBsdInfo {
        pub pbi_flags: u32,
        pub pbi_status: u32,
        pub pbi_xstatus: u32,
        pub pbi_pid: u32,
        pub pbi_ppid: u32,
        pub pbi_uid: u32,
        pub pbi_gid: u32,
        pub pbi_ruid: u32,
        pub pbi_rgid: u32,
        pub pbi_svuid: u32,
        pub pbi_svgid: u32,
        pub rfu_1: u32,
        pub pbi_comm: [c_char; 16],
        pub pbi_name: [c_char; 32],
        pub pbi_nfiles: u32,
        pub pbi_pgid: u32,
        pub pbi_pjobc: u32,
        pub e_tdev: u32,
        pub e_tpgid: u32,
        pub pbi_nice: i32,
        pub pbi_start_tvsec: u64,
        pub pbi_start_tvusec: u64,
    }

    unsafe extern "C" {
        pub fn proc_pidinfo(
            pid: i32,
            flavor: i32,
            arg: u64,
            buffer: *mut c_void,
            buffersize: i32,
        ) -> i32;
    }
}

#[cfg(target_os = "macos")]
fn query_macos_proc_info(pid: u32) -> Result<macos_proc::ProcBsdInfo, Option<i32>> {
    use core::mem::{MaybeUninit, size_of};

    let Ok(raw_pid) = i32::try_from(pid) else {
        return Err(None);
    };
    if raw_pid <= 0 {
        return Err(Some(3)); // ESRCH
    }
    let mut info = MaybeUninit::<macos_proc::ProcBsdInfo>::zeroed();
    let size = size_of::<macos_proc::ProcBsdInfo>() as i32;
    // SAFETY: `info` is validly aligned writable buffer of `size` bytes matching `PROC_PIDTBSDINFO`.
    let written = unsafe {
        macos_proc::proc_pidinfo(
            raw_pid,
            macos_proc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written == size {
        // SAFETY: `proc_pidinfo` wrote the full `ProcBsdInfo` struct.
        Ok(unsafe { info.assume_init() })
    } else {
        Err(io::Error::last_os_error().raw_os_error())
    }
}

#[cfg(target_os = "macos")]
fn capture_process_started_at(pid: u32) -> Option<String> {
    let info = query_macos_proc_info(pid).ok()?;
    if info.pbi_status == macos_proc::SZOMB {
        return None;
    }
    Some(format!(
        "macos:{}.{:06}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    ))
}

#[cfg(target_os = "macos")]
fn check_platform_liveness(owner: &ProcessOwner) -> ProcessLiveness {
    const ESRCH: i32 = 3;

    match query_macos_proc_info(owner.pid) {
        Ok(info) => {
            if info.pbi_status == macos_proc::SZOMB {
                return ProcessLiveness::ConfirmedDead {
                    reason: format!("process {} has terminated (zombie)", owner.pid),
                };
            }
            let actual_marker = format!(
                "macos:{}.{:06}",
                info.pbi_start_tvsec, info.pbi_start_tvusec
            );
            match owner.process_started_at.as_deref() {
                Some(expected) if expected == actual_marker => ProcessLiveness::Alive,
                Some(_) => ProcessLiveness::ConfirmedDead {
                    reason: format!("process {} exited and its PID was reused", owner.pid),
                },
                None => ProcessLiveness::Ambiguous,
            }
        }
        Err(Some(ESRCH)) => ProcessLiveness::ConfirmedDead {
            reason: format!("process {} is no longer running", owner.pid),
        },
        Err(_) => match unix_kill_zero(owner.pid) {
            KillZeroOutcome::NoSuchProcess => ProcessLiveness::ConfirmedDead {
                reason: format!("process {} is no longer running", owner.pid),
            },
            KillZeroOutcome::Exists => ProcessLiveness::Ambiguous,
        },
    }
}

#[cfg(windows)]
mod win32_proc {
    use core::ffi::c_void;

    pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    pub const SYNCHRONIZE: u32 = 0x0010_0000;
    pub const WAIT_OBJECT_0: u32 = 0;
    pub const STILL_ACTIVE: u32 = 259;
    pub const ERROR_ACCESS_DENIED: i32 = 5;
    pub const ERROR_INVALID_PARAMETER: i32 = 87;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct FileTime {
        pub dw_low_date_time: u32,
        pub dw_high_date_time: u32,
    }

    pub struct HandleGuard(*mut c_void);

    impl HandleGuard {
        pub fn new(raw: *mut c_void) -> Option<Self> {
            if raw.is_null() { None } else { Some(Self(raw)) }
        }

        pub fn as_raw(&self) -> *mut c_void {
            self.0
        }
    }

    impl Drop for HandleGuard {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a non-null process handle returned by `OpenProcess`.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn OpenProcess(
            dw_desired_access: u32,
            b_inherit_handle: i32,
            dw_process_id: u32,
        ) -> *mut c_void;
        pub fn WaitForSingleObject(h_handle: *mut c_void, dw_milliseconds: u32) -> u32;
        pub fn GetExitCodeProcess(h_process: *mut c_void, lp_exit_code: *mut u32) -> i32;
        pub fn GetProcessTimes(
            h_process: *mut c_void,
            lp_creation_time: *mut FileTime,
            lp_exit_time: *mut FileTime,
            lp_kernel_time: *mut FileTime,
            lp_user_time: *mut FileTime,
        ) -> i32;
        pub fn CloseHandle(h_object: *mut c_void) -> i32;
    }
}

#[cfg(windows)]
fn query_win32_creation_marker(handle: *mut core::ffi::c_void) -> Option<String> {
    let mut creation = win32_proc::FileTime::default();
    let mut exit = win32_proc::FileTime::default();
    let mut kernel = win32_proc::FileTime::default();
    let mut user = win32_proc::FileTime::default();
    // SAFETY: `handle` is an open process handle and all out-pointers are valid `FileTime` structs.
    let ok = unsafe {
        win32_proc::GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user)
    };
    if ok == 0 {
        return None;
    }
    let ticks =
        (u64::from(creation.dw_high_date_time) << 32) | u64::from(creation.dw_low_date_time);
    Some(format!("win:{ticks}"))
}

#[cfg(windows)]
fn capture_process_started_at(pid: u32) -> Option<String> {
    // SAFETY: `OpenProcess` with `PROCESS_QUERY_LIMITED_INFORMATION` queries only metadata.
    let raw =
        unsafe { win32_proc::OpenProcess(win32_proc::PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    let guard = win32_proc::HandleGuard::new(raw)?;
    query_win32_creation_marker(guard.as_raw())
}

#[cfg(windows)]
fn check_platform_liveness(owner: &ProcessOwner) -> ProcessLiveness {
    let mut has_sync = true;
    // SAFETY: `OpenProcess` requests metadata + wait state for `owner.pid`.
    let mut raw = unsafe {
        win32_proc::OpenProcess(
            win32_proc::PROCESS_QUERY_LIMITED_INFORMATION | win32_proc::SYNCHRONIZE,
            0,
            owner.pid,
        )
    };
    if raw.is_null()
        && io::Error::last_os_error().raw_os_error() == Some(win32_proc::ERROR_ACCESS_DENIED)
    {
        has_sync = false;
        // SAFETY: fallback without `SYNCHRONIZE` if the handle permits query-only access.
        raw = unsafe {
            win32_proc::OpenProcess(win32_proc::PROCESS_QUERY_LIMITED_INFORMATION, 0, owner.pid)
        };
    }

    let Some(guard) = win32_proc::HandleGuard::new(raw) else {
        return match io::Error::last_os_error().raw_os_error() {
            Some(win32_proc::ERROR_INVALID_PARAMETER) => ProcessLiveness::ConfirmedDead {
                reason: format!("process {} is no longer running", owner.pid),
            },
            _ => ProcessLiveness::Ambiguous,
        };
    };

    if has_sync {
        // SAFETY: `guard` holds a valid handle opened with `SYNCHRONIZE`.
        let wait = unsafe { win32_proc::WaitForSingleObject(guard.as_raw(), 0) };
        if wait == win32_proc::WAIT_OBJECT_0 {
            return ProcessLiveness::ConfirmedDead {
                reason: format!("process {} has exited", owner.pid),
            };
        }
    }

    let mut exit_code = 0u32;
    // SAFETY: `guard` holds a valid handle with `PROCESS_QUERY_LIMITED_INFORMATION`.
    if unsafe { win32_proc::GetExitCodeProcess(guard.as_raw(), &mut exit_code) } != 0
        && exit_code != win32_proc::STILL_ACTIVE
    {
        return ProcessLiveness::ConfirmedDead {
            reason: format!("process {} exited with status {exit_code}", owner.pid),
        };
    }

    let Some(actual_marker) = query_win32_creation_marker(guard.as_raw()) else {
        return ProcessLiveness::Ambiguous;
    };

    match owner.process_started_at.as_deref() {
        Some(expected) if expected == actual_marker => ProcessLiveness::Alive,
        Some(_) => ProcessLiveness::ConfirmedDead {
            reason: format!("process {} exited and its PID was reused", owner.pid),
        },
        None => ProcessLiveness::Ambiguous,
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn capture_process_started_at(_pid: u32) -> Option<String> {
    None
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn check_platform_liveness(owner: &ProcessOwner) -> ProcessLiveness {
    match unix_kill_zero(owner.pid) {
        KillZeroOutcome::NoSuchProcess => ProcessLiveness::ConfirmedDead {
            reason: format!("process {} is no longer running", owner.pid),
        },
        KillZeroOutcome::Exists => ProcessLiveness::Ambiguous,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_linux_proc_stat_including_spaces_and_parentheses_in_comm() {
        let sample = "282264 (siphon (worker) v1) R 282263 282263 282263 34818 282263 4194304 489 0 0 0 0 0 0 0 20 0 1 0 1956858 18214912 1848 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 1 0 0 0 0 0 0 0 0 0 0 0 0 0";
        assert_eq!(
            parse_linux_proc_stat(sample),
            Some(('R', "linux:1956858".to_owned()))
        );

        let zombie = "99 (defunct) Z 1 99 99 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 424242 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0";
        assert_eq!(
            parse_linux_proc_stat(zombie),
            Some(('Z', "linux:424242".to_owned()))
        );
    }

    #[test]
    fn current_process_is_alive_and_mismatched_start_marker_is_confirmed_dead() {
        let current = current_process_owner();
        assert_eq!(check_process_liveness(&current), ProcessLiveness::Alive);
        assert!(current.process_started_at.is_some());

        let reused = ProcessOwner::new(current.pid, Some("wrong-start-marker".to_owned())).unwrap();
        assert!(matches!(
            check_process_liveness(&reused),
            ProcessLiveness::ConfirmedDead { .. }
        ));

        let ambiguous = ProcessOwner::new(current.pid, None).unwrap();
        assert_eq!(
            check_process_liveness(&ambiguous),
            ProcessLiveness::Ambiguous
        );

        let missing = ProcessOwner::new(u32::MAX / 2, Some("missing".to_owned())).unwrap();
        assert!(matches!(
            check_process_liveness(&missing),
            ProcessLiveness::ConfirmedDead { .. }
        ));
    }
}
