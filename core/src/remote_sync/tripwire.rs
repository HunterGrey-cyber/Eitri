//! The size tripwire: a tree-session batch on its way to the laptop that has received more than
//! `remote_dev.tripwire_mib`, or whose current file alone is larger, pauses the tree session before it is applied.
//!
//! Mutagen reports no batch total, only the running `totalReceivedSize` and the current file's `expectedSize`, so
//! either passing the limit trips; a single large file rewritten in place trips too. Mutagen cannot add an ignore to a
//! running session, so the whole tree session pauses and the person chooses: sync it anyway (`allow`), or leave it on
//! the server (`exclude`, applied by re-creating the session when the project is quiet).

use std::path::{Component, Path};

use super::engine::{Side, Staging};

pub const MIB: u64 = 1024 * 1024;

/// What the tripwire knows besides the staging progress.
pub struct Allowances<'a> {
    pub limit_bytes: u64,
    /// `SyncRecord.tripwire_allowed`: paths the person let through.
    pub allowed: &'a [String],
    /// `SyncRecord.tripwire_allow_batch`: the current batch is let through once.
    pub allow_batch: bool,
    /// The session's first sync after `eitri host open`, whose size the person was shown.
    pub first_sync: bool,
}

/// Whether `staging` trips the wire.
pub fn trips(staging: &Staging, allowances: &Allowances<'_>) -> bool {
    if staging.side != Side::Laptop || allowances.first_sync || allowances.allow_batch {
        return false;
    }
    let large =
        staging.total_received_bytes > allowances.limit_bytes || staging.expected_bytes > allowances.limit_bytes;
    large
        && !staging
            .path
            .as_deref()
            .is_some_and(|path| allowances.allowed.iter().any(|allowed| within(path, allowed)))
}

/// Whether `path` is `allowed` or under it.
pub fn within(path: &str, allowed: &str) -> bool {
    let allowed = allowed.trim_end_matches('/');
    !allowed.is_empty() && (path == allowed || path.strip_prefix(allowed).is_some_and(|rest| rest.starts_with('/')))
}

/// The top-most path on the way to `rel` that the laptop's copy at `root` does not have: the new directory a large
/// batch is filling, or `rel` itself when everything on the way exists (a file rewritten in place).
pub fn top_missing(root: &Path, rel: &str) -> String {
    let mut prefix = String::new();
    for component in Path::new(rel).components() {
        let Component::Normal(name) = component else {
            return rel.to_owned();
        };
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(&name.to_string_lossy());
        if std::fs::symlink_metadata(root.join(&prefix)).is_err() {
            return prefix;
        }
    }
    rel.to_owned()
}

/// The band line for a trip. `path` is the named path (none when the batch named no file), `bytes` what it would send.
pub fn paused_text(root: &Path, host: &str, path: Option<&str>, bytes: u64) -> String {
    let mib = bytes.div_ceil(MIB);
    let root = root.display();
    match path {
        Some(path) => format!(
            "sync paused: {path} on {host} would send {mib} MiB to this laptop; sync of {root} with {host} waits until you \
             choose: sync it anyway (eitri host sync allow {path}) or leave it on the server (eitri host sync exclude \
             {path})"
        ),
        None => format!(
            "sync paused: a {mib} MiB change on {host} would reach this laptop; sync of {root} with {host} waits until you \
             choose: sync it anyway (eitri host sync allow) or leave a path on the server (eitri host sync exclude \
             <path>)"
        ),
    }
}

/// The line for a large change that finished between two looks (only without Mutagen's monitor stream). The laptop's
/// side grew; whether the bytes came from the server or were written here cannot be told from the totals.
pub fn arrived_text(host: &str, bytes: u64) -> String {
    format!(
        "this laptop's copy grew by {} MiB within one sync cycle; if that came from {host}, it arrived before sync \
         could pause",
        bytes.div_ceil(MIB)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn staging(path: Option<&str>, total: u64, expected: u64) -> Staging {
        Staging {
            side: Side::Laptop,
            expected_bytes: expected,
            received_bytes: 0,
            total_received_bytes: total,
            received_files: 0,
            expected_files: 1,
            path: path.map(str::to_owned),
        }
    }

    fn allowances(allowed: &[String]) -> Allowances<'_> {
        Allowances {
            limit_bytes: 64 * MIB,
            allowed,
            allow_batch: false,
            first_sync: false,
        }
    }

    #[test]
    fn running_total_or_one_large_file_trips_towards_the_laptop_only() {
        let none: Vec<String> = vec![];
        assert!(trips(&staging(Some("data/a.bin"), 65 * MIB, MIB), &allowances(&none)));
        assert!(trips(&staging(Some("big.iso"), MIB, 300 * MIB), &allowances(&none)));
        assert!(!trips(&staging(Some("a"), 10 * MIB, 10 * MIB), &allowances(&none)));
        let mut to_server = staging(Some("a"), 500 * MIB, 500 * MIB);
        to_server.side = Side::Server;
        assert!(!trips(&to_server, &allowances(&none)));
        let allowed = vec!["data".to_owned()];
        assert!(!trips(
            &staging(Some("data/a.bin"), 65 * MIB, MIB),
            &allowances(&allowed)
        ));
        assert!(trips(
            &staging(Some("database/a.bin"), 65 * MIB, MIB),
            &allowances(&allowed)
        ));
        let mut first = allowances(&none);
        first.first_sync = true;
        assert!(!trips(&staging(None, 900 * MIB, 0), &first));
        let mut batch = allowances(&none);
        batch.allow_batch = true;
        assert!(!trips(&staging(None, 900 * MIB, 0), &batch));
    }

    #[test]
    fn top_missing_names_the_new_directory_or_the_file_itself() {
        let root = agent::state_dirs::test_workspace_dir("tripwire-top");
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(root.join("big.bin"), "x").unwrap();
        assert_eq!(top_missing(&root, "data/new/deep/a.bin"), "data/new");
        assert_eq!(top_missing(&root, "fresh/a.bin"), "fresh");
        assert_eq!(top_missing(&root, "big.bin"), "big.bin");
    }

    #[test]
    fn texts_name_the_path_the_host_and_the_size() {
        let text = paused_text(Path::new("/srv/project"), "au", Some("assets/video"), 70 * MIB + 1);
        assert!(text.starts_with("sync paused: assets/video on au would send 71 MiB to this laptop"));
        for path in [Some("assets/video"), None] {
            assert!(paused_text(Path::new("/srv/project"), "au", path, MIB)
                .contains("sync of /srv/project with au waits until you choose"));
        }
        assert!(text.contains("eitri host sync allow assets/video"));
        assert!(text.contains("eitri host sync exclude assets/video"));
        assert!(paused_text(Path::new("/srv/project"), "au", None, 64 * MIB)
            .starts_with("sync paused: a 64 MiB change on au"));
        assert_eq!(
            arrived_text("au", 100 * MIB),
            "this laptop's copy grew by 100 MiB within one sync cycle; if that came from au, it arrived before sync \
             could pause"
        );
    }
}
