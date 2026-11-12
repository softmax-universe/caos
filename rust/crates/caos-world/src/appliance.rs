//! The Firecracker runner appliance's boot contract, as both sides see it
//! (design/firecracker-runners.md, "Appliance boot contract"): runnerd's
//! Firecracker backend writes it, and `caos vm-init` (PID 1 in the guest) reads
//! it.

use std::collections::BTreeMap;

/// Printed to the console once the runner has been started: the host's
/// readiness signal. Before it, a timeout is a boot failure; after it, the VM
/// belongs to its job for as long as the job takes.
pub const READY_LINE: &str = "caos-vm-init: runner started";

/// What the host writes to the config disk: one JSON object, NUL-padded to the
/// disk's size.
#[derive(Debug, PartialEq)]
pub struct BootConfig {
    pub job: String,
    pub env: BTreeMap<String, String>,
}

impl BootConfig {
    /// Serialize for the config disk (the host side).
    pub fn encode(&self) -> String {
        serde_json::json!({ "job": self.job, "env": self.env }).to_string()
    }

    /// Parse the config disk's contents: the JSON object, then NUL padding.
    pub fn decode(raw: &[u8]) -> Result<BootConfig, String> {
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        let text = std::str::from_utf8(&raw[..end])
            .map_err(|e| format!("boot config is not UTF-8: {e}"))?;
        let v: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("boot config is not JSON: {e}"))?;
        let job = v
            .get("job")
            .and_then(serde_json::Value::as_str)
            .filter(|j| !j.is_empty())
            .ok_or("boot config has no job")?
            .to_string();
        let env = match v.get("env") {
            None => BTreeMap::new(),
            Some(env) => serde_json::from_value(env.clone())
                .map_err(|e| format!("boot config env is not a string map: {e}"))?,
        };
        Ok(BootConfig { job, env })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_config_round_trips_through_nul_padding() {
        let config = BootConfig {
            job: r#"{"req":"r","nonce":"n"}"#.to_string(),
            env: BTreeMap::from([("CAOS_SERVER_URL".to_string(), "http://h".to_string())]),
        };
        let mut raw = config.encode().into_bytes();
        raw.resize(4096, 0);
        assert_eq!(BootConfig::decode(&raw).unwrap(), config);
    }

    #[test]
    fn a_config_with_no_job_is_refused() {
        assert!(BootConfig::decode(br#"{"env":{}}"#).is_err());
        assert!(BootConfig::decode(b"").is_err());
    }
}
