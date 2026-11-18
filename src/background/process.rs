//! Cross-platform OS process identity capture, detachment, fail-closed child tracking, and
//! liveness verification.
//!
//! ## Supported platform & process-tree boundary
//!
//! - **Supported OSes**: Linux (`/proc/<pid>/stat` start time + TGID/PGID/SID), macOS
//!   (`proc_pidinfo` BSD start time + `proc_listpids`/`getsid`), and Windows (`GetProcessTimes`
//!   creation time + `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` Job Objects). On other Unix targets without
//!   start-time verification, live matching PIDs conservatively report [`ProcessLiveness::Ambiguous`].
//! - **Fail-closed startup tracking**: Tracked `yt-dlp` subprocesses are paused before image
//!   execution (`fork` + close-on-exec gate pipe prior to `execv` on Unix; `CREATE_SUSPENDED` prior
//!   to `ResumeThread` on Windows) until process identity capture, Win32 Job Object assignment, and
//!   marker persistence all succeed. Any tracking failure terminates and reaps the child before
//!   returning an error.
//! - **Mode-specific tree boundary**:
//!   - `--background` jobs detach into their own `setsid()` session (Unix) or detached Job-bounded
//!     worker (Windows), so reconciliation preserves active worker/downloader/post-processor
//!     descendants inside that session until they exit.
//!   - Managed foreground `download --upload <ID>` commands keep the caller's terminal process group
//!     on Unix so interactive TTY reads (`SIGTTIN`-free) and `Ctrl-C` (`SIGINT`) behave natively;
//!     reconciliation tracks the foreground owner and exact direct `yt-dlp` child `(pid, started_at)`
//!     without claiming ownership of a shared interactive shell process group.
//! - **Database identity**: Companion `.owners/<token>.json` markers resolve through canonicalized
//!   database paths (`std::fs::canonicalize`) across relative paths, symlinked parents/files, macOS
//!   `/var` aliases, and Windows case/8.3 aliases (hardlinked SQLite WAL paths are unsupported).

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};

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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TrackedChildRecord {
    owner_pid: u32,
    owner_process_started_at: Option<String>,
    child_pid: u32,
    child_process_started_at: Option<String>,
    child_pgid: Option<u32>,
}

/// Specification for spawning `yt-dlp` with optional fail-closed subprocess tracking.
pub(crate) struct TrackedCommandSpec<'a> {
    pub binary: &'a OsStr,
    pub args: &'a [OsString],
    pub working_dir: Option<&'a Path>,
    pub inherit_stdin: bool,
    pub pipe_stdout: bool,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub create_no_window: bool,
}

/// Spawned `yt-dlp` child handle returned by [`spawn_tracked_command`].
pub(crate) struct TrackedChild {
    inner: TrackedChildInner,
}

enum TrackedChildInner {
    Std(Child),
    #[cfg(unix)]
    UnixGated(unix_proc::UnixTrackedChild),
    #[cfg(windows)]
    Win32(win32_proc::Win32TrackedChild),
}

impl TrackedChild {
    pub(crate) fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        match &mut self.inner {
            TrackedChildInner::Std(child) => child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
            #[cfg(unix)]
            TrackedChildInner::UnixGated(child) => child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
            #[cfg(windows)]
            TrackedChildInner::Win32(child) => child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        }
    }

    pub(crate) fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        match &mut self.inner {
            TrackedChildInner::Std(child) => child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
            #[cfg(unix)]
            TrackedChildInner::UnixGated(child) => child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
            #[cfg(windows)]
            TrackedChildInner::Win32(child) => child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        }
    }

    pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
        match &mut self.inner {
            TrackedChildInner::Std(child) => child.wait(),
            #[cfg(unix)]
            TrackedChildInner::UnixGated(child) => child.wait(),
            #[cfg(windows)]
            TrackedChildInner::Win32(child) => child.wait(),
        }
    }
}

/// RAII guard holding platform process-tree containment (e.g. Win32 Job Object) and cleaning up
/// the companion `.owners/<owner_token>.json` file after the child process exits.
pub struct TrackedChildGuard {
    record_path: Option<PathBuf>,
    owners_dir: Option<PathBuf>,
    #[cfg(windows)]
    _win32_job: Option<win32_proc::Win32JobGuard>,
}

impl TrackedChildGuard {
    fn none() -> Self {
        Self {
            record_path: None,
            owners_dir: None,
            #[cfg(windows)]
            _win32_job: None,
        }
    }
}

impl Drop for TrackedChildGuard {
    fn drop(&mut self) {
        if let (Some(record_path), Some(owners_dir)) =
            (self.record_path.take(), self.owners_dir.take())
        {
            remove_tracked_child_at(&record_path, &owners_dir);
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
            // session/pgid with old children. Only inspect tracked direct child identity and
            // explicitly isolated background group.
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
            // Background workers run inside their own `setsid()` session/PGID, so descendants such
            // as `ffmpeg` keep ownership alive until the session clears. Foreground commands share
            // the caller's terminal group and never inspect `child_pgid`.
            if execution_mode == ExecutionMode::Background
                && let Some(pgid) = record.child_pgid
                && has_live_process_in_group(pgid, true)
            {
                return Some(ProcessLiveness::Alive);
            }
        }
    }
    None
}

/// Spawns `spec`, failing closed and reaping the child if required tracking cannot be
/// established before the target `yt-dlp` image begins executing.
pub(crate) fn spawn_tracked_command(
    spec: &TrackedCommandSpec<'_>,
    tracking: Option<&ProcessTrackerConfig>,
) -> io::Result<(TrackedChild, TrackedChildGuard)> {
    spawn_tracked_command_inner(spec, tracking, false)
}

fn spawn_tracked_command_inner(
    spec: &TrackedCommandSpec<'_>,
    tracking: Option<&ProcessTrackerConfig>,
    fail_after_spawn_for_test: bool,
) -> io::Result<(TrackedChild, TrackedChildGuard)> {
    let Some(cfg) = tracking else {
        let mut command = Command::new(spec.binary);
        command.args(spec.args);
        if let Some(dir) = spec.working_dir {
            command.current_dir(dir);
        }
        command.stdin(if spec.inherit_stdin {
            Stdio::inherit()
        } else {
            Stdio::null()
        });
        command.stdout(if spec.pipe_stdout {
            Stdio::piped()
        } else {
            Stdio::inherit()
        });
        command.stderr(Stdio::piped());
        #[cfg(windows)]
        if spec.create_no_window {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let child = command.spawn()?;
        return Ok((
            TrackedChild {
                inner: TrackedChildInner::Std(child),
            },
            TrackedChildGuard::none(),
        ));
    };

    #[cfg(unix)]
    {
        unix_proc::spawn_tracked(spec, cfg, fail_after_spawn_for_test)
    }

    #[cfg(windows)]
    {
        win32_proc::spawn_tracked(spec, cfg, fail_after_spawn_for_test)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (cfg, fail_after_spawn_for_test);
        let mut command = Command::new(spec.binary);
        command.args(spec.args);
        if let Some(dir) = spec.working_dir {
            command.current_dir(dir);
        }
        command.stdin(if spec.inherit_stdin {
            Stdio::inherit()
        } else {
            Stdio::null()
        });
        command.stdout(if spec.pipe_stdout {
            Stdio::piped()
        } else {
            Stdio::inherit()
        });
        command.stderr(Stdio::piped());
        let child = command.spawn()?;
        Ok((
            TrackedChild {
                inner: TrackedChildInner::Std(child),
            },
            TrackedChildGuard::none(),
        ))
    }
}

/// Handle to a detached worker child process spawned for background IPC startup.
pub(crate) enum WorkerProcess {
    #[cfg(not(windows))]
    Std(Child),
    #[cfg(windows)]
    Win32(win32_proc::Win32SpawnedChild),
}

impl WorkerProcess {
    pub(crate) fn kill(&mut self) -> io::Result<()> {
        match self {
            #[cfg(not(windows))]
            Self::Std(child) => child.kill(),
            #[cfg(windows)]
            Self::Win32(child) => child.kill(),
        }
    }

    pub(crate) fn wait_code(&mut self) -> io::Result<Option<i32>> {
        match self {
            #[cfg(not(windows))]
            Self::Std(child) => child.wait().map(|status| status.code()),
            #[cfg(windows)]
            Self::Win32(child) => child.wait_code(),
        }
    }
}

/// Spawned detached worker process along with its handshake pipe ends.
pub(crate) struct SpawnedWorker {
    pub(crate) process: WorkerProcess,
    pub(crate) stdin: Box<dyn Write + Send>,
    pub(crate) stdout: Box<dyn Read + Send>,
}

/// Spawns `exe` detached with restricted stdio handles (`stdin` and `stdout` piped for handshake,
/// `stderr` directed to null device), preventing caller stdout/stderr pipe inheritance on Windows.
pub(crate) fn spawn_detached_worker_process(
    exe: &Path,
    args: &[String],
    working_dir: &Path,
) -> io::Result<SpawnedWorker> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        unsafe extern "C" {
            fn setsid() -> i32;
        }

        let mut cmd = Command::new(exe);
        cmd.args(args);
        cmd.current_dir(working_dir);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());
        // SAFETY: `pre_exec` runs between `fork` and `exec` in a freshly forked child process
        // that is never a process-group leader, so `setsid()` creates a new detached session.
        unsafe {
            cmd.pre_exec(|| {
                if setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("worker stdin pipe was unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("worker stdout pipe was unavailable"))?;
        Ok(SpawnedWorker {
            process: WorkerProcess::Std(child),
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
        })
    }

    #[cfg(windows)]
    {
        win32_proc::spawn_detached_with_handle_list(exe, args, working_dir)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let mut cmd = Command::new(exe);
        cmd.args(args);
        cmd.current_dir(working_dir);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());
        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("worker stdin pipe was unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("worker stdout pipe was unavailable"))?;
        Ok(SpawnedWorker {
            process: WorkerProcess::Std(child),
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
        })
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
        let invalid = (-1isize) as *mut core::ffi::c_void;

        if let Ok(null_file) = OpenOptions::new().read(true).write(true).open("NUL") {
            let raw = null_file.into_raw_handle();
            // SAFETY: queries and updates current process standard handles, then closes old
            // startup pipe handles once without double-closing duplicates.
            unsafe {
                let olds = [
                    win32_proc::GetStdHandle(STD_INPUT_HANDLE),
                    win32_proc::GetStdHandle(STD_OUTPUT_HANDLE),
                    win32_proc::GetStdHandle(STD_ERROR_HANDLE),
                ];
                let _ = win32_proc::SetStdHandle(STD_INPUT_HANDLE, raw);
                let _ = win32_proc::SetStdHandle(STD_OUTPUT_HANDLE, raw);
                let _ = win32_proc::SetStdHandle(STD_ERROR_HANDLE, raw);
                for (idx, h) in olds.iter().enumerate() {
                    if !h.is_null() && *h != invalid && *h != raw && !olds[..idx].contains(h) {
                        let _ = win32_proc::CloseHandle(*h);
                    }
                }
            }
        }
    }
}

fn resolve_stable_db_path(db_path: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(db_path)
}

fn owners_dir_for_canonical_db(canonical_db_path: &Path) -> PathBuf {
    let mut name = canonical_db_path.as_os_str().to_os_string();
    name.push(".owners");
    PathBuf::from(name)
}

fn owner_record_path_canonical(canonical_db_path: &Path, owner_token: OwnershipToken) -> PathBuf {
    owners_dir_for_canonical_db(canonical_db_path).join(format!("{}.json", owner_token.as_i64()))
}

fn write_tracked_child(
    db_path: &Path,
    owner_token: OwnershipToken,
    record: &TrackedChildRecord,
) -> io::Result<(PathBuf, PathBuf)> {
    let canonical_db = resolve_stable_db_path(db_path)?;
    let dir = owners_dir_for_canonical_db(&canonical_db);
    fs::create_dir_all(&dir)?;
    let target = owner_record_path_canonical(&canonical_db, owner_token);
    let tmp = dir.join(format!(
        "{}.{}.tmp",
        owner_token.as_i64(),
        std::process::id()
    ));
    let json = serde_json::to_vec(record)
        .map_err(|err| io::Error::other(format!("serialize tracked child record: {err}")))?;
    if let Err(err) = fs::write(&tmp, &json) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = fs::rename(&tmp, &target) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    Ok((target, dir))
}

fn read_tracked_child(db_path: &Path, owner_token: OwnershipToken) -> Option<TrackedChildRecord> {
    let canonical_db = resolve_stable_db_path(db_path).ok()?;
    let bytes = fs::read(owner_record_path_canonical(&canonical_db, owner_token)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn remove_tracked_child_at(record_path: &Path, owners_dir: &Path) {
    let _ = fs::remove_file(record_path);
    let _ = fs::remove_dir(owners_dir);
}

pub(crate) fn remove_tracked_child(db_path: Option<&Path>, owner_token: OwnershipToken) {
    let Some(path) = db_path else {
        return;
    };
    let resolved = resolve_stable_db_path(path).unwrap_or_else(|_| path.to_path_buf());
    let owners_dir = owners_dir_for_canonical_db(&resolved);
    let record_path = owner_record_path_canonical(&resolved, owner_token);
    remove_tracked_child_at(&record_path, &owners_dir);
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PrimaryLiveness {
    Alive,
    Ambiguous,
    DeadGone { reason: String },
    DeadReused { reason: String },
}

#[cfg(unix)]
mod unix_proc {
    use core::ffi::{c_char, c_int};
    use std::ffi::{CString, OsStr};
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
    use std::path::PathBuf;
    use std::process::ExitStatus;

    use crate::jobs::ExecutionMode;

    use super::{
        ProcessTrackerConfig, TrackedChild, TrackedChildGuard, TrackedChildInner,
        TrackedChildRecord, TrackedCommandSpec, capture_process_started_at,
        remove_tracked_child_at, resolve_stable_db_path, write_tracked_child,
    };

    const F_DUPFD: c_int = 0;
    const F_SETFD: c_int = 2;
    const FD_CLOEXEC: c_int = 1;
    const EINTR: i32 = 4;
    const ENOENT: i32 = 2;
    const EACCES: i32 = 13;
    const ENOTDIR: i32 = 20;
    const SIGKILL: c_int = 9;

    unsafe extern "C" {
        fn pipe(fds: *mut c_int) -> c_int;
        fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
        fn fork() -> c_int;
        fn read(fd: c_int, buf: *mut core::ffi::c_void, count: usize) -> isize;
        fn write(fd: c_int, buf: *const core::ffi::c_void, count: usize) -> isize;
        fn close(fd: c_int) -> c_int;
        fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
        fn chdir(path: *const c_char) -> c_int;
        fn execv(path: *const c_char, argv: *const *const c_char) -> c_int;
        fn _exit(status: c_int) -> !;
        fn kill(pid: c_int, sig: c_int) -> c_int;
        fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
        fn getpgrp() -> c_int;
    }

    pub(crate) struct UnixTrackedChild {
        pid: c_int,
        reaped: bool,
        pub(crate) stdout: Option<File>,
        pub(crate) stderr: Option<File>,
    }

    impl UnixTrackedChild {
        pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
            use std::os::unix::process::ExitStatusExt;

            if self.reaped {
                return Err(io::Error::other("tracked child already reaped"));
            }
            let status = reap_child(self.pid)?;
            self.reaped = true;
            Ok(ExitStatus::from_raw(status))
        }
    }

    impl Drop for UnixTrackedChild {
        fn drop(&mut self) {
            if !self.reaped {
                let _ = reap_child(self.pid);
                self.reaped = true;
            }
        }
    }

    struct AbortOnDrop {
        pid: c_int,
        marker_paths: Option<(PathBuf, PathBuf)>,
        armed: bool,
    }

    impl AbortOnDrop {
        fn new(pid: c_int) -> Self {
            Self {
                pid,
                marker_paths: None,
                armed: true,
            }
        }

        fn disarm(mut self) -> Option<(PathBuf, PathBuf)> {
            self.armed = false;
            self.marker_paths.take()
        }
    }

    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            if self.armed {
                // SAFETY: `self.pid` is an un-reaped direct child PID owned by this process.
                let _ = unsafe { kill(self.pid, SIGKILL) };
                let _ = reap_child(self.pid);
                if let Some((record_path, owners_dir)) = self.marker_paths.take() {
                    remove_tracked_child_at(&record_path, &owners_dir);
                }
            }
        }
    }

    fn reap_child(pid: c_int) -> io::Result<c_int> {
        let mut status: c_int = 0;
        loop {
            // SAFETY: `pid` is a direct child PID; `status` is a valid out-pointer.
            let rc = unsafe { waitpid(pid, &mut status, 0) };
            if rc == pid {
                return Ok(status);
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(EINTR) {
                continue;
            }
            return Err(err);
        }
    }

    fn bump_fd_above_2(fd: OwnedFd) -> io::Result<OwnedFd> {
        let raw = fd.as_raw_fd();
        if raw > 2 {
            return Ok(fd);
        }
        // SAFETY: `F_DUPFD` duplicates valid `raw` to first descriptor `>= 3`.
        let new_fd = unsafe { fcntl(raw, F_DUPFD, 3 as c_int) };
        if new_fd == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `new_fd` is a newly created file descriptor owned by `dup_owned`.
        let dup_owned = unsafe { OwnedFd::from_raw_fd(new_fd) };
        if unsafe { fcntl(dup_owned.as_raw_fd(), F_SETFD, FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(dup_owned)
    }

    fn create_cloexec_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
        let mut fds = [-1 as c_int; 2];
        // SAFETY: `fds` points to a 2-element `c_int` array.
        if unsafe { pipe(fds.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `pipe` succeeded and initialized `fds[0]` and `fds[1]` as distinct FDs.
        let r = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let w = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        for fd in [&r, &w] {
            if unsafe { fcntl(fd.as_raw_fd(), F_SETFD, FD_CLOEXEC) } == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok((bump_fd_above_2(r)?, bump_fd_above_2(w)?))
    }

    fn to_cstring(s: &OsStr) -> io::Result<CString> {
        CString::new(s.as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "command argument contains interior NUL byte",
            )
        })
    }

    fn resolve_exec_candidates(binary: &OsStr) -> io::Result<Vec<CString>> {
        if binary.as_bytes().contains(&b'/') {
            return Ok(vec![to_cstring(binary)?]);
        }
        let mut candidates = Vec::new();
        if let Some(path_var) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path_var) {
                let candidate: PathBuf = if dir.as_os_str().is_empty() {
                    PathBuf::from(binary)
                } else {
                    dir.join(binary)
                };
                if let Ok(cstr) = CString::new(candidate.as_os_str().as_bytes()) {
                    candidates.push(cstr);
                }
            }
        }
        if candidates.is_empty() {
            candidates.push(to_cstring(binary)?);
        }
        Ok(candidates)
    }

    fn write_errno_and_exit(err_w: c_int, err: i32) -> ! {
        let bytes = err.to_ne_bytes();
        // SAFETY: best-effort non-allocating write of 4-byte `errno` to parent error pipe.
        let _ = unsafe { write(err_w, bytes.as_ptr().cast(), bytes.len()) };
        unsafe { _exit(127) };
    }

    pub(super) fn spawn_tracked(
        spec: &TrackedCommandSpec<'_>,
        cfg: &ProcessTrackerConfig,
        fail_after_spawn_for_test: bool,
    ) -> io::Result<(TrackedChild, TrackedChildGuard)> {
        let canonical_db = cfg
            .db_path
            .as_deref()
            .map(resolve_stable_db_path)
            .transpose()?;

        let mut argv_cstrs = Vec::with_capacity(1 + spec.args.len());
        argv_cstrs.push(to_cstring(spec.binary)?);
        for arg in spec.args {
            argv_cstrs.push(to_cstring(arg)?);
        }
        let mut argv_ptrs: Vec<*const c_char> = argv_cstrs.iter().map(|s| s.as_ptr()).collect();
        argv_ptrs.push(core::ptr::null());

        let candidate_cstrs = resolve_exec_candidates(spec.binary)?;
        let candidate_ptrs: Vec<*const c_char> =
            candidate_cstrs.iter().map(|s| s.as_ptr()).collect();

        let cwd_cstr = spec
            .working_dir
            .map(|p| to_cstring(p.as_os_str()))
            .transpose()?;

        let (gate_r, gate_w) = create_cloexec_pipe()?;
        let (err_r, err_w) = create_cloexec_pipe()?;
        let stdout_pipe = if spec.pipe_stdout {
            Some(create_cloexec_pipe()?)
        } else {
            None
        };
        let (stderr_r, stderr_w) = create_cloexec_pipe()?;

        let null_in_fd = if spec.inherit_stdin {
            None
        } else {
            let file = OpenOptions::new().read(true).open("/dev/null")?;
            let owned = OwnedFd::from(file);
            Some(bump_fd_above_2(owned)?)
        };

        // SAFETY: parent pre-allocated all strings and pipe descriptors above. Inside the child
        // branch (`pid == 0`), only async-signal-safe syscalls execute before `_exit` or `execv`.
        let pid = unsafe { fork() };
        if pid == -1 {
            return Err(io::Error::last_os_error());
        }

        if pid == 0 {
            let gate_r_fd = gate_r.as_raw_fd();
            let err_w_fd = err_w.as_raw_fd();
            let out_w_fd = stdout_pipe.as_ref().map(|(_, w)| w.as_raw_fd());
            let stderr_w_fd = stderr_w.as_raw_fd();
            let null_in_raw = null_in_fd.as_ref().map(AsRawFd::as_raw_fd);

            // SAFETY: close parent ends before awaiting release gate.
            unsafe {
                let _ = close(gate_w.as_raw_fd());
                let _ = close(err_r.as_raw_fd());
                if let Some((ref out_r, _)) = stdout_pipe {
                    let _ = close(out_r.as_raw_fd());
                }
                let _ = close(stderr_r.as_raw_fd());
            }

            // Pause execution cleanly prior to `chdir`/`dup2`/`execv` until parent finishes
            // persisting child tracking metadata (or exits/closes `gate_w`).
            let mut gate_byte = [0u8; 1];
            loop {
                // SAFETY: `gate_r_fd` is open and valid; `gate_byte` is writable 1-byte buffer.
                let n = unsafe { read(gate_r_fd, gate_byte.as_mut_ptr().cast(), 1) };
                if n == 1 {
                    if gate_byte[0] != b'G' {
                        unsafe { _exit(125) };
                    }
                    break;
                }
                if n == 0 {
                    unsafe { _exit(125) };
                }
                if io::Error::last_os_error().raw_os_error() == Some(EINTR) {
                    continue;
                }
                unsafe { _exit(125) };
            }
            // SAFETY: done with gate descriptor prior to `execv`.
            unsafe {
                let _ = close(gate_r_fd);
            }

            if let Some(ref cwd) = cwd_cstr {
                // SAFETY: `cwd` is a pre-allocated NUL-terminated C string.
                if unsafe { chdir(cwd.as_ptr()) } == -1 {
                    write_errno_and_exit(
                        err_w_fd,
                        io::Error::last_os_error().raw_os_error().unwrap_or(1),
                    );
                }
            }

            if let Some(in_fd) = null_in_raw {
                // SAFETY: `in_fd > 2` and `dup2` clears `FD_CLOEXEC` on fd `0`.
                if unsafe { dup2(in_fd, 0) } == -1 {
                    write_errno_and_exit(
                        err_w_fd,
                        io::Error::last_os_error().raw_os_error().unwrap_or(1),
                    );
                }
                unsafe {
                    let _ = close(in_fd);
                }
            }
            if let Some(out_fd) = out_w_fd {
                // SAFETY: `out_fd > 2` and `dup2` clears `FD_CLOEXEC` on fd `1`.
                if unsafe { dup2(out_fd, 1) } == -1 {
                    write_errno_and_exit(
                        err_w_fd,
                        io::Error::last_os_error().raw_os_error().unwrap_or(1),
                    );
                }
                unsafe {
                    let _ = close(out_fd);
                }
            }
            // SAFETY: `stderr_w_fd > 2` and `dup2` clears `FD_CLOEXEC` on fd `2`.
            if unsafe { dup2(stderr_w_fd, 2) } == -1 {
                write_errno_and_exit(
                    err_w_fd,
                    io::Error::last_os_error().raw_os_error().unwrap_or(1),
                );
            }
            unsafe {
                let _ = close(stderr_w_fd);
            }

            let mut saved_eacces: Option<i32> = None;
            let mut last_err: i32 = ENOENT;
            for &prog_ptr in &candidate_ptrs {
                // SAFETY: `prog_ptr` and `argv_ptrs` are NUL-terminated static pre-fork arrays.
                unsafe { execv(prog_ptr, argv_ptrs.as_ptr()) };
                let err = io::Error::last_os_error().raw_os_error().unwrap_or(1);
                if err == ENOENT || err == ENOTDIR {
                    last_err = err;
                    continue;
                }
                if err == EACCES {
                    if saved_eacces.is_none() {
                        saved_eacces = Some(err);
                    }
                    last_err = err;
                    continue;
                }
                write_errno_and_exit(err_w_fd, err);
            }
            write_errno_and_exit(err_w_fd, saved_eacces.unwrap_or(last_err));
        }

        // Parent path: close child ends immediately.
        drop(gate_r);
        drop(err_w);
        drop(stderr_w);
        drop(null_in_fd);
        let out_r = stdout_pipe.map(|(r, w)| {
            drop(w);
            r
        });

        let mut abort_guard = AbortOnDrop::new(pid);
        let child_pid = u32::try_from(pid).map_err(|_| io::Error::other("invalid child pid"))?;

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let child_started_at = Some(
            capture_process_started_at(child_pid)
                .ok_or_else(|| io::Error::other("failed to capture child start-time marker"))?,
        );
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let child_started_at = capture_process_started_at(child_pid);

        let child_pgid = match cfg.execution_mode {
            // Background workers created a dedicated session via `setsid()`.
            // Foreground commands intentionally remain in the terminal's foreground group.
            ExecutionMode::Background => {
                // SAFETY: `getpgrp()` takes no arguments and always succeeds.
                u32::try_from(unsafe { getpgrp() }).ok().filter(|p| *p > 0)
            }
            ExecutionMode::Foreground => None,
        };

        if let Some(db_path) = &canonical_db {
            let record = TrackedChildRecord {
                owner_pid: cfg.owner.pid,
                owner_process_started_at: cfg.owner.process_started_at.clone(),
                child_pid,
                child_process_started_at: child_started_at,
                child_pgid,
            };
            abort_guard.marker_paths =
                Some(write_tracked_child(db_path, cfg.owner_token, &record)?);
        }

        if fail_after_spawn_for_test {
            return Err(io::Error::other("injected post-spawn tracking failure"));
        }

        // Release child to execute `yt-dlp`, then await `execv` confirmation from `err_r`.
        let mut gate_file = File::from(gate_w);
        if let Err(err) = gate_file.write_all(b"G") {
            drop(gate_file);
            return Err(err);
        }
        drop(gate_file);

        let mut err_file = File::from(err_r);
        let mut err_bytes = [0u8; 4];
        let mut total = 0usize;
        while total < err_bytes.len() {
            match err_file.read(&mut err_bytes[total..]) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }
        if total == 4 {
            let raw_errno = i32::from_ne_bytes(err_bytes);
            drop(abort_guard);
            return Err(io::Error::from_raw_os_error(raw_errno));
        } else if total > 0 {
            drop(abort_guard);
            return Err(io::Error::other("incomplete child exec error payload"));
        }

        let guard = match abort_guard.disarm() {
            Some((record_path, owners_dir)) => TrackedChildGuard {
                record_path: Some(record_path),
                owners_dir: Some(owners_dir),
            },
            None => TrackedChildGuard::none(),
        };
        Ok((
            TrackedChild {
                inner: TrackedChildInner::UnixGated(UnixTrackedChild {
                    pid,
                    reaped: false,
                    stdout: out_r.map(File::from),
                    stderr: Some(File::from(stderr_r)),
                }),
            },
            guard,
        ))
    }
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
            let invalid = (-1isize) as *mut c_void;
            if raw.is_null() || raw == invalid {
                None
            } else {
                Some(Self(raw))
            }
        }

        pub fn as_raw(&self) -> *mut c_void {
            self.0
        }

        pub fn take(mut self) -> *mut c_void {
            let raw = self.0;
            self.0 = core::ptr::null_mut();
            raw
        }
    }

    impl Drop for HandleGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` is a valid non-null Win32 handle owned by this guard.
                let _ = unsafe { CloseHandle(self.0) };
            }
        }
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    pub struct SecurityAttributes {
        pub nLength: u32,
        pub lpSecurityDescriptor: *mut c_void,
        pub bInheritHandle: i32,
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    pub struct StartupInfoW {
        pub cb: u32,
        pub lpReserved: *mut u16,
        pub lpDesktop: *mut u16,
        pub lpTitle: *mut u16,
        pub dwX: u32,
        pub dwY: u32,
        pub dwXSize: u32,
        pub dwYSize: u32,
        pub dwXCountChars: u32,
        pub dwYCountChars: u32,
        pub dwFillAttribute: u32,
        pub dwFlags: u32,
        pub wShowWindow: u16,
        pub cbReserved2: u16,
        pub lpReserved2: *mut u8,
        pub hStdInput: *mut c_void,
        pub hStdOutput: *mut c_void,
        pub hStdError: *mut c_void,
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    pub struct StartupInfoExW {
        pub StartupInfo: StartupInfoW,
        pub lpAttributeList: *mut c_void,
    }

    #[repr(C)]
    #[allow(dead_code, non_snake_case)]
    pub struct ProcessInformation {
        pub hProcess: *mut c_void,
        pub hThread: *mut c_void,
        pub dwProcessId: u32,
        pub dwThreadId: u32,
    }

    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ_WRITE: u32 = 0x0000_0001 | 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
    const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
    const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DUPLICATE_SAME_ACCESS: u32 = 0x0000_0002;
    const ERROR_INVALID_HANDLE: i32 = 6;
    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
    const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x0002_0002;

    pub struct Win32JobGuard(*mut c_void);

    impl Drop for Win32JobGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` is a valid Win32 Job Object handle owned by this guard.
                let _ = unsafe { CloseHandle(self.0) };
            }
        }
    }

    fn create_kill_on_close_job() -> std::io::Result<Win32JobGuard> {
        use core::mem::{size_of, zeroed};

        // SAFETY: creating an unnamed Job Object with null security attributes is always safe.
        let handle = unsafe { CreateJobObjectW(core::ptr::null(), core::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let guard = Win32JobGuard(handle);
        // SAFETY: all fields of `JobObjectExtendedLimitInformation` are valid zero-initialized.
        let mut info: JobObjectExtendedLimitInformation = unsafe { zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: configuring `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` on valid job handle.
        let ok = unsafe {
            SetInformationJobObject(
                guard.0,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                (&raw mut info).cast(),
                size_of::<JobObjectExtendedLimitInformation>() as u32,
            )
        };
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(guard)
        }
    }

    fn wait_process_exit_code(process_handle: *mut c_void) -> std::io::Result<u32> {
        const INFINITE: u32 = 0xFFFF_FFFF;
        const WAIT_FAILED: u32 = 0xFFFF_FFFF;
        // SAFETY: `process_handle` is a valid waitable process handle.
        let wait = unsafe { WaitForSingleObject(process_handle, INFINITE) };
        if wait == WAIT_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        let mut exit_code = 0u32;
        // SAFETY: querying exit code after `WaitForSingleObject` completes.
        if unsafe { GetExitCodeProcess(process_handle, &mut exit_code) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(exit_code)
    }

    pub struct Win32SpawnedChild {
        process: HandleGuard,
    }

    impl Win32SpawnedChild {
        pub fn kill(&mut self) -> std::io::Result<()> {
            // SAFETY: `self.process.as_raw()` is a live Win32 process handle.
            let ok = unsafe { TerminateProcess(self.process.as_raw(), 1) };
            if ok != 0 {
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            let mut exit_code = 0u32;
            // SAFETY: querying exit code disambiguates already-exited processes from access errors.
            if err.raw_os_error() == Some(ERROR_ACCESS_DENIED)
                && unsafe { GetExitCodeProcess(self.process.as_raw(), &mut exit_code) } != 0
                && exit_code != STILL_ACTIVE
            {
                return Ok(());
            }
            Err(err)
        }

        pub fn wait_code(&mut self) -> std::io::Result<Option<i32>> {
            wait_process_exit_code(self.process.as_raw()).map(|code| Some(code as i32))
        }
    }

    pub(crate) struct Win32TrackedChild {
        process: HandleGuard,
        pub(crate) stdout: Option<std::fs::File>,
        pub(crate) stderr: Option<std::fs::File>,
    }

    impl Win32TrackedChild {
        pub(crate) fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
            use std::os::windows::process::ExitStatusExt;
            let code = wait_process_exit_code(self.process.as_raw())?;
            Ok(std::process::ExitStatus::from_raw(code))
        }
    }

    struct Win32AbortGuard {
        process: Option<HandleGuard>,
        marker_paths: Option<(std::path::PathBuf, std::path::PathBuf)>,
    }

    impl Win32AbortGuard {
        fn disarm(
            &mut self,
        ) -> Option<(
            HandleGuard,
            Option<(std::path::PathBuf, std::path::PathBuf)>,
        )> {
            self.process
                .take()
                .map(|proc| (proc, self.marker_paths.take()))
        }
    }

    impl Drop for Win32AbortGuard {
        fn drop(&mut self) {
            if let Some(process) = self.process.take() {
                // SAFETY: terminating and reaping an aborted suspended/untracked child process.
                unsafe {
                    let _ = TerminateProcess(process.as_raw(), 1);
                }
                let _ = wait_process_exit_code(process.as_raw());
                if let Some((record_path, owners_dir)) = self.marker_paths.take() {
                    super::remove_tracked_child_at(&record_path, &owners_dir);
                }
            }
        }
    }

    struct AttrListGuard {
        ptr: *mut c_void,
        _backing: Vec<usize>,
        _unique_handles: Vec<*mut c_void>,
    }

    impl Drop for AttrListGuard {
        fn drop(&mut self) {
            if !self.ptr.is_null() {
                // SAFETY: `self.ptr` was initialized by `InitializeProcThreadAttributeList`.
                unsafe { DeleteProcThreadAttributeList(self.ptr) };
            }
        }
    }

    fn path_to_wide_nul(path: &std::path::Path) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str()
            .encode_wide()
            .chain(core::iter::once(0))
            .collect()
    }

    fn inheritable_security_attributes() -> SecurityAttributes {
        SecurityAttributes {
            nLength: core::mem::size_of::<SecurityAttributes>() as u32,
            lpSecurityDescriptor: core::ptr::null_mut(),
            bInheritHandle: 1,
        }
    }

    fn create_anonymous_pipe(parent_reads: bool) -> std::io::Result<(HandleGuard, HandleGuard)> {
        let mut sa = inheritable_security_attributes();
        let mut read_raw = core::ptr::null_mut();
        let mut write_raw = core::ptr::null_mut();
        // SAFETY: creating an anonymous pipe with valid handle out-pointers.
        if unsafe { CreatePipe(&mut read_raw, &mut write_raw, &mut sa, 0) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let read_guard = HandleGuard::new(read_raw)
            .ok_or_else(|| std::io::Error::other("null pipe read handle"))?;
        let write_guard = HandleGuard::new(write_raw)
            .ok_or_else(|| std::io::Error::other("null pipe write handle"))?;
        let non_inherited = if parent_reads {
            read_guard.as_raw()
        } else {
            write_guard.as_raw()
        };
        // SAFETY: clear inheritance on parent pipe end so child cannot inherit it.
        if unsafe { SetHandleInformation(non_inherited, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((read_guard, write_guard))
    }

    fn open_inheritable_nul(desired_access: u32) -> std::io::Result<HandleGuard> {
        let mut sa = inheritable_security_attributes();
        let nul_name: [u16; 4] = [b'N' as u16, b'U' as u16, b'L' as u16, 0];
        // SAFETY: opening `NUL` device with inheritable security attributes.
        let nul_raw = unsafe {
            CreateFileW(
                nul_name.as_ptr(),
                desired_access,
                FILE_SHARE_READ_WRITE,
                &mut sa,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                core::ptr::null_mut(),
            )
        };
        HandleGuard::new(nul_raw).ok_or_else(std::io::Error::last_os_error)
    }

    fn duplicate_inheritable_std_handle(
        std_handle_id: u32,
        fallback_access: u32,
    ) -> std::io::Result<HandleGuard> {
        let invalid = (-1isize) as *mut c_void;
        // SAFETY: queries standard handle from current process PEB.
        let raw = unsafe { GetStdHandle(std_handle_id) };
        if raw.is_null() || raw == invalid {
            return open_inheritable_nul(fallback_access);
        }
        let mut dup = core::ptr::null_mut();
        // SAFETY: duplicates handle in current process into a fresh inheritable handle without
        // mutating parent's own standard handle flags.
        let current_proc = unsafe { GetCurrentProcess() };
        if unsafe {
            DuplicateHandle(
                current_proc,
                raw,
                current_proc,
                &mut dup,
                0,
                1,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(ERROR_INVALID_HANDLE) {
                return open_inheritable_nul(fallback_access);
            }
            return Err(err);
        }
        HandleGuard::new(dup).ok_or_else(|| std::io::Error::other("null duplicated std handle"))
    }

    fn build_handle_list_attrs(handles: &[*mut c_void]) -> std::io::Result<AttrListGuard> {
        let mut unique_handles = Vec::with_capacity(handles.len());
        for &h in handles {
            if !unique_handles.contains(&h) {
                unique_handles.push(h);
            }
        }
        let mut attr_size: usize = 0;
        // SAFETY: query required buffer size for 1 thread attribute entry.
        let _ = unsafe {
            InitializeProcThreadAttributeList(core::ptr::null_mut(), 1, 0, &mut attr_size)
        };
        if attr_size == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut backing = vec![0usize; attr_size.div_ceil(core::mem::size_of::<usize>())];
        let attr_ptr: *mut c_void = backing.as_mut_ptr().cast();
        // SAFETY: `attr_ptr` points to properly aligned buffer of `attr_size` bytes.
        if unsafe { InitializeProcThreadAttributeList(attr_ptr, 1, 0, &mut attr_size) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut attr_guard = AttrListGuard {
            ptr: attr_ptr,
            _backing: backing,
            _unique_handles: unique_handles,
        };
        let byte_len = attr_guard._unique_handles.len() * core::mem::size_of::<*mut c_void>();
        // SAFETY: `_unique_handles` lives inside `AttrListGuard` for the full lifetime of
        // `StartupInfoExW` and restricts child handle inheritance strictly to those handles.
        if unsafe {
            UpdateProcThreadAttribute(
                attr_ptr,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                attr_guard._unique_handles.as_mut_ptr().cast(),
                byte_len,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(attr_guard)
    }

    fn build_command_line<S: AsRef<std::ffi::OsStr>>(
        binary: &std::ffi::OsStr,
        args: &[S],
    ) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;

        let mut cmd_line = Vec::<u16>::new();
        super::push_windows_quoted_arg(&mut cmd_line, &binary.encode_wide().collect::<Vec<u16>>());
        for arg in args {
            cmd_line.push(b' ' as u16);
            super::push_windows_quoted_arg(
                &mut cmd_line,
                &arg.as_ref().encode_wide().collect::<Vec<u16>>(),
            );
        }
        cmd_line.push(0);
        cmd_line
    }

    pub fn spawn_detached_with_handle_list(
        exe: &std::path::Path,
        args: &[String],
        working_dir: &std::path::Path,
    ) -> std::io::Result<super::SpawnedWorker> {
        use core::mem::{size_of, zeroed};
        use std::fs::File;
        use std::os::windows::io::FromRawHandle;

        let (stdin_r_guard, stdin_w_guard) = create_anonymous_pipe(false)?;
        let (stdout_r_guard, stdout_w_guard) = create_anonymous_pipe(true)?;
        let nul_guard = open_inheritable_nul(GENERIC_READ | GENERIC_WRITE)?;

        let _attr_guard = build_handle_list_attrs(&[
            stdin_r_guard.as_raw(),
            stdout_w_guard.as_raw(),
            nul_guard.as_raw(),
        ])?;

        // SAFETY: `StartupInfoExW` is valid zeroed with `cb` set explicitly.
        let mut si_ex: StartupInfoExW = unsafe { zeroed() };
        si_ex.StartupInfo.cb = size_of::<StartupInfoExW>() as u32;
        si_ex.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si_ex.StartupInfo.hStdInput = stdin_r_guard.as_raw();
        si_ex.StartupInfo.hStdOutput = stdout_w_guard.as_raw();
        si_ex.StartupInfo.hStdError = nul_guard.as_raw();
        si_ex.lpAttributeList = _attr_guard.ptr;

        let mut cmd_line = build_command_line(exe.as_os_str(), args);
        let cwd_w = path_to_wide_nul(working_dir);

        // SAFETY: `ProcessInformation` is a plain out-struct initialized zeroed.
        let mut pi: ProcessInformation = unsafe { zeroed() };
        let creation_flags =
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | EXTENDED_STARTUPINFO_PRESENT;

        // SAFETY: all strings are NUL-terminated UTF-16, `cmd_line` is mutable with argv[0]
        // properly quoted, `si_ex` has valid `StartupInfoExW` layout, and `bInheritHandles = 1`
        // uses `HANDLE_LIST`.
        let created = unsafe {
            CreateProcessW(
                core::ptr::null(),
                cmd_line.as_mut_ptr(),
                core::ptr::null(),
                core::ptr::null(),
                1,
                creation_flags,
                core::ptr::null(),
                cwd_w.as_ptr(),
                &si_ex,
                &mut pi,
            )
        };
        if created == 0 {
            return Err(std::io::Error::last_os_error());
        }

        if let Some(thread_guard) = HandleGuard::new(pi.hThread) {
            drop(thread_guard);
        }
        let process = HandleGuard::new(pi.hProcess)
            .ok_or_else(|| std::io::Error::other("null child process handle"))?;

        drop(stdin_r_guard);
        drop(stdout_w_guard);
        drop(nul_guard);

        // SAFETY: `stdin_w_guard` and `stdout_r_guard` own valid pipe handles whose ownership is
        // now transferred exclusively into `std::fs::File`.
        let stdin_file = unsafe { File::from_raw_handle(stdin_w_guard.take()) };
        let stdout_file = unsafe { File::from_raw_handle(stdout_r_guard.take()) };

        Ok(super::SpawnedWorker {
            process: super::WorkerProcess::Win32(Win32SpawnedChild { process }),
            stdin: Box::new(stdin_file),
            stdout: Box::new(stdout_file),
        })
    }

    pub(super) fn spawn_tracked(
        spec: &super::TrackedCommandSpec<'_>,
        cfg: &super::ProcessTrackerConfig,
        fail_after_spawn_for_test: bool,
    ) -> std::io::Result<(super::TrackedChild, super::TrackedChildGuard)> {
        use core::mem::{size_of, zeroed};
        use std::fs::File;
        use std::os::windows::io::FromRawHandle;

        let job_guard = create_kill_on_close_job()?;
        let canonical_db = cfg
            .db_path
            .as_deref()
            .map(super::resolve_stable_db_path)
            .transpose()?;

        let stdin_child = if spec.inherit_stdin {
            duplicate_inheritable_std_handle(STD_INPUT_HANDLE, GENERIC_READ)?
        } else {
            open_inheritable_nul(GENERIC_READ)?
        };
        let (stdout_parent, stdout_child) = if spec.pipe_stdout {
            let (r, w) = create_anonymous_pipe(true)?;
            (Some(r), w)
        } else {
            (
                None,
                duplicate_inheritable_std_handle(STD_OUTPUT_HANDLE, GENERIC_WRITE)?,
            )
        };
        let (stderr_parent, stderr_child) = create_anonymous_pipe(true)?;

        let attr_guard = build_handle_list_attrs(&[
            stdin_child.as_raw(),
            stdout_child.as_raw(),
            stderr_child.as_raw(),
        ])?;

        // SAFETY: `StartupInfoExW` is valid zeroed with `cb` set explicitly.
        let mut si_ex: StartupInfoExW = unsafe { zeroed() };
        si_ex.StartupInfo.cb = size_of::<StartupInfoExW>() as u32;
        si_ex.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si_ex.StartupInfo.hStdInput = stdin_child.as_raw();
        si_ex.StartupInfo.hStdOutput = stdout_child.as_raw();
        si_ex.StartupInfo.hStdError = stderr_child.as_raw();
        si_ex.lpAttributeList = attr_guard.ptr;

        let mut cmd_line = build_command_line(spec.binary, spec.args);
        let cwd_w = spec.working_dir.map(path_to_wide_nul);
        let cwd_ptr = cwd_w.as_ref().map_or(core::ptr::null(), Vec::as_ptr);

        let mut creation_flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED;
        if spec.create_no_window {
            creation_flags |= CREATE_NO_WINDOW;
        }

        // SAFETY: `ProcessInformation` is a plain out-struct initialized zeroed.
        let mut pi: ProcessInformation = unsafe { zeroed() };
        // SAFETY: strings are NUL-terminated UTF-16, `si_ex` restricts inherited handles, and
        // `CREATE_SUSPENDED` prevents child execution before Job Object + marker tracking exist.
        if unsafe {
            CreateProcessW(
                core::ptr::null(),
                cmd_line.as_mut_ptr(),
                core::ptr::null(),
                core::ptr::null(),
                1,
                creation_flags,
                core::ptr::null(),
                cwd_ptr,
                &si_ex,
                &mut pi,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }

        let thread_guard = HandleGuard::new(pi.hThread)
            .ok_or_else(|| std::io::Error::other("null child main thread handle"))?;
        let mut abort_guard = Win32AbortGuard {
            process: Some(
                HandleGuard::new(pi.hProcess)
                    .ok_or_else(|| std::io::Error::other("null child process handle"))?,
            ),
            marker_paths: None,
        };
        drop(attr_guard);
        drop(stdin_child);
        drop(stdout_child);
        drop(stderr_child);

        let process_raw = abort_guard
            .process
            .as_ref()
            .expect("process handle just initialized")
            .as_raw();

        // Assign Win32 Job Object and persist tracking record before resuming main thread.
        // SAFETY: `job_guard.0` and `process_raw` are valid handles for the job and suspended child.
        if unsafe { AssignProcessToJobObject(job_guard.0, process_raw) } == 0 {
            return Err(std::io::Error::last_os_error());
        }

        let child_started_at = super::query_win32_creation_marker(process_raw)
            .ok_or_else(|| std::io::Error::other("failed to query child start-time marker"))?;

        if let Some(db_path) = &canonical_db {
            let record = super::TrackedChildRecord {
                owner_pid: cfg.owner.pid,
                owner_process_started_at: cfg.owner.process_started_at.clone(),
                child_pid: pi.dwProcessId,
                child_process_started_at: Some(child_started_at),
                child_pgid: None,
            };
            abort_guard.marker_paths = Some(super::write_tracked_child(
                db_path,
                cfg.owner_token,
                &record,
            )?);
        }

        if fail_after_spawn_for_test {
            return Err(std::io::Error::other(
                "injected post-spawn tracking failure",
            ));
        }

        // SAFETY: `thread_guard` owns the suspended initial thread of `pi.hProcess`.
        if unsafe { ResumeThread(thread_guard.as_raw()) } == u32::MAX {
            return Err(std::io::Error::last_os_error());
        }
        drop(thread_guard);

        let (process, marker_paths) = abort_guard
            .disarm()
            .ok_or_else(|| std::io::Error::other("missing disarmed child process handle"))?;

        // SAFETY: parent pipe ends hold exclusive ownership transferred into `std::fs::File`.
        let stdout_file = stdout_parent.map(|h| unsafe { File::from_raw_handle(h.take()) });
        let stderr_file = unsafe { File::from_raw_handle(stderr_parent.take()) };

        let guard = match marker_paths {
            Some((record_path, owners_dir)) => super::TrackedChildGuard {
                record_path: Some(record_path),
                owners_dir: Some(owners_dir),
                _win32_job: Some(job_guard),
            },
            None => super::TrackedChildGuard {
                record_path: None,
                owners_dir: None,
                _win32_job: Some(job_guard),
            },
        };

        Ok((
            super::TrackedChild {
                inner: super::TrackedChildInner::Win32(Win32TrackedChild {
                    process,
                    stdout: stdout_file,
                    stderr: Some(stderr_file),
                }),
            },
            guard,
        ))
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
        pub fn TerminateProcess(h_process: *mut c_void, u_exit_code: u32) -> i32;
        pub fn GetProcessTimes(
            h_process: *mut c_void,
            lp_creation_time: *mut FileTime,
            lp_exit_time: *mut FileTime,
            lp_kernel_time: *mut FileTime,
            lp_user_time: *mut FileTime,
        ) -> i32;
        pub fn GetStdHandle(n_std_handle: u32) -> *mut c_void;
        pub fn SetStdHandle(n_std_handle: u32, h_handle: *mut c_void) -> i32;
        pub fn GetCurrentProcess() -> *mut c_void;
        pub fn DuplicateHandle(
            h_source_process_handle: *mut c_void,
            h_source_handle: *mut c_void,
            h_target_process_handle: *mut c_void,
            lp_target_handle: *mut *mut c_void,
            dw_desired_access: u32,
            b_inherit_handle: i32,
            dw_options: u32,
        ) -> i32;
        pub fn ResumeThread(h_thread: *mut c_void) -> u32;
        pub fn CreatePipe(
            h_read_pipe: *mut *mut c_void,
            h_write_pipe: *mut *mut c_void,
            lp_pipe_attributes: *mut SecurityAttributes,
            n_size: u32,
        ) -> i32;
        pub fn SetHandleInformation(h_object: *mut c_void, dw_mask: u32, dw_flags: u32) -> i32;
        pub fn CreateFileW(
            lp_file_name: *const u16,
            dw_desired_access: u32,
            dw_share_mode: u32,
            lp_security_attributes: *mut SecurityAttributes,
            dw_creation_disposition: u32,
            dw_flags_and_attributes: u32,
            h_template_file: *mut c_void,
        ) -> *mut c_void;
        pub fn InitializeProcThreadAttributeList(
            lp_attribute_list: *mut c_void,
            dw_attribute_count: u32,
            dw_flags: u32,
            lp_size: *mut usize,
        ) -> i32;
        pub fn UpdateProcThreadAttribute(
            lp_attribute_list: *mut c_void,
            dw_flags: u32,
            attribute: usize,
            lp_value: *mut c_void,
            cb_size: usize,
            lp_previous_value: *mut c_void,
            lp_return_size: *mut usize,
        ) -> i32;
        pub fn DeleteProcThreadAttributeList(lp_attribute_list: *mut c_void);
        pub fn CreateProcessW(
            lp_application_name: *const u16,
            lp_command_line: *mut u16,
            lp_process_attributes: *const c_void,
            lp_thread_attributes: *const c_void,
            b_inherit_handles: i32,
            dw_creation_flags: u32,
            lp_environment: *const c_void,
            lp_current_directory: *const u16,
            lp_startup_info: *const StartupInfoExW,
            lp_process_information: *mut ProcessInformation,
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

#[cfg(any(windows, test))]
fn push_windows_quoted_arg(out: &mut Vec<u16>, arg: &[u16]) {
    let quote = b'"' as u16;
    let space = b' ' as u16;
    let tab = b'\t' as u16;
    let backslash = b'\\' as u16;

    let needs_quotes = arg.is_empty() || arg.iter().any(|&u| u == space || u == tab || u == quote);
    if !needs_quotes {
        out.extend_from_slice(arg);
        return;
    }

    out.push(quote);
    let mut backslashes = 0usize;
    for &unit in arg {
        if unit == backslash {
            backslashes += 1;
        } else if unit == quote {
            for _ in 0..(backslashes * 2 + 1) {
                out.push(backslash);
            }
            out.push(quote);
            backslashes = 0;
        } else {
            for _ in 0..backslashes {
                out.push(backslash);
            }
            backslashes = 0;
            out.push(unit);
        }
    }
    for _ in 0..(backslashes * 2) {
        out.push(backslash);
    }
    out.push(quote);
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
        let nested_dir = dir.join("nested");
        fs::create_dir_all(&nested_dir).unwrap();
        let db_path = dir.join("siphon.db");
        fs::write(&db_path, b"").unwrap();
        let canonical_db = fs::canonicalize(&db_path).unwrap();
        let alias_path = nested_dir.join("..").join("siphon.db");

        #[cfg(unix)]
        let symlink_alias = {
            let link = nested_dir.join("alias.db");
            std::os::unix::fs::symlink(&db_path, &link).unwrap();
            link
        };
        #[cfg(windows)]
        let case_alias = dir.join("SIPHON.DB");

        let dead_owner = ProcessOwner::new(u32::MAX / 2, Some("dead".to_owned())).unwrap();
        let token = OwnershipToken::new(42).unwrap();
        let current = current_process_owner();

        // When owner died (e.g. foreground `siphon download --upload`) while child is still live,
        // liveness reports `Alive` across path aliases and preserves the canonical tracking file.
        write_tracked_child(
            &alias_path,
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
        #[cfg(unix)]
        assert_eq!(
            check_active_owner_liveness(
                &dead_owner,
                Some(&symlink_alias),
                Some(token),
                ExecutionMode::Foreground
            ),
            ProcessLiveness::Alive
        );
        #[cfg(windows)]
        assert_eq!(
            check_active_owner_liveness(
                &dead_owner,
                Some(&case_alias),
                Some(token),
                ExecutionMode::Foreground
            ),
            ProcessLiveness::Alive
        );
        assert!(owner_record_path_canonical(&canonical_db, token).is_file());

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
                Some(&alias_path),
                Some(token),
                ExecutionMode::Foreground
            ),
            ProcessLiveness::ConfirmedDead { .. }
        ));
        assert!(!owner_record_path_canonical(&canonical_db, token).exists());
        assert!(!owners_dir_for_canonical_db(&canonical_db).exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn spawn_tracked_command_fails_closed_when_tracking_setup_fails() {
        let dir = std::env::temp_dir().join(format!("siphon-track-fail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("siphon.db");
        fs::write(&db_path, b"").unwrap();
        let canonical_db = fs::canonicalize(&db_path).unwrap();

        let owner = current_process_owner();
        let exe = std::env::current_exe().unwrap();
        let args = [OsString::from("--list")];
        let spec = TrackedCommandSpec {
            binary: exe.as_os_str(),
            args: &args,
            working_dir: Some(&dir),
            inherit_stdin: false,
            pipe_stdout: true,
            create_no_window: true,
        };

        // 1. Blocked `.owners` directory path forces `write_tracked_child` failure.
        let blocked_owners = owners_dir_for_canonical_db(&canonical_db);
        fs::write(&blocked_owners, b"not-a-directory").unwrap();
        let cfg_blocked = ProcessTrackerConfig {
            db_path: Some(db_path.clone()),
            owner_token: OwnershipToken::new(7).unwrap(),
            owner: owner.clone(),
            execution_mode: ExecutionMode::Foreground,
        };
        assert!(spawn_tracked_command_inner(&spec, Some(&cfg_blocked), false).is_err());
        assert!(blocked_owners.is_file());
        fs::remove_file(&blocked_owners).unwrap();

        // 2. Post-spawn failure injection simulates Job/marker failure after OS child creation and
        //    verifies fail-closed termination, reap, and marker cleanup.
        let token_injected = OwnershipToken::new(8).unwrap();
        let cfg_injected = ProcessTrackerConfig {
            db_path: Some(db_path),
            owner_token: token_injected,
            owner,
            execution_mode: ExecutionMode::Foreground,
        };
        assert!(spawn_tracked_command_inner(&spec, Some(&cfg_injected), true).is_err());
        assert!(!owner_record_path_canonical(&canonical_db, token_injected).exists());
        assert!(!blocked_owners.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn windows_command_line_quoting_preserves_spaces_quotes_and_trailing_backslashes() {
        let quote = |s: &str| {
            let mut out = Vec::<u16>::new();
            let wide: Vec<u16> = s.encode_utf16().collect();
            push_windows_quoted_arg(&mut out, &wide);
            String::from_utf16(&out).unwrap()
        };
        assert_eq!(quote("simple"), "simple");
        assert_eq!(quote(""), "\"\"");
        assert_eq!(quote("C:\\My Dir\\"), "\"C:\\My Dir\\\\\"");
        assert_eq!(quote("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(quote("a\\b\\\\\"c"), "\"a\\b\\\\\\\\\\\"c\"");
    }
}
