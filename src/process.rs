//! Running a child process with a deadline and a size limit.
//!
//! Every external command this program starts — `systemctl`, `launchctl`,
//! `powershell.exe`, `secret-tool`, the reconnect worker — can hang or flood a
//! pipe, and the installed background task must not hang with it. So all of them
//! go through one supervisor that:
//!
//! * writes stdin, reads stdout and reads stderr **concurrently** with waiting,
//!   because reading after `wait` deadlocks as soon as the child fills a pipe;
//! * stops the whole process tree at the deadline: a new process group on Unix
//!   (`killpg`), a kill-on-close job object on Windows, so a helper the child
//!   started cannot outlive it;
//! * reaps the child after killing it, so no zombie is left behind;
//! * caps each captured stream, and stops the child rather than growing memory.
//!
//! On Windows the child is created **suspended** and only resumed once it is in
//! the job object: spawning first and assigning afterwards would let it do work
//! — including start a grandchild — that the job would then not own. On Unix the
//! child is placed in its own process group at creation for the same reason.
//!
//! A caller may also cancel: [`supervise`] polls a predicate between reads, which
//! is how `SIGINT` and "paused by another process" reach a running worker.

use std::io::{Read, Write};
use std::process::ExitStatus;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// How much of one stream is kept.
pub const OUTPUT_CAP: usize = 64 * 1024;

/// How long a platform command may take.
pub const COMMAND_LIMIT: Duration = Duration::from_secs(30);

/// How long a credential helper may take.
pub const CREDENTIAL_LIMIT: Duration = Duration::from_secs(5);

/// How often the supervisor checks for cancellation and the deadline.
const POLL: Duration = Duration::from_millis(50);

/// Why a child stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// It exited on its own.
    Exited(ExitStatus),
    /// The deadline passed and the supervisor killed it.
    TimedOut,
    /// The caller's predicate asked for it to stop.
    Cancelled,
    /// One stream reached [`OUTPUT_CAP`], so it was killed to stay bounded.
    Truncated,
}

/// What one supervised run produced.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub stop: Stop,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Outcome {
    /// Whether the child exited successfully on its own.
    pub fn succeeded(&self) -> bool {
        matches!(&self.stop, Stop::Exited(status) if status.success())
    }

    /// The exit status, when it exited.
    pub fn exit_code(&self) -> Option<i32> {
        match &self.stop {
            Stop::Exited(status) => status.code(),
            _ => None,
        }
    }

    /// A short, secret-free description for a log or an error message.
    pub fn describe_stop(&self) -> String {
        match &self.stop {
            Stop::Exited(status) => match status.code() {
                Some(code) => format!("exit status {code}"),
                None => "terminated by a signal".to_string(),
            },
            Stop::TimedOut => "timed out".to_string(),
            Stop::Cancelled => "cancelled".to_string(),
            Stop::Truncated => "produced too much output".to_string(),
        }
    }

    /// The captured stderr, trimmed and lossily decoded — for a diagnostic line,
    /// never for a persisted record of a secret.
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }

    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }
}

/// Failure to even start or supervise a child.
#[derive(Debug)]
pub struct ProcessError {
    pub program: String,
    pub message: String,
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.program, self.message)
    }
}

impl std::error::Error for ProcessError {}

/// Run `argv` to completion, or until `limit` elapses.
pub fn run(
    argv: &[String],
    input: Option<&[u8]>,
    limit: Duration,
) -> Result<Outcome, ProcessError> {
    supervise(argv, input, limit, &|| false)
}

/// Run `argv`, polling `cancel` while it runs. `cancel` returning `true` stops
/// the whole tree and reports [`Stop::Cancelled`].
pub fn supervise(
    argv: &[String],
    input: Option<&[u8]>,
    limit: Duration,
    cancel: &dyn Fn() -> bool,
) -> Result<Outcome, ProcessError> {
    let program = argv.first().cloned().unwrap_or_default();
    if program.is_empty() {
        return Err(ProcessError {
            program: "(none)".to_string(),
            message: "no program to run".to_string(),
        });
    }
    let mut child = platform::spawn(argv, input.is_some()).map_err(|err| ProcessError {
        program: program.clone(),
        message: err.to_string(),
    })?;

    let (tx, rx) = mpsc::channel::<Stream>();
    if let Some(mut stdin) = child.stdin.take() {
        if let Some(bytes) = input {
            let bytes = bytes.to_vec();
            std::thread::spawn(move || {
                // A child that exits early breaks the pipe; that is its business,
                // and the exit status says so.
                let _ = stdin.write_all(&bytes);
            });
        }
    }
    if let Some(stdout) = child.stdout.take() {
        spawn_reader(stdout, Kind::Stdout, tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_reader(stderr, Kind::Stderr, tx.clone());
    }
    drop(tx);

    let started = Instant::now();
    let mut captured = Captured::default();
    let mut stop: Option<Stop> = None;
    let mut exit: Option<ExitStatus> = None;
    loop {
        match rx.recv_timeout(POLL) {
            Ok(Stream::Data(kind, bytes)) => captured.push(kind, bytes),
            Ok(Stream::Done) => captured.finished += 1,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => captured.finished = 2,
        }
        if stop.is_none() && captured.overflowed {
            stop = Some(Stop::Truncated);
        }
        if stop.is_none() && cancel() {
            stop = Some(Stop::Cancelled);
        }
        if stop.is_none() && started.elapsed() >= limit {
            stop = Some(Stop::TimedOut);
        }

        match exit {
            Some(_) => {
                // The child is gone; the readers end when the pipes close.
                if captured.finished >= 2 {
                    break;
                }
            }
            None => {
                if stop.is_some() {
                    child.kill_tree();
                    exit = Some(child.wait().map_err(|err| ProcessError {
                        program: program.clone(),
                        message: err.to_string(),
                    })?);
                    continue;
                }
                if let Some(status) = child.try_wait().map_err(|err| ProcessError {
                    program: program.clone(),
                    message: err.to_string(),
                })? {
                    exit = Some(status);
                }
            }
        }
    }

    // The readers may still be draining what the kernel buffered before the
    // child died; take it, bounded, rather than waiting on them forever.
    while let Ok(message) = rx.recv_timeout(Duration::from_millis(200)) {
        match message {
            Stream::Data(kind, bytes) => captured.push(kind, bytes),
            Stream::Done => {}
        }
    }

    Ok(Outcome {
        stop: stop.unwrap_or_else(|| Stop::Exited(exit.unwrap_or_else(platform::unknown_status))),
        stdout: captured.stdout,
        stderr: captured.stderr,
    })
}

enum Stream {
    Data(Kind, Vec<u8>),
    Done,
}

#[derive(Clone, Copy)]
enum Kind {
    Stdout,
    Stderr,
}

fn spawn_reader(mut reader: Box<dyn Read + Send>, kind: Kind, tx: mpsc::Sender<Stream>) {
    std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(Stream::Data(kind, buffer[..n].to_vec())).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(Stream::Done);
    });
}

#[derive(Default)]
struct Captured {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    overflowed: bool,
    finished: usize,
}

impl Captured {
    fn push(&mut self, kind: Kind, bytes: Vec<u8>) {
        let target = match kind {
            Kind::Stdout => &mut self.stdout,
            Kind::Stderr => &mut self.stderr,
        };
        if target.len() >= OUTPUT_CAP {
            self.overflowed = true;
            return;
        }
        let room = OUTPUT_CAP - target.len();
        if bytes.len() > room {
            self.overflowed = true;
            target.extend_from_slice(&bytes[..room]);
        } else {
            target.extend_from_slice(&bytes);
        }
    }
}

/// The child, as the supervisor needs it: pipes, a non-blocking status check, a
/// whole-tree kill, and a blocking wait that reaps.
struct Child {
    stdin: Option<Box<dyn Write + Send>>,
    stdout: Option<Box<dyn Read + Send>>,
    stderr: Option<Box<dyn Read + Send>>,
    inner: platform::Inner,
}

impl Child {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.inner.try_wait()
    }

    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.inner.wait()
    }

    fn kill_tree(&mut self) {
        self.inner.kill_tree()
    }
}

#[cfg(unix)]
mod platform {
    use super::Child;
    use std::io::{Read, Write};
    use std::process::{Command, ExitStatus, Stdio};

    /// The child's own process group, so the whole tree can be killed.
    pub struct Inner {
        process: std::process::Child,
        pgid: i32,
    }

    pub fn spawn(argv: &[String], piped_stdin: bool) -> std::io::Result<Child> {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .stdin(if piped_stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Its own group at creation: a grandchild cannot escape the kill by
        // changing its own process group before the deadline fires.
        command.process_group(0);
        let mut process = command.spawn()?;
        let pgid = process.id() as i32;
        let stdin = process
            .stdin
            .take()
            .map(|stdin| Box::new(stdin) as Box<dyn Write + Send>);
        let stdout = process
            .stdout
            .take()
            .map(|stdout| Box::new(stdout) as Box<dyn Read + Send>);
        let stderr = process
            .stderr
            .take()
            .map(|stderr| Box::new(stderr) as Box<dyn Read + Send>);
        Ok(Child {
            stdin,
            stdout,
            stderr,
            inner: Inner { process, pgid },
        })
    }

    impl Inner {
        pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
            self.process.try_wait()
        }

        pub fn wait(&mut self) -> std::io::Result<ExitStatus> {
            self.process.wait()
        }

        pub fn kill_tree(&mut self) {
            // `SIGKILL` to the group: the shell, its children and any grandchild
            // that stayed in the group all go. `Child::wait` reaps the leader.
            unsafe {
                libc::killpg(self.pgid, libc::SIGKILL);
            }
        }
    }

    pub fn unknown_status() -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(9)
    }
}

#[cfg(windows)]
mod platform {
    use super::Child;
    use std::ffi::OsString;
    use std::io::{Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{FromRawHandle, RawHandle};
    use std::process::ExitStatus;
    use std::ptr;
    use windows_sys::Win32::Foundation::SetHandleInformation;
    use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, GetExitCodeProcess, ResumeThread, WaitForSingleObject, CREATE_NO_WINDOW,
        CREATE_SUSPENDED, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
    };

    /// One pipe: the parent's end stays with us, the child's end is inherited.
    fn pipe() -> std::io::Result<(HANDLE, HANDLE)> {
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: 1,
        };
        let mut read: HANDLE = ptr::null_mut();
        let mut write: HANDLE = ptr::null_mut();
        let ok = unsafe { CreatePipe(&mut read, &mut write, &mut attributes, 0) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((read, write))
    }

    fn hide(handle: HANDLE) {
        // The parent's end must not leak into the child, or the child could keep
        // its own stdout open and the reader would never see EOF.
        unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
    }

    fn wide(text: &str) -> Vec<u16> {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// The executable's command line, with every token quoted the way
    /// `CommandLineToArgvW` parses it.
    fn command_line(argv: &[String]) -> String {
        argv.iter()
            .map(|arg| crate::service::windows_arg(arg))
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub struct Inner {
        process: HANDLE,
        job: HANDLE,
        exit: Option<ExitStatus>,
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            unsafe {
                if !self.job.is_null() {
                    // Closing the job kills whatever is still in it; that is the
                    // property the limit is set for.
                    CloseHandle(self.job);
                }
                if !self.process.is_null() {
                    CloseHandle(self.process);
                }
            }
        }
    }

    pub fn spawn(argv: &[String], piped_stdin: bool) -> std::io::Result<Child> {
        let (out_read, out_write) = pipe()?;
        let (err_read, err_write) = pipe()?;
        hide(out_read);
        hide(err_read);
        let (child_in, parent_in) = if piped_stdin {
            let (read, write) = pipe()?;
            // The child inherits `read`; the parent keeps `write` and hides it,
            // so the child sees EOF exactly when the parent's copy closes.
            hide(write);
            (read, Some(write))
        } else {
            (ptr::null_mut(), None)
        };

        let join = |handle: HANDLE| -> std::io::Result<Box<dyn Read + Send>> {
            let file = unsafe { std::fs::File::from_raw_handle(handle as RawHandle) };
            Ok(Box::new(file))
        };

        let application = OsString::from(&argv[0]);
        let application = wide(&application.to_string_lossy());
        let mut command_line = command_line(argv).encode_utf16().collect::<Vec<u16>>();
        command_line.push(0);

        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        startup.dwFlags = STARTF_USESTDHANDLES;
        startup.hStdInput = child_in;
        startup.hStdOutput = out_write;
        startup.hStdError = err_write;
        let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

        let created = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                1,
                CREATE_SUSPENDED | CREATE_NO_WINDOW,
                ptr::null(),
                ptr::null(),
                &startup,
                &mut info,
            )
        };
        // The child's ends belong to the child now; closing ours is what lets the
        // readers see EOF when it exits.
        unsafe {
            CloseHandle(out_write);
            CloseHandle(err_write);
            if !child_in.is_null() {
                CloseHandle(child_in);
            }
        }
        if created == 0 {
            let err = unsafe { GetLastError() };
            unsafe {
                CloseHandle(out_read);
                CloseHandle(err_read);
            }
            return Err(std::io::Error::from_raw_os_error(err as i32));
        }

        let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        let mut limit: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limit.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = !job.is_null()
            && unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &limit as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            } != 0;
        let assigned = configured && unsafe { AssignProcessToJobObject(job, info.hProcess) } != 0;
        if !assigned {
            // The child has run nothing yet: it is still suspended. Terminate it
            // and reap before reporting, so a failure leaves no process behind.
            let err = std::io::Error::last_os_error();
            unsafe {
                TerminateJobObject(job, 1);
                windows_sys::Win32::System::Threading::TerminateProcess(info.hProcess, 1);
                WaitForSingleObject(info.hProcess, 5_000);
                CloseHandle(info.hThread);
                CloseHandle(info.hProcess);
                CloseHandle(job);
                CloseHandle(out_read);
                CloseHandle(err_read);
            }
            return Err(err);
        }
        unsafe {
            ResumeThread(info.hThread);
            CloseHandle(info.hThread);
        }

        // Move the parent's read ends into ordinary `File`s so the readers are
        // platform-neutral from here on.
        let stdout = join(out_read)?;
        let stderr = join(err_read)?;
        let stdin: Option<Box<dyn Write + Send>> = parent_in.map(|handle| {
            let file = unsafe { std::fs::File::from_raw_handle(handle as RawHandle) };
            Box::new(file) as Box<dyn Write + Send>
        });

        Ok(Child {
            stdin,
            stdout: Some(stdout),
            stderr: Some(stderr),
            inner: Inner {
                process: info.hProcess,
                job,
                exit: None,
            },
        })
    }

    impl Inner {
        pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
            if let Some(status) = self.exit {
                return Ok(Some(status));
            }
            let wait = unsafe { WaitForSingleObject(self.process, 0) };
            if wait != WAIT_OBJECT_0 {
                return Ok(None);
            }
            let mut code = 0u32;
            if unsafe { GetExitCodeProcess(self.process, &mut code) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let status = exit_status(code);
            self.exit = Some(status);
            Ok(Some(status))
        }

        pub fn wait(&mut self) -> std::io::Result<ExitStatus> {
            if let Some(status) = self.exit {
                return Ok(status);
            }
            let wait = unsafe { WaitForSingleObject(self.process, u32::MAX) };
            if wait != WAIT_OBJECT_0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut code = 0u32;
            if unsafe { GetExitCodeProcess(self.process, &mut code) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let status = exit_status(code);
            self.exit = Some(status);
            Ok(status)
        }

        pub fn kill_tree(&mut self) {
            if !self.job.is_null() {
                // Terminating the job takes every descendant with it; the
                // process handle then reports the exit.
                unsafe { TerminateJobObject(self.job, 1) };
            }
        }
    }

    fn exit_status(code: u32) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(code)
    }

    pub fn unknown_status() -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(1)
    }

    // Keeps the unused-import lint honest for the handle type without hiding a
    // genuine mistake: `INVALID_HANDLE_VALUE` is the sentinel `CreatePipe` never
    // returns on success, and the check documents that assumption.
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::Child;
    use std::process::ExitStatus;

    pub struct Child;

    pub fn spawn(argv: &[String], piped_stdin: bool) -> std::io::Result<Child> {
        let mut command = std::process::Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .stdin(if piped_stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.spawn()?;
        Ok(Child)
    }

    impl super::platform::Child {
        pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
            Ok(None)
        }

        pub fn wait(&mut self) -> std::io::Result<ExitStatus> {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "process supervision is not implemented for this target",
            ))
        }

        pub fn kill_tree(&mut self) {}
    }

    pub fn unknown_status() -> ExitStatus {
        use std::os::raw::c_int;
        let _ = c_int::default();
        unreachable!("no exit status on this target")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    #[cfg(unix)]
    #[test]
    fn a_finished_child_reports_its_output_and_status() {
        let run = run(
            &argv(&["/bin/sh", "-c", "echo out; echo err >&2; exit 3"]),
            None,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(run.exit_code(), Some(3));
        assert_eq!(run.stdout_text(), "out");
        assert_eq!(run.stderr_text(), "err");
        assert!(!run.succeeded(), "a non-zero exit is not success");
    }

    #[test]
    fn stdin_is_written_and_closed() {
        #[cfg(unix)]
        let parts = ["/bin/sh", "-c", "cat"];
        #[cfg(windows)]
        let parts = ["cmd.exe", "/C", "more"];
        let run = run(&argv(&parts), Some(b"payload\n"), Duration::from_secs(10)).unwrap();
        assert!(run.succeeded(), "{:?}", run.stop);
        assert_eq!(run.stdout_text(), "payload");
    }

    #[cfg(unix)]
    #[test]
    fn a_flooded_pipe_does_not_deadlock() {
        // Far more than a pipe buffer, and more than the capture cap.
        let run = run(
            &argv(&[
                "/bin/sh",
                "-c",
                "i=0; while [ $i -lt 200000 ]; do echo 0123456789012345678901234567890123456789; i=$((i+1)); done",
            ]),
            None,
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(run.stop, Stop::Truncated);
        assert_eq!(run.stdout.len(), OUTPUT_CAP);
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_tree_is_killed_at_the_deadline() {
        let started = Instant::now();
        // The shell starts a grandchild that would sleep for a minute.
        let run = run(
            &argv(&["/bin/sh", "-c", "sleep 60 & sleep 60"]),
            None,
            Duration::from_millis(200),
        )
        .unwrap();
        assert_eq!(run.stop, Stop::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deadline is enforced, not merely reported"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancelled_runs_report_cancelled() {
        let run = supervise(
            &argv(&["/bin/sh", "-c", "sleep 30"]),
            None,
            Duration::from_secs(30),
            &|| true,
        )
        .unwrap();
        assert_eq!(run.stop, Stop::Cancelled);
    }

    #[test]
    fn a_missing_program_is_an_error() {
        let err = run(
            &argv(&["/nonexistent/program-for-srun-portal-tests"]),
            None,
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(err.program.contains("nonexistent"), "{err}");
    }
}
