//! Telling the user the import happened: a one-time toast, a line on stderr every launch, and the
//! skipped lines for the `?` overlay.
//!
//! The toast is shown once, the first time the import changes anything, and again only when what it
//! imports changes: its fingerprint -- a SHA-256 over the result, not over the files, so a comment
//! or a reordering that ends in the same keys is no news -- is kept in
//! `<state home>/eitri/keymap-import.json` (0600, written like Eitri's other state files).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::keymap::{Action, HelpRow, Keymap, OptValue, TmuxImport};

/// The file the last notice's fingerprint is kept in, from `XDG_STATE_HOME`/`HOME` by the same rule
/// as every other state file of Eitri's (`crate::layout::persist::state_subdir`).
pub fn state_file(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    crate::layout::persist::state_subdir(xdg_state_home, home, "keymap-import.json")
}

fn signature(action: &Action) -> String {
    let mut out = action.name();
    let mut options = action.options();
    options.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, value) in options {
        let value = match value {
            OptValue::Int(i) => i.to_string(),
            OptValue::Bool(b) => b.to_string(),
            OptValue::Str(s) => s,
            OptValue::Other(o) => o.to_string(),
        };
        out.push_str(&format!(" {name}={value}"));
    }
    out
}

/// SHA-256, hex, over the import's result: its prefix, its bindings as sorted `(key, action with
/// its options, repeatable)`, and the keys it unbound, sorted.
pub fn fingerprint(import: &TmuxImport) -> String {
    let mut text = format!("prefix {}\n", import.prefix.map(|p| p.to_string()).unwrap_or_default());
    let mut bindings: Vec<String> = import
        .bindings
        .iter()
        .map(|b| format!("bind {}\t{}\t{}\n", b.key, signature(&b.action), b.repeatable))
        .collect();
    bindings.sort();
    let mut removed: Vec<String> = import.removed.iter().map(|k| format!("unbind {k}\n")).collect();
    removed.sort();
    for line in bindings.iter().chain(&removed) {
        text.push_str(line);
    }
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The most the notice file can hold: a fingerprint and its key.
const NOTICE_FILE_LIMIT: u64 = 4096;

#[derive(serde::Serialize, serde::Deserialize)]
struct NoticeFile {
    fingerprint: String,
}

/// Whether this result has not been announced yet: no file, an unreadable one, or another
/// fingerprint in it.
pub fn needs_notice(file: &Path, fingerprint: &str) -> bool {
    // Read like a config: only a regular file, and never more than a record this small can be.
    match super::read_regular(file, NOTICE_FILE_LIMIT) {
        Ok(text) => serde_json::from_str::<NoticeFile>(&text).map_or(true, |f| f.fingerprint != fingerprint),
        Err(_) => true,
    }
}

/// Records that this result was announced: written 0600 beside the file first (refusing to write
/// through a symlink there) and renamed over it, so a window killed mid-write leaves the previous
/// one, and a symlink in the file's place is replaced rather than written through.
pub fn remember(file: &Path, fingerprint: &str) -> std::io::Result<()> {
    let dir = file.parent().unwrap_or(file);
    agent::private_fs::create_private_dir_all(dir, dir)?;
    let text = serde_json::to_string(&NoticeFile {
        fingerprint: fingerprint.to_string(),
    })
    .expect("a string serializes");
    let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
    let written = agent::private_fs::write_private(&tmp, text.as_bytes()).and_then(|()| std::fs::rename(&tmp, file));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// A path as the user would type it: under the home directory, `~/...`.
fn tilde(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn files(import: &TmuxImport, home: Option<&Path>) -> String {
    import
        .files
        .iter()
        .map(|f| tilde(f, home))
        .collect::<Vec<_>>()
        .join(" and ")
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

/// The toast, one line: where the keys came from, the prefix and how many keys came across, how
/// to see them and how to turn the import off. `None` when the import changes nothing.
pub fn toast_text(import: &TmuxImport, keymap: &Keymap, home: Option<&Path>) -> Option<String> {
    if !import.changes_anything() {
        return None;
    }
    let prefix = keymap.prefix().human();
    let mut text = format!(
        "Using your tmux keys from {}: prefix {prefix}, {} ({} skipped).",
        files(import, home),
        plural(import.bindings.len(), "key"),
        import.skipped.len()
    );
    if let Some(key) = keymap.keys_for(&Action::PanelKeymap).first() {
        text.push_str(&format!(" {prefix} {} lists them;", key.human()));
    }
    text.push_str(&format!(" {} = \"off\" turns this off.", super::SETTING));
    Some(text)
}

/// The stderr line every launch prints, like the account line: what the import read and did, or
/// that there was nothing to read, or that it is off (`import` is `None`).
pub fn log_line(import: Option<&TmuxImport>, home: Option<&Path>) -> String {
    match import {
        None => "[keymap] tmux: off".to_string(),
        Some(import) if import.files.is_empty() => "[keymap] tmux: no config found".to_string(),
        Some(import) => format!(
            "[keymap] tmux: {}: prefix {}, {} imported, {} skipped",
            files(import, home),
            import
                .prefix
                .map(|p| p.to_string())
                .unwrap_or_else(|| "unchanged".into()),
            import.bindings.len(),
            import.skipped.len()
        ),
    }
}

/// The `?` overlay's "Skipped from tmux" rows: `<file>:<line>` and what was written, with why.
pub fn skipped_rows(import: &TmuxImport, home: Option<&Path>) -> Vec<HelpRow> {
    import
        .skipped
        .iter()
        .map(|s| {
            let file = tilde(&s.origin.file, home);
            let keys = if s.origin.line == 0 {
                file
            } else {
                format!("{file}:{}", s.origin.line)
            };
            let what = if s.origin.text.is_empty() {
                s.reason.clone()
            } else {
                format!("{} \u{2014} {}", s.origin.text, s.reason)
            };
            HelpRow { keys, what }
        })
        .collect()
}
