//! Every test that opens a display starts one of its own: checked from the sources on every plain
//! `cargo test`, not only by a run under `--ignored` (fix round 3 of GUI tests' own display,
//! 2026-09-29; tightened in fix round 4, the same day).
//!
//! **Why.** `support/own_x_server.rs` works only where a test calls it, and nothing failed when one
//! did not. That happened twice in one night: `panel_stream_scroll` took the inherited display and
//! opened its windows on the owner's screen (incident 1), and then `web_host_origin` reached `main`
//! from `fix/ime-v1` checking only `GDK_BACKEND=x11` -- which XWayland's `DISPLAY=:0` satisfies --
//! while the branch that put every other test on the helper was still open, and would have merged
//! unguarded. So this reads every Rust file under `shell/tests` and `neovide-editor/tests` as text and
//! fails on any that opens a display without `#[path]`-including `own_x_server.rs` and calling its
//! `isolate` or `init_gtk`, unless [`ALLOWED`] names it, with its reason and the calls that stand in
//! for the helper. Modelled on `core/src/socket_path_guard.rs` and `shell/src/main.rs`'s
//! `shell_src_writes_no_accelerator_literal`, which read sources as text the same way.
//!
//! **What counts as opening a display** ([`find_signals`]), everything found in code with every comment
//! and string literal blanked out first ([`code_and_strings`]), so a message naming `LiveHarness` is
//! not one:
//! - a name only such code uses (`LiveHarness`, `NeovideEditorPane`, `WebView`, GTK's application and
//!   window types, winit's `EventLoop`, ...: [`CODE_SIGNALS`]);
//! - any path that starts with a display crate ([`DISPLAY_CRATES`]: `gtk4`, `gdk4`, `webkit6`, `winit`,
//!   `xcb`, ...), and any `use` or `extern crate` of one however it is written -- renamed, globbed,
//!   grouped ([`use_items`]) -- so `use gtk4 as g;` does not hide GTK from it (fix round 4); and, since
//!   the follow-up to round 4, every name such a `use` binds, through any chain of aliases and every
//!   binding of a name bound twice ([`resolved`]), wherever it is used as a path, a call, a macro or an
//!   attribute ([`used_positions`]), behind `self::`, `crate::`, `super::` or a module of the file's own
//!   too: `g::init()` and `crate::g::init()` in `main` are GTK's init as `gtk4::init()` is, and a glob of a
//!   display path (`use gtk4::*;`, `use g::*;`) binds the names the scan follows (`init`, `test_synced`);
//! - a string literal that starts a display server or client, an input injector, a clipboard or screen
//!   tool, or the product's own GUI binary ([`PROGRAMS`]), by the command it starts: its first word, or
//!   that word's file name, so `"/usr/bin/xdotool key a"` counts as `"xdotool"` does; and, where the
//!   file runs a shell (`sh -c`), a program named as any word of another literal (fix round 4).
//!
//! A file that matches and is fine anyway fails closed: it goes on [`ALLOWED`] with the reason, where a
//! reviewer reads it.
//!
//! **What it requires of one that does** ([`offence`], fix round 4 for the last three): the real helper
//! `#[path]`-included; `main` -- a `harness = false` test's -- calling `own_x_server::isolate` or
//! `init_gtk` **before the first display signal in that `main`** (a call in a function nothing runs, or
//! after the first connection, isolates nothing); no `gtk4::init` of its own; and none of gtk-rs's test
//! macros (`#[gtk4::test]`, `#[gtk::test]`, `test_synced`, under any name or chain of aliases, or used
//! after a glob brought them in), which are refused outright: they initialise GTK before the test body, so a
//! call of the helper inside it comes too late.
//!
//! **The allowlist is bound to the file's real `main`** (fix round 4): its calls, in order; GTK
//! initialised exactly once, counted over the whole file in every spelling, an alias's included (an
//! earlier `gtk4::init` would connect before the checks, and an ordered search finds the later one); and
//! each judge of a display -- the socket, the compositor, its outputs -- the subject of a `match` (or
//! `if let Err`) whose `Err` arm exits the process or returns from `main`.
//!
//! **What it does not check.** This is a tripwire for a test author who forgets the helper, or uses it
//! wrongly by accident. It is not a defence against one who means to get round it: whatever can be
//! done to text can be done here. Six such ways were found by the round-3 and round-4 reviews and the
//! review of the alias follow-up, and are left, on purpose, so nobody has to wonder (dated record,
//! 2026-09-29, fix round 4, "merged, with three scanner limits parked" and "two of the three parked
//! scanner limits closed"):
//! - an inline `mod own_x_server { .. }` of the author's own, next to the real file included under
//!   another name -- the `#[path]` attribute and the module declaration are checked apart;
//! - the helper call under `#[cfg(any())]` or `if false { .. }`: text is read, not what runs;
//! - `let _ = own_x_server::init_gtk(..)`: the server it returns is dropped, and stopped, at once, so
//!   the test loses its display (GTK finds none and fails) rather than reaching another;
//! - a second `fn main` in a nested module ahead of the real one: the first textual `fn main(` is taken
//!   ([`main_range`]);
//! - the helper called in a closure inside `main` that nothing calls
//!   (`let setup = || own_x_server::isolate(..)`): its position in `main` is what is read;
//! - GTK's init or a test macro, under a name a `use` or a glob gives it, taken as a value and called
//!   under another (`use gtk4::init as start; let f = start; f()`): a bare name counts only where it is
//!   used as a path, a call, a macro or an attribute, since a bare word is as often a local or a field.
//!
//! Also not covered, though nobody is being clever: the modules a test `#[path]`-includes from `src/`
//! (their text is not read, so a display opened only there is not seen), a program started by a name
//! held in a variable, a script written to a file and then run, a display opened in another function
//! that `main` calls before the helper (only what is written in `main` is put in order; GTK is covered
//! anyway, since a GTK init anywhere in the file is refused, but a winit `EventLoop::new` in a `fn setup()`
//! called ahead of the helper is not), and anything outside these two crates' `tests/` (no other crate's
//! tests open a display today).

use std::path::{Path, PathBuf};

/// A test that opens a display without the helper, why, and the calls that must stand in for it: each
/// found in its `main`, in this order.
struct Allowed {
    file: &'static str,
    why: &'static str,
    order: &'static [&'static str],
    /// The spellings of GTK's init the file may hold ([`INIT_FORMS`]), each exactly once in the whole file:
    /// a second one, or any other spelling, could connect before the checks in `order`.
    inits: &'static [&'static str],
    /// The calls in `order` that judge a display: each must be the subject of a `match` (or an `if let
    /// Err`) in `main` whose `Err` arm exits the process or returns from `main`.
    refusals: &'static [&'static str],
}

const ALLOWED: &[Allowed] = &[Allowed {
    file: "neovide-editor/tests/cursor_animation.rs",
    why: "measures GL/dmabuf presentation on a real compositor, which an Xvfb is not, so it refuses \
          every display but a headless wlroots compositor's: the session bus cut, then the socket and \
          its listener judged without connecting, then GTK, then the outputs before any window or pane",
    order: &[
        "own_x_server::cut_session_bus(",
        "sandbox_wayland_socket(",
        "require_headless_compositor(",
        "gtk4::init(",
        "require_headless_outputs(",
        "NeovideEditorPane::",
    ],
    inits: &["gtk4::init"],
    refusals: &[
        "sandbox_wayland_socket(",
        "require_headless_compositor(",
        "require_headless_outputs(",
    ],
}];

/// Crates that only ever talk to a display, an input device or a clipboard: any path in code that starts
/// with one, and any `use` or `extern crate` of one however it is written (renamed, globbed, grouped),
/// is a signal. Matching the crate rather than its entry points is what leaves no alias, glob or group
/// import (`use gtk4 as g;`, `use gtk4::{init};`) to slip past a list of function names.
const DISPLAY_CRATES: &[&str] = &[
    "gtk4",
    "gtk",
    "gdk4",
    "gdk",
    "webkit6",
    "webkit2gtk",
    "winit",
    "smithay_client_toolkit",
    "xcb",
    "x11rb",
    "x11",
    "x11_dl",
    "wayland_client",
    "arboard",
    "copypasta",
];

/// Names that open a display without a crate in front of them (a type imported by name, or one of the
/// fork's own), matched as whole paths in code (never in a comment or a string). The helper's own entry
/// points ([`ENTRY_POINTS`]) count too: a file that calls them means to open a display, and must
/// include the real helper to do it.
const CODE_SIGNALS: &[&str] = &[
    "Application",
    "ApplicationWindow",
    "Window::new",
    "Window::builder",
    "GLArea",
    "WebView",
    "LiveHarness",
    "EventLoop",
    "EventLoopBuilder",
    "NeovideEditorPane",
    "XOpenDisplay",
    "Display::open",
];

/// Every spelling of the call that initialises GTK.
const INIT_FORMS: &[&str] = &["gtk4::init", "gtk::init", "gtk4::rt::init", "gtk::rt::init"];

/// Programs that open a display, or drive, capture or reach one of whichever session they inherit: display
/// servers and nested compositors, input injectors, screen and clipboard tools, a notification sender, and
/// the product's own GUI binaries. A string literal names one by the command it starts ([`program_named`]).
const PROGRAMS: &[&str] = &[
    "Xvfb",
    "Xwayland",
    "Xephyr",
    "xvfb-run",
    "sway",
    "swaymsg",
    "xdotool",
    "ydotool",
    "wtype",
    "grim",
    "slurp",
    "wl-copy",
    "wl-paste",
    "xclip",
    "xsel",
    "notify-send",
    "CARGO_BIN_EXE_shell",
    "CARGO_BIN_EXE_eitri-supervisor",
];

/// A literal that names one of these anywhere in a file means a script is run by a shell there, so a
/// program named as any word of another literal there counts too (`sh -c "echo | xclip"`).
const SHELLS: &[&str] = &["sh", "bash", "dash", "zsh", "ksh", "fish"];

/// The helper's isolation entry points.
const ENTRY_POINTS: &[&str] = &["own_x_server::isolate", "own_x_server::init_gtk"];

/// The names gtk-rs's crate goes by, and the macros in it that initialise GTK before a test body runs.
const GTK_ROOTS: &[&str] = &["gtk4", "gtk"];
const TEST_MACROS: &[&str] = &["test", "test_synced"];

/// This file and the helper name every signal themselves; neither is a test that opens a display.
const NOT_SCANNED: &[&str] = &["shell/tests/own_display_scan.rs", "shell/tests/support/own_x_server.rs"];

/// A string literal: its contents, escapes left as written, and where it starts in the blanked code.
struct Literal {
    text: String,
    at: usize,
}

/// `source` with every comment and every string, byte-string and character literal replaced by
/// spaces (newlines kept, so a line number in one is a line number in the other), and its string
/// literals.
fn code_and_strings(source: &str) -> (String, Vec<Literal>) {
    let chars: Vec<char> = source.chars().collect();
    let at = |i: usize| chars.get(i).copied();
    let blank = |c: char| if c == '\n' { '\n' } else { ' ' };
    let mut code = String::with_capacity(source.len());
    let mut strings = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let after_ident = i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
        // A string's prefix (`b`, `c`, `r`, `br`, `cr`), its hashes, and where its body starts.
        let raw_at = |j: usize| {
            let mut hashes = 0;
            while at(j + hashes) == Some('#') {
                hashes += 1;
            }
            (at(j + hashes) == Some('"')).then_some(hashes)
        };
        let prefixed = if after_ident {
            None
        } else {
            match (c, at(i + 1)) {
                ('r', _) => raw_at(i + 1).map(|h| (1, Some(h))),
                ('b' | 'c', Some('r')) => raw_at(i + 2).map(|h| (2, Some(h))),
                ('b' | 'c', Some('"')) => Some((1, None)),
                _ => None,
            }
        };
        if c == '/' && at(i + 1) == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                code.push(' ');
                i += 1;
            }
        } else if c == '/' && at(i + 1) == Some('*') {
            let mut depth = 0;
            while i < chars.len() {
                if chars[i] == '/' && at(i + 1) == Some('*') {
                    depth += 1;
                    code.push_str("  ");
                    i += 2;
                } else if chars[i] == '*' && at(i + 1) == Some('/') {
                    depth -= 1;
                    code.push_str("  ");
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    code.push(blank(chars[i]));
                    i += 1;
                }
            }
        } else if let Some((prefix, Some(hashes))) = prefixed {
            // A raw string: no escapes; it ends at a quote followed by as many hashes.
            let start = i + prefix + hashes + 1;
            let mut end = start;
            while end < chars.len() && !(chars[end] == '"' && (0..hashes).all(|k| at(end + 1 + k) == Some('#'))) {
                end += 1;
            }
            strings.push(Literal {
                text: chars[start..end.min(chars.len())].iter().collect(),
                at: code.len(),
            });
            let stop = (end + 1 + hashes).min(chars.len());
            code.extend(chars[i..stop].iter().map(|&c| blank(c)));
            i = stop;
        } else if c == '"' || matches!(prefixed, Some((_, None))) {
            let start = if c == '"' { i + 1 } else { i + 2 };
            let mut end = start;
            while end < chars.len() && chars[end] != '"' {
                end += if chars[end] == '\\' { 2 } else { 1 };
            }
            let end = end.min(chars.len());
            strings.push(Literal {
                text: chars[start..end].iter().collect(),
                at: code.len(),
            });
            let stop = (end + 1).min(chars.len());
            code.extend(chars[i..stop].iter().map(|&c| blank(c)));
            i = stop;
        } else if c == '\'' && (at(i + 1) == Some('\\') || at(i + 2) == Some('\'')) {
            // A character literal (`'"'`, `'\''`, `'\u{7f}'`), not a lifetime or a label: its closing
            // quote comes after the one character, or after the backslash and the character it escapes.
            let mut end = if at(i + 1) == Some('\\') { i + 3 } else { i + 2 };
            while end < chars.len() && chars[end] != '\'' && end < i + 12 {
                end += 1;
            }
            let stop = (end + 1).min(chars.len());
            code.extend(chars[i..stop].iter().map(|&c| blank(c)));
            i = stop;
        } else {
            code.push(c);
            i += 1;
        }
    }
    (code, strings)
}

/// Where `code` holds `path` as a whole path: not inside a longer name (`ApplicationFlags` is not
/// `Application`) and not a segment of a longer path it does not start (`gio::Application` is,
/// `MyApplication` is not).
fn path_positions(code: &str, path: &str) -> Vec<usize> {
    code.match_indices(path)
        .map(|(at, _)| at)
        .filter(|&at| {
            let before = code[..at].chars().next_back().is_none_or(|c| !is_ident_char(c));
            let after = code[at + path.len()..].chars().next().is_none_or(|c| !is_ident_char(c));
            before && after
        })
        .collect()
}

fn has_path(code: &str, path: &str) -> bool {
    !path_positions(code, path).is_empty()
}

/// Where `code` names `krate` as the first segment of a path (`krate::...`, `::krate::...`, or behind only
/// `self::`, `crate::`, `super::` or a module of the file's own, [`starts_path`]), not as the tail of a longer
/// one (`other::krate::...`) or a part of a longer name.
fn root_positions(code: &str, krate: &str) -> Vec<usize> {
    let mods = local_modules(code);
    path_positions(code, krate)
        .into_iter()
        .filter(|&at| starts_path(code, at, &mods) && code[at + krate.len()..].trim_start().starts_with("::"))
        .collect()
}

/// Whether a path at `at` in `code` starts there: not a method (`x.g`), and not the tail of a longer path
/// (`y::g`) unless all that leads it is a leading `::`, `self::`, `crate::`, `super::` or a module the file
/// declares (`mods`), which only say where in this file the name is bound (`crate::g::init()` is `g::init()`).
fn starts_path(code: &str, at: usize, mods: &[String]) -> bool {
    let mut before = code[..at].trim_end();
    loop {
        let Some(rest) = before.strip_suffix("::") else {
            return !before.ends_with('.');
        };
        let rest = rest.trim_end();
        let segment_len: usize = rest
            .chars()
            .rev()
            .take_while(|&c| is_ident_char(c))
            .map(char::len_utf8)
            .sum();
        let segment = &rest[rest.len() - segment_len..];
        if segment.is_empty() {
            return true; // a leading `::`
        }
        if !(["self", "crate", "super"].contains(&segment) || mods.iter().any(|m| m == segment)) {
            return false;
        }
        before = rest[..rest.len() - segment_len].trim_end();
    }
}

/// A literal with each backslash escape (`\n`, `\"`) read as a space, so a command line written into one
/// splits into its words.
fn unescaped(literal: &str) -> String {
    let mut out = String::with_capacity(literal.len());
    let mut chars = literal.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next();
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

/// The command a literal starts: its first whitespace-separated word, or that word's file name if it is a
/// path (`/usr/bin/xdotool key a` starts `xdotool`).
fn command_of(literal: &str) -> Option<String> {
    let plain = unescaped(literal);
    let first = plain.split_whitespace().next()?;
    Some(Path::new(first).file_name()?.to_string_lossy().into_owned())
}

/// The program of [`PROGRAMS`] a string literal names: the command it starts or, where the file runs a
/// shell (`shell_runs`), any word of it.
fn program_named(literal: &str, shell_runs: bool) -> Option<&'static str> {
    let command = command_of(literal);
    if let Some(program) = PROGRAMS.iter().find(|program| Some(**program) == command.as_deref()) {
        return Some(program);
    }
    if shell_runs {
        let plain = unescaped(literal);
        return plain
            .split(|c: char| !(c.is_alphanumeric() || "_-.+".contains(c)))
            .find_map(|word| PROGRAMS.iter().find(|program| **program == word).copied());
    }
    None
}

/// One thing in a source that opens a display, and where.
struct Signal {
    /// Where it is in the blanked code (a string literal's is where it starts).
    at: usize,
    /// What it is, as a message names it.
    what: String,
    /// One of the helper's own entry points: a call that isolates, not one that opens a display.
    entry: bool,
}

/// Every place `code` (and its string `literals`, and the names its `uses` bring in, [`resolved`]) opens a
/// display or calls the helper that isolates one, in the order they come: a name a `use` binds to a display
/// crate counts wherever it is used as a path, a call, a macro or an attribute ([`used_positions`]), as the
/// crate's own name does where it starts a path.
fn find_signals(code: &str, literals: &[Literal], uses: &[UseItem]) -> Vec<Signal> {
    let mut found = Vec::new();
    for name in CODE_SIGNALS {
        for at in path_positions(code, name) {
            found.push(Signal {
                at,
                what: name.to_string(),
                entry: false,
            });
        }
    }
    for name in ENTRY_POINTS {
        for at in path_positions(code, name) {
            found.push(Signal {
                at,
                what: name.to_string(),
                entry: true,
            });
        }
    }
    for krate in DISPLAY_CRATES {
        for at in root_positions(code, krate) {
            found.push(Signal {
                at,
                what: krate.to_string(),
                entry: false,
            });
        }
        for item in uses
            .iter()
            .filter(|item| item.path.first().is_some_and(|root| root == krate))
        {
            found.push(Signal {
                at: item.at,
                what: krate.to_string(),
                entry: false,
            });
        }
    }
    // A binding named like a code signal (`use webkit6::WebView;`) is found by that name already.
    for (name, path) in display_bindings(uses)
        .into_iter()
        .filter(|(name, _)| !CODE_SIGNALS.contains(name))
    {
        for at in used_positions(code, name) {
            found.push(Signal {
                at,
                what: format!("{name} (`{}`)", path.join("::")),
                entry: false,
            });
        }
    }
    let shell_runs = literals
        .iter()
        .any(|literal| command_of(&literal.text).is_some_and(|command| SHELLS.contains(&command.as_str())));
    for literal in literals {
        if let Some(program) = program_named(&literal.text, shell_runs) {
            found.push(Signal {
                at: literal.at,
                what: format!("\"{program}\""),
                entry: false,
            });
        }
    }
    found.sort_by_key(|signal| signal.at);
    found
}

/// What in `source` opens a display, each once, in the order it comes.
fn display_signals(source: &str) -> Vec<String> {
    let (code, literals) = code_and_strings(source);
    let mut names: Vec<String> = Vec::new();
    for signal in find_signals(&code, &literals, &resolved(&code)) {
        if !names.contains(&signal.what) {
            names.push(signal.what);
        }
    }
    names
}

/// One name a `use` declaration or an `extern crate` item brings in.
struct UseItem {
    /// The path it names, with `self` dropped (`use a::{self}` names `a`) and `*` last for a glob.
    path: Vec<String>,
    /// The name it is given by `as`, if it is renamed.
    alias: Option<String>,
    /// Where its declaration starts in the code.
    at: usize,
    /// Brought in by a glob rather than named ([`resolved`]): a name the scan follows that a glob of a display
    /// path would bring in, such as `init` from `use gtk4::*;`.
    from_glob: bool,
}

/// A position in code for [`use_items`]'s small parser.
struct Cursor<'a> {
    code: &'a str,
    i: usize,
}

impl<'a> Cursor<'a> {
    fn skip_whitespace(&mut self) {
        while self.code.as_bytes().get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
    }

    /// Consumes `token` if it is next (after any whitespace).
    fn eat(&mut self, token: &str) -> bool {
        self.skip_whitespace();
        let found = self.code[self.i..].starts_with(token);
        if found {
            self.i += token.len();
        }
        found
    }

    /// Consumes an identifier (a keyword such as `self` or `as` included) if one is next.
    fn ident(&mut self) -> Option<&'a str> {
        self.skip_whitespace();
        let bytes = self.code.as_bytes();
        let start = self.i;
        while bytes.get(self.i).is_some_and(|&b| is_ident_byte(b)) {
            self.i += 1;
        }
        (self.i > start).then(|| &self.code[start..self.i])
    }
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// One `use` tree after `prefix`: a path, a `{group}` of trees, or a `*`, then its `as`. The declaration
/// (at `at` in the code) lands in `out` one item per name it brings in.
fn parse_use_tree(c: &mut Cursor, prefix: &[String], at: usize, out: &mut Vec<UseItem>) {
    let mut path = prefix.to_vec();
    c.eat("::"); // an absolute path, or the crate root of a 2015 one
    loop {
        if c.eat("{") {
            loop {
                if c.eat("}") {
                    return;
                }
                parse_use_tree(c, &path, at, out);
                // Each turn round the loop consumes a comma, so it ends at the group's `}`.
                if !c.eat(",") {
                    c.eat("}");
                    return;
                }
            }
        }
        if c.eat("*") {
            path.push("*".into());
            out.push(UseItem {
                path,
                alias: None,
                at,
                from_glob: false,
            });
            return;
        }
        let Some(name) = c.ident() else { return };
        path.push(name.to_string());
        if !c.eat("::") {
            break;
        }
    }
    let mut alias = None;
    let before_as = c.i;
    if c.ident() == Some("as") {
        alias = c.ident().map(str::to_string);
    } else {
        c.i = before_as;
    }
    if path.last().is_some_and(|last| last == "self") {
        path.pop();
    }
    out.push(UseItem {
        path,
        alias,
        at,
        from_glob: false,
    });
}

/// Every name the `use` declarations and `extern crate` items in `code` bring in, groups and globs
/// flattened: `use gtk4::{self as g, init}` is `gtk4` as `g`, and `gtk4::init`.
fn use_items(code: &str) -> Vec<UseItem> {
    let bytes = code.as_bytes();
    let mut items = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !is_ident_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_ident_byte(bytes[i]) {
            i += 1;
        }
        let mut c = Cursor { code, i };
        match &code[start..i] {
            // `use<'a>` in a return type is precise capturing, not an import.
            "use" if !c.eat("<") => {
                parse_use_tree(&mut c, &[], start, &mut items);
                i = c.i;
            }
            "extern" if c.ident() == Some("crate") => {
                if let Some(name) = c.ident() {
                    let before_as = c.i;
                    let alias = if c.ident() == Some("as") {
                        c.ident().map(str::to_string)
                    } else {
                        c.i = before_as;
                        None
                    };
                    items.push(UseItem {
                        path: vec![name.to_string()],
                        alias,
                        at: start,
                        from_glob: false,
                    });
                    i = c.i;
                }
            }
            _ => {}
        }
    }
    items
}

/// The name `item` binds: its alias, or the last segment of its path; none for a glob or `as _`.
fn bound_name(item: &UseItem) -> Option<&str> {
    let name = item.alias.as_deref().or(item.path.last().map(String::as_str))?;
    (name != "*" && name != "_").then_some(name)
}

/// The paths a glob can bring in that the scan follows by name: GTK's init ([`INIT_FORMS`]) and gtk-rs's test
/// macros.
fn followed_paths() -> Vec<Vec<String>> {
    let macros = GTK_ROOTS
        .iter()
        .flat_map(|root| TEST_MACROS.iter().map(move |name| format!("{root}::{name}")));
    INIT_FORMS
        .iter()
        .map(|form| form.to_string())
        .chain(macros)
        .map(|path| path.split("::").map(str::to_string).collect())
        .collect()
}

/// The modules `code` declares (`mod m;`, `mod m { .. }`), other than one named like a display crate: a path
/// through one (`m::g`) is a path through this file, as `self::g` is.
fn local_modules(code: &str) -> Vec<String> {
    let mut mods = Vec::new();
    for at in path_positions(code, "mod") {
        let mut c = Cursor { code, i: at + 3 };
        if let Some(name) = c.ident() {
            if !DISPLAY_CRATES.contains(&name) && !mods.iter().any(|m| m == name) {
                mods.push(name.to_string());
            }
        }
    }
    mods
}

/// `path` without the leading `self`, `crate`, `super` and module-of-this-file segments that only say where in
/// the file its first real name is bound (`crate::g::init` is `g::init`), leaving at least one segment.
fn local_tail(path: &[String], mods: &[String]) -> Vec<String> {
    let mut path = path;
    while path.len() > 1 && (["self", "crate", "super"].contains(&path[0].as_str()) || mods.contains(&path[0])) {
        path = &path[1..];
    }
    path.to_vec()
}

/// Every name the `use` declarations and `extern crate` items in `code` bring in ([`use_items`]), with the root of
/// each path followed through the names the others bind, so an alias of an alias is still the crate it came
/// from: in `use gtk4 as g; use crate::g::{test_synced as run};` the second is `gtk4::test_synced` as `run`. A
/// path through `self::`, `crate::`, `super::` or a module the file declares is the path after it
/// ([`local_tail`]). Scope is not read, so it fails closed: a binding anywhere in the file counts everywhere,
/// and a name bound more than once is followed through every binding, a chain that reaches a display crate
/// winning. A glob of a display path (`use gtk4::*;`, `use g::*;`) binds each name in it the scan follows
/// ([`followed_paths`]: `init`, `rt`, `test`, `test_synced`), marked as brought in by a glob.
fn resolved(code: &str) -> Vec<UseItem> {
    let mods = local_modules(code);
    let resolve_all = |items: Vec<UseItem>| -> Vec<UseItem> {
        let bound: Vec<(String, Vec<String>)> = items
            .iter()
            .filter_map(|item| Some((bound_name(item)?.to_string(), local_tail(&item.path, &mods))))
            .collect();
        items
            .into_iter()
            .map(|item| UseItem {
                path: resolve(&local_tail(&item.path, &mods), &bound, &mods, bound.len().min(8) + 1),
                ..item
            })
            .collect()
    };
    let mut items = resolve_all(use_items(code));
    let mut from_globs: Vec<UseItem> = Vec::new();
    for item in items
        .iter()
        .filter(|item| item.path.last().is_some_and(|last| last == "*"))
    {
        let prefix = &item.path[..item.path.len() - 1];
        if !prefix
            .first()
            .is_some_and(|root| DISPLAY_CRATES.contains(&root.as_str()))
        {
            continue;
        }
        for followed in followed_paths() {
            if followed.len() > prefix.len() && followed.starts_with(prefix) {
                let path = followed[..=prefix.len()].to_vec();
                if !from_globs.iter().any(|known| known.path == path) {
                    from_globs.push(UseItem {
                        path,
                        alias: None,
                        at: item.at,
                        from_glob: true,
                    });
                }
            }
        }
    }
    if from_globs.is_empty() {
        return items;
    }
    // Once more, so a name bound through one a glob brought in (`use gtk4::*; use self::rt as r;`) is followed.
    items.extend(from_globs);
    resolve_all(items)
}

/// `path` with its root replaced by what it is bound to in `bound`, repeatedly, at most `depth` times (a chain
/// of more than eight aliases is not an accident, and the cap keeps a file of them from stalling the scan): through
/// every binding of the root, the first chain that reaches a display crate, or else the first chain.
fn resolve(path: &[String], bound: &[(String, Vec<String>)], mods: &[String], depth: usize) -> Vec<String> {
    let Some(root) = path.first().filter(|_| depth > 0) else {
        return path.to_vec();
    };
    let mut first = None;
    for (_, target) in bound
        .iter()
        .filter(|(name, target)| name == root && target.first() != Some(name))
    {
        let spliced: Vec<String> = target.iter().chain(&path[1..]).cloned().collect();
        let chain = resolve(&local_tail(&spliced, mods), bound, mods, depth - 1);
        if chain
            .first()
            .is_some_and(|root| DISPLAY_CRATES.contains(&root.as_str()))
        {
            return chain;
        }
        first.get_or_insert(chain);
    }
    first.unwrap_or_else(|| path.to_vec())
}

/// Each name `uses` ([`resolved`]) binds to a display crate or a path in one, other than the crate's own
/// name (which [`root_positions`] finds already), and the path it stands for.
fn display_bindings(uses: &[UseItem]) -> Vec<(&str, &[String])> {
    uses.iter()
        .filter_map(|item| Some((bound_name(item)?, item.path.as_slice())))
        .filter(|(name, path)| {
            path.first().is_some_and(|root| DISPLAY_CRATES.contains(&root.as_str())) && *path != [name.to_string()]
        })
        .collect()
}

/// Where `code` holds `path` as the start of a path of its own ([`starts_path`]): not a method (`x.g`), not the
/// tail of a longer path (`y::g`) other than one through this file (`self::g`, `m::g`), not inside a longer name.
fn rooted_positions(code: &str, path: &str) -> Vec<usize> {
    let mods = local_modules(code);
    path_positions(code, path)
        .into_iter()
        .filter(|&at| starts_path(code, at, &mods))
        .collect()
}

/// Where `code` uses the name `name` a `use` binds ([`rooted_positions`]) as something that can connect: the start
/// of a longer path (`g::init`), a call (`run(..)`), a macro (`m!`) or an attribute (`#[test]`). Not a bare word,
/// which in code is as often a local, a field or a pattern of the same name (`let g = 1;`, `Rgb { g: 0 }`).
fn used_positions(code: &str, name: &str) -> Vec<usize> {
    rooted_positions(code, name)
        .into_iter()
        .filter(|&at| {
            let after = code[at + name.len()..].trim_start();
            ["::", "(", "!"].iter().any(|next| after.starts_with(next))
                || (code[..at].trim_end().ends_with("#[") && after.starts_with(['(', ']']))
        })
        .collect()
}

/// GTK's init under the names `uses` ([`resolved`]) give it, each with the spelling of [`INIT_FORMS`] it
/// stands for: `g::init` for `use gtk4 as g;`, `start` for `use gtk4::init as start;`. A spelling that is
/// itself one of [`INIT_FORMS`] (`gtk::init` for `use gtk4 as gtk;`) is left to that form's own count.
fn aliased_inits(uses: &[UseItem]) -> Vec<(String, &'static str)> {
    let mut spellings = Vec::new();
    for (name, path) in display_bindings(uses) {
        for form in INIT_FORMS {
            let segments: Vec<&str> = form.split("::").collect();
            if path.len() <= segments.len() && path.iter().zip(&segments).all(|(a, b)| a == b) {
                let spelling = std::iter::once(name)
                    .chain(segments[path.len()..].iter().copied())
                    .collect::<Vec<_>>()
                    .join("::");
                if !INIT_FORMS.contains(&spelling.as_str()) {
                    spellings.push((spelling, *form));
                }
            }
        }
    }
    spellings
}

/// Where `code` calls `spelling` ([`rooted_positions`], then an opening parenthesis): a call, not the `use`
/// that names it.
fn calls(code: &str, spelling: &str) -> usize {
    rooted_positions(code, spelling)
        .into_iter()
        .filter(|&at| code[at + spelling.len()..].trim_start().starts_with('('))
        .count()
}

/// gtk-rs's test macros (`#[gtk4::test]`, `gtk4::test_synced`, ...), which initialise GTK before the test
/// body runs: the first one `code` uses, as written, under any name the crate is imported by or brought
/// in by name, through any chain of aliases (`uses` are [`resolved`]).
fn gtk_test_macro(code: &str, uses: &[UseItem]) -> Option<String> {
    let gtk = |item: &UseItem| item.path.first().is_some_and(|root| GTK_ROOTS.contains(&root.as_str()));
    // `use gtk4 as gtk;` is the spelling gtk-rs documents `#[gtk::test]` under; `extern crate` renames too.
    let mut roots: Vec<&str> = GTK_ROOTS.to_vec();
    roots.extend(
        uses.iter()
            .filter(|item| item.path.len() == 1 && gtk(item))
            .filter_map(bound_name),
    );
    for root in roots {
        for name in TEST_MACROS {
            let path = format!("{root}::{name}");
            if has_path(code, &path) {
                return Some(path);
            }
        }
    }
    let is_macro = |item: &&UseItem| {
        gtk(item) && item.path.len() >= 2 && TEST_MACROS.contains(&item.path[item.path.len() - 1].as_str())
    };
    // Imported by name, refused on the import; brought in by a glob, where it is used (`use gtk4::*;` alone
    // initialises nothing).
    uses.iter()
        .filter(is_macro)
        .find(|item| !item.from_glob || bound_name(item).is_some_and(|name| !used_positions(code, name).is_empty()))
        .map(|item| item.path.join("::"))
}

/// The `#[path = "..."]` attributes in `source`, as the literal each gives.
fn path_attributes(source: &str) -> Vec<String> {
    let (code, _) = code_and_strings(source);
    code.lines()
        .zip(source.lines())
        .filter(|(code, _)| code.trim_start().starts_with("#[path"))
        .filter_map(|(_, raw)| {
            let start = raw.find('"')? + 1;
            let end = start + raw[start..].find('"')?;
            Some(raw[start..end].to_string())
        })
        .collect()
}

/// Where `fn main`'s body is in `code` (its braces included), braces matched in code so none in a string
/// or comment counts. The first `fn main(` in the text: another one in a nested module is a way round
/// this, recorded in the header.
fn main_range(code: &str) -> Option<std::ops::Range<usize>> {
    let start = code.find("fn main(")?;
    let open = start + code[start..].find('{')?;
    let mut depth = 0;
    for (offset, c) in code[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open..open + offset + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// The code of `fn main`'s body.
fn main_body(code: &str) -> Option<&str> {
    main_range(code).map(|range| &code[range])
}

/// Where the bracket that closes the one at `open` in `text` is (brackets of every kind counted together).
fn closing(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0;
    for (offset, c) in text[open..].char_indices() {
        match c {
            '(' | '{' | '[' => depth += 1,
            ')' | '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// The body of the `Err(..) =>` arm at the top level of the `match` block `block` (braces included): a
/// `{ block }`, or the expression up to the comma that ends it.
fn err_arm(block: &str) -> Option<&str> {
    let mut depth = 0;
    for (at, c) in block.char_indices() {
        if depth == 1
            && block[at..].starts_with("Err(")
            && block[..at].chars().next_back().is_none_or(|p| !is_ident_char(p))
        {
            let arrow = at + block[at..].find("=>")? + 2;
            let rest = block[arrow..].trim_start();
            let start = block.len() - rest.len();
            if rest.starts_with('{') {
                return Some(&block[start..=closing(block, start)?]);
            }
            let mut nested = 0;
            for (offset, c) in rest.char_indices() {
                match c {
                    '(' | '{' | '[' => nested += 1,
                    ')' | ']' => nested -= 1,
                    '}' if nested == 0 => return Some(&rest[..offset]),
                    '}' => nested -= 1,
                    ',' if nested == 0 => return Some(&rest[..offset]),
                    _ => {}
                }
            }
            return Some(rest);
        }
        match c {
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Whether `main`'s code `body` refuses when `call` (e.g. `require_headless_outputs(`) does: the call is what
/// a `match` or an `if let Err` looks at, and the `Err` arm exits the process or returns from `main`.
/// Textual, as everything here is: a refusal written some other way fails it, and is then written this
/// way or added to the scan on purpose.
fn refuses_by_exiting(body: &str, call: &str) -> Result<(), String> {
    let name = call.trim_end_matches('(');
    let Some(at) = body.find(call) else {
        return Err(format!("its `main` does not call `{name}`"));
    };
    let unjudged =
        || format!("`main` calls `{name}` but does not match on its result, so nothing there refuses when it does");
    let close = closing(body, at + call.len() - 1).ok_or_else(unjudged)?;
    let rest = body[close + 1..].trim_start();
    if !rest.starts_with('{') {
        return Err(unjudged());
    }
    let block_open = body.len() - rest.len();
    let block = &body[block_open..=closing(body, block_open).ok_or_else(unjudged)?];
    // What leads the call in its statement: `let s = match `, `if let Err(e) = `.
    let head = &body[body[..at].rfind([';', '{', '}']).map_or(0, |i| i + 1)..at];
    let arm = if has_path(head, "match") {
        err_arm(block).ok_or_else(|| format!("the `match` on `{name}` in `main` has no `Err` arm"))?
    } else if has_path(head, "if") && has_path(head, "let") && head.contains("Err(") {
        block
    } else {
        return Err(unjudged());
    };
    if arm.contains("process::exit(") || has_path(arm, "return") {
        Ok(())
    } else {
        Err(format!(
            "the `Err` arm of `main`'s match on `{name}` neither exits (`std::process::exit(`) nor returns, \
             so `main` goes on to open a window after a refusal"
        ))
    }
}

/// Why the test at `rel` (a path from the workspace root, whose directory is `dir`) breaks the rule,
/// or `None` if it keeps it. `helper` is the helper's canonical path.
fn offence(rel: &str, source: &str, dir: &Path, helper: &Path) -> Option<String> {
    let (code, literals) = code_and_strings(source);
    let uses = resolved(&code);
    // Outright, before anything is weighed: no call of the helper inside such a test can come first.
    if let Some(found) = gtk_test_macro(&code, &uses) {
        return Some(format!(
            "{rel}: uses gtk-rs's test macro `{found}`, which initialises GTK on the display it inherits before \
             the test body runs, so a call of `own_x_server::init_gtk` inside it comes too late -- write a \
             `harness = false` `main` that calls `own_x_server::init_gtk` first thing instead"
        ));
    }
    let signals = find_signals(&code, &literals, &uses);
    if signals.is_empty() {
        return None;
    }
    if let Some(allowed) = ALLOWED.iter().find(|a| a.file == rel) {
        let refused = |what: String| Some(format!("{rel}: on the allowlist ({}), but {what}", allowed.why));
        let Some(body) = main_body(&code) else {
            return refused("no `fn main` could be found in it".into());
        };
        let mut from = 0;
        for call in allowed.order {
            match body[from..].find(call) {
                Some(at) => from += at + call.len(),
                None => {
                    return refused(format!(
                        "its `main` no longer calls `{call}` where the allowlist says, after {:?}",
                        allowed.order.split(|c| c == call).next().unwrap_or_default()
                    ))
                }
            }
        }
        // Counted over the whole file, not `main`: an earlier init anywhere would connect before the checks.
        for form in INIT_FORMS {
            let count = path_positions(&code, form).len();
            let want = usize::from(allowed.inits.contains(form));
            if count != want {
                return refused(format!(
                    "`{form}` appears {count} time(s) in the file and may appear {} -- GTK connects to a \
                     display where it is initialised, and only the one after the checks in its `main` may",
                    if want == 1 { "exactly once" } else { "not at all" }
                ));
            }
        }
        for (spelling, form) in aliased_inits(&uses) {
            let count = calls(&code, &spelling);
            if count != 0 {
                return refused(format!(
                    "`{spelling}` (`{form}` under a name a `use` gives it) is called {count} time(s) in the file \
                     and may be called not at all -- GTK connects to a display where it is initialised, and only \
                     the init the allowlist names, after the checks in its `main`, may"
                ));
            }
        }
        for call in allowed.refusals {
            if let Err(problem) = refuses_by_exiting(body, call) {
                return refused(problem);
            }
        }
        return None;
    }
    let includes_helper = has_path(&code, "mod own_x_server")
        && path_attributes(source)
            .iter()
            .any(|path| path.ends_with("own_x_server.rs") && dir.join(path).canonicalize().is_ok_and(|p| p == helper));
    let inits_gtk_itself = INIT_FORMS.iter().any(|init| has_path(&code, init))
        || aliased_inits(&uses)
            .iter()
            .any(|(spelling, _)| calls(&code, spelling) != 0);
    let mut names: Vec<&str> = Vec::new();
    for signal in &signals {
        if !names.contains(&signal.what.as_str()) {
            names.push(&signal.what);
        }
    }
    let what = format!("{rel}: opens a display ({})", names.join(", "));
    let fix = "`#[path]`-include `shell/tests/support/own_x_server.rs` as `mod own_x_server` and call \
               `own_x_server::init_gtk` (GTK) or `own_x_server::isolate` (anything else) first thing in \
               `main`; a test that cannot use an Xvfb goes on this file's ALLOWED, with its reason and \
               the calls that stand in for the helper";
    // Only `main` counts: a call in a function nothing runs isolates nothing, and one that comes after
    // the first thing in `main` that connects has isolated nothing that connected.
    let main = main_range(&code);
    let in_main = |signal: &&Signal| main.as_ref().is_some_and(|range| range.contains(&signal.at));
    let helper_call = signals.iter().filter(|signal| signal.entry).find(in_main);
    let first_open = signals.iter().filter(|signal| !signal.entry).find(in_main);
    let line = |at: usize| code[..at].matches('\n').count() + 1;
    if !includes_helper {
        Some(format!(
            "{what}, but does not include `shell/tests/support/own_x_server.rs` -- {fix}"
        ))
    } else if main.is_none() {
        Some(format!(
            "{what}, but has no `fn main` (a `harness = false` test) to call `own_x_server::isolate` or \
             `own_x_server::init_gtk` in -- {fix}"
        ))
    } else if let Some(call) = helper_call {
        match first_open.filter(|open| open.at < call.at) {
            Some(open) => Some(format!(
                "{what}, but its `main` connects to a display (`{}`, line {}) before it calls the helper \
                 (line {}) -- {fix}",
                open.what,
                line(open.at),
                line(call.at)
            )),
            None if inits_gtk_itself => Some(format!(
                "{what} and calls `gtk4::init` itself: call `own_x_server::init_gtk` instead, which also \
                 checks that GDK connected to its own server and nothing else"
            )),
            None => None,
        }
    } else {
        Some(format!(
            "{what}, but its `main` never calls `own_x_server::isolate` or `own_x_server::init_gtk` -- {fix}"
        ))
    }
}

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// `(path from the workspace root, source)` of every file this checks.
fn scanned() -> Vec<(String, String)> {
    let root = workspace();
    let mut files = Vec::new();
    for dir in ["shell/tests", "neovide-editor/tests"] {
        rust_files(&root.join(dir), &mut files);
    }
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let rel = path.strip_prefix(&root).unwrap().to_string_lossy().into_owned();
            let source = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{rel}: {e}"));
            (rel, source)
        })
        .filter(|(rel, _)| !NOT_SCANNED.contains(&rel.as_str()))
        .collect()
}

fn helper() -> PathBuf {
    workspace()
        .join("shell/tests/support/own_x_server.rs")
        .canonicalize()
        .expect("the helper exists")
}

#[test]
fn every_test_that_opens_a_display_starts_its_own() {
    let root = workspace();
    let helper = helper();
    let files = scanned();
    let mut opening = Vec::new();
    let mut offenders = Vec::new();
    for (rel, source) in &files {
        if !display_signals(source).is_empty() {
            opening.push(rel.clone());
        }
        let dir = root.join(rel).parent().unwrap().to_path_buf();
        if let Some(offence) = offence(rel, source, &dir, &helper) {
            offenders.push(offence);
        }
    }
    // A walk or a lexer gone blind would pass everything: 13 open a display today.
    assert!(
        opening.len() >= 13,
        "only {} of {} files open a display -- the scan is broken: {opening:?}",
        opening.len(),
        files.len()
    );
    for known in [
        "shell/tests/panel_stream_scroll.rs",
        "neovide-editor/tests/fullscreen_setting.rs",
    ] {
        assert!(
            opening.iter().any(|f| f == known),
            "{known} is not seen to open a display"
        );
    }
    assert!(
        offenders.is_empty(),
        "{} test file(s) could reach the display they were started from:\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

/// Every [`ALLOWED`] entry is still a file that opens a display: an entry left behind would excuse
/// whatever takes its name next.
#[test]
fn no_allowlist_entry_is_stale() {
    let files = scanned();
    for allowed in ALLOWED {
        let (_, source) = files
            .iter()
            .find(|(rel, _)| rel == allowed.file)
            .unwrap_or_else(|| panic!("{} is on ALLOWED and no longer exists", allowed.file));
        assert!(
            !display_signals(source).is_empty(),
            "{} is on ALLOWED and no longer opens a display",
            allowed.file
        );
    }
}

/// The scanner itself, on sources written to break each rule, so it cannot pass by going blind.
#[test]
fn the_scan_catches_what_it_is_for() {
    let helper = helper();
    let dir = workspace().join("shell/tests");
    let judge = |source: &str| offence("shell/tests/example.rs", source, &dir, &helper);
    let include = "#[path = \"support/own_x_server.rs\"]\nmod own_x_server;\n";

    // What web_host_origin was on `main`: GTK on whatever display it inherited.
    let unguarded = "fn main() {\n    if std::env::var(\"GDK_BACKEND\").as_deref() != Ok(\"x11\") { return; }\n    \
                     gtk4::init().unwrap();\n    let app = gtk4::Application::builder().build();\n}\n";
    assert!(
        judge(unguarded).is_some_and(|o| o.contains("does not include")),
        "{:?}",
        judge(unguarded)
    );
    // Each signal on its own, in code.
    for line in [
        "let h = LiveHarness::with_options(o);",
        "let e = winit::event_loop::EventLoop::new();",
        "let w = webkit6::WebView::new();",
        "let p = neovide_editor::NeovideEditorPane::with_options(o);",
        "let c = x11rb::connect(None);",
        "let c = xcb::Connection::connect(None);",
        "let c = arboard::Clipboard::new();",
        "#[gtk4::test] fn t() { gtk4::Label::new(None); }",
        "gtk4::test_synced(|| gtk4::Label::new(None));",
        "std::process::Command::new(\"Xvfb\").spawn();",
        "std::process::Command::new(env!(\"CARGO_BIN_EXE_shell\")).spawn();",
        "std::process::Command::new(r#\"xdotool\"#).spawn();",
    ] {
        let source = format!("fn main() {{\n    {line}\n}}\n");
        assert!(judge(&source).is_some(), "not caught: {line}");
    }
    // The helper included but never called, or GTK started around it.
    assert!(
        judge(&format!("{include}fn main() {{ gtk4::init().unwrap(); }}")).is_some_and(|o| o.contains("never calls"))
    );
    let around =
        format!("{include}fn main() {{ let _s = own_x_server::isolate(\"t\", \"1x1x24\"); gtk4::init().unwrap(); }}");
    assert!(judge(&around).is_some_and(|o| o.contains("calls `gtk4::init` itself")));
    // A copy of the helper somewhere else is not the helper.
    let elsewhere = "#[path = \"../../elsewhere/own_x_server.rs\"]\nmod own_x_server;\n\
                     fn main() { let _s = own_x_server::init_gtk(\"t\", \"1x1x24\"); }";
    assert!(judge(elsewhere).is_some_and(|o| o.contains("does not include")));

    // What keeps the rule, and what opens nothing.
    assert_eq!(
        judge(&format!(
            "{include}fn main() {{ let _s = own_x_server::init_gtk(\"t\", \"1x1x24\"); }}"
        )),
        None
    );
    assert_eq!(
        judge(&format!(
            "{include}fn main() {{ let _s = own_x_server::isolate(\"t\", \"1x1x24\"); let h = LiveHarness::new(); }}"
        )),
        None
    );
    let mentions = "// LiveHarness, gtk4::init() and WebView in a comment\n/* EventLoop */\n\
                    fn main() { let m = \"LiveHarness sends Quit; gtk4::init()\"; let q = '\"'; let r = r#\"WebView\"#; }\n";
    assert_eq!(display_signals(mentions), Vec::<String>::new());
    assert_eq!(
        display_signals("fn main() { let f = gio::ApplicationFlags::NON_UNIQUE; }"),
        Vec::<String>::new()
    );
    // Code after a tricky literal is still read as code.
    assert_eq!(
        display_signals("fn f<'a>(x: &'a str) { let q = '\\''; let s = \"a \\\" b\"; gtk4::init(); }"),
        vec!["gtk4"]
    );
}

/// The lexer on the literals Rust has, including the ones a naive split on `"` gets wrong.
#[test]
fn code_and_strings_blanks_every_literal_and_comment() {
    let (code, strings) = code_and_strings(
        "a \"s1 \\\" x\" b r#\"s2 \" y\"# c b\"s3\" d '\"' e '\\'' f /* x /* y */ z */ g // h\ni 'l: loop {}",
    );
    let strings: Vec<&str> = strings.iter().map(|literal| literal.text.as_str()).collect();
    assert_eq!(strings, vec!["s1 \\\" x", "s2 \" y", "s3"]);
    let words: Vec<&str> = code.split_whitespace().collect();
    assert_eq!(words, vec!["a", "b", "c", "d", "e", "f", "g", "i", "'l:", "loop", "{}"]);
    assert_eq!(code.lines().count(), 2, "newlines are kept");
}

// ---- fix round 4 (2026-09-29): what the round-3 reviews showed the scan could be walked around by -----------

/// The include a passing source starts with.
const INCLUDE: &str = "#[path = \"support/own_x_server.rs\"]\nmod own_x_server;\n";
/// A call of the helper that keeps the rule when it comes first in `main`.
const HELPER_CALL: &str = "let _s = own_x_server::init_gtk(\"t\", \"1x1x24\");";

/// `offence` for a source that pretends to be `shell/tests/example.rs`.
fn judge(source: &str) -> Option<String> {
    offence(
        "shell/tests/example.rs",
        source,
        &workspace().join("shell/tests"),
        &helper(),
    )
}

/// The cases `judge` does not refuse with a message containing `needle`, one line each.
fn not_refused(cases: &[(&str, String)], needle: &str) -> Vec<String> {
    cases
        .iter()
        .filter_map(|(name, source)| match judge(source) {
            Some(offence) if offence.contains(needle) => None,
            other => Some(format!(
                "  {name}: wanted a refusal containing {needle:?}, got {other:?}"
            )),
        })
        .collect()
}

/// The cases `judge` refuses although they keep the rule, one line each.
fn wrongly_refused(cases: &[(&str, String)]) -> Vec<String> {
    cases
        .iter()
        .filter_map(|(name, source)| judge(source).map(|offence| format!("  {name}: refused: {offence}")))
        .collect()
}

/// Item 1: gtk-rs's test macros initialise GTK before the body runs, so a helper call inside them is too late.
#[test]
fn gtk_rs_test_macros_are_refused_outright() {
    let refused = |name: &'static str, body: &str| (name, format!("{INCLUDE}{body}"));
    let cases = vec![
        refused(
            "#[gtk4::test] with the helper first in its body (Codex)",
            &format!("#[gtk4::test]\nfn t() {{ {HELPER_CALL} }}\nfn main() {{}}\n"),
        ),
        refused(
            "#[gtk::test] on `use gtk4 as gtk`, the spelling gtk-rs documents",
            &format!("use gtk4 as gtk;\n#[gtk::test]\nfn t() {{ {HELPER_CALL} }}\nfn main() {{}}\n"),
        ),
        refused(
            "#[g::test] on any other alias",
            &format!("use gtk4 as g;\n#[g::test]\nfn t() {{ {HELPER_CALL} }}\nfn main() {{}}\n"),
        ),
        refused(
            "gtk4::test_synced after the helper in main",
            &format!("fn main() {{ {HELPER_CALL} gtk4::test_synced(|| {{}}); }}\n"),
        ),
        refused(
            "gtk::test_synced after the helper in main",
            &format!("fn main() {{ {HELPER_CALL} gtk::test_synced(|| {{}}); }}\n"),
        ),
        refused(
            "the macro imported by name",
            &format!("use gtk4::test;\n#[test]\nfn t() {{ {HELPER_CALL} }}\nfn main() {{}}\n"),
        ),
        refused(
            "test_synced imported in a group",
            &format!("use gtk4::{{self, test_synced}};\nfn main() {{ {HELPER_CALL} test_synced(|| {{}}); }}\n"),
        ),
    ];
    let mut missed = not_refused(&cases, "gtk-rs's test macro");
    // The allowlisted file is no exception: a macro in it initialises GTK before its own guards.
    let real = std::fs::read_to_string(workspace().join(ALLOWED[0].file)).unwrap();
    let with_macro = format!("{real}\n#[gtk4::test]\nfn extra() {{}}\n");
    let verdict = offence(
        ALLOWED[0].file,
        &with_macro,
        &workspace().join("neovide-editor/tests"),
        &helper(),
    );
    if !verdict.as_deref().is_some_and(|o| o.contains("gtk-rs's test macro")) {
        missed.push(format!("  the allowlisted file with a macro added: got {verdict:?}"));
    }
    assert!(
        missed.is_empty(),
        "{} case(s) not refused:\n{}",
        missed.len(),
        missed.join("\n")
    );

    // A comment, a string and plain `#[test]` are none of it.
    let fine = vec![
        (
            "the macros named in a comment and a string",
            format!("{INCLUDE}// #[gtk4::test]\n/* gtk4::test_synced */\nfn main() {{ {HELPER_CALL} let m = \"gtk4::test\"; }}\n"),
        ),
        (
            "a plain #[test]",
            format!("{INCLUDE}#[test]\nfn t() {{}}\nfn main() {{ {HELPER_CALL} }}\n"),
        ),
    ];
    let wrong = wrongly_refused(&fine);
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// Item 2: a crate that only ever talks to a display is a display signal wherever it is named -- imported,
/// renamed, globbed, grouped, or written out -- not only where one of its entry points is.
#[test]
fn display_crates_are_signals_however_they_are_imported() {
    let must_signal = [
        "use gtk4 as g;\nfn main() { g::init().unwrap(); }",
        "use gtk4::{init};\nfn main() { init().unwrap(); }",
        "use gtk4::{self, init};\nfn main() { init().unwrap(); }",
        "use gtk4::*;\nfn main() {}",
        "use gtk4::{self as g};\nfn main() {}",
        "use {gtk4, glib};\nfn main() {}",
        "pub use gtk4::prelude::*;\nfn main() {}",
        "use ::gtk4::prelude::*;\nfn main() {}",
        "use gdk4::prelude::*;\nfn main() {}",
        "use webkit6::prelude::*;\nfn main() {}",
        "use winit as w;\nfn main() {}",
        "use smithay_client_toolkit as sctk;\nfn main() {}",
        "use xcb as x;\nfn main() {}",
        "extern crate gtk4 as g;\nfn main() {}",
        "fn main() { gtk4::rt::init().unwrap(); }",
        "fn main() { use gtk4::prelude::*; }",
        "fn main() { let d = gdk4::Display::default(); }",
    ];
    let mut missed: Vec<String> = Vec::new();
    for source in must_signal {
        let signals = display_signals(source);
        if signals.is_empty() {
            missed.push(format!("  no signal: {source:?}"));
        } else if !judge(source).is_some_and(|o| o.contains("does not include")) {
            missed.push(format!("  not refused as unguarded: {source:?}"));
        }
    }
    assert!(
        missed.is_empty(),
        "{} case(s) missed:\n{}",
        missed.len(),
        missed.join("\n")
    );

    // Neither a name that only contains one, another crate, a comment, a string, nor a local called xcb.
    for source in [
        "use std::{io, fmt};\nfn main() {}",
        "use gtk4_macros::helper;\nfn main() {}",
        "use my_winit::Thing;\nfn main() {}",
        "use glib::prelude::*;\nuse gio::prelude::*;\nfn main() {}",
        "mod gtk4_notes;\nfn main() {}",
        "// use gtk4 as g;\n/* extern crate winit; */\nfn main() {}",
        "fn main() { let s = \"use gtk4 as g;\"; let t = r#\"gtk4::init()\"#; }",
        "fn main() { let xcb = 1; let _ = xcb + 1; }",
    ] {
        assert_eq!(
            display_signals(source),
            Vec::<String>::new(),
            "wrongly a signal: {source:?}"
        );
    }
}

/// The flattener the alias and macro checks stand on: every shape a `use` can take.
#[test]
fn use_items_flattens_groups_globs_and_renames() {
    let flat = |code: &str| -> Vec<(String, Option<String>)> {
        use_items(code)
            .into_iter()
            .map(|item| (item.path.join("::"), item.alias))
            .collect()
    };
    let named = |path: &str, alias: Option<&str>| (path.to_string(), alias.map(str::to_string));
    assert_eq!(flat("use gtk4 as gtk;"), vec![named("gtk4", Some("gtk"))]);
    assert_eq!(
        flat("use gtk4::{self as g, init, prelude::*};"),
        vec![
            named("gtk4", Some("g")),
            named("gtk4::init", None),
            named("gtk4::prelude::*", None)
        ]
    );
    assert_eq!(
        flat("pub(crate) use a::{b::{c, d as e}, f,};"),
        vec![named("a::b::c", None), named("a::b::d", Some("e")), named("a::f", None)]
    );
    assert_eq!(
        flat("use {gtk4, glib as g,};"),
        vec![named("gtk4", None), named("glib", Some("g"))]
    );
    assert_eq!(flat("use ::gtk4::x;"), vec![named("gtk4::x", None)]);
    assert_eq!(
        flat("extern crate gtk4 as g;\nextern crate winit;"),
        vec![named("gtk4", Some("g")), named("winit", None)]
    );
    assert_eq!(flat("fn f() -> impl Sized + use<'a> {}"), vec![]);
    assert_eq!(flat("#[macro_use]\nmod m;\nfn f() { let user = 1; }"), vec![]);
    assert_eq!(flat("use std::io;\nuse std::fmt::{self, Write};").len(), 3);
}

/// Item 3: a program is recognised by the command a literal starts, not only by a literal that is exactly
/// its name -- a path to it, a command line, and a program named inside a shell's `-c` script.
#[test]
fn programs_are_matched_by_command() {
    let in_main = |line: &str| format!("fn main() {{\n    {line}\n}}\n");
    let programs = [
        "xdotool",
        "Xvfb",
        "Xwayland",
        "Xephyr",
        "xvfb-run",
        "sway",
        "swaymsg",
        "ydotool",
        "wtype",
        "grim",
        "slurp",
        "wl-copy",
        "wl-paste",
        "xclip",
        "xsel",
        "notify-send",
    ];
    let mut missed: Vec<String> = Vec::new();
    let mut must_signal: Vec<String> = Vec::new();
    for program in programs {
        must_signal.push(format!("std::process::Command::new(\"{program}\").spawn();"));
        must_signal.push(format!("std::process::Command::new(\"/usr/bin/{program}\").spawn();"));
        must_signal.push(format!("std::process::Command::new(\"{program} --version\").spawn();"));
    }
    must_signal.extend(
        [
            r#"std::process::Command::new("xdotool key --clearmodifiers a").status();"#,
            r#"std::process::Command::new("Xvfb :99 -nolisten tcp").spawn();"#,
            r#"let c = "sway --config /dev/null";"#,
            r#"let c = "xdotool\nkey a";"#,
            r#"std::process::Command::new("sh").args(["-c", "echo hi | xclip -selection clipboard"]).status();"#,
            r#"std::process::Command::new("bash").arg("-c").arg("cat f | wl-copy").status();"#,
            r#"std::process::Command::new("/bin/bash").arg("-lc").arg("grim -").status();"#,
            r#"std::process::Command::new("sh").arg("-c").arg("cd /tmp && sway --config /dev/null").status();"#,
            r#"let c = "sh -c 'xclip -o'";"#,
        ]
        .map(String::from),
    );
    for line in &must_signal {
        if display_signals(&in_main(line)).is_empty() {
            missed.push(format!("  not a signal: {line}"));
        }
    }
    assert!(
        missed.is_empty(),
        "{} case(s) missed:\n{}",
        missed.len(),
        missed.join("\n")
    );

    // Words that only contain a program's name, a message that does not start with one, and a shell that
    // runs something else.
    for line in [
        r#"let m = "the sway compositor is not running";"#,
        r#"let m = "`xdotool` is not on PATH";"#,
        r#"let m = "install xdotool";"#,
        r#"let m = "swayed grimace xdotoolkit slurp-ish";"#,
        r#"std::process::Command::new("sh").args(["-c", "echo grimace swayed xdotoolkit"]).status();"#,
        r#"std::process::Command::new("cargo").args(["build", "-p", "shell"]).status();"#,
    ] {
        assert_eq!(
            display_signals(&in_main(line)),
            Vec::<String>::new(),
            "wrongly a signal: {line}"
        );
    }
}

/// Item 5: the allowlist binds to `cursor_animation`'s real `main` -- GTK is initialised once, after the
/// checks, and each check that can refuse ends the process when it does.
#[test]
fn the_allowlisted_main_is_bound_to_its_guard_sequence() {
    let file = ALLOWED[0].file;
    let real = std::fs::read_to_string(workspace().join(file)).unwrap();
    let dir = workspace().join("neovide-editor/tests");
    let judge_file = |source: &str| offence(file, source, &dir, &helper());
    assert_eq!(judge_file(&real), None, "the real file must keep the rule");

    // Mutations of the real `main`, which is the last function in the file.
    let main_at = real
        .rfind("fn main() {")
        .expect("cursor_animation.rs has a `fn main() {`");
    let open = main_at + "fn main() {".len();
    let insert_first = |code: &str| format!("{}\n    {code}{}", &real[..open], &real[open..]);
    let replace_in_main = |needle: &str, nth: usize, with: &str| -> String {
        let at = real[main_at..]
            .match_indices(needle)
            .nth(nth)
            .map(|(offset, _)| main_at + offset)
            .unwrap_or_else(|| panic!("`{needle}` #{nth} is not in `main` any more -- update this test"));
        format!("{}{with}{}", &real[..at], &real[at + needle.len()..])
    };
    let guards = [
        "sandbox_wayland_socket",
        "require_headless_compositor",
        "require_headless_outputs",
    ];

    let mut missed: Vec<String> = Vec::new();
    let mut expect = |name: &str, source: String, needle: &str| match judge_file(&source) {
        Some(offence) if offence.contains(needle) => {}
        other => missed.push(format!(
            "  {name}: wanted a refusal containing {needle:?}, got {other:?}"
        )),
    };
    expect(
        "GTK initialised first, the original init left after the guards (Codex)",
        insert_first("gtk4::init().unwrap();"),
        "exactly once",
    );
    expect(
        "a second init at the end of main",
        replace_in_main(
            "println!(\"cursor_animation: passed",
            0,
            "gtk4::init().unwrap();\n    println!(\"cursor_animation: passed",
        ),
        "exactly once",
    );
    expect(
        "another spelling of init",
        insert_first("gtk4::rt::init().unwrap();"),
        "gtk4::rt::init",
    );
    for (k, guard) in guards.iter().enumerate() {
        expect(
            &format!("{guard}'s refusal no longer exits"),
            replace_in_main("std::process::exit(1);", k, "let _went_on = ();"),
            guard,
        );
    }
    expect(
        "require_headless_outputs's result thrown away",
        replace_in_main(
            "match require_headless_outputs() {",
            0,
            "let _ignored = require_headless_outputs();\n    match Some(0) {",
        ),
        "require_headless_outputs",
    );
    // A `return` refuses as well as an exit does.
    assert_eq!(
        judge_file(&replace_in_main("std::process::exit(1);", 1, "return;")),
        None
    );

    // The other shapes a refusal can take, on a `main` that is otherwise in order.
    let synthetic = |compositor: &str, outputs: &str| {
        format!(
            "fn main() {{ own_x_server::cut_session_bus(\"t\");\n\
             let s = match sandbox_wayland_socket() {{ Ok(s) => s, Err(e) => {{ std::process::exit(1); }} }};\n\
             {compositor}\n gtk4::init().unwrap();\n {outputs}\n let p = NeovideEditorPane::with_options(o); }}\n"
        )
    };
    let arm = "match require_headless_compositor(&s) { Ok(_) => {} Err(e) => { std::process::exit(1) } }";
    let outputs = "match require_headless_outputs() { Ok(_) => {} Err(_) => return, }";
    assert_eq!(
        judge_file(&synthetic(arm, outputs)),
        None,
        "the synthetic main keeps the rule"
    );
    let if_let = "if let Err(e) = require_headless_compositor(&s) { eprintln!(\"{e}\"); std::process::exit(1); }";
    assert_eq!(
        judge_file(&synthetic(if_let, outputs)),
        None,
        "an `if let Err` that exits refuses"
    );
    expect(
        "an `if let Err` that only prints",
        synthetic(
            "if let Err(e) = require_headless_compositor(&s) { eprintln!(\"{e}\"); }",
            outputs,
        ),
        "require_headless_compositor",
    );
    expect(
        "a result bound and dropped",
        synthetic("let _ = require_headless_compositor(&s);", outputs),
        "require_headless_compositor",
    );
    expect(
        "an expression arm that does not exit",
        synthetic(arm, "match require_headless_outputs() { Ok(_) => {} Err(_) => (), }"),
        "require_headless_outputs",
    );
    assert!(
        missed.is_empty(),
        "{} case(s) not refused:\n{}",
        missed.len(),
        missed.join("\n")
    );
}

/// Item 4: the helper counts only when `main` calls it, before anything in `main` opens a display -- a call
/// in a function nothing runs, or one that comes after the connection, isolates nothing.
#[test]
fn the_helper_must_come_first_in_main() {
    let isolate = "own_x_server::isolate(\"t\", \"1x1x24\")";
    let with = |body: String| format!("{INCLUDE}{body}");
    let never_calls = vec![
        (
            "the helper in a function nothing runs, xdotool in main (Codex)",
            with(format!(
                "fn unused() {{ let _s = {isolate}; }}\nfn main() {{ std::process::Command::new(\"xdotool\").args([\"key\", \"a\"]).status().unwrap(); }}\n"
            )),
        ),
        (
            "the helper in a function main does not call",
            with(format!(
                "fn main() {{ let w = gtk4::Window::new(); }}\nfn later() {{ let _s = {isolate}; }}\n"
            )),
        ),
    ];
    let before = vec![
        (
            "a window before init_gtk",
            with(
                "fn main() { let w = gtk4::Window::new(); let _s = own_x_server::init_gtk(\"t\", \"1x1x24\"); }\n"
                    .into(),
            ),
        ),
        (
            "a LiveHarness before isolate",
            with(format!(
                "fn main() {{ let h = LiveHarness::new(); let _s = {isolate}; }}\n"
            )),
        ),
        (
            "xdotool before isolate",
            with(format!(
                "fn main() {{ std::process::Command::new(\"xdotool\").status().ok(); let _s = {isolate}; }}\n"
            )),
        ),
        (
            "an import of a display crate inside main, before the helper",
            with(format!("fn main() {{ use gtk4::prelude::*; let _s = {isolate}; }}\n")),
        ),
        (
            "an event loop built before isolate",
            with(format!(
                "fn main() {{ let e = winit::event_loop::EventLoop::new(); let _s = {isolate}; }}\n"
            )),
        ),
    ];
    let no_main = vec![(
        "a libtest #[test] with no main to call the helper in",
        with(
            "#[test]\nfn t() { let _s = own_x_server::init_gtk(\"t\", \"1x1x24\"); let w = gtk4::Window::new(); }\n"
                .into(),
        ),
    )];
    let mut missed = not_refused(&never_calls, "never calls");
    missed.extend(not_refused(&before, "before it calls the helper"));
    missed.extend(not_refused(&no_main, "no `fn main`"));
    assert!(
        missed.is_empty(),
        "{} case(s) not refused:\n{}",
        missed.len(),
        missed.join("\n")
    );

    // What keeps the rule: the helper first, signals elsewhere, the helper in a branch main takes.
    let fine = vec![
        (
            "the helper first, then a window",
            with("fn main() { let _s = own_x_server::init_gtk(\"t\", \"1x1x24\"); let w = gtk4::Window::new(); }\n".into()),
        ),
        (
            "signals only in another function",
            with(
                "fn open() { let w = gtk4::Window::new(); }\nfn main() { let _s = own_x_server::init_gtk(\"t\", \"1x1x24\"); open(); }\n"
                    .into(),
            ),
        ),
        (
            "the helper in a branch (unfocused_cursor's re-execution)",
            with(format!(
                "fn measure() {{ let h = LiveHarness::new(); }}\nfn main() {{ if std::env::var(\"CASE\").is_ok() {{ let _s = {isolate}; measure(); return; }} }}\n"
            )),
        ),
    ];
    let wrong = wrongly_refused(&fine);
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

// ---- the parked limits' follow-up (2026-09-29): aliases resolved outside the flattener -----------------------

/// Limits (1) and (3) of the dated record's "merged, with three scanner limits parked": a name a `use` binds to
/// a display crate, or to a path in one -- `g` in `use gtk4 as g;`, `start` in `use gtk4::init as start;`,
/// through any chain of them -- is that crate wherever it is used: in `main`'s order, in the allowlisted file's
/// count of GTK's init, and in the macro ban.
#[test]
fn aliases_count_in_main_and_in_the_macro_ban() {
    let with = |body: &str| format!("{INCLUDE}{body}");
    // (1) An alias used in `main` before the helper is a connection before it.
    let before = vec![
        (
            "g::init() before init_gtk, `use gtk4 as g;` (Codex)",
            with(&format!(
                "use gtk4 as g;\nfn main() {{ g::init().unwrap(); {HELPER_CALL} }}\n"
            )),
        ),
        (
            "an item renamed out of an alias, `use g::init as start;`",
            with(&format!(
                "use gtk4 as g;\nuse g::init as start;\nfn main() {{ start().unwrap(); {HELPER_CALL} }}\n"
            )),
        ),
        (
            "an alias of an alias, `use gtk4 as g; use g::rt as r;`",
            with(&format!(
                "use gtk4 as g;\nuse g::rt as r;\nfn main() {{ r::init().unwrap(); {HELPER_CALL} }}\n"
            )),
        ),
        (
            "an extern crate rename, `extern crate winit as w;`",
            with(&format!(
                "extern crate winit as w;\nfn main() {{ let s = w::dpi::LogicalSize::new(1, 1); {HELPER_CALL} }}\n"
            )),
        ),
    ];
    // (3) A test macro brought in through an alias.
    let macros = vec![
        (
            "test_synced grouped through an alias, `use g::{test_synced as run}` (Codex)",
            with(&format!(
                "use gtk4 as g;\nuse g::{{test_synced as run}};\nfn main() {{ {HELPER_CALL} run(|| {{}}); }}\n"
            )),
        ),
        (
            "the attribute imported through an alias, `use g::test;`",
            with(&format!(
                "use gtk4 as g;\nuse g::test;\n#[test]\nfn t() {{ {HELPER_CALL} }}\nfn main() {{}}\n"
            )),
        ),
        (
            "the attribute on an alias of an alias, `#[h::test]`",
            with(&format!(
                "use gtk4 as g;\nuse g as h;\n#[h::test]\nfn t() {{ {HELPER_CALL} }}\nfn main() {{}}\n"
            )),
        ),
    ];
    let itself = vec![(
        "g::init() after init_gtk, `use gtk4 as g;`",
        with(&format!(
            "use gtk4 as g;\nfn main() {{ {HELPER_CALL} g::init().unwrap(); }}\n"
        )),
    )];
    let mut missed = not_refused(&before, "before it calls the helper");
    missed.extend(not_refused(&macros, "gtk-rs's test macro"));
    missed.extend(not_refused(&itself, "calls `gtk4::init` itself"));

    // (1) in the allowlisted file: an aliased init ahead of its guards is a second init.
    let file = ALLOWED[0].file;
    let real = std::fs::read_to_string(workspace().join(file)).unwrap();
    let main_at = real
        .rfind("fn main() {")
        .expect("cursor_animation.rs has a `fn main() {`");
    let open = main_at + "fn main() {".len();
    let aliased = |import: &str, call: &str| {
        format!(
            "{}{import}\n{}\n    {call}{}",
            &real[..main_at],
            &real[main_at..open],
            &real[open..]
        )
    };
    for (name, source, needle) in [
        (
            "`use gtk4 as g;` and g::init() first in cursor_animation's main",
            aliased("use gtk4 as g;", "g::init().unwrap();"),
            "g::init",
        ),
        (
            "`use g::rt::init as start;` and start() first in cursor_animation's main",
            aliased("use gtk4 as g;\nuse g::rt::init as start;", "start().unwrap();"),
            "start",
        ),
    ] {
        let verdict = offence(file, &source, &workspace().join("neovide-editor/tests"), &helper());
        if !verdict.as_deref().is_some_and(|o| o.contains(needle)) {
            missed.push(format!(
                "  {name}: wanted a refusal containing {needle:?}, got {verdict:?}"
            ));
        }
    }
    assert!(
        missed.is_empty(),
        "{} case(s) not refused:\n{}",
        missed.len(),
        missed.join("\n")
    );

    // As before: a path written out is judged as it was, and an alias of anything else is nothing.
    assert!(
        judge(&with(&format!(
            "fn main() {{ let w = gtk4::Window::new(); {HELPER_CALL} }}\n"
        )))
        .is_some_and(|o| o.contains("before it calls the helper")),
        "a written-out path before the helper is still refused"
    );
    let fine = vec![
        (
            "a written-out path after the helper",
            with(&format!("fn main() {{ {HELPER_CALL} let w = gtk4::Window::new(); }}\n")),
        ),
        (
            "an alias used after the helper",
            with(&format!("use gtk4 as g;\nfn main() {{ {HELPER_CALL} let w = g::Window::new(); }}\n")),
        ),
        (
            "an unrelated alias before the helper",
            with(&format!(
                "use std::process as p;\nuse p::{{id as pid}};\nfn main() {{ p::id(); pid(); {HELPER_CALL} gtk4::Window::new(); }}\n"
            )),
        ),
        (
            "an alias's name as a method or a path's tail before the helper",
            with(&format!(
                "use gtk4 as g;\nfn main() {{ x.g(); y::g::z(); {HELPER_CALL} g::Label::new(None); }}\n"
            )),
        ),
        (
            "a non-macro item grouped through an alias",
            with(&format!("use gtk4 as g;\nuse g::{{Label as L}};\nfn main() {{ {HELPER_CALL} L::new(None); }}\n")),
        ),
    ];
    let wrong = wrongly_refused(&fine);
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    let unrelated = "use std::io as sio;\nuse sio::{Write as W};\nfn main() { sio::stdout(); }\n";
    assert_eq!(display_signals(unrelated), Vec::<String>::new());
}

/// The review of the alias change (2026-09-29): a glob of a display crate, direct or through an alias, binds GTK's
/// init and test macros as a named import does; an alias reached through `self::`, `crate::`, `super::` or a
/// module declared in the file is the alias; a name bound twice is followed through every binding, not the
/// first; and a bare name bound to a crate is not a connection where it is only a local, a field or a pattern.
#[test]
fn globs_qualified_aliases_and_rebindings_count_too() {
    let with = |body: &str| format!("{INCLUDE}{body}");
    let before = vec![
        (
            "a glob of gtk4, then init() before the helper",
            with(&format!("use gtk4::*;\nfn main() {{ init().unwrap(); {HELPER_CALL} }}\n")),
        ),
        (
            "a glob through an alias, `use gtk4 as g; use g::*;`",
            with(&format!(
                "use gtk4 as g;\nuse g::*;\nfn main() {{ init().unwrap(); {HELPER_CALL} }}\n"
            )),
        ),
        (
            "a glob of gtk4::rt, then init()",
            with(&format!("use gtk4::rt::*;\nfn main() {{ init().unwrap(); {HELPER_CALL} }}\n")),
        ),
        (
            "self::g::init() before the helper",
            with(&format!(
                "use gtk4 as g;\nfn main() {{ self::g::init().unwrap(); {HELPER_CALL} }}\n"
            )),
        ),
        (
            "crate::g::init() before the helper",
            with(&format!(
                "use gtk4 as g;\nfn main() {{ crate::g::init().unwrap(); {HELPER_CALL} }}\n"
            )),
        ),
        (
            "m::g::init() through `mod m { pub use gtk4 as g; }`",
            with(&format!(
                "mod m {{ pub use gtk4 as g; }}\nfn main() {{ m::g::init().unwrap(); {HELPER_CALL} }}\n"
            )),
        ),
        (
            "an alias called as a struct field's value, `S { f: g::init() }`",
            with(&format!(
                "use gtk4 as g;\nfn main() {{ let s = S {{ f: g::init() }}; {HELPER_CALL} }}\n"
            )),
        ),
        (
            "the alias bound again, to something else, before the gtk4 binding",
            with(&format!(
                "fn a() {{ use std::fmt as g; }}\nuse gtk4 as g;\nuse g::rt as r;\nfn main() {{ r::init().unwrap(); {HELPER_CALL} }}\n"
            )),
        ),
    ];
    let macros = vec![
        (
            "test_synced through a glob of gtk4, the helper inside it",
            with(&format!("use gtk4::*;\nfn main() {{ test_synced(|| {{ {HELPER_CALL} }}); }}\n")),
        ),
        (
            "test_synced through a glob of an alias, after the helper",
            with(&format!(
                "use gtk4 as g;\nuse g::*;\nfn main() {{ {HELPER_CALL} test_synced(|| {{}}); }}\n"
            )),
        ),
        (
            "test_synced grouped through `crate::g` (Codex)",
            with(&format!(
                "use gtk4 as g;\nuse crate::g::{{test_synced as run}};\nfn main() {{ run(|| {{}}); {HELPER_CALL} }}\n"
            )),
        ),
        (
            "test_synced through `self::g`",
            with(&format!(
                "use gtk4 as g;\nuse self::g::test_synced as run;\nfn main() {{ {HELPER_CALL} run(|| {{}}); }}\n"
            )),
        ),
        (
            "test_synced through `m::g`, `mod m { pub use gtk4 as g; }`",
            with(&format!(
                "mod m {{ pub use gtk4 as g; }}\nuse m::g::{{test_synced as run}};\nfn main() {{ {HELPER_CALL} run(|| {{}}); }}\n"
            )),
        ),
        (
            "the alias bound again, to something else, before the gtk4 binding",
            with(&format!(
                "mod a {{ use std::fmt as g; }}\nuse gtk4 as g;\nuse g::{{test_synced as run}};\nfn main() {{ {HELPER_CALL} run(|| {{}}); }}\n"
            )),
        ),
    ];
    let itself = vec![(
        "init() through a glob, after the helper",
        with(&format!(
            "use gtk4::*;\nfn main() {{ {HELPER_CALL} init().unwrap(); }}\n"
        )),
    )];
    let mut missed = not_refused(&before, "before it calls the helper");
    missed.extend(not_refused(&macros, "gtk-rs's test macro"));
    missed.extend(not_refused(&itself, "calls `gtk4::init` itself"));

    // In the allowlisted file: an init a glob brought in, ahead of its guards, is a second init.
    let file = ALLOWED[0].file;
    let real = std::fs::read_to_string(workspace().join(file)).unwrap();
    let main_at = real
        .rfind("fn main() {")
        .expect("cursor_animation.rs has a `fn main() {`");
    let open = main_at + "fn main() {".len();
    for (name, import, call, needle) in [
        (
            "a glob of gtk4 and init() first",
            "use gtk4::*;",
            "init().unwrap();",
            "`init`",
        ),
        (
            "crate::g::init() first",
            "use gtk4 as g;",
            "crate::g::init().unwrap();",
            "g::init",
        ),
    ] {
        let source = format!(
            "{}{import}\n{}\n    {call}{}",
            &real[..main_at],
            &real[main_at..open],
            &real[open..]
        );
        let verdict = offence(file, &source, &workspace().join("neovide-editor/tests"), &helper());
        if !verdict.as_deref().is_some_and(|o| o.contains(needle)) {
            missed.push(format!(
                "  cursor_animation, {name}: wanted a refusal containing {needle:?}, got {verdict:?}"
            ));
        }
    }
    assert!(
        missed.is_empty(),
        "{} case(s) not refused:\n{}",
        missed.len(),
        missed.join("\n")
    );

    // A bare name bound to a crate is a connection only where it is used as one: not as a local, a field, a
    // pattern, or the tail of another path.
    let fine = vec![
        (
            "a struct field named like the alias (Codex)",
            with(&format!(
                "use gtk4 as g;\nstruct Rgb {{ r: u8, g: u8, b: u8 }}\nfn main() {{ let bg = Rgb {{ r: 0, g: 0, b: 0 }}; {HELPER_CALL} let l = g::Label::new(None); }}\n"
            )),
        ),
        (
            "a local named like the alias",
            with(&format!(
                "use gtk4 as g;\nfn main() {{ let g = 1; let y = g + 1; {HELPER_CALL} }}\n"
            )),
        ),
        (
            "a match binding named like the alias",
            with(&format!(
                "use gtk4 as g;\nfn main() {{ let _ = match 1 {{ g => g }}; {HELPER_CALL} }}\n"
            )),
        ),
        (
            "a prelude glob, and a path written out after the helper",
            with(&format!(
                "use gtk4::prelude::*;\nfn main() {{ {HELPER_CALL} let w = gtk4::Window::new(); }}\n"
            )),
        ),
        (
            "a path through another crate's module named like a local one",
            with(&format!(
                "use gtk4 as g;\nfn main() {{ y::g::z(); {HELPER_CALL} }}\n"
            )),
        ),
    ];
    let wrong = wrongly_refused(&fine);
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));

    // A type found by its own name is named once.
    assert_eq!(
        display_signals("use webkit6::WebView;\nfn main() { let w = WebView::new(); }\n"),
        vec!["webkit6", "WebView"]
    );
}
