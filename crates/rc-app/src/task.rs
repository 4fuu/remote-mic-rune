//! Helper task control: installing the elevated scheduled task (one UAC),
//! starting and stopping the helper through schtasks, and reading its status.
//!
//! Split in two:
//! - Pure, host-testable: argument builders and status-line parsers. These
//!   encode the exact command-line contracts with `schtasks` and
//!   `rc-helper.exe`.
//! - Windows-only execution: `CREATE_NO_WINDOW` on every console process, a
//!   wait timeout that kills hung children, and `ShellExecuteW` with the
//!   `runas` verb for the UAC prompts.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use rc_protocol::consts::TASK_NAME;

/// The helper binary, expected in the same directory as rc-app.exe.
pub const HELPER_EXE: &str = "rc-helper.exe";

/// Windows: do not flash a console window for background processes.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // used by the Windows spawn path
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How long a subprocess may run before we kill it and report a timeout.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// Command-line contracts (pure; unit-tested)
// ---------------------------------------------------------------------------

/// Arguments making rc-helper.exe register the scheduled task (run elevated
/// via `runas`, which triggers the single UAC prompt).
pub fn helper_install_args() -> &'static [&'static str] {
    &["--install-task"]
}

/// Arguments making rc-helper.exe unregister the task (also run elevated).
pub fn helper_remove_args() -> &'static [&'static str] {
    &["--remove-task"]
}

/// Arguments making rc-helper.exe print its task status line (no elevation).
pub fn helper_status_args() -> &'static [&'static str] {
    &["--task-status"]
}

/// `schtasks /run /tn <task>`: start the registered helper task. No UAC: the
/// task runs with its stored (elevated) credentials once registered.
pub fn schtasks_run_args(task: &str) -> Vec<String> {
    vec!["/run".into(), "/tn".into(), task.into()]
}

/// `schtasks /end /tn <task>`: stop a running task instance. The bridge pipe
/// keeps running; by design the helper exits when the app disconnects, and
/// `/end` just forces its task instance down.
pub fn schtasks_end_args(task: &str) -> Vec<String> {
    vec!["/end".into(), "/tn".into(), task.into()]
}

/// `schtasks /query /tn <task>`: the fallback status check when the helper
/// binary itself is not available.
pub fn schtasks_query_args(task: &str) -> Vec<String> {
    vec!["/query".into(), "/tn".into(), task.into()]
}

/// The program every schtasks invocation runs.
pub fn schtasks_program() -> &'static str {
    "schtasks.exe"
}

/// Status of the scheduled task as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    NotInstalled,
    Ready,
    Running,
    /// Something else: the raw line(s), trimmed.
    Unknown(String),
}

impl TaskStatus {
    pub fn label(&self) -> String {
        match self {
            TaskStatus::NotInstalled => "Task not installed".to_string(),
            TaskStatus::Ready => "Task installed, helper not running".to_string(),
            TaskStatus::Running => "Task running".to_string(),
            TaskStatus::Unknown(text) => format!("Task status: {text}"),
        }
    }
}

/// Parse `rc-helper.exe --task-status` output. The helper contract is one
/// short line containing one of `running`, `ready`/`installed`, or
/// `not installed`/`missing`/`absent`; anything else is shown verbatim.
pub fn parse_task_status(stdout: &str) -> TaskStatus {
    let line = stdout.trim();
    if line.is_empty() {
        return TaskStatus::Unknown("no output".to_string());
    }
    let lower = line.to_ascii_lowercase();
    if lower.contains("running") {
        TaskStatus::Running
    } else if lower.contains("ready") || lower.contains("installed, idle") || lower.trim() == "installed" {
        TaskStatus::Ready
    } else if lower.contains("not installed") || lower.contains("missing") || lower.contains("absent") {
        TaskStatus::NotInstalled
    } else {
        TaskStatus::Unknown(line.chars().take(64).collect())
    }
}

/// Parse `schtasks /query /tn <task>` output (fallback path).
pub fn parse_schtasks_query(stdout: &str) -> TaskStatus {
    let lower = stdout.to_ascii_lowercase();
    if lower.contains("the system cannot find") || lower.contains("does not exist") {
        return TaskStatus::NotInstalled;
    }
    for line in stdout.lines() {
        let line = line.trim();
        // Two output shapes: `Status: Running` and the table form
        // `RemoteMicRuneHelper                  Ready`, where the status is
        // the last word of the line.
        let value = match line.split_once(':') {
            Some((_, value)) => value.trim(),
            None if line.contains(' ') => line.rsplit_once(' ').map(|(_, word)| word.trim()).unwrap_or(""),
            None => continue,
        };
        match value.to_ascii_lowercase().as_str() {
            "running" => return TaskStatus::Running,
            "ready" => return TaskStatus::Ready,
            _ => {}
        }
    }
    TaskStatus::Unknown(stdout.trim().chars().take(64).collect())
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// Result of one captured subprocess run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// True when the child exceeded [`COMMAND_TIMEOUT`] and was killed.
    pub timed_out: bool,
}

impl RunOutput {
    pub fn ok(&self) -> bool {
        self.status == Some(0)
    }
}

/// Run a console program, hide its window, capture its output and enforce a
/// timeout. Pipes are drained by reader threads, so children that write more
/// than the OS pipe buffer cannot deadlock the wait.
pub fn run_capture(program: &str, args: &[String], timeout: Duration) -> Result<RunOutput, String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    hide_window(&mut cmd);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("cannot start {program}: {e}"))?;
    let stdout_rx = drain_pipe(child.stdout.take());
    let stderr_rx = drain_pipe(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                timed_out = true;
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(e) => return Err(format!("{program} failed: {e}")),
        }
    };
    // The pipes close when the child dies, so the readers finish on their own.
    let stdout = stdout_rx.recv().unwrap_or_default();
    let stderr = stderr_rx.recv().unwrap_or_default();
    Ok(RunOutput {
        status: status.and_then(|s| s.code()),
        stdout,
        stderr,
        timed_out,
    })
}

/// Read a pipe to the end on a helper thread; sends `""` when the pipe is
/// gone (killed child before spawn completed its pipes, etc).
fn drain_pipe<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_string(&mut text);
        }
        let _ = tx.send(text);
    });
    rx
}

fn hide_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// Path of rc-helper.exe next to the running app.
pub fn helper_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate the app executable: {e}"))?;
    let dir = exe.parent().ok_or("the app executable has no parent directory")?;
    let helper = dir.join(HELPER_EXE);
    if !helper.exists() {
        return Err(format!(
            "{} not found next to the app; install the helper build into the same directory",
            HELPER_EXE
        ));
    }
    Ok(helper)
}

/// Run one helper/schtasks command that needs no elevation.
pub fn run_plain(program: &str, args: &[String]) -> Result<RunOutput, String> {
    run_capture(program, args, COMMAND_TIMEOUT)
}

/// Install the scheduled task with exactly one UAC prompt
/// (`ShellExecuteW` "runas" on `rc-helper.exe --install-task`).
pub fn install_task(helper: &Path) -> Result<(), String> {
    run_elevated(helper, helper_install_args())
}

/// Remove the scheduled task (one UAC prompt, as removal needs elevation too).
pub fn remove_task(helper: &Path) -> Result<(), String> {
    run_elevated(helper, helper_remove_args())
}

/// Start the helper task without a prompt (`schtasks /run`).
pub fn start_task() -> Result<RunOutput, String> {
    let args = schtasks_run_args(TASK_NAME);
    run_plain(schtasks_program(), &args)
}

/// Stop the running task instance (`schtasks /end`). The bridge keeps
/// running; the helper also exits on app disconnect by design.
pub fn stop_task() -> Result<RunOutput, String> {
    let args = schtasks_end_args(TASK_NAME);
    run_plain(schtasks_program(), &args)
}

/// Ask the helper binary (or schtasks, as fallback) for the task status.
pub fn query_task_status() -> Result<TaskStatus, String> {
    if let Ok(helper) = helper_path() {
        let args: Vec<String> = helper_status_args().iter().map(|s| s.to_string()).collect();
        let out = run_plain(&helper.to_string_lossy(), &args)?;
        if out.ok() || !out.stdout.trim().is_empty() {
            return Ok(parse_task_status(&out.stdout));
        }
        return Ok(parse_task_status(&out.stderr));
    }
    let out = run_plain(schtasks_program(), &schtasks_query_args(TASK_NAME))?;
    if !out.ok() && out.stdout.trim().is_empty() {
        return Ok(TaskStatus::NotInstalled);
    }
    Ok(parse_schtasks_query(&out.stdout))
}

// ---------------------------------------------------------------------------
// Elevation (Windows)
// ---------------------------------------------------------------------------

/// Run a program elevated: `ShellExecuteW` with the `runas` verb, which
/// shows the UAC prompt and returns once the user answers it. Exit codes are
/// not available through ShellExecuteW; success is its "greater than 32"
/// contract.
#[cfg(windows)]
pub fn run_elevated(program: &Path, args: &[&str]) -> Result<(), String> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    let operation = wide(OsStr::new("runas"));
    let file = wide(program.as_os_str());
    let parameters = wide(OsStr::new(&args.join(" ")));
    const SW_SHOWNORMAL: i32 = 1;
    let result = unsafe {
        shell32::ShellExecuteW(
            0,
            operation.as_ptr(),
            file.as_ptr(),
            parameters.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    const SE_ERR_SUCCESS_MIN: isize = 33; // ShellExecuteW returns >32 on success
    if result >= SE_ERR_SUCCESS_MIN {
        Ok(())
    } else {
        Err(format!(
            "elevated launch failed (ShellExecuteW returned {result}); the UAC prompt may have been cancelled"
        ))
    }
}

#[cfg(windows)]
mod shell32 {
    #[link(name = "shell32")]
    extern "system" {
        pub fn ShellExecuteW(
            hwnd: isize,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show: i32,
        ) -> isize;
    }
}

/// Non-Windows stand-in: the buttons report clearly instead of pretending.
#[cfg(not(windows))]
pub fn run_elevated(_program: &Path, _args: &[&str]) -> Result<(), String> {
    Err("elevation (UAC) is only available on Windows".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_arg_sets_are_stable() {
        assert_eq!(helper_install_args(), &["--install-task"]);
        assert_eq!(helper_remove_args(), &["--remove-task"]);
        assert_eq!(helper_status_args(), &["--task-status"]);
    }

    #[test]
    fn schtasks_args_match_the_documented_commands() {
        assert_eq!(schtasks_run_args(TASK_NAME), ["/run", "/tn", "RemoteMicRuneHelper"]);
        assert_eq!(schtasks_end_args(TASK_NAME), ["/end", "/tn", "RemoteMicRuneHelper"]);
        assert_eq!(schtasks_query_args(TASK_NAME), ["/query", "/tn", "RemoteMicRuneHelper"]);
        assert_eq!(schtasks_program(), "schtasks.exe");
    }

    #[test]
    fn task_status_lines_are_recognized() {
        assert_eq!(parse_task_status("running"), TaskStatus::Running);
        assert_eq!(parse_task_status("READY"), TaskStatus::Ready);
        assert_eq!(parse_task_status("installed, idle"), TaskStatus::Ready);
        assert_eq!(parse_task_status("not installed"), TaskStatus::NotInstalled);
        assert_eq!(parse_task_status("missing"), TaskStatus::NotInstalled);
        assert_eq!(parse_task_status(""), TaskStatus::Unknown("no output".into()));
        assert_eq!(parse_task_status("weird state"), TaskStatus::Unknown("weird state".into()));
    }

    #[test]
    fn schtasks_query_output_is_parsed() {
        let ready = "\
\nTaskName                             Status\n\
==================================== =========\n\
RemoteMicRuneHelper                  Ready\n";
        assert_eq!(parse_schtasks_query(ready), TaskStatus::Ready);
        let running = "Status: Running\n";
        assert_eq!(parse_schtasks_query(running), TaskStatus::Running);
        let missing = "\
ERROR: The system cannot find the file specified.\n";
        assert_eq!(parse_schtasks_query(missing), TaskStatus::NotInstalled);
    }

    #[test]
    fn run_capture_plumbs_output_and_exit_code() {
        #[cfg(windows)]
        let (program, args) = ("cmd.exe", vec!["/c".to_string(), "echo hello".to_string()]);
        #[cfg(not(windows))]
        let (program, args) = ("/bin/echo", vec!["hello".to_string()]);
        let out = run_capture(program, &args, COMMAND_TIMEOUT).unwrap();
        assert!(out.ok());
        assert!(!out.timed_out);
        assert!(out.stdout.trim().contains("hello"), "stdout: {:?}", out.stdout);
    }

    #[test]
    fn run_capture_times_out_and_kills() {
        #[cfg(windows)]
        let (program, args) = ("cmd.exe", vec!["/c".to_string(), "ping -n 30 127.0.0.1".to_string()]);
        #[cfg(not(windows))]
        let (program, args) = ("/bin/sleep", vec!["30".to_string()]);
        let started = Instant::now();
        let out = run_capture(program, &args, Duration::from_millis(300)).unwrap();
        assert!(out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5), "the kill must happen promptly");
    }

    #[test]
    fn run_capture_reports_missing_programs_without_panicking() {
        let err = run_capture("rc-app-definitely-not-a-program", &[], Duration::from_secs(1)).unwrap_err();
        assert!(err.contains("cannot start"), "error: {err}");
    }
}
