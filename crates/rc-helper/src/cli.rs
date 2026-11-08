//! Command line interface of the elevated helper.
//!
//! Parsing is pure so it is unit-tested on any host; nothing here touches
//! Windows APIs.

use std::path::PathBuf;

/// What the helper should do in this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// Register the one-UAC scheduled task (`schtasks /create /xml`).
    pub install_task: bool,
    /// Delete the scheduled task (`schtasks /delete /f`).
    pub remove_task: bool,
    /// Query the scheduled task state (`schtasks /query`, parse Running/Ready).
    pub task_status: bool,
    /// Inject into this pid instead of the auto-detected RC003 host.
    /// Use only when the user knows which host is the remote's.
    pub target_pid: Option<u32>,
    /// Path of the agent DLL. Default: `rc_agent.dll` next to the helper exe.
    pub agent_dll: Option<PathBuf>,
    /// Seconds to run; 0 = unlimited.
    pub duration_secs: u64,
    /// Tell the agent to run in observe mode: edges reported, nothing cleared.
    pub observe: bool,
    /// Tell the agent not to restore original report bytes after a touch.
    pub no_restore: bool,
    /// One extra clear-only usage for field verification (home 0x4A makes
    /// clearing observable). Must be a known button usage.
    pub canary_usage: Option<u16>,
    /// Override the shared/ambiguous host refusals. Use only when the user
    /// knows which remote (or host) is the intended one.
    pub force: bool,
    /// Scan the registry, print the verdict and the injection plan, exit.
    pub dry_run: bool,
    /// Log file path. Default: `%ProgramData%\RemoteMicRune\helper-<pid>.log`.
    pub log: Option<PathBuf>,
    /// Exit after the app link goes away (instead of retrying the descriptor).
    /// The scheduled task passes this flag.
    pub follow_app: bool,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            install_task: false,
            remove_task: false,
            task_status: false,
            target_pid: None,
            agent_dll: None,
            duration_secs: 0,
            observe: false,
            no_restore: false,
            canary_usage: None,
            force: false,
            dry_run: false,
            log: None,
            follow_app: false,
        }
    }
}

pub const USAGE: &str = "\
rc-helper — elevated helper for remote-mic-rune

Locates the WUDFHost.exe hosting the RC003 (VID_2717/PID_32B8), injects
rc_agent.dll into it, owns the 2 s lease that keeps the agent armed, and
bridges the agent to the app over the RemoteMicRune bridge pipe.

Usage:
  rc-helper [flags]

Flags:
  --install-task        Register the scheduled task (RunLevel Highest, no
                        trigger; the app starts it with `schtasks /run`).
                        Needs elevation; answers UAC once per install.
  --remove-task         Delete the scheduled task (needs elevation).
  --task-status         Query the scheduled task (no elevation needed).
  --target-pid <pid>    Inject into this pid, skipping auto-detection.
                        Overrides the shared-host refusal as well. Use only
                        when you know which host is the remote's.
  --agent-dll <path>    Agent DLL path (default: rc_agent.dll next to the exe).
  --duration <secs>     Exit after this many seconds; 0 = unlimited (default).
  --observe             Agent reports edges but never clears reports.
  --no-restore          Agent leaves modified report bytes unrestored.
  --canary-usage <hex>  Add ONE extra clear-only usage (e.g. 4a = home) so
                        report clearing is observable in the field.
  --force               Continue on shared/ambiguous host verdicts. Only when
                        you know which remote is the intended one; injecting
                        a shared host restarts/touches the user's other BLE
                        devices' host.
  --dry-run             Scan + verdict + plan, then exit (no injection).
  --log <path>          Log file (default %ProgramData%\\RemoteMicRune\\
                        helper-<pid>.log; ProgramData, not LOCALAPPDATA,
                        because a scheduled task runs a fresh session-0
                        context).
  --follow-app          Exit after the app link goes away (instead of
                        retrying the descriptor). Passed by the scheduled
                        task so the helper never outlives the app.

Exit codes:
  0  clean run / clean teardown (duration, signal, app gone, agent bye)
  1  task command failed or other generic failure
  2  bad command line
  3  refused: shared host (another device shares the WUDFHost) — see --force
  4  refused: ambiguous (two or more RC003 hosts found)
  5  refused: no live RC003 host (device missing or host not running)
  6  refused: agent DLL missing / wrong architecture
  7  injection failed
  12 agent never said hello (injected but link never authenticated)
";

/// Parse `std::env::args().skip(1)`-style arguments.
pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Args, String> {
    let mut out = Args::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let mut value_arg = |out: &mut Args, name: &str| -> Result<(), String> {
            let value = iter.next().ok_or_else(|| format!("{name} needs a value"))?;
            Ok(set_value(out, name, &value)?)
        };
        match arg.as_str() {
            "--install-task" => out.install_task = true,
            "--remove-task" => out.remove_task = true,
            "--task-status" => out.task_status = true,
            "--target-pid" => value_arg(&mut out, "--target-pid")?,
            "--agent-dll" => value_arg(&mut out, "--agent-dll")?,
            "--duration" => value_arg(&mut out, "--duration")?,
            "--observe" => out.observe = true,
            "--no-restore" => out.no_restore = true,
            "--canary-usage" => value_arg(&mut out, "--canary-usage")?,
            "--force" => out.force = true,
            "--dry-run" => out.dry_run = true,
            "--log" => value_arg(&mut out, "--log")?,
            "--follow-app" => out.follow_app = true,
            "--help" | "-h" => return Err(String::new()),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(out)
}

fn set_value(out: &mut Args, name: &str, value: &str) -> Result<(), String> {
    match name {
        "--target-pid" => {
            out.target_pid = Some(
                value
                    .parse::<u32>()
                    .map_err(|_| format!("{name}: {value:?} is not a pid"))?,
            );
        }
        "--agent-dll" => out.agent_dll = Some(PathBuf::from(value)),
        "--duration" => {
            out.duration_secs = value
                .parse::<u64>()
                .map_err(|_| format!("{name}: {value:?} is not a number of seconds"))?;
        }
        "--canary-usage" => {
            out.canary_usage = Some(parse_usage_hex(name, value)?);
        }
        "--log" => out.log = Some(PathBuf::from(value)),
        _ => unreachable!("set_value called with a flagless argument"),
    }
    Ok(())
}

/// Parse a hex usage, with or without an `0x` prefix.
pub fn parse_usage_hex(name: &str, value: &str) -> Result<u16, String> {
    let bare = value.trim().trim_start_matches("0x");
    let usage = u16::from_str_radix(bare, 16)
        .map_err(|_| format!("{name}: {value:?} is not a hex usage"))?;
    if usage == 0 {
        return Err(format!("{name}: usage 0 is the empty-slot marker"));
    }
    Ok(usage)
}

/// Exit codes, documented in [`USAGE`] and used by `main`.
pub mod exit {
    pub const OK: i32 = 0;
    pub const FAILURE: i32 = 1;
    pub const USAGE: i32 = 2;
    pub const SHARED_HOST: i32 = 3;
    pub const AMBIGUOUS: i32 = 4;
    pub const NO_HOST: i32 = 5;
    pub const BAD_DLL: i32 = 6;
    pub const INJECT_FAILED: i32 = 7;
    pub const NO_AGENT_HELLO: i32 = 12;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args, String> {
        parse_args(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_are_conservative() {
        let a = args(&[]).unwrap();
        assert_eq!(a, Args::default());
        assert!(!a.force);
        assert!(!a.observe);
        assert!(!a.follow_app);
        assert_eq!(a.duration_secs, 0);
    }

    #[test]
    fn flags_and_values_parse() {
        let a = args(&[
            "--target-pid",
            "4012",
            "--agent-dll",
            "C:\\temp\\rc_agent.dll",
            "--duration",
            "600",
            "--observe",
            "--no-restore",
            "--canary-usage",
            "4a",
            "--force",
            "--dry-run",
            "--log",
            "C:\\temp\\h.log",
            "--follow-app",
        ])
        .unwrap();
        assert_eq!(a.target_pid, Some(4012));
        assert_eq!(a.agent_dll, Some(PathBuf::from("C:\\temp\\rc_agent.dll")));
        assert_eq!(a.duration_secs, 600);
        assert!(a.observe);
        assert!(a.no_restore);
        assert_eq!(a.canary_usage, Some(0x4A));
        assert!(a.force);
        assert!(a.dry_run);
        assert_eq!(a.log, Some(PathBuf::from("C:\\temp\\h.log")));
        assert!(a.follow_app);
    }

    #[test]
    fn canary_accepts_0x_prefix_and_uppercase() {
        assert_eq!(parse_usage_hex("--canary-usage", "0x4A").unwrap(), 0x4A);
        assert_eq!(parse_usage_hex("--canary-usage", "F1").unwrap(), 0xF1);
        assert!(parse_usage_hex("--canary-usage", "zz").is_err());
        assert!(parse_usage_hex("--canary-usage", "0").is_err());
    }

    #[test]
    fn duration_zero_means_unlimited() {
        assert_eq!(args(&["--duration", "0"]).unwrap().duration_secs, 0);
        assert!(args(&["--duration", "-5"]).is_err());
        assert!(args(&["--duration", "abc"]).is_err());
    }

    #[test]
    fn missing_values_are_usage_errors() {
        assert!(args(&["--target-pid"]).is_err());
        assert!(args(&["--canary-usage"]).is_err());
        assert!(args(&["--log"]).is_err());
    }

    #[test]
    fn unknown_args_are_usage_errors() {
        assert!(args(&["--bogus"]).is_err());
        assert!(args(&["extra"]).is_err());
    }

    #[test]
    fn task_flags_parse() {
        let a = args(&["--install-task"]).unwrap();
        assert!(a.install_task);
        assert!(!a.remove_task);
        assert!(!a.task_status);
        let a = args(&["--remove-task", "--task-status"]).unwrap();
        assert!(a.remove_task && a.task_status);
    }
}
