//! Cross-platform OS process identity capture, detachment, and liveness verification.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use serde::{Deserialize, Serialize};

use crate::jobs::{ExecutionMode, OwnershipToken, ProcessOwner};

/// Result of checking whether a previously recorded [`ProcessOwner`] is still alive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessLiveness {
    /// The OS confirmed the process (or an active child/process group it spawned) is still active.
    Alive,
    /// The OS confirmed the recorded owner process and any tracked child tree are gone.
    ConfirmedDead {
        /// Human-readable explanation stored when marking an interrupted job.
        reason: String,
    },
    /// A process with the recorded PID exists, but identity could not be conclusively verified.
    Ambiguous,
}

/// Optional subprocess tracking configuration attached when launching `yt-dlp` under a reserved
/// slot or background job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessTrackerConfig {
    /// File-backed SQLite database path used to namespace `.owners/` runtime child markers.
    pub db_path: Option<PathBuf>,
    /// Fencing ownership token of the active reservation or background job.
    pub owner_token: OwnershipToken,
    /// OS process identity of the Siphon owner process launching `yt-dlp`.
    pub owner: ProcessOwner,
    /// Execution mode of the Siphon owner (`foreground` vs `background`).
    pub execution_mode: ExecutionMode,
    /// On Unix, spawn `yt-dlp` in its own dedicated process group (`setpgid(0, 0)`) when the
    /// Siphon owner is a foreground process without its own `setsid()` session.
    pub isolate_process_group: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TrackedChildRecord {
    owner_pid: u32,
    owner_process_started_at: Option<String>,
    child_pid: u32,
    child_process_started_at: Option<String>,
    child_pgid: Option<u32>,
}

/// RAII guard holding platform process-tree containment (e.g. Win32 Job Object) and cleaning up
/// the companion `.owners/<owner_token>.json` file after the child process exits.
pub struct TrackedChildGuard {
    db_path: Option<PathBuf>,
    owner_token: Option<OwnershipToken>,
    #[cfg(windows)]
    _win32_job: Option<Win32JobGuard>,
}

impl TrackedChildGuard {
    fn none() -> Self {
        Self {
            db_path: None,
            owner_token: None,
            #[cfg(windows)]
            _win32_job: None,
        }
    }
}

impl Drop for TrackedChildGuard {
    fn drop(&mut self) {
        if let Some(token) = self.owner_token.take() {
            remove_tracked_child(self.db_path.as_deref(), token);
        }
    }
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
    match check_primary_liveness(owner) {
        PrimaryLiveness::Alive => ProcessLiveness::Alive,
        PrimaryLiveness::Ambiguous => ProcessLiveness::Ambiguous,
        PrimaryLiveness::DeadGone { reason } | PrimaryLiveness::DeadReused { reason } => {
            ProcessLiveness::ConfirmedDead { reason }
        }
    }
}

/// Checks whether `owner` OR any active child/process group launched by it (for example `yt-dlp`
/// or `ffmpeg` under a killed Siphon owner) is still alive, removing any stale companion marker
/// once confirmed dead.
#[must_use]
pub(crate) fn check_active_owner_liveness(
    owner: &ProcessOwner,
    db_path: Option<&Path>,
    owner_token: Option<OwnershipToken>,
    execution_mode: ExecutionMode,
) -> ProcessLiveness {
    let tracked = db_path
        .zip(owner_token)
        .and_then(|(path, token)| read_tracked_child(path, token))
        .filter(|rec| {
            rec.owner_pid == owner.pid && rec.owner_process_started_at == owner.process_started_at
        });

    match check_primary_liveness(owner) {
        PrimaryLiveness::Alive => ProcessLiveness::Alive,
        PrimaryLiveness::Ambiguous => ProcessLiveness::Ambiguous,
        PrimaryLiveness::DeadReused { reason } => {
            // PID `owner.pid` was recycled by an unrelated process; never conflate `owner.pid`'s
            // session/pgid with old children. Only inspect an explicitly isolated child PGID if
            // different from `owner.pid`.
            if let Some(record) = &tracked
                && let Some(res) = check_tracked_child_liveness(record, execution_mode)
            {
                return res;
            }
            if let Some(token) = owner_token {
                remove_tracked_child(db_path, token);
            }
            ProcessLiveness::ConfirmedDead { reason }
        }
        PrimaryLiveness::DeadGone { reason } => {
            if let Some(record) = &tracked
                && let Some(res) = check_tracked_child_liveness(record, execution_mode)
            {
                return res;
            }
            if execution_mode == ExecutionMode::Background
                && has_live_process_in_group(owner.pid, true)
            {
                return ProcessLiveness::Alive;
            }
            if let Some(token) = owner_token {
                remove_tracked_child(db_path, token);
            }
            ProcessLiveness::ConfirmedDead { reason }
        }
    }
}

fn check_tracked_child_liveness(
    record: &TrackedChildRecord,
    execution_mode: ExecutionMode,
) -> Option<ProcessLiveness> {
    let child_owner =
        ProcessOwner::new(record.child_pid, record.child_process_started_at.clone()).ok()?;
    match check_primary_liveness(&child_owner) {
        PrimaryLiveness::Alive => return Some(ProcessLiveness::Alive),
        PrimaryLiveness::Ambiguous => return Some(ProcessLiveness::Ambiguous),
        PrimaryLiveness::DeadReused { .. } => {}
        PrimaryLiveness::DeadGone { .. } => {
            // Even if `yt-dlp` itself exited, keep ownership while a descendant such as `ffmpeg`
            // remains active inside its dedicated process group (or worker session).
            if let Some(pgid) = record.child_pgid
                && pgid != record.owner_pid
                && has_live_process_in_group(pgid, execution_mode == ExecutionMode::Background)
            {
                return Some(ProcessLiveness::Alive);
            }
        }
    }
    None
}

/// Spawns `command`, optionally tracking its child/process-group tree under `tracking` before the
/// child executes its target image (`execve` on Unix) or under a `KILL_ON_JOB_CLOSE` Win32 Job Object.
pub(crate) fn spawn_tracked_command(
    command: &mut Command,
    tracking: Option<&ProcessTrackerConfig>,
) -> io::Result<(Child, TrackedChildGuard)> {
    let Some(cfg) = tracking else {
        let child = command.spawn()?;
        return Ok((child, TrackedChildGuard::none()));
    };

    #[cfg(unix)]
    {
        spawn_tracked_command_unix(command, cfg)
    }

    #[cfg(windows)]
    {
        spawn_tracked_command_windows(command, cfg)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = cfg;
        let child = command.spawn()?;
        Ok((child, TrackedChildGuard::none()))
    }
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

fn owners_dir_for_db(db_path: &Path) -> PathBuf {
    let mut name = db_path.as_os_str().to_os_string();
    name.push(".owners");
    PathBuf::from(name)
}

fn owner_record_path(db_path: &Path, owner_token: OwnershipToken) -> PathBuf {
    owners_dir_for_db(db_path).join(format!("{}.json", owner_token.as_i64()))
}

fn write_tracked_child(
    db_path: &Path,
    owner_token: OwnershipToken,
    record: &TrackedChildRecord,
) -> io::Result<()> {
    let dir = owners_dir_for_db(db_path);
    fs::create_dir_all(&dir)?;
    let target = owner_record_path(db_path, owner_token);
    let tmp = dir.join(format!(
        "{}.{}.tmp",
        owner_token.as_i64(),
        std::process::id()
    ));
    let json = serde_json::to_vec(record)
        .map_err(|err| io::Error::other(format!("serialize tracked child record: {err}")))?;
    fs::write(&tmp, &json)?;
    if let Err(err) = fs::rename(&tmp, &target) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

fn read_tracked_child(db_path: &Path, owner_token: OwnershipToken) -> Option<TrackedChildRecord> {
    let bytes = fs::read(owner_record_path(db_path, owner_token)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(crate) fn remove_tracked_child(db_path: Option<&Path>, owner_token: OwnershipToken) {
    let Some(path) = db_path else {
        return;
    };
    let record_path = owner_record_path(path, owner_token);
    let _ = fs::remove_file(record_path);
    let _ = fs::remove_dir(owners_dir_for_db(path));
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PrimaryLiveness {
    Alive,
    Ambiguous,
    DeadGone { reason: String },
    DeadReused { reason: String },
}

#[cfg(unix)]
fn spawn_tracked_command_unix(
    command: &mut Command,
    cfg: &ProcessTrackerConfig,
) -> io::Result<(Child, TrackedChildGuard)> {
    use std::os::unix::process::CommandExt;

    unsafe extern "C" {
        fn setpgid(pid: i32, pgid: i32) -> i32;
        fn getpgrp() -> i32;
    }

    let isolate_pgid = cfg.isolate_process_group;
    if isolate_pgid {
        // SAFETY: `pre_exec` runs between `fork` and `exec`; `setpgid(0, 0)` places the
        // managed subprocess into its own dedicated process group (`pgid == child_pid`).
        unsafe {
            command.pre_exec(|| {
                if setpgid(0, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let child = command.spawn()?;
    let child_pid = child.id();
    // SAFETY: `getpgrp()` takes no arguments and always succeeds.
    let current_pgid = unsafe { getpgrp() };
    let child_pgid = if isolate_pgid {
        Some(child_pid)
    } else {
        u32::try_from(current_pgid).ok().filter(|p| *p > 0)
    };

    let mut wrote_marker = false;
    if let Some(db_path) = &cfg.db_path
        && let Some(child_started_at) = capture_process_started_at(child_pid)
    {
        let record = TrackedChildRecord {
            owner_pid: cfg.owner.pid,
            owner_process_started_at: cfg.owner.process_started_at.clone(),
            child_pid,
            child_process_started_at: Some(child_started_at),
            child_pgid,
        };
        let _ = write_tracked_child(db_path, cfg.owner_token, &record);
        wrote_marker = true;
    }

    let guard = TrackedChildGuard {
        db_path: cfg.db_path.clone(),
        owner_token: wrote_marker.then_some(cfg.owner_token),
    };
    Ok((child, guard))
}

#[cfg(windows)]
struct Win32JobGuard(*mut core::ffi::c_void);

#[cfg(windows)]
impl Drop for Win32JobGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is a valid Win32 Job Object handle owned by this guard.
            let _ = unsafe { win32_proc::CloseHandle(self.0) };
        }
    }
}

#[cfg(windows)]
fn create_kill_on_close_job() -> Option<Win32JobGuard> {
    use core::mem::{size_of, zeroed};

    // SAFETY: creating an unnamed Job Object with null security attributes is always safe.
    let handle = unsafe { win32_proc::CreateJobObjectW(core::ptr::null(), core::ptr::null()) };
    if handle.is_null() {
        return None;
    }
    let guard = Win32JobGuard(handle);
    // SAFETY: all fields of `JobObjectExtendedLimitInformation` are valid when zero-initialized.
    let mut info: win32_proc::JobObjectExtendedLimitInformation = unsafe { zeroed() };
    info.BasicLimitInformation.LimitFlags = win32_proc::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let ok = unsafe {
        win32_proc::SetInformationJobObject(
            guard.0,
            win32_proc::JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
            (&raw mut info).cast(),
            size_of::<win32_proc::JobObjectExtendedLimitInformation>() as u32,
        )
    };
    if ok == 0 { None } else { Some(guard) }
}

#[cfg(windows)]
fn spawn_tracked_command_windows(
    command: &mut Command,
    cfg: &ProcessTrackerConfig,
) -> io::Result<(Child, TrackedChildGuard)> {
    use std::os::windows::io::AsRawHandle;

    let job_guard = create_kill_on_close_job();
    let child = command.spawn()?;

    if let Some(job) = &job_guard {
        let proc_handle = child.as_raw_handle().cast();
        // SAFETY: `job.0` and `proc_handle` are valid handles for the job and spawned process.
        let _ = unsafe { win32_proc::AssignProcessToJobObject(job.0, proc_handle) };
    }

    let child_pid = child.id();
    let mut wrote_marker = false;
    if let Some(db_path) = &cfg.db_path
        && let Some(child_started_at) = capture_process_started_at(child_pid)
    {
        let record = TrackedChildRecord {
            owner_pid: cfg.owner.pid,
            owner_process_started_at: cfg.owner.process_started_at.clone(),
            child_pid,
            child_process_started_at: Some(child_started_at),
            child_pgid: None,
        };
        let _ = write_tracked_child(db_path, cfg.owner_token, &record);
        wrote_marker = true;
    }

    let guard = TrackedChildGuard {
        db_path: cfg.db_path.clone(),
        owner_token: wrote_marker.then_some(cfg.owner_token),
        _win32_job: job_guard,
    };
    Ok((child, guard))
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillZeroOutcome {
    Exists,
    NoSuchProcess,
}

#[cfg(unix)]
fn unix_kill_zero_target(raw_pid: i32) -> KillZeroOutcome {
    const ESRCH: i32 = 3;

    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    // SAFETY: `kill(raw_pid, 0)` performs existence checking without delivering a signal.
    let rc = unsafe { kill(raw_pid, 0) };
    if rc == 0 {
        return KillZeroOutcome::Exists;
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(ESRCH) => KillZeroOutcome::NoSuchProcess,
        _ => KillZeroOutcome::Exists,
    }
}

#[cfg(unix)]
fn unix_kill_zero(pid: u32) -> KillZeroOutcome {
    let Ok(raw_pid) = i32::try_from(pid) else {
        return KillZeroOutcome::Exists;
    };
    if raw_pid <= 0 {
        return KillZeroOutcome::NoSuchProcess;
    }
    unix_kill_zero_target(raw_pid)
}

#[cfg(target_os = "linux")]
fn has_live_process_in_group(target_pgid: u32, check_session: bool) -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str.is_empty() || !name_str.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(contents) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((state, pgrp, session, _)) = parse_linux_proc_stat_info(&contents) else {
            continue;
        };
        if matches!(state, 'Z' | 'X') {
            continue;
        }
        if pgrp == target_pgid || (check_session && session == target_pgid) {
            return true;
        }
    }
    false
}

#[cfg(target_os = "macos")]
fn has_live_process_in_group(target_pgid: u32, check_session: bool) -> bool {
    let Ok(expected_sid) = i32::try_from(target_pgid) else {
        return false;
    };
    let pids = if check_session {
        macos_proc::list_pids(macos_proc::PROC_ALL_PIDS, 0)
    } else {
        macos_proc::list_pids(macos_proc::PROC_PGRP_ONLY, target_pgid)
    };
    for pid in pids {
        let Ok(info) = query_macos_proc_info(pid) else {
            continue;
        };
        if info.pbi_status == macos_proc::SZOMB {
            continue;
        }
        if info.pbi_pgid == target_pgid {
            return true;
        }
        if check_session {
            let Ok(raw_pid) = i32::try_from(pid) else {
                continue;
            };
            // SAFETY: `getsid(raw_pid)` safely queries session ID or returns `-1` with errno.
            if unsafe { macos_proc::getsid(raw_pid) } == expected_sid {
                return true;
            }
        }
    }
    false
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn has_live_process_in_group(target_pgid: u32, _check_session: bool) -> bool {
    let Ok(raw_pgid) = i32::try_from(target_pgid) else {
        return false;
    };
    if raw_pgid <= 0 {
        return false;
    }
    unix_kill_zero_target(-raw_pgid) == KillZeroOutcome::Exists
}

#[cfg(not(unix))]
fn has_live_process_in_group(_target_pgid: u32, _check_session: bool) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn capture_process_started_at(pid: u32) -> Option<String> {
    let contents = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (state, _, _, marker) = parse_linux_proc_stat_info(&contents)?;
    if matches!(state, 'Z' | 'X') {
        return None;
    }
    Some(marker)
}

#[cfg(target_os = "linux")]
fn check_primary_liveness(owner: &ProcessOwner) -> PrimaryLiveness {
    let stat_path = format!("/proc/{}/stat", owner.pid);
    match fs::read_to_string(&stat_path) {
        Ok(contents) => {
            let Some((state, _, _, actual_marker)) = parse_linux_proc_stat_info(&contents) else {
                return PrimaryLiveness::Ambiguous;
            };
            if matches!(state, 'Z' | 'X') {
                return PrimaryLiveness::DeadGone {
                    reason: format!("process {} has terminated (state {state})", owner.pid),
                };
            }
            match owner.process_started_at.as_deref() {
                Some(expected) if expected == actual_marker => PrimaryLiveness::Alive,
                Some(_) => PrimaryLiveness::DeadReused {
                    reason: format!("process {} exited and its PID was reused", owner.pid),
                },
                None => PrimaryLiveness::Ambiguous,
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if Path::new("/proc/self/stat").exists()
                || unix_kill_zero(owner.pid) == KillZeroOutcome::NoSuchProcess
            {
                PrimaryLiveness::DeadGone {
                    reason: format!("process {} is no longer running", owner.pid),
                }
            } else {
                PrimaryLiveness::Ambiguous
            }
        }
        Err(_) => match unix_kill_zero(owner.pid) {
            KillZeroOutcome::NoSuchProcess => PrimaryLiveness::DeadGone {
                reason: format!("process {} is no longer running", owner.pid),
            },
            KillZeroOutcome::Exists => PrimaryLiveness::Ambiguous,
        },
    }
}

/// Parses `(state_char, pgrp, session, "linux:<starttime_ticks>")` from `/proc/<pid>/stat`.
#[cfg(any(target_os = "linux", test))]
fn parse_linux_proc_stat_info(contents: &str) -> Option<(char, u32, u32, String)> {
    let end_comm = contents.rfind(')')?;
    let rest = contents.get(end_comm + 1..)?.trim_start();
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let _ppid = fields.next()?;
    let pgrp = fields.next()?.parse::<u32>().ok()?;
    let session = fields.next()?.parse::<u32>().ok()?;
    // Field 22 (1-indexed) in `/proc/<pid>/stat` is index 19 after `(comm)`.
    // We consumed indices 0..=3 (`state`, `ppid`, `pgrp`, `session`), so index 19 is 15 skips later.
    let starttime = fields.nth(15)?;
    if starttime.is_empty() || !starttime.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((state, pgrp, session, format!("linux:{starttime}")))
}

#[cfg(target_os = "macos")]
mod macos_proc {
    use core::ffi::{c_char, c_void};

    pub const PROC_ALL_PIDS: u32 = 1;
    pub const PROC_PGRP_ONLY: u32 = 2;
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
        pub fn proc_listpids(kind: u32, typeinfo: u32, buffer: *mut c_void, buffersize: i32)
        -> i32;
        pub fn getsid(pid: i32) -> i32;
    }

    pub fn list_pids(kind: u32, typeinfo: u32) -> Vec<u32> {
        // SAFETY: passing null buffer queries the required byte size for PID enumeration.
        let bytes_needed = unsafe { proc_listpids(kind, typeinfo, core::ptr::null_mut(), 0) };
        if bytes_needed <= 0 {
            return Vec::new();
        }
        let count_estimate =
            (bytes_needed as usize / core::mem::size_of::<i32>()).saturating_add(64);
        let mut buf = vec![0i32; count_estimate];
        let byte_cap = (buf.len() * core::mem::size_of::<i32>()) as i32;
        // SAFETY: `buf` is writable for `byte_cap` bytes matching `pid_t` (`i32`).
        let bytes_written =
            unsafe { proc_listpids(kind, typeinfo, buf.as_mut_ptr().cast(), byte_cap) };
        if bytes_written <= 0 {
            return Vec::new();
        }
        let n = (bytes_written as usize / core::mem::size_of::<i32>()).min(buf.len());
        buf.into_iter()
            .take(n)
            .filter_map(|pid| u32::try_from(pid).ok().filter(|p| *p > 0))
            .collect()
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
fn check_primary_liveness(owner: &ProcessOwner) -> PrimaryLiveness {
    const ESRCH: i32 = 3;

    match query_macos_proc_info(owner.pid) {
        Ok(info) => {
            if info.pbi_status == macos_proc::SZOMB {
                return PrimaryLiveness::DeadGone {
                    reason: format!("process {} has terminated (zombie)", owner.pid),
                };
            }
            let actual_marker = format!(
                "macos:{}.{:06}",
                info.pbi_start_tvsec, info.pbi_start_tvusec
            );
            match owner.process_started_at.as_deref() {
                Some(expected) if expected == actual_marker => PrimaryLiveness::Alive,
                Some(_) => PrimaryLiveness::DeadReused {
                    reason: format!("process {} exited and its PID was reused", owner.pid),
                },
                None => PrimaryLiveness::Ambiguous,
            }
        }
        Err(Some(ESRCH)) => PrimaryLiveness::DeadGone {
            reason: format!("process {} is no longer running", owner.pid),
        },
        Err(_) => match unix_kill_zero(owner.pid) {
            KillZeroOutcome::NoSuchProcess => PrimaryLiveness::DeadGone {
                reason: format!("process {} is no longer running", owner.pid),
            },
            KillZeroOutcome::Exists => PrimaryLiveness::Ambiguous,
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
    pub const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: u32 = 9;
    pub const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct FileTime {
        pub dw_low_date_time: u32,
        pub dw_high_date_time: u32,
    }

    #[repr(C)]
    #[allow(dead_code, non_snake_case)]
    pub struct IoCounters {
        pub ReadOperationCount: u64,
        pub WriteOperationCount: u64,
        pub OtherOperationCount: u64,
        pub ReadTransferCount: u64,
        pub WriteTransferCount: u64,
        pub OtherTransferCount: u64,
    }

    #[repr(C)]
    #[allow(dead_code, non_snake_case)]
    pub struct JobObjectBasicLimitInformation {
        pub PerProcessUserTimeLimit: i64,
        pub PerJobUserTimeLimit: i64,
        pub LimitFlags: u32,
        pub MinimumWorkingSetSize: usize,
        pub MaximumWorkingSetSize: usize,
        pub ActiveProcessLimit: u32,
        pub Affinity: usize,
        pub PriorityClass: u32,
        pub SchedulingClass: u32,
    }

    #[repr(C)]
    #[allow(dead_code, non_snake_case)]
    pub struct JobObjectExtendedLimitInformation {
        pub BasicLimitInformation: JobObjectBasicLimitInformation,
        pub IoInfo: IoCounters,
        pub ProcessMemoryLimit: usize,
        pub JobMemoryLimit: usize,
        pub PeakProcessMemoryUsed: usize,
        pub PeakJobMemoryUsed: usize,
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
        pub fn CreateJobObjectW(
            lp_job_attributes: *const c_void,
            lp_name: *const u16,
        ) -> *mut c_void;
        pub fn SetInformationJobObject(
            h_job: *mut c_void,
            job_object_information_class: u32,
            lp_job_object_information: *mut c_void,
            cb_job_object_information_length: u32,
        ) -> i32;
        pub fn AssignProcessToJobObject(h_job: *mut c_void, h_process: *mut c_void) -> i32;
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
fn check_primary_liveness(owner: &ProcessOwner) -> PrimaryLiveness {
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
            Some(win32_proc::ERROR_INVALID_PARAMETER) => PrimaryLiveness::DeadGone {
                reason: format!("process {} is no longer running", owner.pid),
            },
            _ => PrimaryLiveness::Ambiguous,
        };
    };

    if has_sync {
        // SAFETY: `guard` holds a valid handle opened with `SYNCHRONIZE`.
        let wait = unsafe { win32_proc::WaitForSingleObject(guard.as_raw(), 0) };
        if wait == win32_proc::WAIT_OBJECT_0 {
            return PrimaryLiveness::DeadGone {
                reason: format!("process {} has exited", owner.pid),
            };
        }
    }

    let mut exit_code = 0u32;
    // SAFETY: `guard` holds a valid handle with `PROCESS_QUERY_LIMITED_INFORMATION`.
    if unsafe { win32_proc::GetExitCodeProcess(guard.as_raw(), &mut exit_code) } != 0
        && exit_code != win32_proc::STILL_ACTIVE
    {
        return PrimaryLiveness::DeadGone {
            reason: format!("process {} exited with status {exit_code}", owner.pid),
        };
    }

    let Some(actual_marker) = query_win32_creation_marker(guard.as_raw()) else {
        return PrimaryLiveness::Ambiguous;
    };

    match owner.process_started_at.as_deref() {
        Some(expected) if expected == actual_marker => PrimaryLiveness::Alive,
        Some(_) => PrimaryLiveness::DeadReused {
            reason: format!("process {} exited and its PID was reused", owner.pid),
        },
        None => PrimaryLiveness::Ambiguous,
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn capture_process_started_at(_pid: u32) -> Option<String> {
    None
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn check_primary_liveness(owner: &ProcessOwner) -> PrimaryLiveness {
    match unix_kill_zero(owner.pid) {
        KillZeroOutcome::NoSuchProcess => PrimaryLiveness::DeadGone {
            reason: format!("process {} is no longer running", owner.pid),
        },
        KillZeroOutcome::Exists => PrimaryLiveness::Ambiguous,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_linux_proc_stat_including_spaces_and_parentheses_in_comm() {
        let sample = "282264 (siphon (worker) v1) R 282263 282200 282100 34818 282263 4194304 489 0 0 0 0 0 0 0 20 0 1 0 1956858 18214912 1848 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 1 0 0 0 0 0 0 0 0 0 0 0 0 0";
        assert_eq!(
            parse_linux_proc_stat_info(sample),
            Some(('R', 282200, 282100, "linux:1956858".to_owned()))
        );

        let zombie = "99 (defunct) Z 1 99 98 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 424242 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0";
        assert_eq!(
            parse_linux_proc_stat_info(zombie),
            Some(('Z', 99, 98, "linux:424242".to_owned()))
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

    #[test]
    fn active_owner_liveness_honors_tracked_child_and_cleans_up_when_dead() {
        let dir = std::env::temp_dir().join(format!("siphon-owner-track-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("siphon.db");

        let dead_owner = ProcessOwner::new(u32::MAX / 2, Some("dead".to_owned())).unwrap();
        let token = OwnershipToken::new(42).unwrap();
        let current = current_process_owner();

        // When owner died (e.g. foreground `siphon download --upload`) while child is still live,
        // liveness reports `Alive` and preserves the tracking file.
        write_tracked_child(
            &db_path,
            token,
            &TrackedChildRecord {
                owner_pid: dead_owner.pid,
                owner_process_started_at: dead_owner.process_started_at.clone(),
                child_pid: current.pid,
                child_process_started_at: current.process_started_at.clone(),
                child_pgid: None,
            },
        )
        .unwrap();
        assert_eq!(
            check_active_owner_liveness(
                &dead_owner,
                Some(&db_path),
                Some(token),
                ExecutionMode::Foreground
            ),
            ProcessLiveness::Alive
        );
        assert!(owner_record_path(&db_path, token).is_file());

        // Once tracked child is also gone, liveness returns `ConfirmedDead` and removes marker.
        write_tracked_child(
            &db_path,
            token,
            &TrackedChildRecord {
                owner_pid: dead_owner.pid,
                owner_process_started_at: dead_owner.process_started_at.clone(),
                child_pid: u32::MAX / 2 - 1,
                child_process_started_at: Some("gone".to_owned()),
                child_pgid: None,
            },
        )
        .unwrap();
        assert!(matches!(
            check_active_owner_liveness(
                &dead_owner,
                Some(&db_path),
                Some(token),
                ExecutionMode::Foreground
            ),
            ProcessLiveness::ConfirmedDead { .. }
        ));
        assert!(!owner_record_path(&db_path, token).exists());
        assert!(!owners_dir_for_db(&db_path).exists());

        let _ = fs::remove_dir_all(&dir);
    }
}
