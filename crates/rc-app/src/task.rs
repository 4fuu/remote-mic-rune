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
//!
//! The status labels and error strings take the UI's [`Lang`] so the app
//! shows them localized; output parsed from schtasks/rc-helper stays
//! whatever those tools printed (embedded verbatim into `Unknown`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use rc_protocol::consts::TASK_NAME;

use crate::i18n::{tr, tr_fmt, Lang};

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
    pub fn label(&self, lang: Lang) -> String {
        match self {
            TaskStatus::NotInstalled => tr(lang, "Task not installed").to_string(),
            TaskStatus::Ready => tr(lang, "Task installed, helper not running").to_string(),
            TaskStatus::Running => tr(lang, "Task running").to_string(),
            TaskStatus::Unknown(text) => tr_fmt!(lang, "Task status: {}", text),
        }
    }
}

/// Parse `rc-helper.exe --task-status` output. The helper contract is one
/// short line containing one of `running`, `ready`/`installed`, or
/// `not installed`/`missing`/`absent`; anything else is shown verbatim.
pub fn parse_task_status(lang: Lang, stdout: &str) -> TaskStatus {
    let line = stdout.trim();
    if line.is_empty() {
        return TaskStatus::Unknown(tr(lang, "no output").to_string());
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

/// Parse `schtasks /query /tn <task>` output (fallback path). Unrecognized
/// output is embedded verbatim — schtasks prints in the OS's language, not
/// the app's.
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
pub fn run_capture(
    lang: Lang,
    program: &str,
    args: &[String],
    timeout: Duration,
) -> Result<RunOutput, String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    hide_window(&mut cmd);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| tr_fmt!(lang, "cannot start {}: {}", program, e))?;
    let stdout_rx = drain_pipe(child.stdout.take());
    let stderr_rx = drain_pipe(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                // Kill the whole tree: on Windows the spawned command is
                // often a shell whose children inherit the output pipes, so
                // killing only the direct child leaves the pipe drains
                // blocked for as long as the grandchildren live.
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    let _ = Command::new("taskkill")
                        .args(["/PID", &child.id().to_string(), "/T", "/F"])
                        .creation_flags(CREATE_NO_WINDOW)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                }
                let _ = child.kill();
                let _ = child.wait();
                timed_out = true;
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(e) => return Err(tr_fmt!(lang, "{} failed: {}", program, e)),
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
pub fn helper_path(lang: Lang) -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|e| tr_fmt!(lang, "cannot locate the app executable: {}", e))?;
    let dir = exe
        .parent()
        .ok_or_else(|| tr(lang, "the app executable has no parent directory").to_string())?;
    let helper = dir.join(HELPER_EXE);
    if !helper.exists() {
        return Err(tr_fmt!(
            lang,
            "{} not found next to the app; install the helper build into the same directory",
            HELPER_EXE
        ));
    }
    Ok(helper)
}

/// Run one helper/schtasks command that needs no elevation.
pub fn run_plain(lang: Lang, program: &str, args: &[String]) -> Result<RunOutput, String> {
    run_capture(lang, program, args, COMMAND_TIMEOUT)
}

/// Install the scheduled task with exactly one UAC prompt
/// (`ShellExecuteW` "runas" on `rc-helper.exe --install-task`).
pub fn install_task(lang: Lang, helper: &Path) -> Result<(), String> {
    run_elevated(lang, helper, helper_install_args())
}

/// Remove the scheduled task (one UAC prompt, as removal needs elevation too).
pub fn remove_task(lang: Lang, helper: &Path) -> Result<(), String> {
    run_elevated(lang, helper, helper_remove_args())
}

/// Start the helper task without a prompt (`schtasks /run`).
pub fn start_task(lang: Lang) -> Result<RunOutput, String> {
    let args = schtasks_run_args(TASK_NAME);
    run_plain(lang, schtasks_program(), &args)
}

/// Stop the running task instance (`schtasks /end`). The bridge keeps
/// running; the helper also exits on app disconnect by design.
pub fn stop_task(lang: Lang) -> Result<RunOutput, String> {
    let args = schtasks_end_args(TASK_NAME);
    run_plain(lang, schtasks_program(), &args)
}

/// Ask the helper binary (or schtasks, as fallback) for the task status.
pub fn query_task_status(lang: Lang) -> Result<TaskStatus, String> {
    if let Ok(helper) = helper_path(lang) {
        let args: Vec<String> = helper_status_args().iter().map(|s| s.to_string()).collect();
        let out = run_plain(lang, &helper.to_string_lossy(), &args)?;
        if out.ok() || !out.stdout.trim().is_empty() {
            return Ok(parse_task_status(lang, &out.stdout));
        }
        return Ok(parse_task_status(lang, &out.stderr));
    }
    let out = run_plain(lang, schtasks_program(), &schtasks_query_args(TASK_NAME))?;
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
pub fn run_elevated(lang: Lang, program: &Path, args: &[&str]) -> Result<(), String> {
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
        Err(tr_fmt!(
            lang,
            "elevated launch failed (ShellExecuteW returned {}); the UAC prompt may have been cancelled",
            result
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
pub fn run_elevated(lang: Lang, _program: &Path, _args: &[&str]) -> Result<(), String> {
    Err(tr(lang, "elevation (UAC) is only available on Windows").to_string())
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
        assert_eq!(parse_task_status(Lang::En, "running"), TaskStatus::Running);
        assert_eq!(parse_task_status(Lang::En, "READY"), TaskStatus::Ready);
        assert_eq!(
            parse_task_status(Lang::En, "installed, idle"),
            TaskStatus::Ready
        );
        assert_eq!(
            parse_task_status(Lang::En, "not installed"),
            TaskStatus::NotInstalled
        );
        assert_eq!(
            parse_task_status(Lang::En, "missing"),
            TaskStatus::NotInstalled
        );
        assert_eq!(
            parse_task_status(Lang::En, ""),
            TaskStatus::Unknown("no output".into())
        );
        assert_eq!(
            parse_task_status(Lang::En, "weird state"),
            TaskStatus::Unknown("weird state".into())
        );
        // The no-output placeholder translates with the UI language.
        assert_eq!(
            parse_task_status(Lang::Zh, ""),
            TaskStatus::Unknown("无输出".into())
        );
        assert_eq!(
            TaskStatus::Unknown("weird state".into()).label(Lang::Zh),
            "任务状态：weird state"
        );
        assert_eq!(TaskStatus::Running.label(Lang::Zh), "任务运行中");
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
        let out = run_capture(Lang::En, program, &args, COMMAND_TIMEOUT).unwrap();
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
        let out = run_capture(Lang::En, program, &args, Duration::from_millis(300)).unwrap();
        assert!(out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5), "the kill must happen promptly");
    }

    #[test]
    fn run_capture_reports_missing_programs_without_panicking() {
        let err = run_capture(
            Lang::En,
            "rc-app-definitely-not-a-program",
            &[],
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(err.contains("cannot start"), "error: {err}");
        let err = run_capture(
            Lang::Zh,
            "rc-app-definitely-not-a-program",
            &[],
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(err.starts_with("无法启动"), "error: {err}");
    }
}
