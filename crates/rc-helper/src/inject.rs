//! Two-stage injection of `rc_agent.dll` into the RC003's WUDFHost.exe.
//!
//! Stage 1 (classic remote-thread DLL load):
//!   OpenProcess -> VirtualAllocEx -> WriteProcessMemory(wide DLL path)
//!   -> CreateRemoteThread(LoadLibraryW).
//!   Success is verified by enumerating the target's modules — NEVER by
//!   trusting the thread return value, because a repeated LoadLibrary of an
//!   already-loaded module returns the existing base (non-zero) and cannot
//!   distinguish "loaded now" from "was already there".
//!
//! Stage 2 (boot):
//!   Write an `AgentBootConfig` (pipe name + token + lease) into the target
//!   and CreateRemoteThread on the DLL's exported `rc_agent_boot(config_ptr)`.
//!   The export's address is computed as remote_base + rva, where the rva
//!   comes from loading the DLL locally without resolving references.
//!
//! Every failure path carries the GetLastError number in its message: once
//! OpenProcess/CreateRemoteThread fail, that code is all the field has.

use std::path::Path;

use rc_protocol::handoff::AgentBootConfig;

/// PE machine field values (IMAGE_FILE_MACHINE_*).
pub const IMAGE_FILE_MACHINE_I386: u16 = 0x014C;
pub const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
pub const IMAGE_FILE_MACHINE_ARM64: u16 = 0xAA64;

/// Module name prefixes that identify a resident agent copy. The check is
/// name-prefix based (not exact path based) so the reconnect flow also finds
/// an agent whose DLL path differs in case or spelling from ours. The
/// artifact builds as `rc_agent.dll` — Rust lib targets underscore the
/// crate name — so BOTH the dash form (per-copy renamed deployments) and
/// the underscore form (the built artifact) must match; a dash-only match
/// rejects the DLL the helper itself just injected.
pub const AGENT_MODULE_PREFIX: &str = "rc-agent";
pub const AGENT_MODULE_PREFIX_UNDERSCORE: &str = "rc_agent";

/// Human-readable machine name for logs and error text.
pub fn machine_name(machine: u16) -> &'static str {
    match machine {
        IMAGE_FILE_MACHINE_AMD64 => "x64",
        IMAGE_FILE_MACHINE_ARM64 => "arm64",
        IMAGE_FILE_MACHINE_I386 => "x86",
        _ => "unknown",
    }
}

/// Parse the `machine` field of a PE file from its first bytes:
/// `MZ` -> `e_lfanew` (0x3C) -> `PE\0\0` -> FileHeader.Machine.
/// Pure: the real-device matrix (x64 helper vs ARM64 host etc.) can never be
/// fully covered by on-device tests, so every combination is unit-tested.
pub fn pe_machine_from_bytes(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < 0x40 || &bytes[0..2] != b"MZ" {
        return None;
    }
    let e_lfanew =
        u32::from_le_bytes([bytes[0x3C], bytes[0x3D], bytes[0x3E], bytes[0x3F]]) as usize;
    let sig_end = e_lfanew.checked_add(6)?;
    if bytes.len() < sig_end || &bytes[e_lfanew..e_lfanew + 4] != b"PE\0\0" {
        return None;
    }
    Some(u16::from_le_bytes([bytes[e_lfanew + 4], bytes[e_lfanew + 5]]))
}

/// True when a target/dll machine pair passes the injection arch gate.
/// The helper targets WUDFHost.exe which is x64 on the supported platforms;
/// anything else readable must be refused BEFORE OpenProcess, because past
/// that point failures surface only as a bare GetLastError that looks like
/// "antivirus blocked injection" and misdirects the investigation.
pub fn arch_gate(target: u16, dll: u16) -> Result<(), String> {
    if target != IMAGE_FILE_MACHINE_AMD64 {
        return Err(format!(
            "target host is {} (machine=0x{target:04X}), not x64; refusing to inject \
             (a non-x64 host cannot load the x64 agent)",
            machine_name(target)
        ));
    }
    if dll != IMAGE_FILE_MACHINE_AMD64 {
        return Err(format!(
            "agent DLL is {} (machine=0x{dll:04X}), not x64; rebuild rc_agent.dll \
             for x64 or pass the matching --agent-dll",
            machine_name(dll)
        ));
    }
    Ok(())
}

/// Does a loaded module belong to the agent? Name-prefix match, case-insensitive.
pub fn module_is_agent(module_name_or_path: &str) -> bool {
    let lower = module_name_or_path.to_ascii_lowercase();
    let file_name = lower.rsplit(['\\', '/']).next().unwrap_or(&lower);
    let matches = |prefix: &str| file_name.starts_with(prefix) && file_name.ends_with(".dll");
    matches(AGENT_MODULE_PREFIX) || matches(AGENT_MODULE_PREFIX_UNDERSCORE)
}

/// Build the boot config handed to `rc_agent_boot`.
pub fn build_boot_config(
    pipe_name: &str,
    token: &str,
    build_id: &str,
) -> Result<AgentBootConfig, String> {
    AgentBootConfig::new(pipe_name, token, build_id)
}

/// Build id reported in HELLO and stamped into the boot config. Ties agent
/// and helper builds together for stale-agent detection.
pub fn helper_build_id() -> String {
    format!("rc-helper/{}", env!("CARGO_PKG_VERSION"))
}

// ---------------------------------------------------------------------------
// Windows plumbing
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::*;
    use std::ffi::c_void;
    use windows::core::{PCSTR, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, WAIT_OBJECT_0};
    // WriteProcessMemory lives in the Debug namespace in windows-rs.
    use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W,
        TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32,
    };
    // FreeLibrary is in Foundation; the LoadLibraryEx family lives in
    // System::LibraryLoader.
    use windows::Win32::Foundation::FreeLibrary;
    use windows::Win32::System::LibraryLoader::{
        DONT_RESOLVE_DLL_REFERENCES, GetModuleHandleW, GetProcAddress, LoadLibraryExW,
    };
    use windows::Win32::System::Memory::{
        VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
    };
    use windows::Win32::System::Threading::{
        CreateRemoteThread, GetExitCodeThread, OpenProcess, QueryFullProcessImageNameW,
        WaitForSingleObject, LPTHREAD_START_ROUTINE, PROCESS_NAME_WIN32, PROCESS_ALL_ACCESS,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    const WAIT_TIMEOUT_MS: u32 = 15_000;

    fn win32_code(err: &windows::core::Error) -> u32 {
        // Win32-origin failures come back as HRESULT 0x8007xxxx; the low
        // word is the GetLastError value worth logging.
        (err.code().0 as u32) & 0xFFFF
    }

    fn raw_last_error() -> u32 {
        unsafe { GetLastError().0 }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn trim_wide(buf: &[u16]) -> String {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }

    /// Read the PE header bytes of a file (the machine field lives in the
    /// first KiB; whole-image reads are unnecessary).
    pub fn pe_machine_of_file(path: &Path) -> Option<u16> {
        let mut file = std::fs::File::open(path).ok()?;
        let mut buf = [0u8; 4096];
        let mut filled = 0usize;
        while filled < buf.len() {
            match std::io::Read::read(&mut file, &mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(_) => return None,
            }
        }
        pe_machine_from_bytes(&buf[..filled])
    }

    /// Enable SeDebugPrivilege in this process's own token. WUDFHost runs
    /// under a service account in session 0: an elevated admin still cannot
    /// open it with PROCESS_VM_* rights until the held-but-disabled debug
    /// privilege is enabled — without this, the very first module snapshot
    /// or OpenProcess fails with access denied (GetLastError=5). One call
    /// at startup covers the boot injection and every recovery re-inject.
    pub fn enable_debug_privilege() -> Result<(), String> {
        use windows::Win32::Foundation::LUID;
        use windows::Win32::Security::{
            AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES,
            SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
        };
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        const ERROR_NOT_ALL_ASSIGNED: u32 = 1300;
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            )
            .map_err(|e| format!("OpenProcessToken failed, GetLastError={}", win32_code(&e)))?;
            let name = wide("SeDebugPrivilege");
            let mut luid = LUID::default();
            LookupPrivilegeValueW(None, PCWSTR(name.as_ptr()), &mut luid).map_err(|e| {
                format!(
                    "LookupPrivilegeValueW(SeDebugPrivilege) failed, GetLastError={}",
                    win32_code(&e)
                )
            })?;
            let privileges = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            let adjusted =
                AdjustTokenPrivileges(token, false, Some(&privileges), 0, None, None);
            let _ = CloseHandle(token);
            adjusted.map_err(|e| {
                format!("AdjustTokenPrivileges failed, GetLastError={}", win32_code(&e))
            })?;
            // AdjustTokenPrivileges reports success even when the privilege
            // is not held; ERROR_NOT_ALL_ASSIGNED is the real verdict.
            if raw_last_error() == ERROR_NOT_ALL_ASSIGNED {
                return Err("SeDebugPrivilege not held (helper not elevated?)".into());
            }
            Ok(())
        }
    }

    /// One loaded module inside the target process.
    #[derive(Debug, Clone)]
    pub struct ModuleInfo {
        pub name: String,
        pub path: String,
        pub base: usize,
    }

    /// Enumerate the modules of a process (needs elevation: hosts live in
    /// session 0). The ONLY trustworthy evidence that a DLL is resident.
    pub fn enum_modules(pid: u32) -> Result<Vec<ModuleInfo>, String> {
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid)
                .map_err(|e| {
                    format!(
                        "CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, {pid}) failed, GetLastError={}",
                        win32_code(&e)
                    )
                })?;
            let mut entry = MODULEENTRY32W::default();
            entry.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
            let mut out = Vec::new();
            let mut ok = Module32FirstW(snap, &mut entry).is_ok();
            while ok {
                out.push(ModuleInfo {
                    name: trim_wide(&entry.szModule),
                    path: trim_wide(&entry.szExePath),
                    base: entry.modBaseAddr as usize,
                });
                ok = Module32NextW(snap, &mut entry).is_ok();
            }
            let _ = CloseHandle(snap);
            if out.is_empty() {
                return Err(format!(
                    "module enumeration returned an empty list (pid={pid}, GetLastError={})",
                    raw_last_error()
                ));
            }
            Ok(out)
        }
    }

    /// All agent modules resident in `pid`.
    pub fn agent_modules(pid: u32) -> Result<Vec<ModuleInfo>, String> {
        Ok(enum_modules(pid)?
            .into_iter()
            .filter(|m| module_is_agent(&m.name) || module_is_agent(&m.path))
            .collect())
    }

    /// Is an agent copy already resident? Reconnect flow instead of re-inject.
    pub fn is_agent_loaded(pid: u32) -> Result<bool, String> {
        agent_modules(pid).map(|m| !m.is_empty())
    }

    /// Full image path of a process (PROCESS_QUERY_LIMITED_INFORMATION).
    pub fn process_image_path(pid: u32) -> Option<String> {
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; 1024];
            let mut size = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buf.as_mut_ptr()),
                &mut size,
            );
            let _ = CloseHandle(handle);
            ok.ok()?;
            Some(trim_wide(&buf[..size as usize]))
        }
    }

    /// Arch preflight: log both sides and refuse on a readable mismatch.
    /// An unreadable image does not block (fail-open with a warning) because
    /// some environments deny the query on system processes; refusing then
    /// would break the working x64 path too.
    pub fn arch_preflight(
        pid: u32,
        dll: &Path,
        logger: &crate::logging::Logger,
    ) -> Result<(), String> {
        let target = process_image_path(pid)
            .and_then(|p| pe_machine_of_file(Path::new(&p)).map(|m| (p, m)));
        let dll_machine = pe_machine_of_file(dll);
        // Borrow `target` in the scrutinee so the fallthrough arm can still
        // render the partial evidence.
        match (target.as_ref(), dll_machine) {
            (Some((path, target_machine)), Some(dll_machine)) => {
                logger.kv(
                    "[ARCH]",
                    &[
                        (
                            "target",
                            format!("{} 0x{target_machine:04X}", machine_name(*target_machine)),
                        ),
                        ("dll", format!("{} 0x{dll_machine:04X}", machine_name(dll_machine))),
                        ("image", path.clone()),
                    ],
                );
                arch_gate(*target_machine, dll_machine)
            }
            _ => {
                logger.kv(
                    "[ARCH]",
                    &[
                        (
                            "target",
                            target
                                .as_ref()
                                .map(|(_, m)| format!("{} 0x{m:04X}", machine_name(*m)))
                                .unwrap_or_else(|| "unreadable".into()),
                        ),
                        (
                            "dll",
                            dll_machine
                                .map(|m| format!("{} 0x{m:04X}", machine_name(m)))
                                .unwrap_or_else(|| "unreadable".into()),
                        ),
                        (
                            "note",
                            "arch check incomplete (image or dll header unreadable); continuing"
                                .into(),
                        ),
                    ],
                );
                Ok(())
            }
        }
    }

    fn open_target(pid: u32) -> Result<HANDLE, String> {
        unsafe {
            OpenProcess(PROCESS_ALL_ACCESS, false, pid).map_err(|e| {
                format!(
                    "OpenProcess(PROCESS_ALL_ACCESS, {pid}) failed, GetLastError={}. \
                     The host lives in session 0; this almost always means the helper is not elevated.",
                    win32_code(&e)
                )
            })
        }
    }

    /// Run `start(param)` in the target and wait for completion.
    unsafe fn run_remote(
        process: HANDLE,
        start: LPTHREAD_START_ROUTINE,
        param: *mut c_void,
        what: &str,
        logger: &crate::logging::Logger,
    ) -> Result<(), String> {
        let thread = CreateRemoteThread(process, None, 0, start, Some(param), 0, None).map_err(
            |e| {
                format!(
                    "CreateRemoteThread({what}) failed, GetLastError={}. Arch was verified \
                     before injection; if this still fails, antivirus interception is the \
                     most common cause.",
                    win32_code(&e)
                )
            },
        )?;
        let wait = WaitForSingleObject(thread, WAIT_TIMEOUT_MS);
        let mut exit_code: u32 = 0;
        let _ = GetExitCodeThread(thread, &mut exit_code);
        logger.kv(
            "[REMOTE]",
            &[
                ("call", what.to_string()),
                ("wait", format!("0x{:08X}", wait.0)),
                ("return", format!("0x{exit_code:08X}")),
            ],
        );
        if wait != WAIT_OBJECT_0 {
            let _ = CloseHandle(thread);
            return Err(format!("{what} did not finish within {WAIT_TIMEOUT_MS} ms"));
        }
        let _ = CloseHandle(thread);
        Ok(())
    }

    fn write_remote(process: HANDLE, remote: *mut c_void, bytes: &[u8], what: &str) -> Result<(), String> {
        let mut written: usize = 0;
        unsafe {
            WriteProcessMemory(
                process,
                remote,
                bytes.as_ptr().cast::<c_void>(),
                bytes.len(),
                Some(&mut written),
            )
            .map_err(|e| {
                format!("WriteProcessMemory({what}) failed, GetLastError={}", win32_code(&e))
            })?;
        }
        if written != bytes.len() {
            return Err(format!(
                "WriteProcessMemory({what}) wrote {written} of {} bytes",
                bytes.len()
            ));
        }
        Ok(())
    }

    /// Stage 1+2 for a fresh target: LoadLibraryW then rc_agent_boot.
    /// Every remote allocation is freed on both success and error paths.
    pub fn inject(
        pid: u32,
        dll: &Path,
        pipe_name: &str,
        token: &str,
        build_id: &str,
        logger: &crate::logging::Logger,
    ) -> Result<(), String> {
        arch_preflight(pid, dll, logger)?;

        if is_agent_loaded(pid)? {
            // Do not load a second image; boot the resident copy with our
            // fresh config (the newest-wins server kicks the old session).
            logger.info(&format!(
                "agent module already resident in pid {pid}; reusing it (reconnect flow)"
            ));
            return boot_existing(pid, dll, pipe_name, token, build_id, logger);
        }

        let dll_str = dll
            .to_str()
            .filter(|s| s.is_ascii())
            .ok_or_else(|| "agent dll path is not valid UTF-8/ASCII".to_string())?
            .to_string();
        let process = open_target(pid)?;

        let result = unsafe {
            let wide_path = wide(&dll_str);
            let size = wide_path.len() * std::mem::size_of::<u16>();

            let remote_path =
                VirtualAllocEx(process, None, size, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
            if remote_path.is_null() {
                return Err(format!("VirtualAllocEx failed, GetLastError={}", raw_last_error()));
            }

            let outcome = (|| {
                write_remote(process, remote_path, wide_path_as_bytes(&wide_path), "dll path")?;

                let kernel32 = wide("kernel32.dll");
                let module = GetModuleHandleW(PCWSTR(kernel32.as_ptr()))
                    .map_err(|_| "GetModuleHandleW(kernel32.dll) failed".to_string())?;
                let load_library: unsafe extern "system" fn() -> isize =
                    GetProcAddress(module, PCSTR::from_raw(b"LoadLibraryW\0".as_ptr()))
                        .ok_or_else(|| "GetProcAddress(LoadLibraryW) failed".to_string())?;
                let start: LPTHREAD_START_ROUTINE =
                    Some(std::mem::transmute::<
                        unsafe extern "system" fn() -> isize,
                        unsafe extern "system" fn(*mut c_void) -> u32,
                    >(load_library));

                run_remote(process, start, remote_path, "LoadLibraryW", logger)?;

                // Verify residency by module enumeration; the thread return
                // value cannot distinguish "loaded now" from "already loaded".
                let agents = agent_modules(pid)?;
                if agents.is_empty() {
                    return Err(format!(
                        "injection verification failed: pid {pid} module list has no rc-agent \
                         module after LoadLibraryW; injection did not take effect"
                    ));
                }
                logger.kv(
                    "[VERIFY-MODULE]",
                    &[
                        ("agent_modules_in_host", agents.len().to_string()),
                        ("base", format!("0x{:X}", agents[0].base)),
                    ],
                );

                boot_with_handle(process, pid, dll, pipe_name, token, build_id, logger)
            })();

            let _ = VirtualFreeEx(process, remote_path, 0, MEM_RELEASE);
            outcome
        };
        unsafe {
            let _ = CloseHandle(process);
        }
        result
    }

    fn wide_path_as_bytes(wide: &[u16]) -> &[u8] {
        unsafe { std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2) }
    }

    /// Boot an ALREADY resident agent with fresh config (reconnect flow).
    pub fn boot_existing(
        pid: u32,
        dll: &Path,
        pipe_name: &str,
        token: &str,
        build_id: &str,
        logger: &crate::logging::Logger,
    ) -> Result<(), String> {
        let process = open_target(pid)?;
        let result =
            unsafe { boot_with_handle(process, pid, dll, pipe_name, token, build_id, logger) };
        unsafe {
            let _ = CloseHandle(process);
        }
        result
    }

    /// Compute the remote address of the `rc_agent_boot` export:
    /// remote_base + (local_export - local_base), using a local
    /// DONT_RESOLVE_DLL_REFERENCES load to obtain the export rva. The
    /// resident module's base address comes from the snapshot, so no
    /// process handle is needed here.
    unsafe fn remote_boot_address(pid: u32, dll: &Path) -> Result<usize, String> {
        let agents = agent_modules(pid)?;
        let resident = agents
            .iter()
            .find(|m| {
                dll.to_str()
                    .map(|d| m.path.eq_ignore_ascii_case(d))
                    .unwrap_or(false)
            })
            .or_else(|| agents.first());
        let Some(resident) = resident else {
            return Err(format!("no rc-agent module resident in pid {pid}; boot cannot proceed"));
        };
        let remote_base = resident.base;

        let dll_str = dll.to_str().ok_or("agent dll path not valid UTF-8")?;
        let wide_path = wide(dll_str);
        let local = LoadLibraryExW(PCWSTR(wide_path.as_ptr()), None, DONT_RESOLVE_DLL_REFERENCES)
            .map_err(|e| {
            format!(
                "LoadLibraryExW(dll, DONT_RESOLVE_DLL_REFERENCES) failed, GetLastError={}",
                win32_code(&e)
            )
        })?;
        let boot_export = GetProcAddress(local, PCSTR::from_raw(b"rc_agent_boot\0".as_ptr()));
        let rva = boot_export.and_then(|proc_addr| {
            let addr = proc_addr as usize;
            let local_base = local.0 as usize;
            addr.checked_sub(local_base)
        });
        let _ = FreeLibrary(local);
        let Some(rva) = rva else {
            return Err("rc_agent.dll does not export rc_agent_boot (stale dll?)".to_string());
        };
        remote_base.checked_add(rva).ok_or_else(|| "boot address overflow".to_string())
    }

    unsafe fn boot_with_handle(
        process: HANDLE,
        pid: u32,
        dll: &Path,
        pipe_name: &str,
        token: &str,
        build_id: &str,
        logger: &crate::logging::Logger,
    ) -> Result<(), String> {
        let remote_boot = remote_boot_address(pid, dll)?;

        let config = build_boot_config(pipe_name, token, build_id)?;
        let config_bytes = config_to_bytes(&config);
        let remote_config = VirtualAllocEx(
            process,
            None,
            config_bytes.len(),
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        if remote_config.is_null() {
            return Err(format!(
                "VirtualAllocEx(boot config) failed, GetLastError={}",
                raw_last_error()
            ));
        }

        let write = write_remote(process, remote_config, &config_bytes, "boot config");
        if let Err(e) = write {
            let _ = VirtualFreeEx(process, remote_config, 0, MEM_RELEASE);
            return Err(e);
        }

        let boot_fn: LPTHREAD_START_ROUTINE = Some(std::mem::transmute::<
            usize,
            unsafe extern "system" fn(*mut c_void) -> u32,
        >(remote_boot));
        let run = run_remote(process, boot_fn, remote_config, "rc_agent_boot", logger);
        // The agent copies the config during boot, so the buffer is freed
        // once the boot thread has returned.
        let _ = VirtualFreeEx(process, remote_config, 0, MEM_RELEASE);
        run
    }

    /// Serialize the repr(C) boot config field by field: independent of any
    /// struct-padding assumptions between helper and agent builds.
    fn config_to_bytes(config: &AgentBootConfig) -> Vec<u8> {
        let mut out = Vec::with_capacity(std::mem::size_of::<AgentBootConfig>());
        out.extend_from_slice(&config.magic.to_le_bytes());
        out.extend_from_slice(&config.size.to_le_bytes());
        out.extend_from_slice(&config.lease_ms.to_le_bytes());
        out.extend_from_slice(&config.hb_ms.to_le_bytes());
        for unit in config.pipe_name {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out.extend_from_slice(&config.pipe_name_len.to_le_bytes());
        out.extend_from_slice(&config.token);
        out.extend_from_slice(&config.token_len.to_le_bytes());
        out.extend_from_slice(&config.build_id);
        out.extend_from_slice(&config.build_id_len.to_le_bytes());
        out
    }
}
#[cfg(target_os = "windows")]
pub use windows_impl::{enable_debug_privilege, inject};

#[cfg(not(target_os = "windows"))]
pub fn inject(_pid: u32, _dll: &Path, _pipe: &str, _token: &str, _build: &str, _logger: &crate::logging::Logger) -> Result<(), String> {
    Err("injection requires Windows".to_string())
}

#[cfg(not(target_os = "windows"))]
pub fn boot_existing(_pid: u32, _dll: &Path, _pipe: &str, _token: &str, _build: &str, _logger: &crate::logging::Logger) -> Result<(), String> {
    Err("injection requires Windows".to_string())
}

#[cfg(not(target_os = "windows"))]
pub fn is_agent_loaded(_pid: u32) -> Result<bool, String> {
    Err("injection requires Windows".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal PE header with machine=0x8664, hand-built to keep the
    // parser independent of any real binary on the test host.
    fn pe_bytes(machine: u16) -> Vec<u8> {
        let mut b = vec![0u8; 0x80];
        b[0] = b'M';
        b[1] = b'Z';
        let e_lfanew: u32 = 0x40;
        b[0x3C..0x40].copy_from_slice(&e_lfanew.to_le_bytes());
        b[0x40..0x44].copy_from_slice(b"PE\0\0");
        b[0x44..0x46].copy_from_slice(&machine.to_le_bytes());
        b
    }

    #[test]
    fn pe_machine_parses_x64_arm64_x86() {
        assert_eq!(
            pe_machine_from_bytes(&pe_bytes(IMAGE_FILE_MACHINE_AMD64)),
            Some(IMAGE_FILE_MACHINE_AMD64)
        );
        assert_eq!(
            pe_machine_from_bytes(&pe_bytes(IMAGE_FILE_MACHINE_ARM64)),
            Some(IMAGE_FILE_MACHINE_ARM64)
        );
        assert_eq!(
            pe_machine_from_bytes(&pe_bytes(IMAGE_FILE_MACHINE_I386)),
            Some(IMAGE_FILE_MACHINE_I386)
        );
    }

    #[test]
    fn pe_machine_rejects_garbage() {
        assert_eq!(pe_machine_from_bytes(b""), None);
        assert_eq!(pe_machine_from_bytes(b"PE\0\0short"), None); // no MZ
        assert_eq!(pe_machine_from_bytes(&[0u8; 0x3F]), None); // too short
        let mut b = pe_bytes(IMAGE_FILE_MACHINE_AMD64);
        b[0x40] = b'X'; // broken PE signature
        assert_eq!(pe_machine_from_bytes(&b), None);
        // e_lfanew pointing outside the buffer
        let mut b = pe_bytes(IMAGE_FILE_MACHINE_AMD64);
        b[0x3C..0x40].copy_from_slice(&0xFF00u32.to_le_bytes());
        assert_eq!(pe_machine_from_bytes(&b), None);
    }

    #[test]
    fn machine_names_are_stable() {
        assert_eq!(machine_name(0x8664), "x64");
        assert_eq!(machine_name(0xAA64), "arm64");
        assert_eq!(machine_name(0x014C), "x86");
        assert_eq!(machine_name(0x1234), "unknown");
    }

    #[test]
    fn arch_gate_requires_x64_everywhere() {
        assert!(arch_gate(IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_AMD64).is_ok());
        // ARM64 host (the exact real-device failure shape): refused BEFORE injection.
        assert!(arch_gate(IMAGE_FILE_MACHINE_ARM64, IMAGE_FILE_MACHINE_AMD64).is_err());
        // x64 host with an arm64 dll: refused.
        assert!(arch_gate(IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64).is_err());
        // The error text carries the machine values for the field.
        let err = arch_gate(IMAGE_FILE_MACHINE_ARM64, IMAGE_FILE_MACHINE_AMD64).unwrap_err();
        assert!(err.contains("0xAA64"));
    }

    #[test]
    fn agent_module_prefix_match() {
        assert!(module_is_agent("rc-agent.dll"));
        assert!(module_is_agent("RC-AGENT.DLL"));
        assert!(module_is_agent(r"C:\build\rc-agent.dll"));
        assert!(module_is_agent("rc-agent-dbg.dll"));
        // The built artifact itself: Rust lib targets underscore the crate
        // name, so the DLL the helper injects is rc_agent.dll.
        assert!(module_is_agent("rc_agent.dll"));
        assert!(module_is_agent(r"C:\ProgramData\RemoteMicRune\rc_agent.dll"));
        assert!(!module_is_agent("wudfhost.exe"));
        assert!(!module_is_agent("kernel32.dll"));
        assert!(!module_is_agent("rc-agent")); // not a dll
    }

    #[test]
    fn boot_config_builds_and_reports_size() {
        let cfg = build_boot_config(
            r"\\.\pipe\RemoteMicRune.Agent.4321",
            "0123456789abcdef",
            "rc-helper/0.1.0",
        )
        .unwrap();
        assert!(cfg.valid());
        assert_eq!(
            cfg.pipe_name().as_deref(),
            Some(r"\\.\pipe\RemoteMicRune.Agent.4321")
        );
        assert_eq!(cfg.token().as_deref(), Some("0123456789abcdef"));
        assert_eq!(cfg.build_id().as_deref(), Some("rc-helper/0.1.0"));
        // Lease values must match the contract constants at compile time.
        assert_eq!(cfg.lease_ms, rc_protocol::consts::LEASE_MS);
        assert_eq!(cfg.hb_ms, rc_protocol::consts::AGENT_HB_MS);
    }

    #[test]
    fn build_id_is_versioned() {
        let id = helper_build_id();
        assert!(id.starts_with("rc-helper/"));
    }
}
