//! Writing the generated file, once, never over an existing one.

use std::os::fd::AsFd;
use std::path::Path;

use agent::private_fs::{create_new_file_at, open_user_config_dir, CreateRefusal};

use super::display::display_text;
use super::text::{fill, text, TextId};
use super::{Lang, WriteOutcome};

/// The file a window reads its configuration from. This module is the only code that creates it.
const INIT_FILE: &str = "init.lua";
/// The user's own file, so the usual mode of one, not Eitri's private 0600.
const INIT_MODE: u32 = 0o644;

/// Creates `init.lua` in `config_dir` with `text`, making the directory when it is missing. Blocks on
/// the file system, so it runs on a worker thread.
pub fn write_init(config_dir: &Path, text: &str, lang: Lang) -> WriteOutcome {
    let outcome = write_init_only(config_dir, text, lang);
    if matches!(outcome, WriteOutcome::Written(_)) {
        match crate::luals::data_dir(&|key| std::env::var_os(key)) {
            Ok(data_dir) => install_after_written(&outcome, config_dir, &data_dir),
            Err(error) => eprintln!("[lua-ls] {error}"),
        }
    }
    outcome
}

#[cfg(test)]
pub(crate) fn write_init_with_data(config_dir: &Path, data_dir: &Path, text: &str, lang: Lang) -> WriteOutcome {
    let outcome = write_init_only(config_dir, text, lang);
    install_after_written(&outcome, config_dir, data_dir);
    outcome
}

pub(crate) fn install_after_written(outcome: &WriteOutcome, config_dir: &Path, data_dir: &Path) {
    if matches!(outcome, WriteOutcome::Written(_)) {
        if let Err(error) = crate::luals::write_luarc(config_dir, data_dir) {
            eprintln!("[lua-ls] could not prepare editor completion: {error}");
        }
    }
}

fn write_init_only(config_dir: &Path, text: &str, lang: Lang) -> WriteOutcome {
    let dir = match open_user_config_dir(config_dir) {
        Ok(dir) => dir,
        Err(refusal) => {
            eprintln!(
                "[onboarding] {}: {refusal}",
                display_text(&config_dir.display().to_string())
            );
            return WriteOutcome::Failed(not_private(config_dir, lang));
        }
    };
    let result = create_new_file_at(dir.as_fd(), INIT_FILE, text.as_bytes(), INIT_MODE);
    outcome_of(result, &config_dir.join(INIT_FILE), lang)
}

/// What the screen says of `result`, the attempt to create the file at `path`.
pub fn outcome_of(result: Result<(), CreateRefusal>, path: &Path, lang: Lang) -> WriteOutcome {
    match result {
        Ok(()) => WriteOutcome::Written(path.to_path_buf()),
        Err(CreateRefusal::Exists) => WriteOutcome::Failed(text(lang, TextId::FailAppeared).to_owned()),
        Err(CreateRefusal::CannotCreateSafely) => WriteOutcome::NotWritten(text(lang, TextId::NotwrittenFs).to_owned()),
        Err(CreateRefusal::Io(e)) => WriteOutcome::Failed(could_not_write(path, &e, lang)),
    }
}

fn not_private(dir: &Path, lang: Lang) -> String {
    let dir = display_text(&dir.display().to_string());
    fill(text(lang, TextId::FailDir), &[("path", &dir)])
}

fn could_not_write(path: &Path, why: &std::io::Error, lang: Lang) -> String {
    let (path, why) = (
        display_text(&path.display().to_string()),
        display_text(&why.to_string()),
    );
    fill(text(lang, TextId::FailIo), &[("path", &path), ("why", &why)])
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::*;
    use crate::source_scan::{code_only, without, word_ends, Source};

    fn write_init(config_dir: &Path, text: &str, lang: Lang) -> WriteOutcome {
        super::write_init_with_data(config_dir, &config_dir.parent().unwrap().join("data"), text, lang)
    }

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eitri-write-{label}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn written_returns_the_path() {
        let base = scratch("written");
        let dir = base.join("eitri");
        let outcome = write_init(&dir, "-- lines\n", Lang::En);
        assert_eq!(outcome, WriteOutcome::Written(dir.join("init.lua")));
        assert_eq!(std::fs::read_to_string(dir.join("init.lua")).unwrap(), "-- lines\n");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_second_write_is_the_appeared_text_and_keeps_the_first() {
        let base = scratch("second");
        let dir = base.join("eitri");
        write_init(&dir, "first", Lang::En);
        assert_eq!(
            write_init(&dir, "second", Lang::En),
            WriteOutcome::Failed(text(Lang::En, TextId::FailAppeared).to_owned())
        );
        assert_eq!(std::fs::read_to_string(dir.join("init.lua")).unwrap(), "first");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_refused_directory_is_the_failed_text() {
        let base = scratch("refused");
        let dir = base.join("eitri");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        let outcome = write_init(&dir, "-- lines\n", Lang::En);
        let wanted = format!(
            "{} is not a directory only you can write; nothing was written",
            display_text(&dir.display().to_string())
        );
        assert_eq!(outcome, WriteOutcome::Failed(wanted));
        assert!(!dir.join("init.lua").exists());
        let WriteOutcome::Failed(zh) = write_init(&dir, "-- lines\n", Lang::ZhCn) else {
            panic!("refused in either language");
        };
        assert!(zh.ends_with("不是只有你能写的目录；未写入任何内容"), "{zh}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn exists_is_the_appeared_text() {
        let outcome = outcome_of(Err(CreateRefusal::Exists), Path::new("/x/init.lua"), Lang::En);
        assert_eq!(
            outcome,
            WriteOutcome::Failed("an init.lua appeared meanwhile (another window?); nothing was written".to_owned())
        );
    }

    #[test]
    fn cannot_create_safely_is_not_written() {
        let outcome = outcome_of(
            Err(CreateRefusal::CannotCreateSafely),
            Path::new("/x/init.lua"),
            Lang::ZhCn,
        );
        assert_eq!(
            outcome,
            WriteOutcome::NotWritten("这个文件系统无法安全地创建 init.lua".to_owned())
        );
    }

    #[test]
    fn an_io_error_is_the_failed_text() {
        let e = std::io::Error::from_raw_os_error(libc::ENOSPC);
        let why = e.to_string();
        let outcome = outcome_of(Err(CreateRefusal::Io(e)), Path::new("/x/init.lua"), Lang::En);
        assert_eq!(
            outcome,
            WriteOutcome::Failed(format!("could not write /x/init.lua: {why}; nothing was written"))
        );
    }

    #[test]
    fn a_path_shows_what_would_not_show_as_itself() {
        let dir = Path::new("/home/\u{202e}x\u{1b}[2J/eitri");
        let said = not_private(dir, Lang::En);
        assert!(said.starts_with("/home/\\u{202e}x\\u{1b}[2J/eitri is not"), "{said}");
    }

    #[test]
    fn a_path_passes_the_one_cut_the_screen_has() {
        // A line separator, a no-break space and an ideographic space are hidden too, and the cut is display_text's.
        let dir = Path::new("/home/a\u{2028}b\u{a0}c\u{3000}d");
        let said = not_private(dir, Lang::En);
        assert!(said.starts_with("/home/a\\u{2028}b\\u{a0}c\\u{3000}d is not"), "{said}");
        let long = PathBuf::from("x".repeat(200));
        let e = std::io::Error::from_raw_os_error(libc::ENOSPC);
        let said = could_not_write(&long, &e, Lang::ZhCn);
        assert!(
            said.contains(&"x".repeat(crate::onboarding::display::MAX_CHARS)),
            "{said}"
        );
        assert!(
            !said.contains(&"x".repeat(crate::onboarding::display::MAX_CHARS + 1)),
            "{said}"
        );
    }

    /// The name in the code, for what `code_only` would blank. In a comment or inside another literal it
    /// is blanked with what holds it.
    const LITERAL: &str = "\"init.lua\"";
    const MARK: &str = "INIT_LUA__";
    /// The end of a longer literal that names the file: a path, or a `format!` string that ends in it.
    const LITERAL_END: &str = "/init.lua\"";

    /// `text` with [`MARK`] for the literal, and after a longer literal that ends in the name, which
    /// stops before it and keeps its own closing quote and any hashes of a raw string.
    fn marked(text: &str) -> String {
        let text = text.replace(LITERAL, MARK);
        let mut out = String::with_capacity(text.len());
        let mut copied = 0;
        for (at, end) in text.match_indices(LITERAL_END) {
            let quote = at + end.len() - 1;
            let hashes = text[quote + 1..].bytes().take_while(|b| *b == b'#').count();
            let after = quote + 1 + hashes;
            out.push_str(&text[copied..at]);
            out.push_str(&text[quote..after]);
            out.push_str(MARK);
            copied = after;
        }
        out.push_str(&text[copied..]);
        out
    }

    /// Calls that create, change or replace what they name. Opening to read is not one of them.
    const WRITING: &[&str] = &[
        "OpenOptions",
        "append",
        "copy",
        "create",
        "create_new",
        "hard_link",
        "open_private",
        "rename",
        "replace_private",
        "symlink",
        "truncate",
        "write",
        "write_private",
    ];

    fn writes(code: &str) -> bool {
        WRITING.iter().any(|call| {
            word_ends(code, call).into_iter().any(|end| {
                let rest = code[end..].trim_start();
                rest.starts_with('(') || rest.starts_with("::")
            })
        })
    }

    /// The statement around `at`: from the last `;`, `{` or `}` before it to the next one after it.
    fn statement_around(code: &str, at: usize) -> (usize, usize) {
        const ENDS: [char; 3] = [';', '{', '}'];
        let start = code[..at].rfind(ENDS).map_or(0, |i| i + 1);
        let end = code[at..].find(ENDS).map_or(code.len(), |i| at + i);
        (start, end)
    }

    /// The name a `let`, `const` or `static` statement binds.
    fn bound_name(statement: &str) -> Option<&str> {
        let mut words = statement.split_whitespace();
        words.find(|w| matches!(*w, "let" | "const" | "static"))?;
        let name = words.find(|w| *w != "mut")?;
        let end = name
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(name.len());
        (end > 0).then(|| &name[..end])
    }

    /// The rest of the block `code[from..]` starts in.
    fn rest_of_block(code: &str, from: usize) -> &str {
        let mut depth = 0usize;
        for (at, c) in code[from..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' if depth == 0 => return &code[from..from + at],
                '}' => depth -= 1,
                _ => {}
            }
        }
        &code[from..]
    }

    /// `code` without the items behind a `cfg(test)` attribute.
    fn production_code(code: &str) -> String {
        let mut bodies = Vec::new();
        for attribute in ["#[cfg(test)]", "#[cfg(all(test"] {
            for (at, _) in code.match_indices(attribute) {
                let open = code[at..].find(['{', ';']).map(|i| at + i);
                if let Some(open) = open.filter(|open| code[*open..].starts_with('{')) {
                    bodies.push(crate::source_scan::braced(code, open));
                }
            }
        }
        without(code, &bodies)
    }

    /// Every statement of `text` that joins a literal naming the file to a call that writes: in one
    /// statement, however it is wrapped, or through a name bound to it and used in the same block.
    fn writes_to_init_lua(text: &str) -> Vec<String> {
        let code = production_code(&code_only(&marked(text)));
        let mut found = Vec::new();
        for (at, _) in code.match_indices(MARK) {
            let (start, end) = statement_around(&code, at);
            let statement = &code[start..end];
            if writes(statement) {
                found.push(statement.trim().to_owned());
            }
            let Some(name) = bound_name(statement) else { continue };
            let rest = rest_of_block(&code, end);
            for (use_at, _) in word_ends(rest, name).into_iter().map(|e| (e - name.len(), ())) {
                let (start, end) = statement_around(rest, use_at);
                if writes(&rest[start..end]) {
                    found.push(rest[start..end].trim().to_owned());
                }
            }
        }
        found
    }

    #[test]
    fn the_scan_sees_a_write_however_it_is_spelled() {
        for writer in [
            r#"fn f() { std::fs::write(dir.join("init.lua"), text).unwrap(); }"#,
            "fn f() {\n    std::fs::write(\n        dir\n            .join(\"init.lua\"),\n        text,\n    );\n}",
            r#"fn f() { let init = dir.join("init.lua"); std::fs::write(&init, text); }"#,
            "fn f() { let mut init = dir.join(\"init.lua\");\n let x = 1;\n write_private(\n &init, text); }",
            r#"fn f() { File::create(dir.join("init.lua")); }"#,
            r#"fn f() { OpenOptions::new().append(true).open(dir.join("init.lua")); }"#,
            r#"fn f() { let tmp = t(); std::fs::rename(tmp, dir.join("init.lua")); }"#,
            r#"fn f() { agent::private_fs::replace_private(&tmp, &dir.join("init.lua"), b"", Flush::Nothing); }"#,
            r#"const INIT: &str = "init.lua"; fn f() { std::fs::write(dir.join(INIT), text); }"#,
            r#"fn f() { std::fs::write(home.join(".config/eitri/init.lua"), text); }"#,
            r#"fn f() { std::fs::write(format!("{dir}/init.lua"), text); }"#,
            r#"fn f() { let init = home.join(".config/eitri/init.lua"); std::fs::write(&init, text); }"#,
            r##"fn f() { std::fs::write(home.join(r#".config/eitri/init.lua"#), text); }"##,
        ] {
            assert!(!writes_to_init_lua(writer).is_empty(), "{writer}");
        }
    }

    #[test]
    fn the_scan_leaves_reads_comments_and_tests_alone() {
        for reader in [
            r#"fn f() { let init = dir.join("init.lua"); kernel.run(&init); }"#,
            r#"fn f() { std::fs::read_to_string(dir.join("init.lua")); }"#,
            r#"fn f() { std::fs::read_to_string(home.join(".config/eitri/init.lua")); }"#,
            r#"fn f() { std::fs::write(other, "say \"a/init.lua\" here"); }"#,
            r#"fn f() { std::fs::write(other, text); let init = dir.join("init.lua"); }"#,
            r#"fn f() { std::fs::write(a, b); } // std::fs::write(dir.join("init.lua"), b)"#,
            r#"fn f() { say("write(dir.join(\"init.lua\"))"); }"#,
            "#[cfg(test)]\nmod tests { fn f() { std::fs::write(dir.join(\"init.lua\"), text); } }",
            r#"fn f() { { let init = dir.join("init.lua"); } let init = 1; std::fs::write(init, text); }"#,
        ] {
            assert_eq!(writes_to_init_lua(reader), Vec::<String>::new(), "{reader}");
        }
    }

    /// The writer in `agent::private_fs` takes the name from its caller and `create_new_file_at` is no
    /// call of the list above, so nothing here needs excepting.
    #[test]
    fn only_the_writer_creates_init_lua() {
        let sources: Vec<Source> = crate::source_scan::product_sources()
            .into_iter()
            .filter(|s| !s.path.contains("/tests/") && !s.path.ends_with("tests.rs"))
            .collect();
        let marks = |s: &Source| code_only(&marked(&s.text)).matches(MARK).count();
        assert!(
            ["core/src/lua/agent.rs", "shell/src/main.rs", "mac/src/assembly.rs"]
                .iter()
                .all(|path| sources.iter().any(|s| s.path == *path && marks(s) > 0)),
            "the scan no longer reads the code that names the file"
        );
        let found: Vec<(String, String)> = sources
            .iter()
            .flat_map(|s| writes_to_init_lua(&s.text).into_iter().map(|w| (s.path.clone(), w)))
            .collect();
        assert_eq!(found, Vec::<(String, String)>::new());
    }
}
