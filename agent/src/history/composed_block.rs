//! The inverse of wire 1's `compose_turn_text`: taking the editor-context block back off a prompt
//! that was read from disk.
//!
//! **Why this is not in `neovibe_core::editor_context`, where the design asked for it.**
//! `docs/superpowers/specs/2026-09-20-resume-history-design.md` §3.1.2 specifies
//! `neovibe_core::editor_context::strip_composed_block`, "written in the same module" as
//! `compose_turn_text`. That is not buildable: `neovibe-core` depends on `agent` (`core/Cargo.toml`),
//! and the history reader that needs this function lives in `agent`, so putting the function in
//! `core` would need `agent -> neovibe-core -> agent`. The function therefore lives here, and
//! `neovibe_core::editor_context` re-exports it under exactly the path the design names, so a
//! caller sees what §3.1.2 describes. The round-trip property test the design asks for
//! (`strip(compose(t, ctx)) == t`) lives beside `compose_turn_text`'s own cases in
//! `core/src/editor_context/compose.rs`, which is the half of §3.1.2 that matters: the two
//! directions cannot be changed independently without one of them going red.
//!
//! **Why stripping at all.** Measured 2026-09-20 (§0.8): the prompt text on disk is the WIRE turn,
//! not what the user typed -- 2 of the 6 SDK-driven sessions on this machine end with a block this
//! project's own `compose_turn_text` appended. `AgentDomainEvent::UserPromptSubmitted`'s own doc
//! says `text` is "the text the user typed, deliberately NOT the text that went on the wire", so
//! feeding the raw on-disk prompt into it would render a file path and a selection nobody wrote,
//! in violation of the field's own contract.
//!
//! **This is not parsing someone else's private format.** The block is text this repository's own
//! code appended; `compose.rs` owns its shape.

/// The trailing sentence both templates share, kept as its own constant only for the two suffixes
/// below to be obviously built from the same words `compose_turn_text` uses.
const HEDGE: &str = "This may or may not be related to the current task.";

/// `compose_turn_text`'s no-selection template, split at the caller-supplied file name.
const OPENED_PREFIX: &str = "The user opened the file ";
/// Built from [`HEDGE`] so that changing the hedge in `compose.rs` and not here fails the
/// round-trip test rather than silently stopping this function from recognising anything.
fn opened_suffix() -> String {
    format!(" in the IDE. {HEDGE}")
}

/// `compose_turn_text`'s selection template, split at the first caller-supplied value.
const SELECTED_PREFIX: &str = "The user selected the lines ";
fn selected_suffix() -> String {
    format!("\n\n{HEDGE}")
}

/// Removes the editor-context block `compose_turn_text` appends, if this text ends with one.
///
/// Returns the input unchanged when it does not end with a block this module's own templates could
/// have produced. It never guesses: no regex, no "looks like a path", no partial match. Recognising
/// one block too few costs a rendered prompt that carries a sentence the model was genuinely sent;
/// recognising one too many would delete text the user wrote, so every ambiguity resolves toward
/// stripping less.
///
/// **It strips the RIGHTMOST well-formed block, and that choice is what bounds the damage --
/// PROVIDED a block was appended at all.** A block `compose_turn_text` appended always reaches the
/// end of the string, so it is always one of the candidates; taking the rightmost therefore cannot
/// cut before the real block's start. Against such a text it can only under-strip, and only when
/// the *selected text carried inside the block* itself looks like a whole composed block.
///
/// **It does NOT follow that no byte a user typed can ever be removed, and an earlier revision of
/// this comment claimed exactly that.** The premise fails for a text with no appended block: the
/// wire text and the typed text are then the same string (`compose_turn_text(t, None) == t`), and a
/// user who types a message that happens to END with something shaped like a block loses it. That is
/// reachable, not theoretical -- a prompt ending in
/// `"\n\nThe user opened the file /p/a.rs in the IDE. <hedge>"`, e.g. quoting what the tool sent,
/// strips to the text before it. There is no fix available at this layer: nothing in the string says
/// whether a block was appended, and a marker that would say it would have to go on the wire, where
/// the model would read it. The consequence is bounded -- a restored prompt in the panel shows less
/// than was typed; nothing is sent, decided or stored differently -- and it is recorded here rather
/// than papered over. §11 invariant 7's "for any input" is wrong for the same reason; see this
/// module's tests, which cover the compose→strip round trip over a matrix of real shapes.
pub fn strip_composed_block(text: &str) -> &str {
    if let Some(start) = selection_block_start(text) {
        return before_block(text, start);
    }
    if let Some(start) = opened_file_block_start(text) {
        return before_block(text, start);
    }
    text
}

/// The block starts at `start`; everything before it is the user's own text, minus the `"\n\n"`
/// `compose_turn_text` inserts between the two. `start == 0` is the `user_text.is_empty()` case,
/// where the block is the entire string and no separator was written.
fn before_block(text: &str, start: usize) -> &str {
    if start == 0 {
        return "";
    }
    // `start` is only ever produced by a candidate check that already required this.
    debug_assert!(text[..start].ends_with("\n\n"));
    &text[..start - 2]
}

/// A candidate can only begin where `compose_turn_text` could have put one: at the very start, or
/// immediately after the `"\n\n"` separator it writes.
fn is_block_boundary(text: &str, index: usize) -> bool {
    index == 0 || text[..index].ends_with("\n\n")
}

/// Byte indices, right to left, where `needle` occurs in `haystack[..limit]`.
fn candidates_right_to_left(haystack: &str, limit: usize, needle: &str) -> Vec<usize> {
    let mut found = haystack[..limit]
        .match_indices(needle)
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    found.reverse();
    found
}

fn selection_block_start(text: &str) -> Option<usize> {
    let suffix = selected_suffix();
    let body_end = text.len().checked_sub(suffix.len())?;
    if !text.ends_with(&suffix) {
        return None;
    }
    for start in candidates_right_to_left(text, body_end, SELECTED_PREFIX) {
        if !is_block_boundary(text, start) {
            continue;
        }
        let body = &text[start + SELECTED_PREFIX.len()..body_end];
        if selection_body_is_well_formed(body) {
            return Some(start);
        }
    }
    None
}

/// `"{start_line} to {end_line} from {file}:\n{content}"`, with `content` free-form (it is whatever
/// the user had selected, and `compose_turn_text` may have appended its own truncation marker to
/// it). Only the frame is checked.
fn selection_body_is_well_formed(body: &str) -> bool {
    let after_start_line = body.trim_start_matches(|c: char| c.is_ascii_digit());
    if after_start_line.len() == body.len() {
        return false;
    }
    let Some(rest) = after_start_line.strip_prefix(" to ") else {
        return false;
    };
    let after_end_line = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    if after_end_line.len() == rest.len() {
        return false;
    }
    let Some(after_from) = after_end_line.strip_prefix(" from ") else {
        return false;
    };
    // The file name, then the colon-newline that introduces the selected lines.
    after_from.contains(":\n")
}

fn opened_file_block_start(text: &str) -> Option<usize> {
    let suffix = opened_suffix();
    let body_end = text.len().checked_sub(suffix.len())?;
    if !text.ends_with(&suffix) {
        return None;
    }
    for start in candidates_right_to_left(text, body_end, OPENED_PREFIX) {
        if !is_block_boundary(text, start) {
            continue;
        }
        // `compose_turn_text` only emits this template for a file name that is not blank, so a
        // zero-length name is not something it could have written.
        if start + OPENED_PREFIX.len() < body_end {
            return Some(start);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape `compose_turn_text` produces with no selection. Written out literally rather
    /// than by calling `compose_turn_text` (which lives in a crate that depends on this one), so
    /// that a change to the template on that side fails here instead of both sides agreeing on a
    /// new wrong answer. The crate-level round trip against the real function is in
    /// `core/src/editor_context/compose.rs`.
    #[test]
    fn strips_the_opened_file_block() {
        let composed = "what does this do?\n\nThe user opened the file /p/src/main.rs in the IDE. This may or may not be related to the current task.";
        assert_eq!(strip_composed_block(composed), "what does this do?");
    }

    #[test]
    fn strips_the_selection_block_including_its_multi_line_content() {
        let composed = "why is this slow?\n\nThe user selected the lines 12 to 14 from /p/src/main.rs:\nfn a() {}\nfn b() {}\n\nThis may or may not be related to the current task.";
        assert_eq!(strip_composed_block(composed), "why is this slow?");
    }

    /// `compose_turn_text` sends the block alone when the user typed nothing, with no separator.
    /// The history reader drops such a prompt entirely (§3.1.2), which it can only do if this
    /// returns the empty string rather than the whole block.
    #[test]
    fn a_block_with_no_user_text_strips_to_nothing() {
        let block = "The user opened the file /p/f.rs in the IDE. This may or may not be related to the current task.";
        assert_eq!(strip_composed_block(block), "");
        let selection = "The user selected the lines 1 to 2 from /p/f.rs:\nx\n\nThis may or may not be related to the current task.";
        assert_eq!(strip_composed_block(selection), "");
    }

    #[test]
    fn text_with_no_block_is_returned_byte_for_byte() {
        for text in [
            "just a question",
            "",
            "a prompt that ends in a sentence about the current task.",
            // Ends with the hedge, but nothing before it is a template this module wrote.
            "This may or may not be related to the current task.",
            // The right words, the wrong frame: no `\n\n` boundary before the block.
            "prompt The user opened the file /f in the IDE. This may or may not be related to the current task.",
        ] {
            assert_eq!(strip_composed_block(text), text, "input: {text:?}");
        }
    }

    /// The selection frame has to actually parse. A sentence that merely starts the same way is
    /// left alone rather than cut at a guess.
    #[test]
    fn a_selection_prefix_that_does_not_parse_is_not_a_block() {
        let text = "q\n\nThe user selected the lines of a poem\n\nThis may or may not be related to the current task.";
        assert_eq!(strip_composed_block(text), text);
    }

    /// The safety direction stated in the function's doc: when a prompt itself *contains* something
    /// block-shaped, only the trailing block is removed and the user's own words survive.
    #[test]
    fn only_the_trailing_block_is_removed_when_the_prompt_quotes_one() {
        let quoted = "look at this:\n\nThe user opened the file /a in the IDE. This may or may not be related to the current task.";
        let composed = format!(
            "{quoted}\n\nThe user opened the file /b in the IDE. This may or may not be related to the current task."
        );
        assert_eq!(strip_composed_block(&composed), quoted);
    }

    /// Stripping is idempotent only in the sense that a second pass removes the *next* block; this
    /// pins that a single call removes exactly one, which is what `compose_turn_text` appends.
    #[test]
    fn one_call_removes_exactly_one_block() {
        let composed = "q\n\nThe user opened the file /a in the IDE. This may or may not be related to the current task.\n\nThe user opened the file /b in the IDE. This may or may not be related to the current task.";
        let once = strip_composed_block(composed);
        assert_eq!(
            once,
            "q\n\nThe user opened the file /a in the IDE. This may or may not be related to the current task."
        );
        assert_eq!(strip_composed_block(once), "q");
    }
}
