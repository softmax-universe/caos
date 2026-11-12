//! Shared protocol pieces the caos client, server and runners agree on: the MCP
//! message shapes, the secret formats, and the reserved argument names.

pub mod mcp;
pub mod secrets;

/// The reserved ArgTree entry carrying a run's secret-cache-isolation tag
/// (design/secrets.md). Present only when the run is granted ≥1 secret.
pub const SECRET_HASH_ARG: &str = "secret-hash";

/// Request header naming the in-flight job an evaluation belongs to, by its job
/// nonce. The server records the dispatches that evaluation makes as children
/// of that job's trace node, so what a job had to build to evaluate a tree shows
/// up in the job's own trace. The nonce is the authority, exactly as for
/// `/sub-run` and `/trace/child`: it names one claimed job and dies with it. A
/// header and not a query parameter, because a query string is logged.
pub const TRACE_NONCE_HEADER: &str = "X-Caos-Job-Nonce";

/// Request header naming a trace node directly, by ArgTree hash. For an
/// evaluation that runs on ANOTHER STACK than the job it belongs to (a dev stack
/// brought up inside a worker), where the job's nonce means nothing. Only a
/// server started with `CAOS_TRACE_PARENTS=yes` honors it: anyone who can send it
/// can hang edges under any trace node.
pub const TRACE_PARENT_HEADER: &str = "X-Caos-Trace-Parent";

/// Canonical bytes to hash for the [`SECRET_HASH_ARG`] entry: the granted
/// secrets' `(worker-visible name, entropy)` pairs, sorted and de-duplicated,
/// each serialized `name\0entropy\n`. Whoever assembles an ArgTree hashes THIS
/// (as a git blob) and stores the resulting digest as the entry — so the key
/// depends on the entropy's DIGEST, never the entropy itself (which is a bearer
/// capability for the cache). The canonical form lives here, in the crate the
/// client and server share, because both compute it and a disagreement would
/// silently split or merge their caches.
pub fn secret_hash_material(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut pairs: Vec<(&str, &str)> = pairs.to_vec();
    pairs.sort_unstable();
    pairs.dedup();
    let mut out = Vec::new();
    for (name, entropy) in pairs {
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        out.extend_from_slice(entropy.as_bytes());
        out.push(b'\n');
    }
    out
}

/// Arg-name prefix reserved for RUNNER SELECTION (SPEC, "Misc"): an ArgTree
/// entry whose name starts with this must be matched EXACTLY by the runner that
/// takes the job.
///
/// It makes the rendezvous symmetric. A runner's `required` set says which jobs
/// it will accept; a `required*` arg says which runners the JOB will accept —
/// so the generic pool, which requires nothing and therefore matches everything,
/// is excluded from a job that names one. `required-caosd` (below) is the live
/// example: it keeps host-only jobs off the generic runnerd.
pub const REQUIRED_ARG_PREFIX: &str = "required";

/// The arg a job carries when it must run ON THE CENTRAL SERVER (the caosd
/// host): it needs something that only exists there. Bound as
/// `--required-caosd=true`. Today that is two workers:
///
/// - dev/test-stack, the devbox daemon, which needs the host's persistent
///   volumes (`CAOS_GRANT_VOLUMES`: the shared nix store, the dev stacks' git
///   repo, their podman store) and reaches the host's redis and registry by name;
/// - every flake-builder build, which pushes to the registry as
///   `caos-registry:5000`. std/flake-builder curries the arg onto its image, so
///   a `run --base:@=DEEP-DEPS/flake-builder` carries it without saying so. The
///   IMAGE such a build produces runs anywhere.
///
/// It is a `required*` arg on purpose, so the rendezvous does the routing: a
/// plain runnerd requires nothing and so never runs such a job, and only the
/// caosd runner — a runnerd on the central host started with
/// `CAOS_RUNNER_REQUIRE=required-caosd=true`, which `stack/serve` starts beside
/// the plain one — can. That runnerd is also the ONLY one that grants volumes
/// at all, so a runner on any other machine mounts nothing of the host's into
/// anything, whatever an image asks for. Being `required*`, it is also
/// inherited by a job's continuations (server `inherited_pool_entries`).
pub const REQUIRED_CAOSD_ARG: &str = "required-caosd";

/// The value [`REQUIRED_CAOSD_ARG`] is bound to. One spelling, because the
/// rendezvous compares blob oids: `yes` or `1` would name a runner that does
/// not exist.
pub const REQUIRED_CAOSD_VALUE: &str = "true";

/// The reserved ArgTree entry that names an INSTANCE (design/daemons.md). A job
/// with one is keyed by `(base, affinity)`: the server gives that key a single
/// owner runner and queues every job for it behind that owner, so two jobs for
/// one instance never run in two containers. A job without it is dispatched as
/// any other. The entry is an ordinary arg, so it is part of the cache key.
pub const AFFINITY_ARG: &str = "affinity";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_hash_material_is_order_independent_and_deduped() {
        let a = secret_hash_material(&[("gh", "E1"), ("npm", "E2")]);
        let b = secret_hash_material(&[("npm", "E2"), ("gh", "E1"), ("gh", "E1")]);
        assert_eq!(a, b, "sorted + de-duplicated, so caller order can't matter");
        // Rotating an entropy changes the material (hence the cache namespace).
        assert_ne!(a, secret_hash_material(&[("gh", "E9"), ("npm", "E2")]));
        // The name is part of it (same secret at a different mount is distinct).
        assert_ne!(a, secret_hash_material(&[("GH", "E1"), ("npm", "E2")]));
    }
}
