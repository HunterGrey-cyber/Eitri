//! What may enter a file a person attaches to a public issue.
//!
//! The lines Eitri prints were written to hold no conversation, and a test keeps them so. A window's output also
//! carries what libraries and child processes print, and an error can quote what it failed on, so every line takes
//! this second pass before it reaches a log. What stays is what a report needs: which part spoke, and what went wrong.

use std::any::Any;

/// The longest line kept; the rest of a longer one is cut.
pub(crate) const MAX_LINE: usize = 1024;
/// The longest text kept inside quotes or backticks. An error that quotes what it failed on quotes it this way, and
/// `{:?}` always does, while the short names a report needs (a model, a display, a field) fit.
const MAX_QUOTED: usize = 16;
/// The longest unbroken word kept: a key, a token or a session id is at least this long.
const MAX_WORD: usize = 32;
/// What the debug switches for typed keys print: a window started with one still logs nothing typed.
const NEVER_KEPT: [&[u8]; 2] = [b"[key] ", b"[ime] "];

/// What `raw`, one line without its newline, may say in a log; `None` when nothing of it may.
pub(crate) fn line(raw: &[u8], home: Option<&[u8]>) -> Option<Vec<u8>> {
    if NEVER_KEPT.iter().any(|start| raw.starts_with(start)) {
        return None;
    }
    let kept = &raw[..raw.len().min(MAX_LINE)];
    // The home first: a home with a long name is a long word, and a path is kept once it starts with `~`.
    let mut out = mask_long_words(&mask_quoted(&mask_addresses(&replace_home(kept, home))));
    if raw.len() > MAX_LINE {
        out.extend_from_slice(b" [cut]");
    }
    Some(out)
}

/// A path keeps its quotes' worth of text: it says where something is, and the home in it is replaced.
fn is_path(quoted: &[u8]) -> bool {
    matches!(quoted.first(), Some(b'/' | b'~')) && !quoted.iter().any(u8::is_ascii_whitespace)
}

/// Where the quote opened at `open` closes, skipping an escaped quote; `None` when the line ends first.
fn closing(line: &[u8], open: usize) -> Option<usize> {
    let mut at = open + 1;
    while at < line.len() {
        match line[at] {
            b'\\' => at += 2,
            byte if byte == line[open] => return Some(at),
            _ => at += 1,
        }
    }
    None
}

fn mask_quoted(line: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len());
    let mut at = 0;
    while at < line.len() {
        let byte = line[at];
        if byte != b'"' && byte != b'`' {
            out.push(byte);
            at += 1;
            continue;
        }
        let close = closing(line, at);
        let end = close.unwrap_or(line.len());
        let inner = &line[at + 1..end];
        out.push(byte);
        if inner.len() > MAX_QUOTED && !is_path(inner) {
            out.extend_from_slice("…".as_bytes());
        } else {
            out.extend_from_slice(inner);
        }
        if close.is_some() {
            out.push(byte);
        }
        at = end + 1;
    }
    out
}

/// Everything of an address after its scheme: a link in a turn says where the turn went, and its query can hold a key.
fn mask_addresses(line: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len());
    let mut at = 0;
    while at < line.len() {
        if line[at..].starts_with(b"://") {
            out.extend_from_slice("://…".as_bytes());
            at += 3;
            while line
                .get(at)
                .is_some_and(|byte| !byte.is_ascii_whitespace() && !matches!(byte, b'"' | b'`' | b')'))
            {
                at += 1;
            }
        } else {
            out.push(line[at]);
            at += 1;
        }
    }
    out
}

fn mask_long_words(line: &[u8]) -> Vec<u8> {
    let in_word = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+' | b'=');
    let mut out = Vec::with_capacity(line.len());
    let mut at = 0;
    while at < line.len() {
        let end = at + line[at..].iter().take_while(|byte| in_word(**byte)).count();
        if end == at {
            out.push(line[at]);
            at += 1;
        } else if end - at >= MAX_WORD {
            out.extend_from_slice(b"<long value>");
            at = end;
        } else {
            out.extend_from_slice(&line[at..end]);
            at = end;
        }
    }
    out
}

/// Whether the name at `end` stops where a home directory's last component does: `/home/ann`, never `/home/anna`.
fn ends_component(text: &[u8], end: usize) -> bool {
    !text
        .get(end)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn replace_home(line: &[u8], home: Option<&[u8]>) -> Vec<u8> {
    let mut out = line.to_vec();
    if let Some(home) = home {
        let len = strip_home(&mut out, home);
        out.truncate(len);
    }
    out
}

/// Replaces `home` with `~` in `text`, in place, and returns the length left. Allocates nothing: a panic hook uses it.
pub(crate) fn strip_home(text: &mut [u8], home: &[u8]) -> usize {
    if home.is_empty() {
        return text.len();
    }
    let (mut read, mut write) = (0, 0);
    while read < text.len() {
        if text[read..].starts_with(home) && ends_component(text, read + home.len()) {
            text[write] = b'~';
            write += 1;
            read += home.len();
        } else {
            text[write] = text[read];
            write += 1;
            read += 1;
        }
    }
    write
}

/// Said in place of a panic's message when it was built at run time and is not one of the known shapes.
pub(crate) const WITHHELD: &str =
    "(withheld: this message was built at run time and may quote what the program was handling; \
     the location says which panic it is)";

/// What the standard library's panics about values say, up to where they would quote one. A string slice's panic
/// prints up to 256 bytes of the string, `unwrap` prints the error, and `assert_eq!` prints both values; all of those
/// can be a prompt or a tool's output.
const QUOTES_A_VALUE: [(&str, &str); 12] = [
    (
        "called `Result::unwrap()` on an `Err` value",
        "called `Result::unwrap()` on an `Err` value",
    ),
    (
        "called `Result::unwrap_err()` on an `Ok` value",
        "called `Result::unwrap_err()` on an `Ok` value",
    ),
    (
        "called `Result::expect_err()` on an `Ok` value",
        "called `Result::expect_err()` on an `Ok` value",
    ),
    ("assertion `left == right` failed", "assertion `left == right` failed"),
    ("assertion `left != right` failed", "assertion `left != right` failed"),
    (
        "assertion `left matches right` failed",
        "assertion `left matches right` failed",
    ),
    ("byte index ", "a string was cut inside a character or past its end"),
    (
        "start byte index ",
        "a string was cut inside a character or past its end",
    ),
    ("end byte index ", "a string was cut inside a character or past its end"),
    ("begin <= end ", "a string was cut with its ends reversed"),
    (
        "failed to slice string",
        "a string was cut inside a character or past its end",
    ),
    (
        "str::floor_char_boundary",
        "a string was cut inside a character or past its end",
    ),
];

/// Those that carry numbers or a system error and nothing of what the program was handling.
const SAFE_WHOLE: [&str; 9] = [
    "index out of bounds: ",
    "range start index ",
    "range end index ",
    "slice index starts at ",
    "failed printing to std",
    "RefCell already ",
    "already borrowed",
    "already mutably borrowed",
    "called `Option::",
];

/// The longest message kept.
const MAX_MESSAGE: usize = 512;

fn cut_at_char(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// What the record says of a panic's message. A literal is in the published source, so it is kept. A message built at
/// run time is kept only as far as its shape is known to hold nothing of what the program was handling.
pub(crate) fn panic_message<'a>(payload: &'a (dyn Any + Send)) -> &'a str {
    if let Some(literal) = payload.downcast_ref::<&'static str>() {
        return cut_at_char(literal, MAX_MESSAGE);
    }
    let Some(built) = payload.downcast_ref::<String>() else {
        return WITHHELD;
    };
    if let Some((_, kept)) = QUOTES_A_VALUE.iter().find(|(start, _)| built.starts_with(start)) {
        return kept;
    }
    let single_line = !built.contains('\n') && built.len() <= MAX_MESSAGE;
    if single_line && SAFE_WHOLE.iter().any(|start| built.starts_with(start)) {
        return built;
    }
    WITHHELD
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scrubbed(raw: &str, home: Option<&str>) -> String {
        String::from_utf8(line(raw.as_bytes(), home.map(str::as_bytes)).unwrap()).unwrap()
    }

    #[test]
    fn an_ordinary_line_is_left_alone() {
        let raw = "[layout] the layout as the pair file left it is written";
        assert_eq!(scrubbed(raw, None), raw);
        assert_eq!(scrubbed("model \"opus\" field `id`", None), "model \"opus\" field `id`");
    }

    #[test]
    fn long_quoted_text_goes_and_an_escaped_quote_does_not_end_it() {
        let said = scrubbed(
            r#"invalid type: string "please fix the \"auth\" bug in main", expected u32"#,
            None,
        );
        assert_eq!(said, r#"invalid type: string "…", expected u32"#);
        assert_eq!(
            scrubbed("unknown variant `a long variant from a message`", None),
            "unknown variant `…`"
        );
    }

    #[test]
    fn a_quote_the_line_never_closes_goes_too() {
        assert_eq!(scrubbed("raw: \"the rest of the prompt runs on", None), "raw: \"…");
    }

    #[test]
    fn a_quoted_path_stays_with_its_home_replaced() {
        assert_eq!(
            scrubbed("\"/home/ann/work/project/a-rather-long-name.rs\"", Some("/home/ann")),
            "\"~/work/project/a-rather-long-name.rs\""
        );
    }

    #[test]
    fn a_home_is_replaced_only_as_a_whole_component() {
        assert_eq!(
            scrubbed("/home/ann/x and /home/anna/y", Some("/home/ann")),
            "~/x and /home/anna/y"
        );
        assert_eq!(scrubbed("root /home/ann", Some("/home/ann")), "root ~");
    }

    #[test]
    fn a_word_as_long_as_a_key_goes() {
        let key = format!("sk-{}-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789", "stand-in");
        assert_eq!(scrubbed(&format!("key {key} used"), None), "key <long value> used");
        let id = "1857dcd5-973b-46a2-8ac2-1234567890ab";
        assert_eq!(scrubbed(&format!("session {id}"), None), "session <long value>");
        assert_eq!(scrubbed("project 410d926fb96fb107", None), "project 410d926fb96fb107");
    }

    #[test]
    fn an_address_keeps_only_its_scheme() {
        assert_eq!(
            scrubbed("opening https://example.org/reset?code=abc failed: no handler", None),
            "opening https://… failed: no handler"
        );
        assert_eq!(
            scrubbed("url \"file:///a\" (see http://x)", None),
            "url \"file://…\" (see http://…)"
        );
    }

    #[test]
    fn typed_keys_never_reach_a_log() {
        assert_eq!(line(b"[key] \"a\" state=empty -> \"a\"", None), None);
        assert_eq!(line(b"[ime] preedit-changed: \"x\"", None), None);
    }

    #[test]
    fn a_long_line_is_cut_and_says_so() {
        let said = scrubbed(&"a ".repeat(MAX_LINE), None);
        assert!(said.ends_with(" [cut]"));
        assert!(said.len() <= MAX_LINE + " [cut]".len());
    }

    #[test]
    fn a_line_of_any_bytes_is_handled() {
        assert_eq!(line(b"\xff\xfe \"\xff", None), Some(b"\xff\xfe \"\xff".to_vec()));
    }

    #[test]
    fn strip_home_works_in_place() {
        let mut text = b"/home/ann/a /home/ann/b".to_vec();
        let len = strip_home(&mut text, b"/home/ann");
        assert_eq!(&text[..len], b"~/a ~/b");
    }

    fn message_of(payload: Box<dyn Any + Send>) -> String {
        panic_message(payload.as_ref()).to_string()
    }

    #[test]
    fn a_literal_message_is_kept_and_a_built_one_is_not() {
        assert_eq!(
            message_of(Box::new("the grid has no such pane")),
            "the grid has no such pane"
        );
        assert_eq!(message_of(Box::new(format!("bad prompt: {}", "my secret"))), WITHHELD);
        assert_eq!(message_of(Box::new(7_u32)), WITHHELD);
    }

    /// The real panics, so a new toolchain that rewords one shows up here.
    fn real(body: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let quiet = std::panic::catch_unwind(body).unwrap_err();
        panic_message(quiet.as_ref()).to_string()
    }

    #[test]
    fn the_standard_panics_lose_the_value_and_keep_their_shape() {
        const SECRET: &str = "the words of a private prompt";
        let said = real(|| {
            let _ = &format!("é{SECRET}")[1..];
        });
        assert!(!said.contains("private") && !said.contains("withheld"), "{said}");
        let said = real(|| {
            let failed: Result<(), String> = Err(SECRET.to_string());
            failed.unwrap();
        });
        assert_eq!(said, "called `Result::unwrap()` on an `Err` value");
        let said = real(|| assert_eq!(SECRET, "the other words"));
        assert_eq!(said, "assertion `left == right` failed");
        let said = real(|| {
            let failed: Result<(), String> = Err(SECRET.to_string());
            failed.expect("the store opens");
        });
        assert_eq!(said, WITHHELD);
    }

    #[test]
    fn a_panic_about_numbers_is_kept_whole() {
        let empty: Vec<u8> = Vec::new();
        let at = std::hint::black_box(3);
        let said = real(std::panic::AssertUnwindSafe(|| {
            let _ = empty[at];
        }));
        assert!(said.starts_with("index out of bounds: the len is 0"), "{said}");
    }
}
