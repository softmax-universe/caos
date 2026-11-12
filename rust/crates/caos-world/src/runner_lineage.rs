//! What a host agent (runnerd) and the runner it starts (`caos runner`) tell
//! each other OUTSIDE the server protocol (design/firecracker-runners.md,
//! phase 0). Both sides compile this module, so they can't drift apart.
//!
//! Downward, the host's required set: a warm runner's follow-up polls must
//! carry its parent's constraints. Otherwise a pooled runner's container can't
//! match its own pool's jobs (the job's `required-pool` is absent from the
//! poll, and the symmetric half of the server's `matches` rejects it), and it
//! can take same-base work from outside the pool. Its eviction lineage must also
//! name that parent, not assume a generic one.
//!
//! Upward, CLAIM RECORDS: one line per job the runner takes, on the runner's own
//! stdout. Only the first job's nonce passes through the host, so without these
//! the host's crash backstop can't name a later job a warm runner died on, and
//! that job's caller waits forever (claimed jobs have no deadline). Stdout is
//! the channel because nothing else writes to it in runner mode: a worker's
//! stdout and stderr are piped and relayed to the runner's STDERR, so a worker
//! can't forge a record.

use std::collections::BTreeMap;

/// Env var carrying the host's required set into the runner it starts: a JSON
/// object of arg name to git oid. These are the same oids the host polls with,
/// so the runner does no hashing and uses the object directly as its lineage
/// entry. Absent or empty means a generic parent (`{}`).
pub const PARENT_REQUIRED_ENV: &str = "CAOS_RUNNER_PARENT_REQUIRED";

/// Serialize a required set for [`PARENT_REQUIRED_ENV`].
pub fn encode_required(required: &BTreeMap<String, String>) -> String {
    serde_json::to_string(required).expect("a string map serializes")
}

/// Parse [`PARENT_REQUIRED_ENV`]'s value. An empty value is the generic parent.
/// Anything malformed is an ERROR, never `{}`: reading a pooled parent as generic
/// would quietly let the runner take work from outside its pool, which is the
/// exact failure this exists to prevent.
pub fn decode_required(value: &str) -> Result<BTreeMap<String, String>, String> {
    if value.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    serde_json::from_str(value)
        .map_err(|e| format!("{PARENT_REQUIRED_ENV} is not a JSON object of strings: {e}"))
}

/// The prefix of a claim record line.
const CLAIM_PREFIX: &str = "caos-runner claimed ";

/// One claim record, without its newline: `caos-runner claimed req=<r> nonce=<n>`.
pub fn claim_record(req: &str, nonce: &str) -> String {
    format!("{CLAIM_PREFIX}req={req} nonce={nonce}")
}

/// The most recent claim in a runner's stdout, as `(req, nonce)`. It is the
/// only job the runner can have died holding: every earlier one was posted
/// before the next was claimed. Lines that are not exact, well-formed records
/// are ignored. That includes a truncated final line, since a record is
/// written and flushed before its job starts.
pub fn last_claim(stdout: &str) -> Option<(String, String)> {
    stdout.lines().rev().find_map(parse_claim)
}

/// The prefix of the exit record a VM's init appends to the runner's report
/// after the runner exits. Docker reports the exit status itself; in a VM the
/// only place the host can learn it is the report device. A missing record means
/// the guest died before its runner did, which is a failure.
const EXIT_PREFIX: &str = "caos-vm-init exit=";

/// The exit record for a runner that exited with `code` (128 + signal for a
/// signal), without its newline.
pub fn exit_record(code: i32) -> String {
    format!("{EXIT_PREFIX}{code}")
}

/// The runner's exit status from a report, if init recorded one.
pub fn exit_status(report: &str) -> Option<i32> {
    report
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(EXIT_PREFIX)?.trim().parse().ok())
}

fn parse_claim(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix(CLAIM_PREFIX)?;
    let (req, nonce) = rest.split_once(' ')?;
    let req = req.strip_prefix("req=")?;
    let nonce = nonce.strip_prefix("nonce=")?;
    let hexish = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric());
    (hexish(req) && hexish(nonce)).then(|| (req.to_string(), nonce.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_round_trips_and_empty_is_generic() {
        let required = BTreeMap::from([("required-pool".to_string(), "ab12".to_string())]);
        assert_eq!(
            decode_required(&encode_required(&required)).unwrap(),
            required
        );
        assert!(decode_required("").unwrap().is_empty());
        assert!(decode_required("{}").unwrap().is_empty());
    }

    #[test]
    fn malformed_required_is_an_error_not_generic() {
        assert!(decode_required("required-pool=x").is_err());
        assert!(decode_required(r#"{"required-pool": 3}"#).is_err());
    }

    #[test]
    fn exit_status_is_read_from_the_last_record() {
        let report = format!("{}\n{}\n", claim_record("aa", "n1"), exit_record(70));
        assert_eq!(exit_status(&report), Some(70));
        assert_eq!(last_claim(&report), Some(("aa".into(), "n1".into())));
        assert_eq!(exit_status(&claim_record("aa", "n1")), None);
    }

    #[test]
    fn last_claim_is_the_latest_well_formed_record() {
        let out = format!(
            "{}\nnoise\n{}\ncaos-runner claimed req=zz nonce=\n",
            claim_record("aa", "n1"),
            claim_record("bb", "n2"),
        );
        assert_eq!(last_claim(&out), Some(("bb".into(), "n2".into())));
        assert_eq!(last_claim("nothing here\n"), None);
        // A record that is not at the start of its line is not a record.
        assert_eq!(last_claim(&format!("x {}", claim_record("aa", "n1"))), None);
    }
}
