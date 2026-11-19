//! Server-side synchronization facts and compare-and-act file operations.
//!
//! Every path a laptop names is a project root (absolute, normal, an existing directory the account owns, resolved
//! as a sidecar started there records it) and a relative path that `check_sync_rel` accepts. Files are reached from
//! the root one directory at a time without following a link: a path through a linked directory is refused, a link
//! at the end is reported as a link and never followed.
pub mod activity;
pub mod files;
pub mod git_config;
pub mod ignores;
pub mod listing;

use std::fs::File;

use super::sidecars::{canonical_root, open_root};

/// The most a laptop may read in one ReadFile call.
pub const MAX_READ_BYTES: u64 = 256 * 1024 * 1024;
/// The most a laptop may write in one ReplaceFileIf call.
pub const MAX_REPLACE_BYTES: usize = 8 * 1024 * 1024;
/// The size of one streamed chunk of a file.
pub const CHUNK_BYTES: usize = 64 * 1024;
/// The longest WaitFileHash may wait.
pub const MAX_WAIT_MS: u64 = 10_000;
/// The most entries one ListTree or PathSizes call looks at, whatever budget it asks for.
pub const MAX_BUDGET: u64 = 4 * 1024 * 1024;
/// The longest a ListTree or PathSizes call may take, a laptop that stops reading included: a walk never holds a
/// blocking thread longer than this.
pub const LIST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(300);
/// The longest a ReadFile call may take, a laptop that stops reading included.
pub const READ_DEADLINE: std::time::Duration = std::time::Duration::from_secs(300);
/// The longest ReplaceFileIf and RemoveFileIf take to say what is there now in a refusal; a file still being hashed
/// then is reported as `unfinished`.
pub const FACT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// The held root directory and the path it resolves to, as a sidecar started there records its root.
pub struct Root {
    pub dir: File,
    pub canonical: String,
}

/// Opens a laptop-named root with the checks a sidecar start makes; the refusal says why it is not one.
pub fn root(root: &str) -> Result<Root, String> {
    if root.len() > 4096 {
        return Err("the root exceeds 4096 bytes".to_owned());
    }
    let dir = open_root(root).map_err(|status| status.message().to_owned())?;
    let canonical = canonical_root(&dir).map_err(|status| status.message().to_owned())?;
    Ok(Root { dir, canonical })
}

/// A relative path accepted for a file action, with the refusal the laptop sees otherwise.
pub fn rel(rel: &str) -> Result<(), String> {
    if rel.len() > 4096 {
        return Err("the path exceeds 4096 bytes".to_owned());
    }
    eitri_server_proto::check_sync_rel(rel)?;
    // A file system that folds case would take `.GIT` for the repository's own directory.
    if rel.split('/').any(|part| part.eq_ignore_ascii_case(".git")) {
        return Err(format!("{rel:?}: a .git component is never a synchronized file"));
    }
    Ok(())
}

/// Lowercase hex of a SHA-256 digest, as every hash on the wire is written.
pub fn hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A wire hash: 64 lowercase hex digits.
pub fn parse_hash(text: &str) -> Result<[u8; 32], String> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 || !bytes.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(format!("{text:?} is not a SHA-256 in lowercase hex"));
    }
    let nibble = |b: u8| if b.is_ascii_digit() { b - b'0' } else { b - b'a' + 10 };
    let mut digest = [0u8; 32];
    for (index, pair) in bytes.chunks(2).enumerate() {
        digest[index] = (nibble(pair[0]) << 4) | nibble(pair[1]);
    }
    Ok(digest)
}

/// A value for an audit line: as it is when it is one plain word, quoted otherwise, so a field cannot pass for two.
pub fn audit_word(text: &str) -> String {
    let plain = |c: char| c.is_ascii_graphic() || (!c.is_ascii() && !c.is_control() && !c.is_whitespace());
    if !text.is_empty() && !text.contains('"') && text.chars().all(plain) {
        text.to_owned()
    } else {
        format!("{text:?}")
    }
}
