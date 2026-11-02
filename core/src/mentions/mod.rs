//! Expands `@buffers`, `@quickfix` and `@diagnostics` from the editor into the turn naming them.
//!
//! The CLI reads typed `@path` mentions itself; file mentions are not expanded here. These three
//! names are editor data rather than files. When sending a message, the application reads nvim and
//! appends each section between `[@name: ...]` and `[@name end]` lines.
//!
//! Editor text passes through [`agent::mention::inert`] so embedded `@path` lines cannot become
//! CLI mentions. It includes no file a direct `@file` mention could not read: files outside the
//! project, files covered by CLI `Read` deny settings ([`agent::read_deny`]), and text read from
//! a different path than the one named are excluded.
//!
//! [`blocks`] is pure: answers in, text out. [`gather`] asks the editor.

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
