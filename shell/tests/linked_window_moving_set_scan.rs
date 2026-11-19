//! The linked windows' code in `eitri-desk` names no toolkit and no other `shell` module.
//!
//! The set is every file under `desk/src/linked/`: each of them may name only itself and `crate::host_traits`; a
//! toolkit name or any other `crate::` path in them would stop the code compiling in a crate that has no toolkit and
//! no `shell`. (`eitri-desk`'s own `toolkit_names_guard` names the toolkit fragments; this scan also reads the `crate::`
//! paths and the `super::` climbs.)
//!
//! Read through `eitri_core::source_scan`, which fails on a missing or empty directory. The checker works on
//! code text (comments and string literals blanked) and on tokens, so a path split over several lines, or
//! written with spaces around `::`, is read the same as one on a single line.

use eitri_core::source_scan::{code_only, rust_sources, Source};

/// Names of the toolkits (and the forked renderer built on one) that `eitri-desk` must not reach.
const TOOLKIT_NAMES: &[&str] = &["gtk4", "gtk", "glib", "gio", "gdk", "gdk4", "webkit6", "neovide_editor"];

/// The `crate::` paths the set may name: its own modules and the host traits.
const ALLOWED_CRATE_PATHS: &[&str] = &["crate::linked", "crate::host_traits"];

/// The directory the set is read from, recursively.
const MOVING_DIR: &str = "desk/src/linked";

/// One place the moving set reaches outside itself.
#[derive(Debug, PartialEq, Eq)]
struct Violation {
    /// 1-based line of the first token of the offending name or path.
    line: usize,
    reason: String,
}

#[derive(Debug, PartialEq, Eq)]
enum Kind<'a> {
    Ident(&'a str),
    PathSep,
    Open,
    Close,
    Other,
}

struct Token<'a> {
    kind: Kind<'a>,
    line: usize,
}

/// The tokens of blanked code: identifiers, `::`, braces and any other single character. Whitespace and line
/// breaks separate tokens and are otherwise ignored.
fn tokens(code: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let mut line = 1;
    let mut chars = code.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let kind = match c {
            '\n' => {
                line += 1;
                continue;
            }
            c if c.is_whitespace() => continue,
            c if c.is_alphanumeric() || c == '_' => {
                let mut end = at + c.len_utf8();
                while let Some(&(next_at, next)) = chars.peek() {
                    if !(next.is_alphanumeric() || next == '_') {
                        break;
                    }
                    end = next_at + next.len_utf8();
                    chars.next();
                }
                Kind::Ident(&code[at..end])
            }
            ':' if chars.peek().is_some_and(|&(_, next)| next == ':') => {
                chars.next();
                Kind::PathSep
            }
            '{' => Kind::Open,
            '}' => Kind::Close,
            _ => Kind::Other,
        };
        out.push(Token { kind, line });
    }
    out
}

/// How many modules deep the file's own module sits below `linked`: `mod.rs` is `linked` itself
/// (0), `spawn.rs` is one level down, `registry/tests/tabs.rs` three. A file outside the directory is the root
/// of its own unit.
fn module_depth(path: &str) -> usize {
    let Some(inside) = path.strip_prefix(&format!("{MOVING_DIR}/")) else {
        return 0;
    };
    let inside = inside.strip_suffix(".rs").unwrap_or(inside);
    let inside = inside.strip_suffix("/mod").unwrap_or(inside);
    if inside == "mod" {
        0
    } else {
        inside.split('/').count()
    }
}

fn allowed_crate_path(path: &str) -> bool {
    ALLOWED_CRATE_PATHS
        .iter()
        .any(|allowed| path == *allowed || path.strip_prefix(allowed).is_some_and(|rest| rest.starts_with("::")))
}

/// Every place `text` (the source of `path`) names a toolkit, a `crate::` path outside the allowed list, a
/// `crate::{..}` group, or a `super::` chain that leaves the moving unit. Comments and string literals do not
/// count.
fn violations(path: &str, text: &str) -> Vec<Violation> {
    let code = code_only(text);
    let toks = tokens(&code);
    let kind_at = |i: usize| toks.get(i).map(|t| &t.kind);
    let file_depth = module_depth(path);
    let mut found = Vec::new();
    // Brace depth at which each inline `mod name {` opened, so `super::` inside one is counted from there.
    let mut braces = 0usize;
    let mut inline_mods: Vec<usize> = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        match &tok.kind {
            Kind::Open => {
                if i >= 2
                    && kind_at(i - 2) == Some(&Kind::Ident("mod"))
                    && matches!(kind_at(i - 1), Some(Kind::Ident(_)))
                {
                    inline_mods.push(braces);
                }
                braces += 1;
            }
            Kind::Close => {
                braces = braces.saturating_sub(1);
                if inline_mods.last() == Some(&braces) {
                    inline_mods.pop();
                }
            }
            Kind::Ident(name) if TOOLKIT_NAMES.contains(name) => found.push(Violation {
                line: tok.line,
                reason: format!("toolkit name `{name}`"),
            }),
            Kind::Ident("crate") if kind_at(i + 1) == Some(&Kind::PathSep) => {
                if kind_at(i + 2) == Some(&Kind::Open) {
                    found.push(Violation {
                        line: tok.line,
                        reason: "`crate::{` group: write the paths out".to_owned(),
                    });
                    continue;
                }
                let mut path_text = String::from("crate");
                let mut j = i + 1;
                while kind_at(j) == Some(&Kind::PathSep) {
                    let Some(Kind::Ident(segment)) = kind_at(j + 1) else {
                        break;
                    };
                    path_text.push_str("::");
                    path_text.push_str(segment);
                    j += 2;
                }
                if !allowed_crate_path(&path_text) {
                    let group = kind_at(j) == Some(&Kind::PathSep) && kind_at(j + 1) == Some(&Kind::Open);
                    found.push(Violation {
                        line: tok.line,
                        reason: if group {
                            format!("`{path_text}::{{` is not an allowed path (write each path out)")
                        } else {
                            format!("`{path_text}` is not an allowed path")
                        },
                    });
                }
            }
            Kind::Ident("super") if i == 0 || kind_at(i - 1) != Some(&Kind::PathSep) => {
                let mut climbs = 0;
                let mut j = i;
                while kind_at(j) == Some(&Kind::Ident("super")) && kind_at(j + 1) == Some(&Kind::PathSep) {
                    climbs += 1;
                    j += 2;
                }
                let depth = file_depth + inline_mods.len();
                if climbs > depth {
                    found.push(Violation {
                        line: tok.line,
                        reason: format!("{climbs} `super::` climb out of a module {depth} below `linked`"),
                    });
                }
            }
            _ => {}
        }
    }
    found
}

/// `violations` rendered as `file:line: <source line> (reason)`.
fn report(path: &str, text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    violations(path, text)
        .into_iter()
        .map(|v| {
            let source = lines.get(v.line - 1).map_or("", |l| l.trim());
            format!("{path}:{}: {source}  <- {}", v.line, v.reason)
        })
        .collect()
}

/// Every file of the set, read through `source_scan`.
fn moving_set() -> Vec<Source> {
    let set = rust_sources(&[MOVING_DIR]);
    assert!(!set.is_empty(), "{MOVING_DIR} holds no source");
    set
}

#[test]
fn the_moving_set_names_no_toolkit_and_no_other_shell_module() {
    let offenders: Vec<String> = moving_set().iter().flat_map(|s| report(&s.path, &s.text)).collect();
    assert!(
        offenders.is_empty(),
        "the linked windows' code in eitri-desk must stay free of toolkit names and of `crate::` paths outside \
         {ALLOWED_CRATE_PATHS:?}; {} violation(s):\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

#[test]
fn the_reader_reports_a_planted_violation_of_each_kind_and_nothing_else() {
    let sample = [
        "use gtk4::glib::Priority;",                       // 1: gtk4, glib
        "fn run() { neovide_editor::start(); }",           // 2: neovide_editor
        "use crate::agent_panel::X;",                      // 3: other module
        "use crate::{a, b};",                              // 4: group
        "// gtk4 glib crate::version::X in a comment",     // 5: none
        "const S: &str = \"gio webkit6 crate::version\";", // 6: none
        "/* neovide_editor::x,",                           // 7: none
        "   crate::version::Y */",                         // 8: none
        "use crate::linked::Thing;",                       // 9: none
        "use crate::host_traits::LinkedWindows;",          // 10: none
        "use crate::gtk_host::{LinkedWindows, X};",        // 11: group under a path that is not itself allowed
        "use crate::linked_window_glue::G;",               // 12: a name that only starts like an allowed one
        "use super::super::elsewhere;",                    // 13: climbs out of a depth-1 file
        "mod inner { use super::super::fine; }",           // 14: none, the inline module is one deeper
        "use crate::",                                     // 15: path split over lines
        "    end_signals::Z;",                             // 16
        "let gtk4_ok = webkit6_ok::<crate",                // 17: none: not whole words
        "    ::version::Y>();",                            // 18: `crate` at 17, spaces around `::`
        "pub(crate) fn f() {}",                            // 19: none
    ]
    .join("\n");
    let got = violations("desk/src/linked/sample.rs", &sample);
    let want = [
        (1, "toolkit name `gtk4`"),
        (1, "toolkit name `glib`"),
        (2, "toolkit name `neovide_editor`"),
        (3, "`crate::agent_panel::X` is not an allowed path"),
        (4, "`crate::{` group: write the paths out"),
        (11, "`crate::gtk_host::{` is not an allowed path (write each path out)"),
        (12, "`crate::linked_window_glue::G` is not an allowed path"),
        (13, "2 `super::` climb out of a module 1 below `linked`"),
        (15, "`crate::end_signals::Z` is not an allowed path"),
        (17, "`crate::version::Y` is not an allowed path"),
    ];
    let want: Vec<Violation> = want
        .into_iter()
        .map(|(line, reason)| Violation {
            line,
            reason: reason.to_owned(),
        })
        .collect();
    assert_eq!(got, want);
}

#[test]
fn a_file_outside_the_directory_may_not_climb_out_with_super() {
    assert!(violations("desk/src/linked/project_stamp.rs", "mod tests { use super::*; }").is_empty());
    assert_eq!(
        violations("desk/src/linked/project_stamp.rs", "use super::super::X;").len(),
        1
    );
    assert!(violations("desk/src/linked/mod.rs", "mod tests { use super::*; }").is_empty());
    assert_eq!(violations("desk/src/linked/mod.rs", "use super::X;").len(), 1);
    assert!(violations("desk/src/linked/registry/tests/tabs.rs", "use super::super::super::X;").is_empty());
    assert_eq!(
        violations(
            "desk/src/linked/registry/tests/tabs.rs",
            "use super::super::super::super::X;"
        )
        .len(),
        1
    );
}
