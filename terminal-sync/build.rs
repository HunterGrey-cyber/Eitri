//! Mechanical generator for the exhaustive `vte::ansi::Handler` forwarding test.
//!
//! WHY A BUILD SCRIPT (and not a checked-in generated file):
//! the whole point of this gate is that a FORGOTTEN forward in `SyncSpy` compiles cleanly,
//! because every `vte::ansi::Handler` method has a silent no-op default. A checked-in list of
//! methods has exactly the same failure mode one level up: it can silently fall behind the
//! trait. So the list is re-derived from vte's own `ansi.rs` on every build, and this script
//! HARD-FAILS (panics, breaking the build) if:
//!   * the vte source cannot be located,
//!   * the `pub trait Handler` block cannot be found,
//!   * a line inside the trait looks like a method but does not parse,
//!   * a parameter type is not in the value table below.
//! Drift therefore turns into a red build, never into a quietly shrinking test.
//!
//! PROVENANCE: the generated file is derived from the `pub trait Handler` block of
//! `vte`'s `src/ansi.rs`. vte is licensed "Apache-2.0 OR MIT" (vte-0.15.0/Cargo.toml).
//! No vte source text is copied into this repository; only method names and parameter
//! types are re-derived, and the generated file records the exact source path and line range.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::{env, fs};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("generated_handler_methods.rs");

    // Eitri (2026-09-23): only `tests/forwarding.rs` includes the generated file -- the library
    // never does. So failing to LOCATE vte (no lockfile where this crate expected one, an offline or
    // `cargo vendor` build, a registry somewhere else) must not fail the library build, which every
    // `shell` build now goes through. It fails exactly the one test that needs the file, by
    // generating a `compile_error!` in its place. A vte whose `Handler` trait has changed shape still
    // panics below, as it always did: that is real drift, and it only happens on a deliberate bump.
    let located = find_lockfile(&manifest_dir).and_then(|lock| {
        println!("cargo:rerun-if-changed={}", lock.display());
        let version = vte_version_from_lockfile(&lock)?;
        let ansi_rs = locate_vte_ansi_rs(&version)?;
        Ok((version, ansi_rs))
    });
    let generated = match located {
        Ok((vte_version, ansi_rs)) => {
            println!("cargo:rerun-if-changed={}", ansi_rs.display());
            let src = fs::read_to_string(&ansi_rs).unwrap_or_else(|e| panic!("cannot read {}: {e}", ansi_rs.display()));
            let (methods, first_line, last_line) = parse_handler_trait(&src, &ansi_rs);
            emit(&methods, &vte_version, &ansi_rs, first_line, last_line)
        }
        Err(why) => {
            println!("cargo:warning=terminal-sync: the Handler forwarding test cannot be generated: {why}");
            // A path that never exists makes cargo rerun this script on every build, so the
            // `compile_error!` below lasts only until vte's source is there (`cargo fetch`).
            let never = PathBuf::from(env::var("OUT_DIR").unwrap()).join("vte-not-located-yet");
            println!("cargo:rerun-if-changed={}", never.display());
            format!("compile_error!({:?});\n", format!("terminal-sync/build.rs: {why}"))
        }
    };
    fs::write(&out, generated).unwrap();
}

// ---------------------------------------------------------------------------------------------
// Locating vte
// ---------------------------------------------------------------------------------------------

/// The lockfile cargo resolved this build with: this crate's own when it is built standalone, the
/// workspace root's when it is a workspace member (which is what it is inside Eitri, where it has
/// none of its own). The first `Cargo.lock` walking up from the manifest directory is that file.
fn find_lockfile(manifest_dir: &Path) -> Result<PathBuf, String> {
    manifest_dir
        .ancestors()
        .map(|dir| dir.join("Cargo.lock"))
        .find(|lock| lock.is_file())
        .ok_or_else(|| format!("no Cargo.lock in {} or any directory above it", manifest_dir.display()))
}

/// Cargo writes `Cargo.lock` during resolution, which happens strictly before build scripts run,
/// so reading it here is safe even on a cold checkout.
///
/// The vte that matters is the one `alacritty_terminal` resolved -- the one `Handler` comes from --
/// not merely the first `vte` in the file: a workspace lockfile can hold several versions once any
/// other dependency pulls in its own (Eitri, 2026-09-23). A lockfile names a dependency with its
/// version only when that name is ambiguous, so a bare `"vte"` means the file's only vte.
fn vte_version_from_lockfile(lock: &Path) -> Result<String, String> {
    let text = fs::read_to_string(lock).map_err(|e| format!("cannot read {} ({e})", lock.display()))?;
    let mut packages: Vec<(String, String, Vec<String>)> = Vec::new();
    let mut in_dependencies = false;
    for line in text.lines().map(str::trim) {
        let quoted = |line: &str| line.trim_end_matches(',').trim_matches('"').to_string();
        if line == "[[package]]" {
            packages.push(Default::default());
            in_dependencies = false;
        } else if let (Some(package), Some(name)) = (packages.last_mut(), line.strip_prefix("name = ")) {
            package.0 = quoted(name);
        } else if let (Some(package), Some(version)) = (packages.last_mut(), line.strip_prefix("version = ")) {
            package.1 = quoted(version);
        } else if line == "dependencies = [" {
            in_dependencies = true;
        } else if line == "]" {
            in_dependencies = false;
        } else if let (true, Some(package)) = (in_dependencies, packages.last_mut()) {
            package.2.push(quoted(line));
        }
    }
    let versions_of = |name: &str| -> Vec<&str> {
        packages
            .iter()
            .filter(|(n, _, _)| n == name)
            .map(|(_, v, _)| v.as_str())
            .collect()
    };
    let vtes = versions_of("vte");
    let alacritty: Vec<&(String, String, Vec<String>)> =
        packages.iter().filter(|(n, _, _)| n == "alacritty_terminal").collect();
    let named = match alacritty.as_slice() {
        [(_, _, dependencies)] => dependencies.iter().find(|d| *d == "vte" || d.starts_with("vte ")),
        [] => None,
        _ => return Err(format!("more than one alacritty_terminal in {}", lock.display())),
    };
    match (named.and_then(|d| d.split(' ').nth(1)), vtes.as_slice()) {
        (Some(version), _) => Ok(version.to_string()),
        (None, [only]) => Ok(only.to_string()),
        (None, []) => Err(format!("no `vte` package found in {}", lock.display())),
        (None, _) => Err(format!(
            "{} vte versions in {} and no alacritty_terminal to say which one it uses",
            vtes.len(),
            lock.display()
        )),
    }
}

fn locate_vte_ansi_rs(version: &str) -> Result<PathBuf, String> {
    let cargo_home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
        .ok_or("neither CARGO_HOME nor HOME is set")?;
    let registry = cargo_home.join("registry").join("src");
    if let Ok(entries) = fs::read_dir(&registry) {
        for e in entries.flatten() {
            let candidate = e.path().join(format!("vte-{version}")).join("src").join("ansi.rs");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(format!(
        "could not find vte-{version}/src/ansi.rs under {} (`cargo fetch` first, or point CARGO_HOME \
         at the registry that holds it)",
        registry.display()
    ))
}

// ---------------------------------------------------------------------------------------------
// Parsing the trait
// ---------------------------------------------------------------------------------------------

struct Method {
    name: String,
    /// (parameter type as written in vte, 0-based index among the non-`self` parameters)
    params: Vec<String>,
}

/// Returns the methods plus the 1-based line range of the `pub trait Handler` block.
fn parse_handler_trait(src: &str, path: &Path) -> (Vec<Method>, usize, usize) {
    let lines: Vec<&str> = src.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.trim_start().starts_with("pub trait Handler {"))
        .unwrap_or_else(|| {
            panic!(
                "terminal-sync/build.rs: `pub trait Handler {{` not found in {}",
                path.display()
            )
        });

    // The trait body ends at the first line that is exactly `}` at column 0.
    let end = (start + 1..lines.len())
        .find(|&i| lines[i] == "}")
        .unwrap_or_else(|| panic!("terminal-sync/build.rs: unterminated trait Handler"));

    let mut methods = Vec::new();
    for (i, raw) in lines[start + 1..end].iter().enumerate() {
        let line = raw.trim();
        if !line.starts_with("fn ") {
            // Anything that is not a method line must be a doc comment, attribute or blank.
            // If a *new* shape shows up (e.g. a multi-line signature, an associated type, a
            // provided method with a real body) we must not silently skip it.
            if line.is_empty()
                || line.starts_with("//")
                || line.starts_with("/*")
                || line.starts_with('*')
                || line.starts_with("#[")
                || line.starts_with(']')
            {
                continue;
            }
            panic!(
                "terminal-sync/build.rs: unrecognised line {} inside `trait Handler`: {raw:?}\n\
                 The generator only understands single-line `fn name(&mut self, ..) {{}}` \
                 declarations. Teach it the new shape rather than skipping the line.",
                start + 2 + i
            );
        }
        methods.push(parse_method(line, start + 2 + i));
    }
    assert!(
        !methods.is_empty(),
        "terminal-sync/build.rs: parsed zero Handler methods"
    );
    (methods, start + 1, end + 1)
}

fn parse_method(line: &str, lineno: usize) -> Method {
    let rest = line.strip_prefix("fn ").unwrap();
    let open = rest
        .find('(')
        .unwrap_or_else(|| panic!("line {lineno}: no `(` in {line:?}"));
    let name = rest[..open].trim().to_string();
    let close = rest
        .rfind(')')
        .unwrap_or_else(|| panic!("line {lineno}: no `)` in {line:?}"));
    assert!(close > open, "line {lineno}: malformed signature {line:?}");
    let tail = rest[close + 1..].trim();
    assert_eq!(
        tail, "{}",
        "line {lineno}: {line:?} is not a no-op provided method; the generator assumes \
         `fn name(..) {{}}` and must be taught anything else"
    );
    let args = split_top_level(&rest[open + 1..close]);
    let mut params = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        let arg = arg.trim();
        if arg.is_empty() {
            continue;
        }
        if i == 0 {
            assert_eq!(
                arg, "&mut self",
                "line {lineno}: unexpected receiver {arg:?} in {line:?}"
            );
            continue;
        }
        // `name: Type`, `_: Type`, `_name: Type`. Split on the FIRST top-level `:`.
        let colon = first_top_level_colon(arg).unwrap_or_else(|| panic!("line {lineno}: parameter {arg:?} has no `:`"));
        params.push(normalise_ws(&arg[colon + 1..]));
    }
    Method { name, params }
}

fn split_top_level(s: &str) -> Vec<String> {
    let (mut depth, mut out, mut cur) = (0i32, Vec::new(), String::new());
    for c in s.chars() {
        match c {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur);
    out
}

fn first_top_level_colon(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'<' | b'(' | b'[' => depth += 1,
            b'>' | b')' | b']' => depth -= 1,
            b':' if depth == 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn normalise_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------------------------
// Code generation
// ---------------------------------------------------------------------------------------------

/// For each parameter type: how to build a distinctive value, and how to render it into the
/// call log. The log is what makes the test detect a forward that goes to the WRONG method or
/// passes the WRONG arguments, so it must include every argument.
///
/// `{i}` in a value template is replaced by the parameter's index, so a method with two
/// same-typed parameters gets two DIFFERENT values and an argument swap is detected.
/// `{b}` in a log template is replaced by the parameter binding.
fn type_table() -> HashMap<&'static str, (&'static str, &'static str)> {
    // (value template, log template)
    let dbg = "{:?}";
    HashMap::from([
        ("usize", ("11usize + {i}", dbg)),
        ("i32", ("23i32 + {i}", dbg)),
        ("u16", ("37u16 + {i}", dbg)),
        ("u8", ("53u8 + {i}", dbg)),
        ("char", ("'\\u{2713}'", dbg)),
        ("String", ("String::from(\"arg{i}\")", dbg)),
        ("&str", ("\"arg{i}\"", dbg)),
        ("&[u8]", ("&[7u8, 9u8, {i}]", dbg)),
        ("Option<String>", ("Some(String::from(\"arg{i}\"))", dbg)),
        ("Option<char>", ("Some('\\u{00e9}')", dbg)),
        ("Option<usize>", ("Some(71usize + {i})", dbg)),
        ("Option<CursorStyle>", ("Some(CursorStyle::default())", dbg)),
        (
            "Option<Hyperlink>",
            (
                "Some(Hyperlink { id: Some(String::from(\"id{i}\")), \
                 uri: String::from(\"https://verdandi.invalid/{i}\") })",
                dbg,
            ),
        ),
        ("CursorShape", ("CursorShape::HollowBlock", dbg)),
        ("Attr", ("Attr::Undercurl", dbg)),
        ("Mode", ("Mode::Unknown(101u16 + {i})", dbg)),
        // Deliberately NOT SyncUpdate(2026): the generated test proves plain forwarding.
        // The SyncUpdate interception is covered separately in tests/barrier.rs.
        ("PrivateMode", ("PrivateMode::Unknown(1003u16 + {i})", dbg)),
        ("ClearMode", ("ClearMode::Saved", dbg)),
        ("LineClearMode", ("LineClearMode::Left", dbg)),
        ("TabulationClearMode", ("TabulationClearMode::All", dbg)),
        ("CharsetIndex", ("CharsetIndex::G3", dbg)),
        (
            "StandardCharset",
            ("StandardCharset::SpecialCharacterAndLineDrawing", dbg),
        ),
        ("Rgb", ("Rgb { r: 11, g: 22u8 + {i}, b: 33 }", dbg)),
        ("CursorIcon", ("CursorIcon::Crosshair", dbg)),
        ("KeyboardModes", ("KeyboardModes::REPORT_ALL_KEYS_AS_ESC", dbg)),
        // KeyboardModesApplyBehavior does NOT derive Debug in vte 0.15.0 (ansi.rs:808), so it is
        // logged through its #[repr(u8)] discriminant instead.
        (
            "KeyboardModesApplyBehavior",
            ("KeyboardModesApplyBehavior::Difference", "{}"),
        ),
        ("ModifyOtherKeys", ("ModifyOtherKeys::EnableAll", dbg)),
        ("ScpCharPath", ("ScpCharPath::RTL", dbg)),
        ("ScpUpdateMode", ("ScpUpdateMode::PresentationToData", dbg)),
    ])
}

fn emit(methods: &[Method], version: &str, path: &Path, first: usize, last: usize) -> String {
    let table = type_table();
    let mut s = String::new();
    s.push_str(&format!(
        "// @generated by terminal-sync/build.rs -- DO NOT EDIT.\n\
         //\n\
         // Derived from the `pub trait Handler` block of vte {version}:\n\
         //   {}\n\
         //   lines {first}..={last} ({n} methods)\n\
         // vte is licensed \"Apache-2.0 OR MIT\"; no vte source text is reproduced here, only\n\
         // method names and parameter types re-derived from that declaration.\n\n",
        path.display(),
        n = methods.len()
    ));

    s.push_str(&format!("pub const VTE_VERSION: &str = \"{version}\";\n"));
    s.push_str(&format!(
        "pub const VTE_HANDLER_SOURCE: &str = \"{}:{first}..={last}\";\n",
        path.display()
    ));
    s.push_str("pub const HANDLER_METHODS: &[&str] = &[\n");
    for m in methods {
        s.push_str(&format!("    \"{}\",\n", m.name));
    }
    s.push_str("];\n\n");

    s.push_str(
        "#[allow(unused_imports)]\n\
         use alacritty_terminal::vte::ansi::{\n\
         \x20   cursor_icon::CursorIcon, Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle,\n\
         \x20   Handler, Hyperlink, KeyboardModes, KeyboardModesApplyBehavior, LineClearMode, Mode,\n\
         \x20   ModifyOtherKeys, PrivateMode, Rgb, ScpCharPath, ScpUpdateMode, StandardCharset,\n\
         \x20   TabulationClearMode,\n};\n\n",
    );

    // ---- the recording inner Handler -------------------------------------------------------
    s.push_str(
        "/// A `Handler` that records every call it receives, name and arguments.\n\
         ///\n\
         /// This is the *inner* handler in the forwarding test. A `SyncSpy` method that was\n\
         /// never written at all falls through to the trait's silent no-op default, so this\n\
         /// recorder sees nothing and the assertion fires.\n\
         #[derive(Default, Debug)]\n\
         pub struct RecordingHandler {\n    pub log: Vec<String>,\n}\n\n\
         #[rustfmt::skip]\n\
         impl Handler for RecordingHandler {\n",
    );
    for m in methods {
        let (sig_args, log_expr) = render_recorder(m, &table);
        s.push_str(&format!(
            "    fn {}(&mut self{}) {{ self.log.push({}); }}\n",
            m.name, sig_args, log_expr
        ));
    }
    s.push_str("}\n\n");

    // ---- the exhaustive caller -------------------------------------------------------------
    s.push_str(
        "/// Calls Handler method `idx` (index into [`HANDLER_METHODS`]) on `h` with a fixed,\n\
         /// per-parameter-distinct set of arguments, and returns the log line the inner\n\
         /// [`RecordingHandler`] must have produced.\n\
         ///\n\
         /// Exhaustive BY CONSTRUCTION: it is generated from the trait declaration, not from\n\
         /// whatever escape sequences a byte-stream test happens to hit.\n\
         #[rustfmt::skip]\n\
         pub fn call_handler_method<H: Handler>(h: &mut H, idx: usize) -> String {\n\
         \x20   match idx {\n",
    );
    for (i, m) in methods.iter().enumerate() {
        let mut binds = String::new();
        let mut logs = Vec::new();
        let mut call_args = Vec::new();
        for (j, ty) in m.params.iter().enumerate() {
            let (val, logt) = table.get(ty.as_str()).unwrap_or_else(|| {
                panic!(
                    "terminal-sync/build.rs: no value for parameter type {ty:?} \
                     (method `{}`). Add it to `type_table()`.",
                    m.name
                )
            });
            let val = val.replace("{i}", &j.to_string());
            binds.push_str(&format!("let a{j} = {val}; "));
            let rendered = logt.replace("{b}", &format!("a{j}"));
            logs.push(format!(
                "format!(\"{rendered}\", a{j}{})",
                if *logt == "{}" { " as u8" } else { "" }
            ));
            call_args.push(format!("a{j}"));
        }
        let log_expr = if logs.is_empty() {
            format!("String::from(\"{}()\")", m.name)
        } else {
            format!("format!(\"{}({{}})\", [{}].join(\", \"))", m.name, logs.join(", "))
        };
        s.push_str(&format!(
            "        {i} => {{ {binds}let expect = {log_expr}; h.{}({}); expect }},\n",
            m.name,
            call_args.join(", ")
        ));
    }
    s.push_str(&format!(
        "        _ => panic!(\"handler method index {{idx}} out of range (0..{})\"),\n    }}\n}}\n",
        methods.len()
    ));
    s
}

fn render_recorder(m: &Method, table: &HashMap<&str, (&str, &str)>) -> (String, String) {
    let mut sig = String::new();
    let mut logs = Vec::new();
    for (j, ty) in m.params.iter().enumerate() {
        let (_, logt) = table
            .get(ty.as_str())
            .unwrap_or_else(|| panic!("no value for parameter type {ty:?} (method `{}`)", m.name));
        sig.push_str(&format!(", a{j}: {ty}"));
        let rendered = logt.replace("{b}", &format!("a{j}"));
        logs.push(format!(
            "format!(\"{rendered}\", a{j}{})",
            if *logt == "{}" { " as u8" } else { "" }
        ));
    }
    let log_expr = if logs.is_empty() {
        format!("String::from(\"{}()\")", m.name)
    } else {
        format!("format!(\"{}({{}})\", [{}].join(\", \"))", m.name, logs.join(", "))
    };
    (sig, log_expr)
}
