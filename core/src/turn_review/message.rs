//! The one message a review sends to the agent, in a fixed shape, and the digest the user's
//! confirmation is bound to.
//!
//! What the message says is decided from the disk at send time ([`super::draft::check_reverts`]):
//! it reports only the reverts that are on disk, and the preview names every one that is not.

use std::collections::BTreeSet;

use sha2::{Digest, Sha256};

use super::draft::{RevertShape, RevertStatus, ReviewDraft};

/// The message to send, the digest the user confirms and the reverts it leaves out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub text: String,
    /// The first 16 hex digits of the sha256 of the text and every status: a confirmation names the
    /// digest it was shown, so a draft or a disk that changed since is never sent unseen.
    pub digest: String,
    /// Every revert that is not on disk, with its status, in the draft's order.
    pub not_on_disk: Vec<(u32, RevertStatus)>,
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A path or file name made safe to put in a message: a control character would let a file name
/// start a line of its own.
fn shown(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { '?' } else { c }).collect()
}

/// The message for `draft`, with `statuses` (one per revert, from `check_reverts`) deciding which
/// reverts it reports. A revert with no status is treated as changed. `latest_turn` is the newest
/// turn of the session: the header says "your last turn" only when every item is on it.
pub fn compose(draft: &ReviewDraft, statuses: &[(u32, RevertStatus)], latest_turn: u32) -> Preview {
    let status_of = |id: u32| {
        statuses
            .iter()
            .find(|(i, _)| *i == id)
            .map_or(RevertStatus::ChangedSince, |(_, s)| *s)
    };
    let reverts: Vec<_> = draft.reverts().iter().map(|r| (r, status_of(r.id))).collect();
    let on_disk: Vec<_> = reverts
        .iter()
        .filter(|(_, s)| *s == RevertStatus::OnDisk)
        .map(|(r, _)| *r)
        .collect();
    let not_on_disk: Vec<(u32, RevertStatus)> = reverts
        .iter()
        .filter(|(_, s)| *s != RevertStatus::OnDisk)
        .map(|(r, s)| (r.id, *s))
        .collect();

    let turns: BTreeSet<u32> = draft
        .comments()
        .iter()
        .map(|c| c.turn)
        .chain(on_disk.iter().map(|r| r.new.turn))
        .collect();
    let whose = if turns.iter().all(|t| *t == latest_turn) {
        "your last turn".to_owned()
    } else {
        let list: Vec<String> = turns.iter().map(u32::to_string).collect();
        format!("turn{} {}", if list.len() == 1 { "" } else { "s" }, list.join(", "))
    };
    let mut sections = vec![format!(
        "Review of {whose}: {}, {}.",
        plural(draft.comments().len(), "comment", "comments"),
        plural(on_disk.len(), "revert", "reverts"),
    )];

    if !on_disk.is_empty() {
        let mut section = vec!["I reverted these changes; the files no longer contain them:".to_owned()];
        for record in &on_disk {
            let path = shown(&record.new.path);
            section.push(match record.new.shape {
                RevertShape::Lines { from, to } => {
                    format!("- {path} lines {from}-{to} (back to how they were before your turn)")
                }
                RevertShape::Deleted => format!("- {path}: deleted (it did not exist before your turn)"),
                RevertShape::Restored => format!("- {path}: restored (you had deleted it)"),
                RevertShape::WholeFile => {
                    format!("- {path}: the whole file is back to how it was before your turn")
                }
            });
        }
        sections.push(section.join("\n"));
    }

    if !draft.comments().is_empty() {
        let mut section = vec!["Comments:".to_owned()];
        for (n, comment) in draft.comments().iter().enumerate() {
            section.push(format!(
                "{}. {}:{}-{}",
                n + 1,
                shown(&comment.path),
                comment.from,
                comment.to
            ));
            for line in &comment.anchor {
                section.push(format!("   > {line}"));
            }
            section.push(format!("   {}", comment.text));
        }
        sections.push(section.join("\n"));
    }

    let text = sections.join("\n\n");
    let mut hash = Sha256::new();
    hash.update(text.as_bytes());
    for (record, status) in &reverts {
        hash.update(format!("\n{}:{}", record.id, status.name()).as_bytes());
    }
    let digest: String = hash.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect();
    Preview {
        text,
        digest,
        not_on_disk,
    }
}
