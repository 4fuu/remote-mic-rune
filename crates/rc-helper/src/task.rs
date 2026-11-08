//! Scheduled task management: one UAC prompt at install, then the app starts
//! the elevated helper with `schtasks /run` — no further prompts.
//!
//! Registration uses `schtasks /create /f /tn <name> /xml <file>` with the
//! XML written to a temp file as UTF-16LE WITH BOM (plain UTF-8 fails; the
//! XML parser rejects it). The XML has NO trigger: the scheduler never fires
//! the task on its own, only `schtasks /run` does. RunLevel HighestAvailable
//! is what lets a normal-privilege app start an elevated helper. Battery
//! policies are explicitly disabled: the command-line form hardcodes
//! "stop on batteries" and would silently swallow starts on battery power,
//! and ExecutionTimeLimit PT0S removes the default 72-hour kill.

use std::path::{Path, PathBuf};

/// Scheduled task name (protocol constant, shared with the app).
pub const TASK_NAME: &str = rc_protocol::consts::TASK_NAME;

/// Arguments the task action carries: follow the app's lifetime, run forever
/// until the app (or a signal) ends the helper.
pub const TASK_ACTION_ARGS: &[&str] = &["--follow-app", "--duration", "0"];

/// Task state parsed from `schtasks /query` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Running,
    Ready,
    Unknown,
}

/// XML text escaping: `& < > "` all appear in Windows exe paths.
pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

/// `YYYY-MM-DDTHH:MM:SS`, the shape `schtasks` itself writes; the app uses a
/// changed `<Date>` as hard evidence that the task was re-registered.
/// Host-testable companion of the Windows `local_date_stamp` path:
/// converts Unix seconds to the schtasks date shape. Only used in
/// unit tests today; kept so the format contract stays pinned.
#[allow(dead_code)]
pub fn task_date_stamp_from_unix(unix_secs: u64) -> String {
    // Reuse the logger's civil-date math; avoids any external crate.
    let days = (unix_secs / 86_400) as i64;
    let secs_of_day = unix_secs % 86_400;
    // civil_from_days is private to logging; replicate the tiny conversion
    // here to keep module boundaries clean.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        year,
        m,
        d,
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

/// The task definition. Pure so its content is unit-tested everywhere it
/// matters (RunLevel, args, no triggers) on any host.
pub fn task_xml(exe: &Path, date_stamp: &str, description: &str) -> String {
    let exe = xml_escape(&exe.display().to_string());
    let args = TASK_ACTION_ARGS.join(" ");
    let description = xml_escape(description);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Date>{date_stamp}</Date>
    <Description>{description}</Description>
  </RegistrationInfo>
  <Triggers />
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>true</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>{args}</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}

/// Encode the XML the way `schtasks /create /xml` requires: UTF-16LE with a
/// byte-order mark.
pub fn task_xml_utf16le(xml: &str) -> Vec<u8> {
    let mut bytes: Vec<u8> = vec![0xFF, 0xFE];
    for unit in xml.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// Write the XML to a temp file (helper pid in the name avoids collisions).
pub fn write_task_xml_temp(xml: &str) -> std::io::Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("remote-mic-rune-task-{}.xml", std::process::id()));
    std::fs::write(&path, task_xml_utf16le(xml))?;
    Ok(path)
}

/// `schtasks /create /f /tn <name> /xml <file>`
pub fn task_create_args(xml_path: &Path) -> Vec<String> {
    vec![
        "/create".into(),
        "/f".into(),
        "/tn".into(),
        TASK_NAME.to_string(),
        "/xml".into(),
        xml_path.display().to_string(),
    ]
}

/// `schtasks /delete /f /tn <name>`
pub fn task_delete_args() -> Vec<String> {
    vec!["/delete".into(), "/f".into(), "/tn".into(), TASK_NAME.to_string()]
}

/// `schtasks /query /tn <name> /fo LIST`
pub fn task_query_args() -> Vec<String> {
    vec!["/query".into(), "/tn".into(), TASK_NAME.to_string(), "/fo".into(), "LIST".into()]
}

/// Parse `schtasks /query /fo LIST` output. English locales write
/// "Status: Running" / "Status: Ready"; a missing task makes schtasks exit
/// non-zero with "ERROR:" text, which the caller reports as None.
pub fn parse_task_status(output: &str) -> Option<TaskState> {
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Status:") {
            let rest = rest.trim().to_ascii_lowercase();
            if rest.contains("running") {
                return Some(TaskState::Running);
            }
            if rest.contains("ready") {
                return Some(TaskState::Ready);
            }
            return Some(TaskState::Unknown);
        }
    }
    // Localized fallback: bare token anywhere in the output.
    let lower = output.to_ascii_lowercase();
    if lower.contains("running") {
        Some(TaskState::Running)
    } else if lower.contains("ready") {
        Some(TaskState::Ready)
    } else {
        None
    }
}

#[cfg(target_os = "windows")]
pub mod windows_impl {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    use windows::Win32::System::SystemInformation::GetLocalTime;
    use windows::Win32::UI::Shell::IsUserAnAdmin;

    /// Subprocess creation flag: the helper is a GUI-subsystem binary with no
    /// console to inherit; without this flag Windows allocates a fresh
    /// console for `schtasks`, flashing a terminal window.
    pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub fn is_elevated() -> bool {
        unsafe { IsUserAnAdmin().as_bool() }
    }

    /// Local-time stamp for the RegistrationInfo date.
    pub fn local_date_stamp() -> String {
        let st = unsafe { GetLocalTime() };
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        )
    }

    /// Run one schtasks command with CREATE_NO_WINDOW and report the outcome.
    pub fn run_task_command(
        cmd_args: &[String],
        label: &str,
        logger: &crate::logging::Logger,
    ) -> bool {
        run_task_command_captured(cmd_args, label, logger).0
    }

    /// Same as `run_task_command` but also returns the combined output so
    /// callers can parse it (task status reporting).
    pub fn run_task_command_captured(
        cmd_args: &[String],
        label: &str,
        logger: &crate::logging::Logger,
    ) -> (bool, String) {
        match std::process::Command::new("schtasks")
            .args(cmd_args)
            .stdin(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let ok = output.status.success();
                logger.kv(
                    "[TASK]",
                    &[
                        ("action", label.to_string()),
                        ("task", TASK_NAME.to_string()),
                        ("ok", ok.to_string()),
                        ("argv", cmd_args.join(" ")),
                        (
                            "detail",
                            if ok {
                                stdout.lines().next().unwrap_or_default().trim().to_string()
                            } else {
                                format!("{} {}", stderr.trim(), stdout.trim()).trim().to_string()
                            },
                        ),
                    ],
                );
                (ok, format!("{stdout}{stderr}"))
            }
            Err(error) => {
                logger.error(&format!("cannot execute schtasks ({label}): {error}"));
                (false, String::new())
            }
        }
    }

    /// `--install-task`: needs elevation; writes the XML temp file and
    /// registers the task pointing at THIS exe.
    pub fn install_task(logger: &crate::logging::Logger) -> bool {
        if !is_elevated() {
            logger.error("--install-task needs administrator rights (UAC is answered once here)");
            return false;
        }
        let exe = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                logger.error(&format!("cannot resolve current exe: {error}"));
                return false;
            }
        };
        let xml = task_xml(
            &exe,
            &local_date_stamp(),
            "remote-mic-rune helper: registered and maintained by the app",
        );
        let xml_path = match write_task_xml_temp(&xml) {
            Ok(path) => path,
            Err(error) => {
                logger.error(&format!("cannot write task XML: {error}"));
                return false;
            }
        };
        let ok = run_task_command(&task_create_args(&xml_path), "install", logger);
        let _ = std::fs::remove_file(&xml_path);
        if ok {
            logger.kv(
                "[TASK-READY]",
                &[
                    ("task", TASK_NAME.to_string()),
                    ("exe", exe.display().to_string()),
                    (
                        "note",
                        "registered at highest run level; the app starts it via schtasks /run"
                            .to_string(),
                    ),
                ],
            );
        }
        ok
    }

    pub fn remove_task(logger: &crate::logging::Logger) -> bool {
        if !is_elevated() {
            logger.error("--remove-task needs administrator rights");
            return false;
        }
        run_task_command(&task_delete_args(), "remove", logger)
    }

    /// `--task-status`: query the task and report the parsed state. The
    /// parsing lives in the pure `parse_task_status` above.
    pub fn task_status(logger: &crate::logging::Logger) -> bool {
        let (ok, output) = run_task_command_captured(&task_query_args(), "status", logger);
        let state = parse_task_status(&output);
        let state_text = match state {
            Some(TaskState::Running) => "Running",
            Some(TaskState::Ready) => "Ready",
            Some(TaskState::Unknown) => "registered (unknown state)",
            None => "not registered",
        };
        logger.kv(
            "[TASK-STATUS]",
            &[("task", TASK_NAME.to_string()), ("state", state_text.to_string())],
        );
        println!("RemoteMicRuneHelper: {state_text}");
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\Program Files\RemoteMicRune\rc-helper.exe";

    #[test]
    fn xml_has_highest_run_level_and_right_args() {
        let xml = task_xml(
            Path::new(EXE),
            "2026-11-08T10:00:00",
            "remote-mic-rune helper: registered and maintained by the app",
        );
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("--follow-app --duration 0"));
        assert!(xml.contains(&format!("<Command>{}</Command>", xml_escape(EXE))));
        assert!(xml.contains("registered and maintained by the app"));
        assert!(xml.contains("<Date>2026-11-08T10:00:00</Date>"));
    }

    #[test]
    fn xml_has_no_triggers_and_battery_safe_settings() {
        let xml = task_xml(Path::new(EXE), "2026-11-08T10:00:00", "test desc");
        assert!(xml.contains("<Triggers />"), "no trigger: only schtasks /run starts it");
        assert!(!xml.contains("CalendarTrigger"));
        assert!(!xml.contains("TimeTrigger"));
        assert!(!xml.contains("BootTrigger"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
        assert!(xml.contains("<AllowStartOnDemand>true</AllowStartOnDemand>"));
        assert!(xml.contains(r#"encoding="UTF-16""#));
    }

    #[test]
    fn xml_escapes_ampersands_and_quotes() {
        let xml = task_xml(
            Path::new(r#"C:\a&b\rc "help".exe"#),
            "2026-11-08T10:00:00",
            "d<&>g",
        );
        assert!(xml.contains("C:\\a&amp;b\\rc &quot;help&quot;.exe"));
        assert!(xml.contains("d&lt;&amp;&gt;g"));
    }

    #[test]
    fn utf16le_encoding_carries_bom() {
        let bytes = task_xml_utf16le("<a>");
        assert_eq!(&bytes[..2], &[0xFF, 0xFE], "BOM is mandatory: plain UTF-8 is rejected");
        // "<a>" -> 3C 00 61 00 3E 00
        assert_eq!(&bytes[2..], &[0x3C, 0x00, 0x61, 0x00, 0x3E, 0x00]);
    }

    #[test]
    fn date_stamp_matches_schtasks_shape() {
        // 2026-11-08T00:00:00Z = 1791552000 (UTC); the stamp is local time,
        // so only the shape is asserted here.
        assert_eq!(task_date_stamp_from_unix(0), "1970-01-01T00:00:00");
        assert_eq!(task_date_stamp_from_unix(1_794_096_000), "2026-11-08T00:00:00");
        assert_eq!(
            parse_task_status("Status: Running").unwrap(),
            TaskState::Running
        );
    }

    #[test]
    fn status_parsing_running_ready_missing() {
        let query_out = "\
Folder: \\

TaskName                                 Status
---------------------------------------- -----------
RemoteMicRuneHelper                      Ready
";
        assert_eq!(parse_task_status(query_out), Some(TaskState::Ready));
        assert_eq!(
            parse_task_status("Status: Running"),
            Some(TaskState::Running)
        );
        assert_eq!(
            parse_task_status("ERROR: The system cannot find the file specified."),
            None
        );
        assert_eq!(parse_task_status(""), None);
    }

    #[test]
    fn schtasks_arg_shapes() {
        let xml_path = Path::new(r"C:\temp\t.xml");
        assert_eq!(
            task_create_args(xml_path),
            vec![
                "/create",
                "/f",
                "/tn",
                "RemoteMicRuneHelper",
                "/xml",
                r"C:\temp\t.xml"
            ]
        );
        assert_eq!(task_delete_args(), vec!["/delete", "/f", "/tn", "RemoteMicRuneHelper"]);
        assert_eq!(task_query_args(), vec!["/query", "/tn", "RemoteMicRuneHelper", "/fo", "LIST"]);
        assert_eq!(TASK_NAME, "RemoteMicRuneHelper");
    }

    #[test]
    fn task_action_args_match_contract() {
        assert_eq!(TASK_ACTION_ARGS, &["--follow-app", "--duration", "0"]);
    }
}
