//! Windows transport for the bridge link: the named-pipe server, security
//! descriptor, random token (BCryptGenRandom), and the `bridge.ini`
//! descriptor file. Compiled only on Windows.

use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::{Path, PathBuf};

use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, ERROR_ACCESS_DENIED, ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING, HANDLE,
    HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows::Win32::Security::Cryptography::{
    BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, NAMED_PIPE_MODE, PIPE_NOWAIT,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
};

use rc_protocol::consts::{
    BRIDGE_DESCRIPTOR_FILE, BRIDGE_PIPE_NAME, BRIDGE_PROTOCOL_VERSION, PIPE_BUFFER_BYTES,
};
use rc_protocol::handoff::BridgeDescriptor;

/// Loopback transport only, but the helper runs elevated and must reach the
/// pipe: grant Everyone generic access on a protected DACL. This is not a
/// security boundary against the same user (who can read the descriptor
/// file anyway); it keeps random local processes from connecting by
/// accident, and the token check is the belt-and-suspenders on top.
pub const SECURITY_DESCRIPTOR_SDDL: &str = "D:P(A;;GA;;;WD)";

/// `SECURITY_DESCRIPTOR_REVISION` (self-descriptor format version 1).
const SDDL_REVISION: u32 = 1;

/// Why a pipe instance could not be created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipeCreateError {
    /// The pipe name is already owned: another app instance is running.
    AlreadyRunning,
    /// Anything else.
    Other(String),
}

impl std::fmt::Display for PipeCreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PipeCreateError::AlreadyRunning => {
                write!(f, "bridge pipe already exists (app already running)")
            }
            PipeCreateError::Other(detail) => write!(f, "pipe creation failed: {detail}"),
        }
    }
}

/// Create one server-side pipe instance. `first = true` additionally passes
/// `FILE_FLAG_FIRST_PIPE_INSTANCE`, which fails when the name is already
/// owned — that is the app's single-instance enforcement point.
pub fn create_server_instance(name: &str, first: bool) -> Result<File, PipeCreateError> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let sa = build_security_attributes().map_err(PipeCreateError::Other)?;
    let mut open_mode = PIPE_ACCESS_DUPLEX;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let pipe_mode: NAMED_PIPE_MODE =
        PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS;
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR::from_raw(wide.as_ptr()),
            open_mode,
            pipe_mode,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER_BYTES,
            PIPE_BUFFER_BYTES,
            0,
            Some(&sa),
        )
    };
    // The security descriptor buffer is only read by CreateNamedPipeW.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sa.lpSecurityDescriptor)));
    }
    if handle.is_invalid() {
        let error = unsafe { GetLastError() };
        return Err(if error == ERROR_ACCESS_DENIED && first {
            PipeCreateError::AlreadyRunning
        } else {
            PipeCreateError::Other(format!(
                "CreateNamedPipeW failed with win32 error {}",
                error.0
            ))
        });
    }
    // Take ownership of the raw handle in a File (DontDrop handles this).
    Ok(unsafe { File::from_raw_handle(handle.0) })
}

fn build_security_attributes() -> Result<SECURITY_ATTRIBUTES, String> {
    let sddl: Vec<u16> = SECURITY_DESCRIPTOR_SDDL
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR::from_raw(sddl.as_ptr()),
            SDDL_REVISION,
            &mut descriptor,
            None,
        )
    }
    .map_err(|error| format!("SDDL conversion failed: {error}"))?;
    Ok(SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    })
}

/// One poll of `ConnectNamedPipe` in nowait mode: `Ok(true)` = a client is
/// connected (or connected during the poll), `Ok(false)` = still listening.
pub fn poll_connected(handle: RawHandle) -> io::Result<bool> {
    match unsafe { ConnectNamedPipe(HANDLE(handle), None) } {
        Ok(()) => Ok(true),
        Err(error) if error.code() == HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) => Ok(true),
        Err(error) if error.code() == HRESULT::from_win32(ERROR_PIPE_LISTENING.0) => Ok(false),
        Err(error) => Err(io::Error::from_raw_os_error(error.code().0)),
    }
}

/// Disconnect the server side of one pipe instance (drops the client; a
/// blocked reader observes the broken pipe immediately).
pub fn disconnect(handle: RawHandle) {
    unsafe {
        let _ = DisconnectNamedPipe(HANDLE(handle));
    }
}

/// Duplex IO over one connected pipe instance.
///
/// Reads run in nowait mode: no data surfaces as `ERROR_NO_DATA` (232) or a
/// zero-byte success, both mapped to `WouldBlock` here. A real disconnect
/// is `ERROR_BROKEN_PIPE` (109), which stays an error and ends the
/// connection.
pub struct PipeIo {
    file: File,
}

impl PipeIo {
    pub fn new(file: File) -> Self {
        Self { file }
    }

    pub fn try_clone(&self) -> io::Result<PipeIo> {
        Ok(PipeIo {
            file: self.file.try_clone()?,
        })
    }

    /// Recover the owned pipe `File` (used to hand a connected instance to
    /// the connection thread).
    pub fn into_file(self) -> File {
        self.file
    }

    pub fn raw_handle(&self) -> RawHandle {
        self.file.as_raw_handle()
    }

    /// Disconnect this instance's server side.
    pub fn disconnect(&self) {
        disconnect(self.raw_handle());
    }
}

impl Read for PipeIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.file.read(buf) {
            // Nowait pipes may report success with 0 bytes while connected;
            // a true EOF does not exist for this mode (broken pipe = error).
            Ok(0) => Err(io::Error::from(ErrorKind::WouldBlock)),
            other => other,
        }
    }
}

impl Write for PipeIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Is this io error just "no data right now" (keep polling)?
pub fn is_idle_error(error: &io::Error) -> bool {
    error.kind() == ErrorKind::WouldBlock
        || error.kind() == ErrorKind::TimedOut
        || error.raw_os_error() == Some(232) // ERROR_NO_DATA (PIPE_NOWAIT)
}

/// 16 random bytes for the auth token. Windows: `BCryptGenRandom` with the
/// system-preferred RNG. The fallbacks (BCrypt failure on Windows,
/// non-Windows host builds) are documented, non-cryptographic mixes — the
/// token only guards against confused-process mixups, not against the same
/// user; a fresh-per-run value that is *unlikely to repeat* is what matters.
pub fn random_16_bytes() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    #[cfg(target_os = "windows")]
    {
        let status = unsafe { BCryptGenRandom(None, &mut bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
        if status.0 >= 0 {
            return bytes;
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
            if file.read_exact(&mut bytes).is_ok() {
                return bytes;
            }
        }
    }
    fill_fallback(&mut bytes);
    bytes
}

/// Last-resort entropy: splitmix64 over time, pid and an address. Never
/// used when the platform RNG works.
fn fill_fallback(bytes: &mut [u8]) {
    let mut state = u64::from(std::process::id()) << 32
        ^ std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
        ^ (&bytes as *const _ as u64);
    for chunk in bytes.chunks_mut(8) {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        for (index, byte) in chunk.iter_mut().enumerate() {
            *byte = (z >> (index * 8)) as u8;
        }
    }
}

/// Fresh 32-hex-char token, one per app run.
pub fn generate_token() -> String {
    random_16_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// App runtime dir: `%LOCALAPPDATA%\RemoteMicRune`.
pub fn app_runtime_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| {
        let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_owned());
        format!("{home}\\AppData\\Local")
    });
    PathBuf::from(base).join(rc_protocol::consts::APP_DIR_NAME)
}

/// Atomically write the descriptor file (temp name + rename, so the helper
/// never reads a half-written file).
pub fn write_descriptor(dir: &Path, token: &str, log: Option<&str>) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let descriptor = BridgeDescriptor {
        version: BRIDGE_PROTOCOL_VERSION,
        pipe: BRIDGE_PIPE_NAME.to_owned(),
        token: token.to_owned(),
        pid: std::process::id(),
        log: log.map(str::to_owned),
    };
    let final_path = dir.join(BRIDGE_DESCRIPTOR_FILE);
    let temp_path = dir.join(format!("{BRIDGE_DESCRIPTOR_FILE}.tmp"));
    std::fs::write(&temp_path, descriptor.encode())?;
    std::fs::rename(&temp_path, &final_path)?;
    Ok(final_path)
}
