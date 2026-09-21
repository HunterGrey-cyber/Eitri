//! Removes the editor-context block that `compose_turn_text` appended to a stored prompt.
//!
//! This function lives in `agent` because the core crate already depends on `agent`.
//! Putting it in core would create a dependency cycle for the history reader. The core
//! editor-context module re-exports it. The round-trip test beside `compose_turn_text` in
//! `core/src/editor_context/compose.rs` checks `strip(compose(t, ctx)) == t`, so changes to
//! either direction must remain compatible.
//!
//! Stored prompts contain the text sent to the model, including appended editor context.
//! `AgentDomainEvent::UserPromptSubmitted` expects the user's own text. Stripping the block
//! keeps generated file paths and selections out of the displayed prompt. The block's
//! format is defined by `compose.rs`.

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
/// Returns the input unchanged unless its suffix matches this module's templates. Ambiguous text
/// is stripped less: leaving a block visible is preferable to deleting text the user wrote.
///
/// The rightmost well-formed block is removed. If a block was appended, it reaches the end of the
/// string and is a candidate, so stripping cannot begin before that block. Selected text that
/// itself resembles a composed block can cause only part of the appended block to be removed.
///
/// When no block was appended, `compose_turn_text(t, None) == t`. User text ending in a block-shaped
/// suffix can therefore lose that suffix. For example, a message quoting
/// `"\n\nThe user opened the file /p/a.rs in the IDE. <hedge>"` can be shortened this way.
/// The string contains no indication of whether a block was appended. This affects only the
/// restored prompt displayed in the panel; nothing is sent, decided or stored differently.
/// The tests check composition followed by stripping across different input shapes.
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
    /// The history reader drops such prompts, so stripping must return an empty string rather than
    /// the whole block.
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
