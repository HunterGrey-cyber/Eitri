//! Bounds on what the laptop keeps of a server's answers.
//!
//! The server account may lie in what its helper says; the gate and the sync checks treat its words as
//! claims. A lie must not cost the laptop memory without end, so every answer the laptop collects (the
//! items of a stream, a list in a unary answer it keeps) is held to an [`AnswerBound`]: a count of items
//! and a byte budget. Each item is charged its encoded size plus [`ITEM_OVERHEAD_BYTES`], so items that
//! encode to nothing are not free. An answer past either bound is refused whole with a line that names
//! the bound; it is never cut short and used, which would read part of a listing or a file as all of it.
//!
//! One message is also bounded by the transport's decode limit (tonic's 4 MiB default for the helper
//! client); that limit bounds one item, not how many arrive.

use prost::Message;

/// What one item costs beyond its encoded bytes: its struct and allocation headers in memory.
pub const ITEM_OVERHEAD_BYTES: usize = 128;

/// The average a file listing may spend per entry. An entry costs its path plus about 15 bytes of fields
/// and [`ITEM_OVERHEAD_BYTES`], so a full listing fits while its paths average up to about 240 bytes.
pub const LIST_TREE_ENTRY_BYTES: usize = 384;

/// What a file read may spend beyond its data limit, for any limit: the chunks' own fields and costs.
pub const READ_FILE_FRAMING_BYTES: usize = 1 << 20;

/// And beyond that, a sixteenth of the data limit. The helper sends each read of the file as one chunk
/// (at most 64 KiB, about 0.2 % of it framing); a sixteenth still covers a mount that reads 4 KiB at a
/// time (about 3 %).
pub const READ_FILE_FRAMING_SHARE: usize = 16;

/// The mode keepers' lines in one Hello: one or two per root the server keeps.
pub const MODE_WARNINGS: AnswerBound = AnswerBound::new("the server's mode lines", 256, 256 << 10);

/// The settings stream's per-message allowance beyond a file's own bytes: its framing and the longest paths and
/// text one message carries (the scope names a home, a config directory and a root).
pub const SETTINGS_MESSAGE_FRAMING_BYTES: usize =
    ITEM_OVERHEAD_BYTES + 3 * eitri_server_proto::TRANSCRIPT_PATH_MAX + eitri_server_proto::TRANSCRIPT_REFUSAL_MAX;

/// What one entry of a transcript list may spend beyond its title: the id, the numbers and the framing.
pub const TRANSCRIPT_ENTRY_FRAMING_BYTES: usize = ITEM_OVERHEAD_BYTES + 128;

/// The longest identifier a server's answer may name (a sidecar's, say); the helper's are 16 hex digits.
pub const ID_BYTES: usize = 128;

/// An identifier from a server's answer, which the laptop keeps in its records and sends back in every
/// call's metadata: 1 to [`ID_BYTES`] letters, digits, `-`, `_` or `.`; the refusal naming `what` if not.
pub fn checked_id(what: &str, id: &str) -> Result<(), String> {
    let fits = !id.is_empty()
        && id.len() <= ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte));
    fits.then_some(()).ok_or_else(|| {
        format!("{what} is not an identifier of at most {ID_BYTES} letters, digits, '-', '_' or '.'; refused")
    })
}

/// A text or blob in a server's answer of `len` bytes against its longest allowed, `max`: the refusal naming `what`.
pub fn checked_len(what: &str, len: usize, max: usize) -> Result<(), String> {
    (len <= max).then_some(()).ok_or_else(|| {
        format!(
            "{what} is {} long, past its bound of {}; refused",
            size_text(len),
            size_text(max)
        )
    })
}

/// A session id from a server's answer, which the laptop shows and sends back: a lowercase hyphenated UUID, the form
/// the CLI names its transcripts in; the refusal naming `what` if not.
pub fn checked_session_uuid(what: &str, id: &str) -> Result<(), String> {
    crate::transcript::is_canonical_session_uuid(id)
        .then_some(())
        .ok_or_else(|| format!("{what} is not a lowercase hyphenated UUID; refused"))
}

/// A count and a byte budget for one answer, and the words that name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnswerBound {
    what: &'static str,
    entries: usize,
    bytes: usize,
}

impl AnswerBound {
    pub const fn new(what: &'static str, entries: usize, bytes: usize) -> Self {
        Self { what, entries, bytes }
    }

    /// A ListTree answer for a request with this `budget`: the budget's entries, a truncation marker
    /// and a refusal, at [`LIST_TREE_ENTRY_BYTES`] each.
    pub fn list_tree(budget: u64) -> Self {
        let entries = usize::try_from(budget).unwrap_or(usize::MAX).saturating_add(2);
        Self::new(
            "the server's file listing",
            entries,
            entries.saturating_mul(LIST_TREE_ENTRY_BYTES),
        )
    }

    /// A ReadFile answer for a request with this data `limit`. The byte budget, which charges each
    /// chunk [`ITEM_OVERHEAD_BYTES`], is what bounds the number of chunks.
    pub fn read_file(limit: u64) -> Self {
        Self::data_read("the server's file read", limit)
    }

    /// A transcript tail: at most [`eitri_server_proto::TRANSCRIPT_TAIL_MAX`] bytes of data, in chunks, with the
    /// framing a file read is allowed.
    pub fn transcript_tail() -> Self {
        Self::data_read("the server's transcript tail", eitri_server_proto::TRANSCRIPT_TAIL_MAX)
    }

    /// A transcript list: at most [`eitri_server_proto::TRANSCRIPTS_LIST_MAX`] entries, each with a title of at most
    /// [`eitri_server_proto::TRANSCRIPT_TITLE_MAX`] bytes. A title past its own bound is the caller's check.
    pub fn transcript_list() -> Self {
        let entries = eitri_server_proto::TRANSCRIPTS_LIST_MAX;
        Self::new(
            "the server's transcript list",
            entries,
            entries * (eitri_server_proto::TRANSCRIPT_TITLE_MAX + TRANSCRIPT_ENTRY_FRAMING_BYTES),
        )
    }

    /// A settings stream: a scope, at most [`eitri_server_proto::SETTINGS_FILES_MAX`] files and, in place of the
    /// rest, a refusal; the files' data together at most [`eitri_server_proto::SETTINGS_TOTAL_MAX`].
    pub fn settings_stream() -> Self {
        let entries = eitri_server_proto::SETTINGS_FILES_MAX + 2;
        Self::new(
            "the server's settings",
            entries,
            (eitri_server_proto::SETTINGS_TOTAL_MAX as usize)
                .saturating_add(entries.saturating_mul(SETTINGS_MESSAGE_FRAMING_BYTES)),
        )
    }

    fn data_read(what: &'static str, limit: u64) -> Self {
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        Self::new(
            what,
            usize::MAX,
            limit
                .saturating_add(READ_FILE_FRAMING_BYTES)
                .saturating_add(limit / READ_FILE_FRAMING_SHARE),
        )
    }

    /// A ReadPath answer for a request with this `limit` (0 meaning the server's maximum): the data at most the
    /// effective cap, framed as [`Self::read_file`] is.
    pub fn read_path(limit: u64) -> Self {
        let most = eitri_server_proto::READ_PATH_MAX as u64;
        let cap = if limit == 0 { most } else { limit.min(most) };
        let mut bound = Self::read_file(cap);
        bound.what = "the server's path read";
        bound
    }

    pub fn entries(&self) -> usize {
        self.entries
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// What an item counts against a byte budget.
    pub fn cost<T: Message>(item: &T) -> usize {
        item.encoded_len().saturating_add(ITEM_OVERHEAD_BYTES)
    }

    /// The refusal of an answer with more items than the bound allows.
    pub fn past_entries(&self) -> String {
        format!("{} passed its bound of {} entries; refused", self.what, self.entries)
    }

    /// The refusal of an answer that costs more than the bound's bytes.
    pub fn past_bytes(&self) -> String {
        format!("{} passed its bound of {}; refused", self.what, size_text(self.bytes))
    }

    /// Whether `text` is this bound's own refusal (and not, say, a server's error text passed on).
    pub fn refused(&self, text: &str) -> bool {
        text == self.past_entries() || text == self.past_bytes()
    }

    /// A tally for items that arrive one at a time.
    pub fn tally(&self) -> Tally<'_> {
        Tally {
            bound: self,
            entries: 0,
            bytes: 0,
        }
    }

    /// Whether an answer of `entries` items costing `bytes` in all is within the bound; the refusal if not.
    pub fn admits(&self, entries: usize, bytes: usize) -> Result<(), String> {
        if entries > self.entries {
            return Err(self.past_entries());
        }
        if bytes > self.bytes {
            return Err(self.past_bytes());
        }
        Ok(())
    }

    /// Items already in memory (a unary answer's list) against the bound.
    pub fn check<'a, T: Message + 'a>(&self, items: impl IntoIterator<Item = &'a T>) -> Result<(), String> {
        let mut tally = self.tally();
        items.into_iter().try_for_each(|item| tally.add(item))
    }
}

/// Items counted so far against one [`AnswerBound`].
#[derive(Debug)]
pub struct Tally<'a> {
    bound: &'a AnswerBound,
    entries: usize,
    bytes: usize,
}

impl Tally<'_> {
    /// Counts one more item; the refusal when it takes the answer past the bound.
    pub fn add<T: Message>(&mut self, item: &T) -> Result<(), String> {
        let entries = self.entries.saturating_add(1);
        let bytes = self.bytes.saturating_add(AnswerBound::cost(item));
        self.bound.admits(entries, bytes)?;
        (self.entries, self.bytes) = (entries, bytes);
        Ok(())
    }
}

fn size_text(bytes: usize) -> String {
    const MIB: usize = 1 << 20;
    const KIB: usize = 1 << 10;
    if bytes >= MIB && bytes % MIB == 0 {
        format!("{} MiB", bytes / MIB)
    } else if bytes >= KIB && bytes % KIB == 0 {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_server_proto::{ListTreeEntry, ReadFileChunk, SyncModeWarning};

    fn entry(rel: &str) -> ListTreeEntry {
        ListTreeEntry {
            rel: rel.into(),
            kind: "file".into(),
            ..Default::default()
        }
    }

    #[test]
    fn an_answer_within_both_bounds_is_kept_and_one_item_more_is_refused() {
        let items = [entry("a"), entry("b")];
        let fits = AnswerBound::new("a listing", 2, items.iter().map(AnswerBound::cost).sum());
        assert_eq!(fits.check(&items), Ok(()));
        let one_fewer = AnswerBound::new("a listing", 1, usize::MAX);
        assert_eq!(
            one_fewer.check(&items),
            Err("a listing passed its bound of 1 entries; refused".into())
        );
        let a_byte_short = AnswerBound::new("a listing", 2, fits.bytes() - 1);
        assert_eq!(
            a_byte_short.check(&items),
            Err(format!(
                "a listing passed its bound of {} bytes; refused",
                fits.bytes() - 1
            ))
        );
    }

    #[test]
    fn one_long_path_is_charged_its_bytes_not_one_entry() {
        let bound = AnswerBound::list_tree(10);
        assert_eq!(bound.entries(), 12);
        assert_eq!(bound.bytes(), 12 * LIST_TREE_ENTRY_BYTES);
        let long = entry(&"x".repeat(bound.bytes()));
        assert_eq!(
            bound.check([&long]),
            Err(format!(
                "the server's file listing passed its bound of {}; refused",
                size_text(bound.bytes())
            ))
        );
    }

    #[test]
    fn empty_chunks_are_not_free() {
        let bound = AnswerBound::read_file(0);
        let empty = ReadFileChunk::default();
        assert_eq!(empty.encoded_len(), 0);
        let fits = READ_FILE_FRAMING_BYTES / ITEM_OVERHEAD_BYTES;
        assert_eq!(bound.check(std::iter::repeat_n(&empty, fits)), Ok(()));
        assert_eq!(
            bound.check(std::iter::repeat_n(&empty, fits + 1)),
            Err("the server's file read passed its bound of 1 MiB; refused".into())
        );
    }

    /// A 64 MiB file read 4 KiB at a time fits; the same data with each chunk padded by a 2 KiB field does not.
    #[test]
    fn a_read_in_small_chunks_fits_and_padded_chunks_do_not() {
        let limit = 64u64 << 20;
        let bound = AnswerBound::read_file(limit);
        let small = ReadFileChunk {
            data: vec![0; 4096],
            ..Default::default()
        };
        let count = (limit / 4096) as usize;
        assert_eq!(bound.check(std::iter::repeat_n(&small, count)), Ok(()));
        let padded = ReadFileChunk {
            refusal: "r".repeat(2048),
            ..small.clone()
        };
        assert!(bound.check(std::iter::repeat_n(&padded, count)).is_err());
    }

    #[test]
    fn a_reads_framing_is_charged_beside_its_data() {
        let limit = 64 << 10;
        let bound = AnswerBound::read_file(limit);
        let chunk = |data: usize, refusal: usize| ReadFileChunk {
            data: vec![0; data],
            refusal: "r".repeat(refusal),
            ..Default::default()
        };
        assert_eq!(bound.check([&chunk(limit as usize, 0)]), Ok(()));
        let past = bound.bytes() + 1;
        assert!(bound.check([&chunk(0, past)]).is_err());
    }

    #[test]
    fn mode_lines_are_bounded_in_count_and_bytes() {
        let line = |text: usize| SyncModeWarning {
            root: "/srv/p".into(),
            text: "t".repeat(text),
            ..Default::default()
        };
        let lines: Vec<_> = (0..MODE_WARNINGS.entries()).map(|_| line(10)).collect();
        assert_eq!(MODE_WARNINGS.check(&lines), Ok(()));
        let more: Vec<_> = (0..=MODE_WARNINGS.entries()).map(|_| line(10)).collect();
        assert_eq!(
            MODE_WARNINGS.check(&more),
            Err("the server's mode lines passed its bound of 256 entries; refused".into())
        );
        let refusal = MODE_WARNINGS.check([&line(MODE_WARNINGS.bytes())]).unwrap_err();
        assert_eq!(refusal, "the server's mode lines passed its bound of 256 KiB; refused");
        assert!(MODE_WARNINGS.refused(&refusal));
        assert!(!MODE_WARNINGS.refused("status: Unavailable, message: \"the server's mode lines passed\""));
    }

    #[test]
    fn an_identifier_is_short_and_plain_or_refused() {
        assert_eq!(checked_id("the id", "0123456789abcdef"), Ok(()));
        assert_eq!(checked_id("the id", &"a".repeat(ID_BYTES)), Ok(()));
        for bad in [
            String::new(),
            "a".repeat(ID_BYTES + 1),
            "a/b".into(),
            "a\nb".into(),
            "é".into(),
        ] {
            assert_eq!(
                checked_id("the server's sidecar id", &bad),
                Err(
                    "the server's sidecar id is not an identifier of at most 128 letters, digits, '-', '_' or '.'; \
                     refused"
                        .into()
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_field_past_its_bound_and_a_session_id_not_in_canonical_form_are_refused() {
        assert_eq!(checked_len("a title", 1024, 1024), Ok(()));
        assert_eq!(
            checked_len("a title", 1025, 1024),
            Err("a title is 1025 bytes long, past its bound of 1 KiB; refused".into())
        );
        let id = "0b7a7d3e-5f0c-4c52-9a0e-3a1f0c6d2b11";
        assert_eq!(checked_session_uuid("the id", id), Ok(()));
        for bad in [
            id.to_uppercase(),
            id.replace('-', ""),
            format!("{{{id}}}"),
            format!("{id}.jsonl"),
        ] {
            assert_eq!(
                checked_session_uuid("the id", &bad),
                Err("the id is not a lowercase hyphenated UUID; refused".into()),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_transcripts_bounds_follow_the_wire_bounds() {
        let list = AnswerBound::transcript_list();
        assert_eq!(list.entries(), eitri_server_proto::TRANSCRIPTS_LIST_MAX);
        assert!(list.bytes() > list.entries() * eitri_server_proto::TRANSCRIPT_TITLE_MAX);
        let tail = AnswerBound::transcript_tail();
        assert_eq!(tail.bytes(), (35 << 20));
        let settings = AnswerBound::settings_stream();
        assert_eq!(settings.entries(), eitri_server_proto::SETTINGS_FILES_MAX + 2);
        assert!(settings.bytes() > eitri_server_proto::SETTINGS_TOTAL_MAX as usize);
        assert!(tail
            .past_bytes()
            .starts_with("the server's transcript tail passed its bound of 35 MiB"));
    }

    #[test]
    fn sizes_are_named_in_the_largest_whole_unit() {
        assert_eq!(size_text(3 << 20), "3 MiB");
        assert_eq!(size_text(3 << 10), "3 KiB");
        assert_eq!(size_text((3 << 20) + 1), "3145729 bytes");
        assert_eq!(size_text(12), "12 bytes");
    }
}
