//! `@buffers`, `@quickfix` and `@diagnostics` in a Claude tab's composer: what the editor holds, put in the turn that
//! names it.
//!
//! The CLI reads a typed `@path` itself, and Eitri has no expansion code for files. These
//! three names are not files, so the CLI cannot read them, and the editor is the only place that has the answer: the
//! host reads it from nvim when the message is sent and appends it to the turn as text, each section framed by a first
//! line `[@name: ...]` and a last line `[@name end]`.
//!
//! That text is the editor's and not the person's, so two rules hold for all of it:
//! - it passes through [`agent::mention::inert`], so a line of a buffer or a diagnostic's message that begins `@path`
//!   is not read by the CLI as a mention;
//! - it names no file `@file` could not read: nothing outside the project, nothing a `Read` deny rule of the CLI's
//!   settings covers ([`agent::read_deny`]), nothing whose text was read from somewhere else than its name.
//!
//! [`blocks`] is pure: the answers in, the text out. [`gather`] asks the editor.

pub mod blocks;
pub mod gather;

/// One of the editor's own sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Buffers,
    Quickfix,
    Diagnostics,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Buffers, Kind::Quickfix, Kind::Diagnostics];

    /// The word after the `@`.
    pub fn word(self) -> &'static str {
        match self {
            Kind::Buffers => "buffers",
            Kind::Quickfix => "quickfix",
            Kind::Diagnostics => "diagnostics",
        }
    }

    fn named(word: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.word() == word)
    }
}

/// The sources a person's text names, in the order they first appear, each once. A name counts as the CLI counts a
/// mention: at the start of the text or after whitespace, and the whole word up to the next whitespace; a leading
/// slash command that is the CLI's own is left alone, as the editor's context block is.
pub fn requested(text: &str) -> Vec<Kind> {
    if crate::editor_context::compose::names_a_cli_local_command(text) {
        return Vec::new();
    }
    agent::mention::editor_sources_named(text)
        .into_iter()
        .filter_map(Kind::named)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_words_are_the_ones_the_history_reader_strips() {
        let words: Vec<&str> = Kind::ALL.iter().map(|kind| kind.word()).collect();
        assert_eq!(words, agent::mention::EDITOR_SOURCES);
    }

    #[test]
    fn a_name_counts_where_the_cli_would_read_a_mention() {
        assert_eq!(requested("@buffers"), [Kind::Buffers]);
        assert_eq!(
            requested("look at @diagnostics\nand @quickfix please"),
            [Kind::Diagnostics, Kind::Quickfix]
        );
        assert_eq!(requested("a\t@buffers\u{3000}@buffers"), [Kind::Buffers], "once");
        assert_eq!(requested("\u{feff}@quickfix"), [Kind::Quickfix]);
    }

    #[test]
    fn what_the_cli_would_not_read_as_a_mention_names_nothing() {
        for text in [
            "me@buffers",
            "`@buffers`",
            "(@buffers)",
            "x:@quickfix",
            "\"@diagnostics\"",
            "\\@buffers",
            "@buffersx",
            "@buffers,",
            "@Buffers",
            "@@buffers",
            "@ buffers",
            "buffers",
            "",
        ] {
            assert!(requested(text).is_empty(), "{text:?}");
        }
    }

    #[test]
    fn the_clis_own_commands_are_left_alone() {
        assert!(requested("/compact keep @buffers in mind").is_empty());
        assert!(requested("/model @diagnostics").is_empty());
        assert_eq!(
            requested("/review @diagnostics"),
            [Kind::Diagnostics],
            "a skill's argument is prompt text"
        );
    }

    #[test]
    fn a_word_after_a_longer_one_is_still_found() {
        assert_eq!(requested("@small.txt @buffers"), [Kind::Buffers]);
        assert_eq!(requested("@sub/@buffers @quickfix"), [Kind::Quickfix]);
    }
}
