//! Boot-time handoff structures: how the helper passes its pipe name, token
//! and lease into the injected DLL, and how the app publishes its bridge
//! endpoint for the helper.
//!
//! The DLL is loaded with `CreateRemoteThread(LoadLibraryW, path)` which
//! cannot carry arguments, so the helper writes an [`AgentBootConfig`] into
//! the target process and starts a second remote thread on
//! `rc_agent_boot(config_ptr)`. The struct is `repr(C)` with explicit sizes;
//! the agent validates magic + size before reading anything.

use crate::consts::BRIDGE_PROTOCOL_VERSION;

pub const BOOT_MAGIC: u32 = 0x5243_4D31; // "RCM1"
pub const BOOT_PIPE_LEN: usize = 96;
pub const BOOT_TOKEN_LEN: usize = 64;
pub const BOOT_BUILD_LEN: usize = 48;

/// Remote-memory boot config written by the helper, read by the agent.
///
/// String fields are fixed-size buffers + byte length. `pipe_name` is
/// UTF-16LE (a Windows wide string, not NUL-terminated when len is set),
/// `token` and `build_id` are ASCII.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentBootConfig {
    pub magic: u32,
    /// `size_of::<AgentBootConfig>()` at the helper's build; the agent
    /// rejects a mismatch instead of reading garbage.
    pub size: u32,
    pub lease_ms: u32,
    pub hb_ms: u32,
    pub pipe_name: [u16; BOOT_PIPE_LEN],
    pub pipe_name_len: u32,
    pub token: [u8; BOOT_TOKEN_LEN],
    pub token_len: u32,
    pub build_id: [u8; BOOT_BUILD_LEN],
    pub build_id_len: u32,
}

impl Default for AgentBootConfig {
    fn default() -> Self {
        Self {
            magic: BOOT_MAGIC,
            size: std::mem::size_of::<Self>() as u32,
            lease_ms: crate::consts::LEASE_MS,
            hb_ms: crate::consts::AGENT_HB_MS,
            pipe_name: [0; BOOT_PIPE_LEN],
            pipe_name_len: 0,
            token: [0; BOOT_TOKEN_LEN],
            token_len: 0,
            build_id: [0; BOOT_BUILD_LEN],
            build_id_len: 0,
        }
    }
}

impl AgentBootConfig {
    pub fn new(pipe_name: &str, token: &str, build_id: &str) -> Result<Self, String> {
        let mut cfg = AgentBootConfig::default();
        let wide: Vec<u16> = pipe_name.encode_utf16().collect();
        if wide.len() > BOOT_PIPE_LEN {
            return Err(format!("pipe name too long ({} UTF-16 units)", wide.len()));
        }
        cfg.pipe_name[..wide.len()].copy_from_slice(&wide);
        cfg.pipe_name_len = wide.len() as u32;

        let token = token.as_bytes();
        if token.len() > BOOT_TOKEN_LEN {
            return Err("token too long".into());
        }
        cfg.token[..token.len()].copy_from_slice(token);
        cfg.token_len = token.len() as u32;

        let build = build_id.as_bytes();
        if build.len() > BOOT_BUILD_LEN {
            return Err("build id too long".into());
        }
        cfg.build_id[..build.len()].copy_from_slice(build);
        cfg.build_id_len = build.len() as u32;
        Ok(cfg)
    }

    pub fn pipe_name(&self) -> Option<String> {
        let len = self.pipe_name_len as usize;
        if len == 0 || len > BOOT_PIPE_LEN {
            return None;
        }
        Some(String::from_utf16_lossy(&self.pipe_name[..len]))
    }

    pub fn token(&self) -> Option<String> {
        let len = self.token_len as usize;
        if len == 0 || len > BOOT_TOKEN_LEN {
            return None;
        }
        String::from_utf8(self.token[..len].to_vec()).ok()
    }

    pub fn build_id(&self) -> Option<String> {
        let len = self.build_id_len as usize;
        if len > BOOT_BUILD_LEN {
            return None;
        }
        String::from_utf8(self.build_id[..len].to_vec()).ok()
    }

    pub fn valid(&self) -> bool {
        self.magic == BOOT_MAGIC && self.size == std::mem::size_of::<Self>() as u32
    }
}

// -- app bridge descriptor file --------------------------------------------

/// The app writes `bridge.ini` into its runtime dir; the helper reads it to
/// find the bridge pipe + token. Lines are `key=value`, LF-separated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeDescriptor {
    pub version: u32,
    pub pipe: String,
    pub token: String,
    pub pid: u32,
    pub log: Option<String>,
}

impl BridgeDescriptor {
    pub fn encode(&self) -> String {
        let mut out = format!(
            "version={}\npipe={}\ntoken={}\npid={}\n",
            self.version, self.pipe, self.token, self.pid
        );
        if let Some(log) = &self.log {
            out.push_str(&format!("log={log}\n"));
        }
        out
    }

    pub fn decode(text: &str) -> Result<Self, String> {
        let mut version = None;
        let mut pipe = None;
        let mut token = None;
        let mut pid = None;
        let mut log = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "version" => version = value.trim().parse::<u32>().ok(),
                "pipe" => pipe = Some(value.trim().to_string()),
                "token" => token = Some(value.trim().to_string()),
                "pid" => pid = value.trim().parse::<u32>().ok(),
                "log" => log = Some(value.trim().to_string()),
                _ => {}
            }
        }
        Ok(BridgeDescriptor {
            version: version.ok_or("descriptor missing version")?,
            pipe: pipe.filter(|p| !p.is_empty()).ok_or("descriptor missing pipe")?,
            token: token.filter(|t| !t.is_empty()).ok_or("descriptor missing token")?,
            pid: pid.ok_or("descriptor missing pid")?,
            log,
        })
    }

    pub fn version_ok(&self) -> bool {
        self.version == BRIDGE_PROTOCOL_VERSION
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_config_round_trip() {
        let cfg = AgentBootConfig::new(
            r"\\.\pipe\RemoteMicRune.Agent.1234",
            "0123456789abcdef0123456789abcdef",
            "rc-agent/0.1.0+abc1234",
        )
        .unwrap();
        assert!(cfg.valid());
        assert_eq!(cfg.pipe_name().as_deref(), Some(r"\\.\pipe\RemoteMicRune.Agent.1234"));
        assert_eq!(cfg.token().as_deref(), Some("0123456789abcdef0123456789abcdef"));
        assert_eq!(cfg.build_id().as_deref(), Some("rc-agent/0.1.0+abc1234"));
        assert_eq!(
            std::mem::size_of::<AgentBootConfig>(),
            4 * 4 + 96 * 2 + 64 + 48 + 4 * 3
        );
    }

    #[test]
    fn boot_config_rejects_oversize() {
        assert!(AgentBootConfig::new(&"x".repeat(200), "t", "b").is_err());
        assert!(AgentBootConfig::new("p", &"t".repeat(100), "b").is_err());
    }

    #[test]
    fn boot_config_invalid_when_corrupted() {
        let mut cfg = AgentBootConfig::new("p", "t", "b").unwrap();
        cfg.magic = 0;
        assert!(!cfg.valid());
        let mut cfg = AgentBootConfig::new("p", "t", "b").unwrap();
        cfg.size += 1;
        assert!(!cfg.valid());
    }

    #[test]
    fn descriptor_round_trip() {
        let d = BridgeDescriptor {
            version: BRIDGE_PROTOCOL_VERSION,
            pipe: r"\\.\pipe\RemoteMicRune.Bridge.v1".into(),
            token: "cafebabe".into(),
            pid: 501,
            log: Some(r"C:\temp\a.log".into()),
        };
        let text = d.encode();
        assert_eq!(BridgeDescriptor::decode(&text).unwrap(), d);
        assert!(d.version_ok());
    }

    #[test]
    fn descriptor_requires_core_fields() {
        assert!(BridgeDescriptor::decode("version=1\npipe=x\n").is_err());
    }
}
