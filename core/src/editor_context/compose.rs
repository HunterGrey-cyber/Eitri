//! Turning what the editor is showing into the text that rides with one turn.
//!
//! Pure: no socket, no nvim, no GTK. The transport half is [`super::feed`]; this half is what
//! decides what the model actually reads, and it is the half worth testing hardest.

/// A live visual selection, as nvim reports it while the selection still exists.
///
/// `text` is the selected lines themselves, not a coordinate pair, and that is not a nicety. The
/// CLI has no pull path and no dirty check -- `getCurrentSelection`, `getLatestSelection` and
/// `checkDocumentDirty` occur **zero** times in the 2.1.272 binary -- so on a buffer with unsaved
/// changes a line range is not merely stale, it is unfalsifiable: nothing downstream can discover
/// that the file on disk says something else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
}

/// What the editor was showing when the turn was sent.
///
/// `selection: None` is a state of its own, never "an empty selection". Measured on real nvim: with
/// no selection ever made, `getpos('v')` silently equals `getpos('.')` and `getregion` returns a
/// plausible one-character region rather than erroring or returning empty -- so a reader that asks
/// `getregion` first and infers from its answer cannot tell "one character" from "nothing". Two
/// surveyed plugins degrade an unset selection into *the whole buffer*; here that would fire on
/// every turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorContext {
    /// Absolute path of the focused buffer's file. Empty for a buffer with no file, which is why
    /// [`compose_turn_text`] checks it rather than assuming a name exists.
    pub file: String,
    pub selection: Option<Selection>,
}

/// The CLI's own cap on how much selected text it forwards, and its own marker.
///
/// Both read byte-for-byte out of `~/.local/share/claude/approved` (2.1.272, the build production
/// execs): `N2r = 2000`, and the helper appends `"\n... (truncated)"`. Copied rather than chosen so
/// a selection reaches the model looking exactly like one from any other editor that integrates
/// with this CLI.
pub const CONTENT_LIMIT: usize = 2000;
pub const TRUNCATION_MARKER: &str = "\n... (truncated)";

/// Appends what the editor is showing to the user's own text.
///
/// The two templates are the CLI's, verbatim. The trailing hedge sentence is **load-bearing**: with
/// it, a cursor position is context the model may ignore; without it, a passive position reads as
/// an instruction, and the model starts acting on whatever file happens to be open.
///
/// Returns the user's text unchanged when there is nothing worth saying -- no context at all, or a
/// buffer with no file. A turn that carries a context block saying nothing is worse than one
/// carrying none: it spends the model's attention and teaches the reader to ignore the block.
pub fn compose_turn_text(user_text: &str, context: Option<&EditorContext>) -> String {
    let Some(context) = context else { return user_text.to_string() };
    if context.file.trim().is_empty() {
        return user_text.to_string();
    }
    let block = match &context.selection {
        Some(selection) => format!(
            "The user selected the lines {} to {} from {}:\n{}\n\nThis may or may not be related to the current task.",
            selection.start_line,
            selection.end_line,
            context.file,
            truncate(&selection.text),
        ),
        // File name only, zero content -- the CLI's own asymmetry, and gemini-cli's. Sending the
        // whole buffer here is the degradation this module exists to avoid.
        None => format!(
            "The user opened the file {} in the IDE. This may or may not be related to the current task.",
            context.file
        ),
    };
    if user_text.is_empty() {
        return block;
    }
    format!("{user_text}\n\n{block}")
}

/// Truncates on a CHARACTER boundary, not a byte one.
///
/// `&s[..CONTENT_LIMIT]` panics mid-codepoint, and a selection of CJK or emoji reaches this
/// function as a matter of course in this project -- the editor's own IME work exists because the
/// owner types Chinese into it.
fn truncate(content: &str) -> String {
    if content.chars().count() <= CONTENT_LIMIT {
        return content.to_string();
    }
    let kept: String = content.chars().take(CONTENT_LIMIT).collect();
    format!("{kept}{TRUNCATION_MARKER}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(file: &str, selection: Option<Selection>) -> EditorContext {
        EditorContext { file: file.to_string(), selection }
    }

    #[test]
    fn a_selection_carries_its_lines_and_their_real_text() {
        let out = compose_turn_text(
            "why is this slow?",
            Some(&ctx("/p/src/main.rs", Some(Selection { start_line: 12, end_line: 14, text: "fn a() {}".into() }))),
        );
        assert_eq!(
            out,
            "why is this slow?\n\nThe user selected the lines 12 to 14 from /p/src/main.rs:\nfn a() {}\n\nThis may or may not be related to the current task."
        );
    }

    /// The asymmetry is the whole design: no selection means the file NAME, and nothing else.
    #[test]
    fn no_selection_sends_the_file_name_and_not_one_byte_of_content() {
        let out = compose_turn_text("hi", Some(&ctx("/p/src/main.rs", None)));
        assert_eq!(
            out,
            "hi\n\nThe user opened the file /p/src/main.rs in the IDE. This may or may not be related to the current task."
        );
    }

    /// Without the hedge a passive cursor position reads as an instruction. It is quoted here so
    /// that deleting it from the templates fails a test rather than quietly changing what the model
    /// believes it was asked to do.
    #[test]
    fn both_templates_carry_the_hedge_sentence() {
        const HEDGE: &str = "This may or may not be related to the current task.";
        let with = compose_turn_text("q", Some(&ctx("/f", Some(Selection { start_line: 1, end_line: 1, text: "x".into() }))));
        let without = compose_turn_text("q", Some(&ctx("/f", None)));
        assert!(with.ends_with(HEDGE), "{with}");
        assert!(without.ends_with(HEDGE), "{without}");
    }

    #[test]
    fn no_context_leaves_the_users_text_exactly_as_typed() {
        assert_eq!(compose_turn_text("just a question", None), "just a question");
    }

    /// A scratch buffer has no file. Saying "the user opened the file  in the IDE" is worse than
    /// saying nothing: it is a sentence that reads as information and carries none.
    #[test]
    fn a_buffer_with_no_file_contributes_nothing() {
        assert_eq!(compose_turn_text("q", Some(&ctx("", None))), "q");
        assert_eq!(compose_turn_text("q", Some(&ctx("   ", Some(Selection { start_line: 1, end_line: 2, text: "x".into() })))), "q");
    }

    #[test]
    fn selected_text_longer_than_the_cli_s_own_limit_is_truncated_and_says_so() {
        // `Z`, not `a`: the templates themselves contain `a` ("related", "task"), so counting `a`
        // counts the frame as well as the content. The first draft of this test did exactly that
        // and failed against correct code.
        let long = "Z".repeat(CONTENT_LIMIT + 50);
        let out = compose_turn_text("q", Some(&ctx("/f", Some(Selection { start_line: 1, end_line: 9, text: long }))));
        assert!(out.contains(TRUNCATION_MARKER), "the marker must be present: {}", &out[..120]);
        assert_eq!(out.matches('Z').count(), CONTENT_LIMIT);
    }

    /// `&s[..2000]` panics mid-codepoint. The owner types Chinese into this editor; a selection of
    /// it is not an exotic case here.
    #[test]
    fn truncation_counts_characters_not_bytes_so_multibyte_text_cannot_panic() {
        let long = "字".repeat(CONTENT_LIMIT + 10);
        let out = compose_turn_text("q", Some(&ctx("/f", Some(Selection { start_line: 1, end_line: 2, text: long }))));
        assert_eq!(out.matches('字').count(), CONTENT_LIMIT);
        assert!(out.contains(TRUNCATION_MARKER));
    }

    /// Exactly at the limit is not truncated -- an off-by-one here adds a "(truncated)" to text that
    /// is complete, which is a lie the model has no way to check.
    #[test]
    fn content_exactly_at_the_limit_is_not_marked_truncated() {
        let exact = "Z".repeat(CONTENT_LIMIT);
        let out = compose_turn_text("q", Some(&ctx("/f", Some(Selection { start_line: 1, end_line: 2, text: exact }))));
        assert!(!out.contains(TRUNCATION_MARKER), "{out}");
    }

    /// An empty prompt with context still sends the context, and without a leading blank line.
    #[test]
    fn an_empty_prompt_still_carries_the_context_cleanly() {
        let out = compose_turn_text("", Some(&ctx("/f", None)));
        assert!(out.starts_with("The user opened"), "{out}");
    }
}
