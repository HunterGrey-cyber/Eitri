//! Hyprland's dispatch commands, and how to read its answers.
//!
//! Hyprland takes dispatches in one of two forms, chosen by the config manager it runs, not by its version. With
//! a `hyprland.conf` (up to 0.56) `hyprctl dispatch movefocus l` names a legacy dispatcher. With a Lua config
//! (0.55 and later; the only kind from 0.57) the text after `dispatch` is spliced into Lua source as
//! `return hl.dispatch(<text>)` and evaluated inside the compositor, which can start processes. So a command
//! is written here only from typed values -- an enum direction, a `u32` pid, an [`OutputName`] that passed
//! [`super::valid_output_name`], a [`HyprAddress`] -- by one function, `command_texts`. Window titles and classes
//! never reach it.
//!
//! `hyprctl eval 'return 1'` tells the two managers apart: the Lua one answers `ok`; the legacy one answers that
//! eval needs the Lua manager, and a Hyprland without `eval` answers "unknown request".
//!
//! Every dispatch answers `ok` on success and anything else on failure, on stdout, with exit status 0 in most
//! cases, so a caller must read the reply. A `--batch` request answers each command's reply joined by three
//! newlines and always exits 0.

use crate::layout::Direction;

/// Which dispatch syntax the running Hyprland takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HyprForm {
    /// Named dispatchers (`movefocus l`), read by the hyprlang config manager.
    Legacy,
    /// Lua dispatcher calls, read by the Lua config manager.
    Lua,
}

impl HyprForm {
    /// The form a reply to [`probe_argv`] means: exactly `ok` (surrounding blanks aside) is the Lua manager; any
    /// other reply, an empty one included, is the legacy manager or a Hyprland that predates `eval`.
    pub fn from_probe_reply(reply: &str) -> HyprForm {
        if reply.trim() == "ok" {
            HyprForm::Lua
        } else {
            HyprForm::Legacy
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            HyprForm::Legacy => "legacy",
            HyprForm::Lua => "lua",
        }
    }
}

/// The command whose reply picks the [`HyprForm`]: Lua that does nothing, which only the Lua manager evaluates.
pub fn probe_argv() -> Vec<String> {
    ["hyprctl", "eval", "return 1"]
        .iter()
        .map(ToString::to_string)
        .collect()
}

/// An output name that passed [`super::valid_output_name`]: ASCII letters, digits, `-`, `_` and `.`, at most 64.
/// It holds no quote, backslash, bracket, `;`, blank or line break, so it can sit inside a Lua string literal
/// or a `--batch` part as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputName(String);

impl OutputName {
    pub fn new(name: &str) -> Option<OutputName> {
        super::valid_output_name(name).then(|| OutputName(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A Hyprland window address as `hyprctl clients -j` prints it: `0x` and lowercase hex digits, at most 18
/// characters in all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyprAddress(String);

impl HyprAddress {
    pub const MAX_LEN: usize = 18;

    pub fn new(text: &str) -> Option<HyprAddress> {
        let digits = text.strip_prefix("0x")?;
        let ok = !digits.is_empty()
            && text.len() <= Self::MAX_LEN
            && digits.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        ok.then(|| HyprAddress(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Everything Eitri asks Hyprland to do. Each field is a typed value whose every possible text is safe inside a
/// Lua string literal; nothing here can carry free text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HyprDispatch {
    FocusDir(Direction),
    FocusPid(u32),
    /// Moves the window of `pid` to `output`. The Lua form names the window, so it is one command that leaves
    /// focus alone; the legacy `movewindow` acts on the focused window, so that form focuses it first.
    MoveToOutput {
        pid: u32,
        output: OutputName,
    },
    FocusAddress(HyprAddress),
}

/// The one place a Hyprland command's text is written: each dispatch becomes one command, or two for the legacy
/// move (focus, then move). Every value spliced in is a typed value from [`HyprDispatch`].
fn command_texts(form: HyprForm, dispatch: &HyprDispatch) -> Vec<String> {
    let letter = |dir: &Direction| match dir {
        Direction::Left => "l",
        Direction::Right => "r",
        Direction::Up => "u",
        Direction::Down => "d",
    };
    match (form, dispatch) {
        (HyprForm::Legacy, HyprDispatch::FocusDir(dir)) => vec![format!("movefocus {}", letter(dir))],
        (HyprForm::Legacy, HyprDispatch::FocusPid(pid)) => vec![format!("focuswindow pid:{pid}")],
        (HyprForm::Legacy, HyprDispatch::MoveToOutput { pid, output }) => vec![
            format!("focuswindow pid:{pid}"),
            format!("movewindow mon:{}", output.as_str()),
        ],
        (HyprForm::Legacy, HyprDispatch::FocusAddress(address)) => {
            vec![format!("focuswindow address:{}", address.as_str())]
        }
        (HyprForm::Lua, HyprDispatch::FocusDir(dir)) => {
            vec![format!("hl.dsp.focus({{ direction = '{}' }})", letter(dir))]
        }
        (HyprForm::Lua, HyprDispatch::FocusPid(pid)) => vec![format!("hl.dsp.focus({{ window = 'pid:{pid}' }})")],
        (HyprForm::Lua, HyprDispatch::MoveToOutput { pid, output }) => vec![format!(
            "hl.dsp.window.move({{ window = 'pid:{pid}', monitor = '{}' }})",
            output.as_str()
        )],
        (HyprForm::Lua, HyprDispatch::FocusAddress(address)) => {
            vec![format!("hl.dsp.focus({{ window = 'address:{}' }})", address.as_str())]
        }
    }
}

/// The `hyprctl` argv that runs `dispatches` in order: one command as `hyprctl dispatch ...` (a legacy one split
/// into words, as `hyprctl` joins them again), several as one `--batch`. `None` for an empty list.
pub fn argv(form: HyprForm, dispatches: &[HyprDispatch]) -> Option<Vec<String>> {
    let texts: Vec<String> = dispatches.iter().flat_map(|d| command_texts(form, d)).collect();
    let mut out = vec!["hyprctl".to_string()];
    match texts.as_slice() {
        [] => return None,
        [one] => {
            out.push("dispatch".to_string());
            match form {
                HyprForm::Legacy => out.extend(one.split(' ').map(ToString::to_string)),
                HyprForm::Lua => out.push(one.clone()),
            }
        }
        many => {
            out.push("--batch".to_string());
            out.push(
                many.iter()
                    .map(|text| format!("dispatch {text}"))
                    .collect::<Vec<_>>()
                    .join(" ; "),
            );
        }
    }
    Some(out)
}

/// How many commands Hyprland runs for a `--batch` argument: it splits on `;` outside `[]` and skips empty
/// stretches between two separators, as `hyprctl`'s server side does.
pub fn batch_len(batch: &str) -> usize {
    let mut depth = 0i64;
    let mut start = 0;
    let mut count = 0;
    let bytes = batch.as_bytes();
    for i in 0..=bytes.len() {
        let ch = bytes.get(i).copied().unwrap_or(b';');
        match ch {
            b'[' => depth += 1,
            b']' => depth -= 1,
            b';' if depth == 0 => {
                if start < i {
                    count += 1;
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    count
}

/// What Hyprland answered for a dispatch `argv` (one built by [`argv`]), when that is not success: the first part
/// of the reply other than `ok`, or the whole reply when a batch's reply has another number of parts than the
/// batch has commands. `None` means every command answered `ok`. An argv that is not a dispatch is not judged.
pub fn reply_failure(argv: &[String], reply: &str) -> Option<String> {
    let expected = match argv.get(1).map(String::as_str) {
        Some("dispatch") => 1,
        Some("--batch") => batch_len(argv.get(2).map_or("", String::as_str)),
        _ => return None,
    };
    let reply = reply.trim();
    let parts: Vec<&str> = reply.split("\n\n\n").map(str::trim).collect();
    if parts.len() != expected {
        return Some(reply.to_string());
    }
    parts.into_iter().find(|part| *part != "ok").map(ToString::to_string)
}

/// Whether a failure reply says the command was in the other form: a legacy manager finding no dispatcher by
/// that name, the Lua manager's note that the syntax may need updating, or a Hyprland whose config manager takes
/// no dispatch. The form is probed again once after one.
pub fn names_other_form(reply: &str) -> bool {
    [
        "Invalid dispatcher",
        "your syntax might need to be updated",
        "doesn't support dispatch",
    ]
    .iter()
    .any(|needle| reply.contains(needle))
}

/// The longest stretch of a reply's first line a log keeps.
pub const KIND_LINE_MAX: usize = 120;

/// A reply's first line with control characters taken out and cut at [`KIND_LINE_MAX`] characters: Hyprland's
/// text, made safe and short enough for one log line.
pub fn first_line(reply: &str) -> String {
    whole_first_line(reply).chars().take(KIND_LINE_MAX).collect()
}

/// A reply's first line with control characters taken out, at its full length.
fn whole_first_line(reply: &str) -> String {
    reply
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

/// What makes two failures the same kind for logging: the operation and the reply's first line with its numbers
/// dropped (digits, and the hex digits of an `0x` address), then cut at [`KIND_LINE_MAX`] characters, so a reply
/// that echoes a pid or an address is one kind for every pid and the kinds a runner remembers stay few. The cut
/// comes after the numbers are dropped: before it, a longer pid would push different text past the cut.
pub fn failure_kind(operation: &str, reply: &str) -> String {
    let line = whole_first_line(reply);
    let mut key = String::with_capacity(line.len());
    let mut rest = line.as_str();
    while let Some(c) = rest.chars().next() {
        if let Some(hex) = rest.strip_prefix("0x") {
            key.push_str("0x");
            rest = hex.trim_start_matches(|h: char| h.is_ascii_hexdigit());
        } else {
            if !c.is_ascii_digit() {
                key.push(c);
            }
            rest = &rest[c.len_utf8()..];
        }
    }
    let key: String = key.chars().take(KIND_LINE_MAX).collect();
    format!("{operation}: {key}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_scan::{code_only, functions, modules, rust_sources, Source};

    fn strings(parts: &[&str]) -> Vec<String> {
        parts.iter().map(ToString::to_string).collect()
    }

    fn out(name: &str) -> OutputName {
        OutputName::new(name).unwrap()
    }

    #[test]
    fn the_probe_reply_picks_the_form() {
        assert_eq!(probe_argv(), strings(&["hyprctl", "eval", "return 1"]));
        assert_eq!(HyprForm::from_probe_reply("ok"), HyprForm::Lua);
        assert_eq!(HyprForm::from_probe_reply("ok\n"), HyprForm::Lua);
        assert_eq!(
            HyprForm::from_probe_reply("eval is only supported with the lua config manager"),
            HyprForm::Legacy
        );
        assert_eq!(HyprForm::from_probe_reply("unknown request"), HyprForm::Legacy);
        assert_eq!(HyprForm::from_probe_reply(""), HyprForm::Legacy);
        assert_eq!(HyprForm::from_probe_reply("okay"), HyprForm::Legacy);
        assert_eq!(HyprForm::from_probe_reply("ok\n\n\nok"), HyprForm::Legacy);
    }

    #[test]
    fn each_dispatch_has_its_legacy_and_lua_argv() {
        let dirs = [
            (Direction::Left, "l"),
            (Direction::Right, "r"),
            (Direction::Up, "u"),
            (Direction::Down, "d"),
        ];
        for (dir, letter) in dirs {
            let d = [HyprDispatch::FocusDir(dir)];
            assert_eq!(
                argv(HyprForm::Legacy, &d),
                Some(strings(&["hyprctl", "dispatch", "movefocus", letter]))
            );
            assert_eq!(
                argv(HyprForm::Lua, &d),
                Some(vec![
                    "hyprctl".to_string(),
                    "dispatch".to_string(),
                    format!("hl.dsp.focus({{ direction = '{letter}' }})")
                ])
            );
        }
        let focus = [HyprDispatch::FocusPid(42)];
        assert_eq!(
            argv(HyprForm::Legacy, &focus),
            Some(strings(&["hyprctl", "dispatch", "focuswindow", "pid:42"]))
        );
        assert_eq!(
            argv(HyprForm::Lua, &focus),
            Some(strings(&["hyprctl", "dispatch", "hl.dsp.focus({ window = 'pid:42' })"]))
        );
        let moved = [HyprDispatch::MoveToOutput {
            pid: 42,
            output: out("DP-1"),
        }];
        assert_eq!(
            argv(HyprForm::Legacy, &moved),
            Some(strings(&[
                "hyprctl",
                "--batch",
                "dispatch focuswindow pid:42 ; dispatch movewindow mon:DP-1"
            ]))
        );
        assert_eq!(
            argv(HyprForm::Lua, &moved),
            Some(strings(&[
                "hyprctl",
                "dispatch",
                "hl.dsp.window.move({ window = 'pid:42', monitor = 'DP-1' })"
            ]))
        );
        let address = [HyprDispatch::FocusAddress(HyprAddress::new("0x55d5c7b0d5a0").unwrap())];
        assert_eq!(
            argv(HyprForm::Legacy, &address),
            Some(strings(&[
                "hyprctl",
                "dispatch",
                "focuswindow",
                "address:0x55d5c7b0d5a0"
            ]))
        );
        assert_eq!(
            argv(HyprForm::Lua, &address),
            Some(strings(&[
                "hyprctl",
                "dispatch",
                "hl.dsp.focus({ window = 'address:0x55d5c7b0d5a0' })"
            ]))
        );
        assert_eq!(argv(HyprForm::Lua, &[]), None);
    }

    #[test]
    fn several_dispatches_are_one_batch_that_splits_into_as_many_commands() {
        let both = [
            HyprDispatch::MoveToOutput {
                pid: 42,
                output: out("HEADLESS-2"),
            },
            HyprDispatch::FocusPid(7),
        ];
        let lua = argv(HyprForm::Lua, &both).unwrap();
        assert_eq!(
            lua,
            strings(&[
                "hyprctl",
                "--batch",
                "dispatch hl.dsp.window.move({ window = 'pid:42', monitor = 'HEADLESS-2' }) ; \
                 dispatch hl.dsp.focus({ window = 'pid:7' })"
            ])
        );
        assert_eq!(batch_len(&lua[2]), 2);
        let legacy = argv(HyprForm::Legacy, &both).unwrap();
        assert_eq!(batch_len(&legacy[2]), 3);
        // Hyprland's own rule: `;` inside brackets does not split, empty stretches are skipped.
        assert_eq!(batch_len("a ; b"), 2);
        assert_eq!(batch_len("a;;b;"), 2);
        assert_eq!(batch_len("dispatch x [a;b] ; y"), 2);
        assert_eq!(batch_len(""), 0);
    }

    #[test]
    fn only_ok_is_success_for_every_command_of_a_batch() {
        let one = strings(&["hyprctl", "dispatch", "movefocus", "l"]);
        assert_eq!(reply_failure(&one, "ok"), None);
        assert_eq!(reply_failure(&one, "ok\n"), None);
        assert_eq!(reply_failure(&one, ""), Some(String::new()));
        assert_eq!(
            reply_failure(&one, "Invalid dispatcher"),
            Some("Invalid dispatcher".to_string())
        );
        assert_eq!(
            reply_failure(&one, "current config provider doesn't support dispatch"),
            Some("current config provider doesn't support dispatch".to_string())
        );
        let batch = strings(&[
            "hyprctl",
            "--batch",
            "dispatch focuswindow pid:42 ; dispatch movewindow mon:DP-1",
        ]);
        assert_eq!(reply_failure(&batch, "ok\n\n\nok"), None);
        assert_eq!(reply_failure(&batch, "ok\n\n\nok\n"), None);
        assert_eq!(
            reply_failure(&batch, "ok\n\n\nInvalid dispatcher"),
            Some("Invalid dispatcher".to_string())
        );
        assert_eq!(
            reply_failure(&batch, "Invalid dispatcher\n\n\nok"),
            Some("Invalid dispatcher".to_string())
        );
        // A reply with fewer parts than the batch has commands is a failure too.
        assert_eq!(reply_failure(&batch, "ok"), Some("ok".to_string()));
        // Only dispatches are judged: a window list is JSON.
        assert_eq!(reply_failure(&strings(&["hyprctl", "clients", "-j"]), "[]"), None);
    }

    #[test]
    fn the_failures_that_name_the_other_form_are_recognised() {
        assert!(names_other_form("Invalid dispatcher"));
        assert!(names_other_form(
            "[string \"return hl.dispatch(movefocus l)\"]:1: ')' expected near 'l'\n\n → Note: dispatch in lua is a \
             shorthand for hl.dispatch(...), your syntax might need to be updated."
        ));
        assert!(names_other_form("current config provider doesn't support dispatch"));
        assert!(!names_other_form("no window found"));
        assert!(!names_other_form(""));
    }

    #[test]
    fn a_failure_kind_is_the_operation_and_a_clean_short_first_line() {
        assert_eq!(
            failure_kind("focus by direction", "Invalid dispatcher\nmore"),
            "focus by direction: Invalid dispatcher"
        );
        // A reply that echoes a pid or an address is one kind whatever the number.
        let echo = |pid: u32| {
            format!("error: 3 [string \"return hl.dispatch(hl.dsp.focus({{ window = 'pid:{pid}' }}))\"]: no window")
        };
        assert_eq!(
            failure_kind("focus by pid", &echo(4242)),
            failure_kind("focus by pid", &echo(7))
        );
        assert_ne!(
            failure_kind("focus by pid", &echo(7)),
            failure_kind("focus by pid", "no such window")
        );
        assert_eq!(
            failure_kind("focus", "no window 0x55d5c7b0d5a0 here"),
            failure_kind("focus", "no window 0x1 here")
        );
        // A first line longer than the cut is still one kind whatever the pid's length: the numbers go first.
        let long_echo = |pid: u32| format!("{} pid:{pid} {}", "w".repeat(KIND_LINE_MAX - 20), "z".repeat(40));
        assert!(long_echo(7).chars().count() > KIND_LINE_MAX);
        let kinds: Vec<String> = [7, 4242, 1_234_567_890]
            .iter()
            .map(|pid| failure_kind("focus by pid", &long_echo(*pid)))
            .collect();
        assert_eq!(kinds[0], kinds[1]);
        assert_eq!(kinds[1], kinds[2]);
        assert_eq!(kinds[0].chars().count(), "focus by pid: ".len() + KIND_LINE_MAX);
        assert_eq!(first_line("a\u{1b}[31mb\tc\r\nnext"), "a[31mbc");
        let long = "x".repeat(300);
        assert_eq!(first_line(&long).chars().count(), KIND_LINE_MAX);
        assert_eq!(first_line(""), "");
    }

    /// Strings that would end the Lua string literal, start another statement, or split a batch, if they ever
    /// reached a command. Each is refused before any argv exists.
    #[test]
    fn hostile_output_names_and_addresses_produce_no_argv() {
        let long = "x".repeat(65);
        let hostile = [
            "DP-1'}) os.execute('x') --",
            "a;b",
            "]]",
            "\n",
            "'",
            "DP-\u{0661}",
            long.as_str(),
            "DP-1 ; dispatch exec x",
            "DP\\1",
            "[[x]]",
            "",
        ];
        let w = super::super::WindowRef {
            pid: 42,
            id: None,
            focused: false,
            visible: true,
            rect: None,
        };
        for bad in hostile {
            assert_eq!(OutputName::new(bad), None, "{bad:?}");
            for form in [HyprForm::Legacy, HyprForm::Lua] {
                assert_eq!(
                    super::super::move_to_output_argv(super::super::Wm::Hyprland, form, &w, bad),
                    None,
                    "{bad:?}"
                );
            }
        }
        for bad in [
            "0x1'",
            "0x",
            "0X1f",
            "1f",
            "0xABC",
            "0x1g",
            "0x0123456789abcdef0",
            "0x1 ; x",
            "0x\u{0661}",
        ] {
            assert_eq!(HyprAddress::new(bad), None, "{bad:?}");
        }
        assert!(HyprAddress::new("0x0123456789abcdef").is_some());
        assert!(OutputName::new(&"x".repeat(64)).is_some());
    }

    /// The one file whose `fn command_texts` may write Lua dispatcher names and Hyprland command text.
    const BUILDER_FILE: &str = "core/src/compositor/hyprland_dispatch.rs";

    /// `source`'s code with its comments and literals blanked, and the byte ranges of its `mod tests` and, in the
    /// builder's file, `fn command_texts`. Both are read from `code_only`'s text, which keeps every byte offset of
    /// `source.text`, so a range from one is a range of the other; this is asserted, not assumed.
    fn code_and_exempt_ranges(source: &Source, with_builder: bool) -> (String, Vec<(usize, usize)>) {
        let code = code_only(&source.text);
        assert_eq!(
            code.len(),
            source.text.len(),
            "{}: offsets of the masked text moved",
            source.path
        );
        let builder = if with_builder && source.path == BUILDER_FILE {
            functions(&code, "command_texts")
        } else {
            Vec::new()
        };
        let ranges = modules(&code, "tests")
            .into_iter()
            .chain(builder)
            .map(|body| {
                let start = body.as_ptr() as usize - code.as_ptr() as usize;
                (start, start + body.len())
            })
            .collect();
        (code, ranges)
    }

    /// Every `hl.dsp` in `sources` outside `fn command_texts` of [`BUILDER_FILE`] and outside `mod tests`, as
    /// `path:byte`. Comments count: a Lua dispatcher name belongs to the builder alone.
    fn lua_sites_outside_the_builder(sources: &[Source]) -> Vec<String> {
        let mut stray = Vec::new();
        for source in sources.iter().filter(|source| source.text.contains("hl.dsp")) {
            let (_, skip) = code_and_exempt_ranges(source, true);
            for (at, _) in source.text.match_indices("hl.dsp") {
                if !skip.iter().any(|(start, end)| (*start..*end).contains(&at)) {
                    stray.push(format!("{}:{at}", source.path));
                }
            }
        }
        stray
    }

    /// Every string literal of `text` outside comments: where its content starts, and the content as written
    /// (escapes not resolved). Line and block comments, raw strings and character literals are stepped over.
    fn string_literals(text: &str) -> Vec<(usize, &str)> {
        let bytes = text.as_bytes();
        let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        let mut found = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let next = bytes.get(i + 1).copied();
            match bytes[i] {
                b'/' if next == Some(b'/') => {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                }
                b'/' if next == Some(b'*') => {
                    let mut depth = 0;
                    while i < bytes.len() {
                        if bytes[i..].starts_with(b"/*") {
                            depth += 1;
                            i += 2;
                        } else if bytes[i..].starts_with(b"*/") {
                            depth -= 1;
                            i += 2;
                            if depth == 0 {
                                break;
                            }
                        } else {
                            i += 1;
                        }
                    }
                }
                b'r' if i == 0 || !ident(bytes[i - 1]) || (bytes[i - 1] == b'b' && (i < 2 || !ident(bytes[i - 2]))) => {
                    let hashes = bytes[i + 1..].iter().take_while(|b| **b == b'#').count();
                    let open = i + 1 + hashes;
                    if bytes.get(open) != Some(&b'"') {
                        i += 1;
                        continue;
                    }
                    let close: Vec<u8> = std::iter::once(b'"').chain(std::iter::repeat_n(b'#', hashes)).collect();
                    let end = (open + 1..bytes.len())
                        .find(|at| bytes[*at..].starts_with(&close))
                        .unwrap_or(bytes.len());
                    found.push((open + 1, &text[open + 1..end]));
                    i = end + close.len();
                }
                b'"' => {
                    let mut end = i + 1;
                    while end < bytes.len() && bytes[end] != b'"' {
                        end += if bytes[end] == b'\\' { 2 } else { 1 };
                    }
                    let end = end.min(bytes.len());
                    found.push((i + 1, &text[i + 1..end]));
                    i = end + 1;
                }
                b'\'' if next == Some(b'\\') => {
                    i += 3;
                    while i < bytes.len() && bytes[i] != b'\'' {
                        i += 1;
                    }
                    i += 1;
                }
                b'\'' => match text[i + 1..].chars().next() {
                    // A character literal; anything else is a lifetime.
                    Some(c) if text[i + 1 + c.len_utf8()..].starts_with('\'') => i += c.len_utf8() + 2,
                    _ => i += 1,
                },
                _ => i += 1,
            }
        }
        found
    }

    /// The hyprctl requests that run text on the compositor: `dispatch`, and `eval` and `repl`, which run Lua on a
    /// Lua config manager.
    const RUNNING_REQUESTS: [&str; 3] = ["dispatch", "eval", "repl"];

    /// Whether a string literal is Hyprland command text: `hl.dispatch`, `hyprctl` followed by one of
    /// [`RUNNING_REQUESTS`], a `--batch` flag (not `--batch-check` and the like), or a part ending at a `;` that
    /// is one of those requests or starts with one and a space.
    fn is_hyprland_command_text(literal: &str) -> bool {
        let batch_flag = literal.match_indices("--batch").any(|(at, flag)| {
            !literal[at + flag.len()..]
                .chars()
                .next()
                .is_some_and(|c| c == '-' || c == '_' || c.is_alphanumeric())
        });
        batch_flag
            || literal.contains("hl.dispatch")
            || RUNNING_REQUESTS.iter().any(|request| {
                literal.contains(&format!("hyprctl {request}"))
                    || literal
                        .split(';')
                        .map(str::trim)
                        .any(|part| part == *request || part.starts_with(&format!("{request} ")))
            })
    }

    /// String literals outside [`BUILDER_FILE`] and outside `mod tests` that are Hyprland command text, as
    /// `path:byte`, unless one of `allowed` (`path`, text the literal contains) names it.
    fn command_text_outside_the_builder(sources: &[Source], allowed: &[(&str, &str)]) -> Vec<String> {
        let mut stray = Vec::new();
        for source in sources.iter().filter(|source| source.path != BUILDER_FILE) {
            let (_, skip) = code_and_exempt_ranges(source, false);
            for (at, literal) in string_literals(&source.text) {
                let exempt = skip.iter().any(|(start, end)| (*start..*end).contains(&at));
                let listed = allowed
                    .iter()
                    .any(|(path, text)| *path == source.path && literal.contains(text));
                if !exempt && !listed && is_hyprland_command_text(literal) {
                    stray.push(format!("{}:{at}", source.path));
                }
            }
        }
        stray
    }

    /// The product's sources are read whole, not only this directory, so a Lua dispatch written anywhere else in
    /// the shell or core fails here too.
    #[test]
    fn lua_dispatch_text_is_written_only_by_the_builder() {
        let sources = rust_sources(&["core/src", "shell/src"]);
        let here = rust_sources(&["core/src/compositor"]);
        crate::source_scan::require_anchors(
            &here,
            &[
                "fn command_texts(form: HyprForm, dispatch: &HyprDispatch) -> Vec<String>",
                "enum HyprDispatch",
            ],
        );
        assert_eq!(lua_sites_outside_the_builder(&sources), Vec::<String>::new());
        // Legacy text reaches a Lua manager as Lua too, so no command text, in either form, is written outside the
        // builder: `hyprctl clients -j` is the one argv of hyprctl built elsewhere, and it holds no dispatch. No
        // other literal is allowed today (the git `--batch-check=` format is not a hyprctl flag).
        let allowed: &[(&str, &str)] = &[];
        assert_eq!(
            command_text_outside_the_builder(&sources, allowed),
            Vec::<String>::new()
        );
        // The builder itself does write them.
        let (_, builder) = crate::source_scan::function_in_raw(&here, "command_texts");
        assert!(builder.contains("hl.dsp.focus"), "{builder}");
        assert!(builder.contains("hl.dsp.window.move"), "{builder}");
        // What the builder is given carries no free text: its variants hold only typed values.
        let code = code_only(&crate::source_scan::file_with(&here, "enum HyprDispatch").text);
        let at = code.find("enum HyprDispatch").unwrap();
        let body = crate::source_scan::braced(&code, at + code[at..].find('{').unwrap());
        for word in ["String", "str", "OsString", "Vec", "Box", "Cow", "char"] {
            assert!(
                crate::source_scan::word_ends(body, word).is_empty(),
                "HyprDispatch must hold no {word}: {body}"
            );
        }
        // And the newtypes are made only through their checks.
        let file = code_only(&crate::source_scan::file_with(&here, "struct HyprAddress").text);
        let file = crate::source_scan::without(&file, &modules(&file, "tests"));
        for (ty, check) in [
            (
                "OutputName",
                "super::valid_output_name(name).then(|| OutputName(name.to_string()))",
            ),
            ("HyprAddress", "ok.then(|| HyprAddress(text.to_string()))"),
        ] {
            // `struct Name(` reads as a call too; what is left is every place a value is built.
            let made = crate::source_scan::calls(&file, ty) - file.matches(&format!("struct {ty}(")).count();
            assert_eq!(made, 1, "{ty} is built only in its `new`");
            assert!(file.contains(check), "{ty}::new checks before it builds");
        }
    }

    fn planted(path: &str, text: &str) -> Source {
        Source {
            path: path.into(),
            text: text.into(),
        }
    }

    #[test]
    fn the_lua_scan_finds_a_stray_dispatch_in_a_second_file_and_in_a_comment() {
        let builder = planted(
            BUILDER_FILE,
            "fn command_texts() { let x = \"hl.dsp.focus\"; }\n#[cfg(test)]\nmod tests { const T: &str = \"hl.dsp.x\"; }\n",
        );
        assert!(lua_sites_outside_the_builder(std::slice::from_ref(&builder)).is_empty());
        let second = planted(
            "core/src/compositor/b.rs",
            "fn other() { let y = format!(\"hl.dsp.exec_cmd({})\", t); }\n",
        );
        let comment = planted("core/src/compositor/c.rs", "// hl.dsp.focus\nfn f() {}\n");
        let found = lua_sites_outside_the_builder(&[builder, second, comment]);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].starts_with("core/src/compositor/b.rs:"));
        assert!(found[1].starts_with("core/src/compositor/c.rs:"));
    }

    /// Only the builder's own `fn command_texts` is exempt, not a function of that name elsewhere, and not
    /// another function of the builder's file.
    #[test]
    fn only_the_builders_own_function_may_write_lua_dispatcher_names() {
        let text = "fn command_texts() { let x = \"hl.dsp.focus\"; }\n";
        assert!(lua_sites_outside_the_builder(&[planted(BUILDER_FILE, text)]).is_empty());
        for path in ["core/src/compositor/mod.rs", "shell/src/companion/command_texts.rs"] {
            assert_eq!(lua_sites_outside_the_builder(&[planted(path, text)]).len(), 1, "{path}");
        }
        let other = "fn command_texts() {}\nfn other() { let x = \"hl.dsp.focus\"; }\n";
        assert_eq!(lua_sites_outside_the_builder(&[planted(BUILDER_FILE, other)]).len(), 1);
    }

    /// Text before a module or function that is wider in bytes than in characters must not move where the
    /// exempt ranges sit: a stray name just before a test module is still found.
    #[test]
    fn multibyte_text_before_a_stray_name_does_not_hide_it() {
        let arrows = "\u{2192}".repeat(40);
        // The module is long enough that, were its range counted in characters, it would cover the stray name.
        let body = "\nconst PAD: u8 = 0;".repeat(30);
        for lead in [
            format!("// {arrows}\n"),
            format!("const A: &str = \"{arrows}\";\n"),
            format!("/* {arrows} */\nconst B: char = '\u{2192}';\n"),
        ] {
            let text = format!("{lead}fn f() {{ let s = \"hl.dsp.focus\"; }}\n#[cfg(test)]\nmod tests {{{body}}}\n");
            let found = lua_sites_outside_the_builder(&[planted("core/src/x.rs", &text)]);
            assert_eq!(
                found,
                vec![format!("core/src/x.rs:{}", text.find("hl.dsp").unwrap())],
                "{lead:?}"
            );
            let text =
                format!("{lead}fn f() {{ let s = \"dispatch focuswindow\"; }}\n#[cfg(test)]\nmod tests {{{body}}}\n");
            let found = command_text_outside_the_builder(&[planted("core/src/x.rs", &text)], &[]);
            assert_eq!(found.len(), 1, "{lead:?}: {found:?}");
        }
    }

    /// Hyprland command text in any form, built anywhere but the builder, is found; the argv that holds no
    /// dispatch, a flag of another program and test code are not.
    #[test]
    fn the_command_text_scan_finds_dispatch_text_written_outside_the_builder() {
        let wrong = [
            "let t = format!(\"dispatch focuswindow pid:{}\", p);",
            "let a = [\"hyprctl\", \"dispatch\", \"movefocus\", \"l\"];",
            "let a = [\"hyprctl\", \"--batch\", &b];",
            "let a = \"hyprctl --batch x\";",
            "let t = \"hl.dispatch(movefocus l)\";",
            "let t = \"hyprctl dispatch movefocus l\";",
            "let t = \"dispatch focuswindow pid:1 ; dispatch movewindow mon:DP-1\";",
            "let t = \"a=1;dispatch x\";",
            "let t = r#\"dispatch x\"#;",
            "let q = '\"'; let t = \"dispatch x\";",
            "let t = b\"dispatch x\";",
            "let a = [\"hyprctl\", \"eval\", \"return 1\"];",
            "let t = \"hyprctl eval hl.exec_cmd('x')\";",
            "let t = format!(\"eval {}\", lua);",
            "let a = [\"hyprctl\", \"repl\", &lua];",
            "let t = \"hyprctl repl return 1\";",
            "let t = \"a=1; repl x\";",
        ];
        for line in wrong {
            let text = format!("fn f() {{ {line} }}\n");
            let found = command_text_outside_the_builder(&[planted("shell/src/x.rs", &text)], &[]);
            assert_eq!(found.len(), 1, "{line}: {found:?}");
        }
        let fine = [
            "let a = [\"hyprctl\", \"clients\", \"-j\"];",
            "let a = [\"git\", \"cat-file\", \"--batch-check=%(objectname)\"];",
            "println!(\"[x] dispatch failed: {e}\");",
            "let t = \"the dispatch was queued\";",
            "// hyprctl dispatch movefocus l\nlet t = 1;",
            "let l: &'static str = \"dispatchable\"; let c = '\\'';",
            "let t = \"evaluate the reply\";",
            "let t = \"a repl prompt\";",
            "let t = \"replace\"; let u = \"evaluation\";",
            "let t = \"the eval failed\";",
        ];
        for line in fine {
            let text = format!("fn f() {{ {line} }}\n");
            let found = command_text_outside_the_builder(&[planted("shell/src/x.rs", &text)], &[]);
            assert!(found.is_empty(), "{line}: {found:?}");
        }
        // Test code and the builder's file are left out; the same text elsewhere is not.
        let in_tests = "#[cfg(test)]\nmod tests { const T: &str = \"hyprctl dispatch x\"; }\n";
        assert!(command_text_outside_the_builder(&[planted("shell/src/x.rs", in_tests)], &[]).is_empty());
        let text = "fn f() { let t = \"dispatch x\"; }\n";
        assert!(command_text_outside_the_builder(&[planted(BUILDER_FILE, text)], &[]).is_empty());
        assert_eq!(
            command_text_outside_the_builder(&[planted("shell/src/x.rs", text)], &[]).len(),
            1
        );
        // An allowed site is named by its path and text, and only that one.
        let allowed: &[(&str, &str)] = &[("shell/src/x.rs", "dispatch x")];
        assert!(command_text_outside_the_builder(&[planted("shell/src/x.rs", text)], allowed).is_empty());
        assert_eq!(
            command_text_outside_the_builder(&[planted("shell/src/y.rs", text)], allowed).len(),
            1
        );
    }
}
