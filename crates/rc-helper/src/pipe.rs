//! Low-level named-pipe transport shared by both links.
//!
//! Blocking-mode byte pipes + polling reads (PeekNamedPipe with the
//! protocol's idle backoff). Polling instead of overlapped I/O keeps every
//! loop cancellable by the shared shutdown flag, which matters for clean
//! teardown ordering (disarm first, everything else after).

#![allow(dead_code)]

#[cfg(target_os = "windows")]
mod windows_impl {
    use windows::core::{PCWSTR, BOOL};
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
        ERROR_FILE_NOT_FOUND,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES,
        FILE_SHARE_MODE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::IO::OVERLAPPED;
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PeekNamedPipe, WaitNamedPipeW,
        PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    /// ERROR_PIPE_CONNECTED: a client connected between CreateNamedPipe and
    /// ConnectNamedPipe; the connection is valid, treat it as success.
    const ERROR_PIPE_CONNECTED: u32 = 535;
    /// ERROR_PIPE_BUSY: all server instances are occupied; wait and retry.
    const ERROR_PIPE_BUSY: u32 = 231;

    fn raw_last_error() -> u32 {
        unsafe { GetLastError().0 }
    }

    fn win32_code(err: &windows::core::Error) -> u32 {
        // Win32-origin failures come back as HRESULT 0x8007xxxx; the low
        // word is the GetLastError value worth logging.
        (err.code().0 as u32) & 0xFFFF
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Polling read on a pipe handle: bytes read, 0 when nothing is
    /// available. A broken pipe surfaces as Err.
    fn handle_read_poll(handle: HANDLE, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut available: u32 = 0;
        unsafe {
            PeekNamedPipe(handle, None, 0, None, Some(&mut available), None).map_err(|_| {
                std::io::Error::from_raw_os_error(raw_last_error() as i32)
            })?;
        }
        if available == 0 {
            return Ok(0);
        }
        let want = (available as usize).min(buf.len());
        let mut read: u32 = 0;
        unsafe {
            ReadFile(handle, Some(&mut buf[..want]), Some(&mut read), None).map_err(|_| {
                std::io::Error::from_raw_os_error(raw_last_error() as i32)
            })?;
        }
        Ok(read as usize)
    }

    /// Write a whole buffer to a pipe handle.
    fn handle_write_all(handle: HANDLE, bytes: &[u8]) -> Result<(), String> {
        let mut written: u32 = 0;
        unsafe {
            WriteFile(handle, Some(bytes), Some(&mut written), None).map_err(|e| {
                format!(
                    "WriteFile failed, GetLastError={} ({written} of {} bytes)",
                    win32_code(&e),
                    bytes.len()
                )
            })?;
        }
        if written as usize != bytes.len() {
            return Err(format!("WriteFile wrote {written} of {} bytes", bytes.len()));
        }
        Ok(())
    }

    /// A server-side pipe instance, bound to the helper's agent pipe name.
    pub struct ServerPipe {
        pub handle: HANDLE,
        pub name: String,
    }

    // SAFETY: HANDLE wraps a kernel pipe object, which is not thread-affine;
    // the raw handle is only ever used by the one thread that currently owns
    // the ServerPipe value (ownership moves accept -> session thread).
    unsafe impl Send for ServerPipe {}

    impl ServerPipe {
        /// Create one duplex, byte-mode, blocking server instance with the
        /// SDDL security descriptor applied (loopback-only transport; the
        /// agent still token-checks every command). Unlimited instances so
        /// the accept loop can park on the next connect while a session is
        /// still draining.
        pub fn create(name: &str, sddl: &str, buffer_bytes: u32) -> Result<ServerPipe, String> {
            let sa = security_attributes(sddl)?;
            let wname = wide(name);
            let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT;
            unsafe {
                let handle = CreateNamedPipeW(
                    PCWSTR(wname.as_ptr()),
                    PIPE_ACCESS_DUPLEX,
                    mode,
                    PIPE_UNLIMITED_INSTANCES,
                    buffer_bytes,
                    buffer_bytes,
                    0,
                    Some(&sa),
                );
                if handle == INVALID_HANDLE_VALUE {
                    return Err(format!(
                        "CreateNamedPipeW({name}) failed, GetLastError={}",
                        raw_last_error()
                    ));
                }
                Ok(ServerPipe {
                    handle,
                    name: name.to_string(),
                })
            }
        }

        /// Block until a client connects. ERROR_PIPE_CONNECTED means a client
        /// connected while the instance was being created: also success.
        pub fn connect_blocking(&self) -> Result<(), String> {
            unsafe {
                match ConnectNamedPipe(self.handle, None::<*mut OVERLAPPED>) {
                    Ok(()) => Ok(()),
                    Err(e) if win32_code(&e) == ERROR_PIPE_CONNECTED => Ok(()),
                    Err(e) => Err(format!(
                        "ConnectNamedPipe({}) failed, GetLastError={}",
                        self.name,
                        win32_code(&e)
                    )),
                }
            }
        }

        pub fn read_poll(&self, buf: &mut [u8]) -> std::io::Result<usize> {
            handle_read_poll(self.handle, buf)
        }

        pub fn write_all(&self, bytes: &[u8]) -> Result<(), String> {
            handle_write_all(self.handle, bytes)
        }

        pub fn flush(&self) {
            unsafe {
                let _ = FlushFileBuffers(self.handle);
            }
        }

        /// Politely end the current session: flush, disconnect, close.
        pub fn close(&mut self) {
            unsafe {
                let _ = FlushFileBuffers(self.handle);
                let _ = DisconnectNamedPipe(self.handle);
                let _ = CloseHandle(self.handle);
            }
        }
    }

    /// Client-side connection (used by the helper's app link).
    pub struct PipeClient {
        pub handle: HANDLE,
    }

    // SAFETY: same reasoning as ServerPipe; the handle follows single-owner
    // semantics (created on the link thread, moved into the pump thread).
    unsafe impl Send for PipeClient {}

    impl PipeClient {
        /// Open the pipe as a client, waiting up to `timeout_ms` when all
        /// server instances are busy (or the server has not come up yet).
        pub fn open(name: &str, timeout_ms: u64) -> Result<PipeClient, String> {
            let wname = wide(name);
            let access = GENERIC_READ | GENERIC_WRITE;
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
            loop {
                unsafe {
                    match CreateFileW(
                        PCWSTR(wname.as_ptr()),
                        access.0,
                        FILE_SHARE_MODE(0),
                        None,
                        OPEN_EXISTING,
                        FILE_FLAGS_AND_ATTRIBUTES(0),
                        None,
                    ) {
                        Ok(handle) => return Ok(PipeClient { handle }),
                        Err(e) => {
                            let code = win32_code(&e);
                            let busy = code == ERROR_PIPE_BUSY;
                            let not_yet = code == ERROR_FILE_NOT_FOUND.0;
                            if !(busy || not_yet) {
                                return Err(format!(
                                    "CreateFileW({name}) failed, GetLastError={code}"
                                ));
                            }
                            if std::time::Instant::now() >= deadline {
                                return Err(format!(
                                    "pipe {name} not reachable within {timeout_ms} ms (last error {code})"
                                ));
                            }
                            if busy {
                                // Tell the server we will wait for an instance.
                                let _ = WaitNamedPipeW(PCWSTR(wname.as_ptr()), 1000);
                            }
                            std::thread::sleep(std::time::Duration::from_millis(200));
                        }
                    }
                }
            }
        }

        pub fn read_poll(&self, buf: &mut [u8]) -> std::io::Result<usize> {
            handle_read_poll(self.handle, buf)
        }

        pub fn write_all(&self, bytes: &[u8]) -> Result<(), String> {
            handle_write_all(self.handle, bytes)
        }

        pub fn flush(&self) {
            unsafe {
                let _ = FlushFileBuffers(self.handle);
            }
        }

        /// Disconnect AND close: the other end sees the pipe break.
        pub fn disconnect(&mut self) {
            unsafe {
                let _ = DisconnectNamedPipe(self.handle);
                let _ = CloseHandle(self.handle);
            }
        }

        /// Close only (keep the connection nominally alive until the close).
        pub fn close(&mut self) {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }

    /// Build SECURITY_ATTRIBUTES from an SDDL string.
    fn security_attributes(sddl: &str) -> Result<SECURITY_ATTRIBUTES, String> {
        let wsddl = wide(sddl);
        let mut psd = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wsddl.as_ptr()),
                SDDL_REVISION_1,
                &mut psd,
                None,
            )
            .map_err(|e| {
                format!(
                    "ConvertStringSecurityDescriptorToSecurityDescriptorW({sddl}) failed, \
                     GetLastError={}",
                    win32_code(&e)
                )
            })?;
        }
        // The descriptor is intentionally kept alive for the pipe's lifetime
        // (a handful per process); freeing it across threads adds lifetime
        // risk for zero practical benefit.
        Ok(SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd.0,
            bInheritHandle: BOOL(0),
        })
    }
}

#[cfg(target_os = "windows")]
pub use windows_impl::{PipeClient, ServerPipe};
