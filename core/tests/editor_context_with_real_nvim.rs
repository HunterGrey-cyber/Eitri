//! The Lua half of wire 1, driven by a real `nvim`.
//!
//! `#[ignore]`d: it needs `nvim` on PATH. It spends no tokens, touches no network, opens no window
//! and needs no display -- `--headless` is the whole harness.
//!
//! This exists because the contract between `nvim_editor_context.lua` and `feed::parse` is two
//! languages with nothing checking that they agree. The unit tests pin the snippet's field NAMES by
//! substring, which catches a rename and nothing else: they would pass against a snippet that read
//! the wrong nvim API, computed the wrong range, or sent a selection when there is none. Only
//! running nvim catches those, and the no-selection arm below is a case measured to be actively
//! dangerous -- `getregion` answers a plausible one-character region rather than erroring.
//!
//! Run: `cargo test -p eitri-core --test editor_context_with_real_nvim -- --ignored`
// The pushes go through the IDE server, which is Linux-only.
#![cfg(target_os = "linux")]

use eitri_core::editor_context::compose::{compose_turn_text, TRUNCATION_MARKER};
use eitri_core::editor_context::feed::{EditorContextFeed, EditorContextReader};
use eitri_core::editor_context::EditorContext;

fn selection_facts(body: &str, commands: &[&str]) -> eitri_core::editor_context::feed::PushFacts {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = std::env::temp_dir().join(format!("nv-w1-cols-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("t.txt");
    std::fs::write(&file, body).unwrap();
    let mut feed = EditorContextFeed::new().expect("the feed must bind");
    let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
    let update = drive_nvim_update(&feed, &mut reader, &file, &["--clean"], commands).expect("an update must arrive");
    assert!(update.context.selection.is_some());
    feed.cleanup();
    std::fs::remove_dir_all(dir).unwrap();
    update.facts
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_one_line_v_selection_says_its_columns() {
    let facts = selection_facts("bravo\n", &["normal! gglvll"]);
    assert_eq!((facts.start_col, facts.end_col), (Some(1), Some(4)));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_v_selection_of_a_non_bmp_character_counts_two_units() {
    let facts = selection_facts("a🙂b\n", &["normal! gglv"]);
    assert_eq!((facts.start_col, facts.end_col), (Some(1), Some(3)));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_v_selection_ending_on_an_empty_line_says_column_zero() {
    let facts = selection_facts("abc\n\n", &["normal! gglvj0"]);
    assert_eq!((facts.start_col, facts.end_col), (Some(1), Some(0)));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_linewise_selection_says_column_zero() {
    let facts = selection_facts("abc\ndef\nghi\n", &["normal! gglVjj"]);
    assert_eq!((facts.start_col, facts.end_col), (Some(0), Some(0)));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_exclusive_selection_ends_before_the_cursor() {
    let facts = selection_facts("a🙂bc\n", &["set selection=exclusive", "normal! gglvl"]);
    assert_eq!((facts.start_col, facts.end_col), (Some(1), Some(3)));
}

/// A project with `a.py` a link to a file outside it, and what the name is replaced with later.
#[cfg(target_os = "linux")]
struct LinkedOut {
    dir: std::path::PathBuf,
    name: std::path::PathBuf,
    secret: std::path::PathBuf,
}

#[cfg(target_os = "linux")]
impl LinkedOut {
    fn new(outside_name: &std::ffi::OsStr) -> LinkedOut {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("nv-w1-rec-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("project")).unwrap();
        std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
        let dir = dir.canonicalize().unwrap();
        std::fs::set_permissions(dir.join("project"), std::fs::Permissions::from_mode(0o700)).unwrap();
        let secret = dir.join("elsewhere").join(outside_name);
        std::fs::write(&secret, "private line\n").unwrap();
        let name = dir.join("project/a.py");
        std::os::unix::fs::symlink(&secret, &name).unwrap();
        LinkedOut { dir, name, secret }
    }

    /// What replaces the link with an ordinary file while nvim holds the buffer.
    fn replace_the_link(&self) -> Vec<String> {
        let name = self.name.display();
        vec![
            format!("call delete('{name}')"),
            format!("call writefile(['an ordinary, longer line of its own'], '{name}')"),
        ]
    }

    fn push(&self, update: eitri_core::editor_context::feed::FeedUpdate) -> serde_json::Value {
        use eitri_core::ide_server::files::run_job;
        use eitri_core::ide_server::{ContextUpdate, Done, Job, ServerConfig};
        let config = ServerConfig {
            root: self.dir.join("project"),
            home: None,
        };
        let job = Job::Context(ContextUpdate {
            context: Some(update.context),
            facts: update.facts,
        });
        match run_job(&config, job) {
            Done::Selection(params) => params,
            other => panic!("not a selection: {other:?}"),
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for LinkedOut {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The update nvim sent last, after `commands`, on a buffer opened at `case.name` through its link.
#[cfg(target_os = "linux")]
fn drive_linked(case: &LinkedOut, commands: &[String]) -> eitri_core::editor_context::feed::FeedUpdate {
    agent::state_dirs::redirect_state_to_a_test_root();
    let mut feed = EditorContextFeed::new().expect("the feed must bind");
    let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
    let commands: Vec<&str> = commands.iter().map(String::as_str).collect();
    let update = drive_nvim_update(&feed, &mut reader, &case.name, &["--clean"], &commands);
    feed.cleanup();
    update.expect("an update must arrive")
}

#[test]
#[cfg(target_os = "linux")]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_buffer_read_through_a_link_says_where_on_the_line_and_its_selection_is_not_pushed() {
    let case = LinkedOut::new(std::ffi::OsStr::new("secret.py"));
    let mut commands = case.replace_the_link();
    commands.push("normal! ggVj".to_owned());
    let update = drive_linked(&case, &commands);
    assert_eq!(update.context.selection.as_ref().unwrap().text, "private line");
    assert_eq!(update.facts.read_from.as_deref(), Some(case.secret.to_str().unwrap()));
    assert_eq!(update.context.file, case.name.display().to_string());
    let params = case.push(update);
    assert_eq!(params["text"], serde_json::json!(""), "{params}");
    assert!(params.get("filePath").is_none(), "{params}");
}

/// A reload moves the record with nothing else to report it: `:checktime` runs no cursor or buffer event.
#[test]
#[cfg(target_os = "linux")]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_reload_that_moves_the_record_is_reported() {
    let case = LinkedOut::new(std::ffi::OsStr::new("secret.py"));
    let mut commands = vec!["sleep 300m".to_owned(), "set autoread".to_owned()];
    commands.extend(case.replace_the_link());
    commands.push("checktime".to_owned());
    let update = drive_linked(&case, &commands);
    assert_eq!(
        update.facts.read_from.as_deref(),
        Some(case.name.to_str().unwrap()),
        "the line after the reload says what was read"
    );
    assert_eq!(
        case.push(update)["filePath"],
        serde_json::json!(case.name.to_str().unwrap())
    );
}

/// A record that is not UTF-8 is what `from_utf8_lossy` hurt in a name: it has to arrive marked, and be refused.
#[test]
#[cfg(target_os = "linux")]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_record_that_is_not_utf8_arrives_marked_and_is_not_pushed() {
    use std::os::unix::ffi::OsStrExt;
    let case = LinkedOut::new(std::ffi::OsStr::from_bytes(b"caf\xe9.py"));
    let update = drive_linked(&case, &["normal! ggVj".to_owned()]);
    assert!(
        update
            .facts
            .read_from
            .as_deref()
            .is_some_and(|record| record.contains('\u{fffd}')),
        "{:?}",
        update.facts
    );
    assert_eq!(update.context.selection.as_ref().unwrap().text, "private line");
    let params = case.push(update);
    assert_eq!(params["text"], serde_json::json!(""), "{params}");
}

/// Opens `file`, runs `normal_command`, and returns whatever the snippet sent.
///
/// The `sleep` is inside nvim's own command sequence rather than in this process: the snippet
/// batches on a 150ms timer, so quitting immediately would race it, and sleeping here instead would
/// race nvim's startup as well.
fn drive_nvim(
    feed: &EditorContextFeed,
    reader: &mut EditorContextReader,
    file: &std::path::Path,
    normal_command: &str,
) -> Option<EditorContext> {
    drive_nvim_with(feed, reader, file, &[], &[normal_command])
}

/// [`drive_nvim`] with extra startup arguments (`--clean`) ahead of the feed's own, and several
/// commands run in order before the sleep.
fn drive_nvim_with(
    feed: &EditorContextFeed,
    reader: &mut EditorContextReader,
    file: &std::path::Path,
    startup_args: &[&str],
    commands: &[&str],
) -> Option<EditorContext> {
    drive_nvim_update(feed, reader, file, startup_args, commands).map(|update| update.context)
}

fn drive_nvim_update(
    feed: &EditorContextFeed,
    reader: &mut EditorContextReader,
    file: &std::path::Path,
    startup_args: &[&str],
    commands: &[&str],
) -> Option<eitri_core::editor_context::feed::FeedUpdate> {
    let mut command = std::process::Command::new("nvim");
    command.arg("--headless");
    command.args(startup_args);
    for arg in feed.nvim_args() {
        command.arg(arg);
    }
    for (key, value) in feed.child_env() {
        command.env(key, value);
    }
    command.args(["-c", &format!("edit {}", file.display())]);
    for each in commands {
        command.args(["-c", each]);
    }
    let status = command
        .args(["-c", "sleep 600m"])
        .args(["-c", "qa!"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("nvim must be on PATH for this test");
    assert!(status.success(), "nvim exited with {status}");
    reader.poll_update()
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_real_nvim_reports_each_selection_state_the_way_this_crate_parses_it() {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = std::env::temp_dir().join(format!("nv-w1-test-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("t.txt");
    std::fs::write(&file, "alpha\nbravo\ncharlie\ndelta\necho\n").unwrap();

    // --- no selection: the dangerous arm -------------------------------------------------------
    // Measured 2026-09-18 on real nvim: with no selection ever made, `getpos('v')` silently equals
    // `getpos('.')` and `getregion` returns `['l']` -- a plausible one-character region. A snippet
    // that asked getregion first and inferred from its answer would send that on every turn, and
    // two surveyed plugins degrade an unset selection into the whole buffer for the same reason.
    let mut feed = EditorContextFeed::new().expect("the feed must bind");
    let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
    let context = drive_nvim(&feed, &mut reader, &file, "normal! 3G").expect("an update must arrive");
    assert_eq!(context.file, file.display().to_string());
    assert_eq!(
        context.selection, None,
        "normal mode must report NO selection, not a one-character one"
    );
    let composed = compose_turn_text("what is this?", Some(&context));
    assert!(composed.contains("opened the file"), "{composed}");
    assert!(
        !composed.contains("alpha"),
        "no buffer content may travel without a selection: {composed}"
    );
    feed.cleanup();

    // --- linewise ------------------------------------------------------------------------------
    let mut feed = EditorContextFeed::new().expect("the feed must bind");
    let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
    let context = drive_nvim(&feed, &mut reader, &file, "normal! 2GVj").expect("an update must arrive");
    let selection = context.selection.clone().expect("V mode must report a selection");
    assert_eq!((selection.start_line, selection.end_line), (2, 3));
    assert_eq!(selection.text, "bravo\ncharlie");
    let composed = compose_turn_text("why?", Some(&context));
    assert!(
        composed.contains("The user selected the lines 2 to 3 from"),
        "{composed}"
    );
    assert!(
        composed.contains("bravo\ncharlie"),
        "the real text must travel, not a coordinate: {composed}"
    );
    assert!(
        composed.ends_with("This may or may not be related to the current task."),
        "{composed}"
    );
    feed.cleanup();

    // --- charwise ------------------------------------------------------------------------------
    // `2Glvll`: line 2, one right onto 'r', then a charwise selection of three characters.
    let mut feed = EditorContextFeed::new().expect("the feed must bind");
    let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
    let context = drive_nvim(&feed, &mut reader, &file, "normal! 2Glvll").expect("an update must arrive");
    let selection = context.selection.clone().expect("v mode must report a selection");
    assert_eq!((selection.start_line, selection.end_line), (2, 2));
    assert_eq!(
        selection.text, "rav",
        "a charwise selection must carry the characters, not the line"
    );
    feed.cleanup();

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_large_unicode_selection_replaces_old_context_with_bounded_text_and_its_true_range() {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = std::env::temp_dir().join(format!("nv-w1-large-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let previous_file = dir.join("previous.txt");
    let selected_file = dir.join("selected.txt");
    std::fs::write(&previous_file, "previous selection\n").unwrap();
    // Four Unicode scalar values, including an emoji and a combining mark. Forty thousand copies
    // are 400,000 bytes on ONE line: the old 400-line bound cannot keep this under the reader cap.
    let unit = "中🙂e\u{301}";
    std::fs::write(&selected_file, format!("{}\ntail\nend\n", unit.repeat(40_000))).unwrap();
    let mut feed = EditorContextFeed::new().expect("the feed must bind");
    let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
    let previous = drive_nvim(&feed, &mut reader, &previous_file, "normal! ggV").unwrap();
    assert_eq!(previous.file, previous_file.display().to_string());
    let context = drive_nvim(&feed, &mut reader, &selected_file, "normal! ggVG")
        .expect("a large selection must replace the previous context, not be silently dropped");
    assert_eq!(context.file, selected_file.display().to_string());
    let selection = context
        .selection
        .as_ref()
        .expect("the current visual selection must arrive");
    assert_eq!((selection.start_line, selection.end_line), (1, 3));
    let expected = format!("{}{TRUNCATION_MARKER}", unit.repeat(500));
    assert_eq!(
        selection.text, expected,
        "the Lua boundary must count Unicode scalars like Rust"
    );
    let composed = compose_turn_text("why?", Some(&context));
    assert_eq!(composed.matches(TRUNCATION_MARKER).count(), 1);
    assert!(composed.contains(&expected));
    feed.cleanup();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_sender_marks_only_selections_longer_than_2000_unicode_characters() {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = std::env::temp_dir().join(format!("nv-w1-limit-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("boundary.txt");
    let text = "中🙂e\u{301}".repeat(500);
    let mut feed = EditorContextFeed::new().expect("the feed must bind");
    let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
    std::fs::write(&file, format!("{text}\n")).unwrap();
    let exact = drive_nvim(&feed, &mut reader, &file, "normal! ggV").unwrap();
    assert_eq!(
        exact.selection.unwrap().text,
        text,
        "an exact-length selection needs no marker"
    );
    std::fs::write(&file, format!("{text}Z\n")).unwrap();
    let longer = drive_nvim(&feed, &mut reader, &file, "normal! ggV").unwrap();
    assert_eq!(longer.selection.unwrap().text, format!("{text}{TRUNCATION_MARKER}"));
    feed.cleanup();
    std::fs::remove_dir_all(dir).unwrap();
}

/// The 400-line cap can truncate well below `CONTENT_LIMIT` (2000 characters): 401 one-character
/// lines leave only 799 characters. The result must carry a truncation marker so the model
/// cannot mistake a partial selection for the complete selection.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_selection_past_the_line_cap_marks_truncation_even_under_the_character_limit() {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = std::env::temp_dir().join(format!("nv-w1-linecap-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("short_lines.txt");

    // The control: exactly 400 one-character lines, selected in full. Nothing is dropped, so no
    // marker belongs here -- this is Codex's own positive control, reproduced against real nvim.
    std::fs::write(&file, "x\n".repeat(400)).unwrap();
    let mut feed = EditorContextFeed::new().expect("the feed must bind");
    let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
    let at_cap = drive_nvim(&feed, &mut reader, &file, "normal! 1GVG").expect("an update must arrive");
    let selection = at_cap.selection.expect("V mode must report a selection");
    assert_eq!((selection.start_line, selection.end_line), (1, 400));
    assert_eq!(selection.text, "x\n".repeat(400).trim_end());
    assert!(
        !selection.text.contains(TRUNCATION_MARKER),
        "exactly 400 lines drops nothing: {}",
        selection.text
    );
    feed.cleanup();

    // One line over: the reported range must still be the TRUE one (1 to 401), and the dropped
    // 401st line must now be marked even though the kept 400 lines are only 799 characters.
    std::fs::write(&file, "x\n".repeat(401)).unwrap();
    let mut feed = EditorContextFeed::new().expect("the feed must bind");
    let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
    let over_cap = drive_nvim(&feed, &mut reader, &file, "normal! 1GVG").expect("an update must arrive");
    let selection = over_cap.selection.expect("V mode must report a selection");
    assert_eq!(
        (selection.start_line, selection.end_line),
        (1, 401),
        "the reported range must be the TRUE one, never the capped one"
    );
    let expected_body = "x\n".repeat(400);
    let expected_body = expected_body.trim_end();
    assert_eq!(
        selection.text,
        format!("{expected_body}{TRUNCATION_MARKER}"),
        "400 lines is what was actually sent, and it must say so"
    );
    feed.cleanup();

    let _ = std::fs::remove_dir_all(&dir);
}

/// Capping a blockwise selection must retain the endpoint's selected column. Forcing
/// `i32::MAX` widens every retained line to its full width, sending unselected text to the
/// model without a truncation marker. Two columns across 500 lines of `abcdefgh` expose it.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_capped_block_selection_sends_only_the_selected_columns() {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = std::env::temp_dir().join(format!("nv-w1-block-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("wide_lines.txt");

    // The control uses 300 lines and columns 1-2, below the cap, to catch regressions in the
    // ordinary uncapped block path too. `\u{16}` is a literal Ctrl-V byte: `:normal!` reads it
    // as the key entering blockwise Visual, just as a real keypress would.
    std::fs::write(&file, "abcdefgh\n".repeat(300)).unwrap();
    let mut feed = EditorContextFeed::new().expect("the feed must bind");
    let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
    let below_cap = drive_nvim(&feed, &mut reader, &file, "normal! 1G0\u{16}299jl").expect("an update must arrive");
    let selection = below_cap.selection.expect("block mode must report a selection");
    assert_eq!((selection.start_line, selection.end_line), (1, 300));
    let expected = vec!["ab"; 300].join("\n");
    assert_eq!(selection.text, expected);
    assert!(!selection.text.contains(TRUNCATION_MARKER));
    feed.cleanup();

    // Above the cap: 500 lines, columns 1-2. An earlier version sent 8-column "abcdefgh" lines,
    // capped to 224 lines by the character limit the wide text hit early, with no marker.
    std::fs::write(&file, "abcdefgh\n".repeat(500)).unwrap();
    let mut feed = EditorContextFeed::new().expect("the feed must bind");
    let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
    let above_cap = drive_nvim(&feed, &mut reader, &file, "normal! 1G0\u{16}499jl").expect("an update must arrive");
    let selection = above_cap.selection.expect("block mode must report a selection");
    assert_eq!(
        (selection.start_line, selection.end_line),
        (1, 500),
        "the reported range must be the TRUE one, never the capped one"
    );
    let expected_body = vec!["ab"; 400].join("\n");
    assert_eq!(
        selection.text,
        format!("{expected_body}{TRUNCATION_MARKER}"),
        "only the selected 2 columns of the kept 400 lines may travel, never the full 8-column width"
    );
    feed.cleanup();

    let _ = std::fs::remove_dir_all(&dir);
}

/// A capped block keeps the user's display columns rather than moving the lower corner's byte
/// column to the cap line. For columns 1-2 of `abcdefghijklmnop` over 500 lines, a cap line
/// containing `\tXYZ` puts byte 2 (`X`) at display column 9. Using that byte corner would
/// incorrectly send `abcdefghi` for a selection of `ab`.
///
/// Nvim supplies the oracle: its own `getregion` returns the whole uncapped block. Every sent
/// line must exactly match the corresponding oracle line; fewer lines with a truncation marker
/// are allowed, different columns are not. `--clean` excludes user configuration so each case
/// alone sets 'virtualedit' and 'selection'.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_capped_block_selection_keeps_its_display_columns_across_a_tab() {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = std::env::temp_dir().join(format!("nv-w1-blocktab-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("tab_at_the_cap.txt");
    let oracle = dir.join("uncapped.txt");
    let mut lines = vec!["abcdefghijklmnop"; 500];
    lines[399] = "\tXYZ";
    std::fs::write(&file, format!("{}\n", lines.join("\n"))).unwrap();
    let oracle_cmd = format!(
        "lua vim.fn.writefile(vim.fn.getregion(vim.fn.getpos('v'), vim.fn.getpos('.'), \
         {{ type = vim.fn.mode() }}), '{}')",
        oracle.display()
    );

    // (settings, keys, the columns every plain line must show, how many lines must be kept).
    // With 'virtualedit' off a position cannot sit inside the <Tab>, so no corner on line 400
    // yields columns 1-2 and that line gives way to line 399. With `block` it can, and all 400
    // lines are kept, the <Tab> line exactly as nvim's own getregion draws it.
    let cases: [(&str, &str, &str, usize); 4] = [
        ("set virtualedit=", "normal! 1G0\u{16}499jl", "ab", 399),
        ("set virtualedit=block", "normal! 1G0\u{16}499jl", "ab", 400),
        // The lower corner LEFT of the upper one: the block is still columns 1-2.
        ("set virtualedit=", "normal! 1G0l\u{16}499jh", "ab", 399),
        // 'selection' exclusive: the block ends just before the lower corner, so it is column 1.
        (
            "set selection=exclusive virtualedit=",
            "normal! 1G0\u{16}499jl",
            "a",
            399,
        ),
    ];
    for (settings, keys, columns, kept) in cases {
        let _ = std::fs::remove_file(&oracle);
        let mut feed = EditorContextFeed::new().expect("the feed must bind");
        let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
        let context = drive_nvim_with(&feed, &mut reader, &file, &["--clean"], &[settings, keys, &oracle_cmd])
            .unwrap_or_else(|| panic!("{settings} / {keys:?}: an update must arrive"));
        let selection = context
            .selection
            .unwrap_or_else(|| panic!("{settings} / {keys:?}: block mode must report a selection"));
        assert_eq!(
            (selection.start_line, selection.end_line),
            (1, 500),
            "{settings} / {keys:?}: the reported range must be the TRUE one"
        );
        let uncapped = std::fs::read_to_string(&oracle).expect("the oracle must have been written");
        let uncapped: Vec<&str> = uncapped.lines().collect();
        assert_eq!(
            uncapped.len(),
            500,
            "{settings} / {keys:?}: the oracle is the whole block"
        );
        assert_eq!(
            uncapped[0], columns,
            "{settings} / {keys:?}: the case selects what it says"
        );
        let body = selection.text.strip_suffix(TRUNCATION_MARKER).unwrap_or_else(|| {
            panic!(
                "{settings} / {keys:?}: a capped block must say so: {:?}",
                selection.text
            )
        });
        let sent: Vec<&str> = body.split('\n').collect();
        assert_eq!(
            sent.len(),
            kept,
            "{settings} / {keys:?}: lines kept; first sent line {:?}",
            sent[0]
        );
        assert_eq!(
            sent[..],
            uncapped[..kept],
            "{settings} / {keys:?}: every line sent must be nvim's own for the uncapped block"
        );
        feed.cleanup();
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[path = "support/embed_client.rs"]
mod embed_client;

/// Injected into a running nvim (what a companion panel does), the snippet reports a selection, and
/// its teardown removes its autocommands and its timer.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn injected_editor_context_reports_and_tears_down() {
    agent::state_dirs::redirect_state_to_a_test_root();
    use embed_client::{opts_map, Embed};
    use rmpv::Value;

    let scratch = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ec-inj-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    let file = scratch.join("t.txt");
    std::fs::write(&file, "alpha\nbravo\ncharlie\ndelta\necho\n").unwrap();
    // nvim reports the real path, and the target directory may be a symlink.
    let file = file.canonicalize().unwrap();

    // The feed only supplies a bound socket here; its environment and loader are not used.
    let mut feed = EditorContextFeed::new().expect("the feed must bind");
    let mut reader = EditorContextReader::new(feed.take_listener().unwrap());

    let mut nvim = Embed::start(&scratch, &[], &[]);
    nvim.wait_until("VimEnter", |n| n.eval("v:vim_did_enter").as_i64() == Some(1));
    nvim.command(&format!("edit {}", file.display()));
    let baseline = nvim.footprint(&["EitriEditorContext"]).active_timers;

    nvim.inject(
        include_str!("../src/editor_context/nvim_editor_context.lua"),
        opts_map(&[("socket", Value::from(feed.socket_path().display().to_string()))]),
    );
    assert_eq!(nvim.footprint(&["EitriEditorContext"]).active_timers, baseline + 1);
    nvim.input("2GVj");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let context = loop {
        match reader.poll() {
            Some(context) if context.selection.is_some() => break context,
            _ => {
                assert!(std::time::Instant::now() < deadline, "no selection arrived");
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    };
    assert_eq!(context.file, file.display().to_string());
    let selection = context.selection.expect("a selection");
    assert_eq!((selection.start_line, selection.end_line), (2, 3));
    assert_eq!(selection.text, "bravo\ncharlie");

    nvim.teardown();
    let after = nvim.footprint(&["EitriEditorContext"]);
    assert_eq!(after.autocmds, 0, "{after:?}");
    assert_eq!(after.active_timers, baseline, "{after:?}");
    nvim.quit();
    feed.cleanup();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A scratch project and an nvim started the way the embedded editor starts it.
struct Recording {
    nvim: embed_client::Embed,
    feed: EditorContextFeed,
    dir: std::path::PathBuf,
}

impl Recording {
    fn start(case: &str, extra_args: &[&str]) -> Recording {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir =
            std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ec-rec-{}-{case}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        for (name, text) in [("a.txt", "alpha\n"), ("b.txt", "bravo\n")] {
            std::fs::write(dir.join(name), text).unwrap();
        }
        let feed = EditorContextFeed::new().expect("the feed must bind");
        let nvim = embed_client::Embed::start_fed(&dir, &feed, extra_args);
        Recording { nvim, feed, dir }
    }

    fn edit(&mut self, name: &str) -> std::path::PathBuf {
        let path = self.dir.join(name);
        self.nvim.command(&format!("edit {}", path.display()));
        path
    }

    fn record(&mut self) -> Option<std::path::PathBuf> {
        self.nvim
            .lua("return vim.b.eitri_read_from", vec![])
            .as_str()
            .map(std::path::PathBuf::from)
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.feed.cleanup();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_saved_as_buffer_moves_its_record() {
    let mut r = Recording::start("saveas", &[]);
    r.edit("a.txt");
    let copy = r.dir.join("c.txt");
    r.nvim.command(&format!("saveas {}", copy.display()));
    assert_eq!(r.record(), Some(copy));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_copy_written_elsewhere_keeps_the_record() {
    let mut r = Recording::start("write-copy", &[]);
    let outside = r.dir.join("outside.txt");
    std::fs::write(&outside, "outside\n").unwrap();
    let link = r.dir.join("link.txt");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    r.edit("link.txt");
    assert_eq!(r.record(), Some(outside.clone()));
    // Only now does the name's real path differ from the record, so a write moving the record would show.
    std::fs::remove_file(&link).unwrap();
    std::fs::write(&link, "ordinary\n").unwrap();
    let copy = r.dir.join("c.txt");
    r.nvim.command(&format!("write {}", copy.display()));
    assert_eq!(r.record(), Some(outside), "the buffer holds what it held");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_renamed_buffer_keeps_its_record() {
    let mut r = Recording::start("rename", &[]);
    let file = r.edit("a.txt");
    let other = r.dir.join("renamed.txt");
    r.nvim.command(&format!("file {}", other.display()));
    assert_eq!(r.record(), Some(file), ":file changes a name and no text");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_new_file_records_its_own_name() {
    let mut r = Recording::start("new-file", &[]);
    let file = r.edit("nothing-there.txt");
    assert!(!file.exists());
    assert_eq!(r.record(), Some(file));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_new_file_written_records_where_it_landed() {
    let mut r = Recording::start("new-file-written", &[]);
    let target = r.dir.join("target.txt");
    let link = r.dir.join("link.txt");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    r.edit("link.txt");
    assert_eq!(
        r.record(),
        Some(link),
        "a link that leads nowhere has nothing on disk behind it"
    );
    r.nvim.command("write");
    assert_eq!(r.record(), Some(target));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_reloaded_buffer_records_what_was_read() {
    let mut r = Recording::start("reload", &[]);
    let first = r.dir.join("first.txt");
    let second = r.dir.join("second.txt");
    let link = r.dir.join("link.txt");
    std::fs::write(&first, "one\n").unwrap();
    std::fs::write(&second, "two\n").unwrap();
    std::os::unix::fs::symlink(&first, &link).unwrap();
    r.edit("link.txt");
    assert_eq!(r.record(), Some(first));

    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&second, &link).unwrap();
    r.nvim.command("edit!");
    assert_eq!(r.record(), Some(second.clone()), ":edit! reads it again");

    let third = r.dir.join("third.txt");
    std::fs::write(&third, "three, longer than the other\n").unwrap();
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&third, &link).unwrap();
    // nvim asks what to do with a changed file unless the buffer is set to follow it.
    r.nvim.command("setlocal autoread");
    r.nvim.command("checktime");
    assert_eq!(r.record(), Some(third), ":checktime reloads through BufReadPost too");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_file_argument_is_recorded_from_the_start() {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ec-rec-{}-arg", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let file = dir.join("given.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let feed = EditorContextFeed::new().expect("the feed must bind");
    let mut nvim = embed_client::Embed::start_fed(&dir, &feed, &[file.to_str().unwrap()]);
    nvim.wait_until("VimEnter", |n| n.eval("v:vim_did_enter").as_i64() == Some(1));
    let name = nvim.lua("return vim.api.nvim_buf_get_name(0)", vec![]);
    assert_eq!(
        name.as_str(),
        file.to_str(),
        "the file on the command line is the current buffer"
    );
    let record = nvim.lua("return vim.b.eitri_read_from", vec![]);
    assert_eq!(
        record.as_str(),
        file.to_str(),
        "the loader ran ahead of the file arguments"
    );
    nvim.quit();
    feed.cleanup();
    let _ = std::fs::remove_dir_all(&dir);
}

/// nvim loads `vim.diagnostic` on first use, and a first load has nothing there to note.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_load_does_not_pull_in_vim_diagnostic() {
    agent::state_dirs::redirect_state_to_a_test_root();
    use embed_client::Embed;
    use rmpv::Value;

    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ec-rec-{}-mod", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let file = dir.join("a.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let feed = EditorContextFeed::new().expect("the feed must bind");
    let mut nvim = Embed::start_fed(&dir, &feed, &[]);
    nvim.command(&format!("edit {}", file.display()));
    let record = nvim.lua("return vim.b.eitri_read_from", vec![]);
    assert_eq!(record.as_str(), file.to_str());
    let loaded = nvim.lua("return package.loaded['vim.diagnostic'] ~= nil", vec![]);
    assert_eq!(loaded, Value::from(false));
    nvim.quit();
    feed.cleanup();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_teardown_forgets_the_records() {
    agent::state_dirs::redirect_state_to_a_test_root();
    use embed_client::{opts_map, Embed};
    use rmpv::Value;

    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ec-rec-{}-td", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let file = dir.join("a.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let feed = EditorContextFeed::new().expect("the feed must bind");
    let mut nvim = Embed::start(&dir, &[], &[]);
    nvim.inject(
        include_str!("../src/editor_context/nvim_editor_context.lua"),
        opts_map(&[("socket", Value::from(feed.socket_path().display().to_string()))]),
    );
    nvim.command(&format!("edit {}", file.display()));
    let record = nvim.lua("return vim.b.eitri_read_from", vec![]);
    assert_eq!(record.as_str(), file.to_str());
    nvim.lua(
        "vim.diagnostic.set(vim.api.nvim_create_namespace('test'), 0, { { lnum = 0, col = 0, message = 'm' } })
         vim.b.eitri_read_from = '/a/file/read/before'
         vim.cmd('edit!')",
        vec![],
    );
    let note = nvim.lua("return vim.b.eitri_diagnostics_stood ~= nil", vec![]);
    assert_eq!(note, Value::from(true), "the reload noted the entry");
    nvim.teardown();
    let record = nvim.lua("return vim.b.eitri_read_from", vec![]);
    assert_eq!(record, Value::Nil, "nothing keeps it true from here on");
    let note = nvim.lua("return vim.b.eitri_diagnostics_stood", vec![]);
    assert_eq!(note, Value::Nil, "nor the note beside it");
    nvim.quit();
    feed.cleanup();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Once it has sent what the editor shows, a snippet with nothing new to say leaves no timer
/// running: an idle nvim is not woken by it (a repeating 150 ms timer woke it about 7 times a
/// second, measured with nvim's own context switches).
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_idle_snippet_leaves_no_timer_running() {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = std::env::temp_dir().join(format!("nv-w1-idle-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("t.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let count = dir.join("timers");
    let mut feed = EditorContextFeed::new().expect("the feed must bind");
    let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
    let report = format!(
        "lua local n = 0; vim.uv.walk(function(h) if h:get_type() == 'timer' and h:is_active() then n = n + 1 end end); \
         vim.fn.writefile({{tostring(n)}}, '{}')",
        count.display()
    );
    let context = drive_nvim_with(&feed, &mut reader, &file, &["--clean"], &["sleep 500m", &report]);
    assert_eq!(
        context.expect("what the editor showed was sent").file,
        file.display().to_string()
    );
    assert_eq!(std::fs::read_to_string(&count).unwrap().trim(), "0", "active timers");
    feed.cleanup();
    std::fs::remove_dir_all(dir).unwrap();
}

/// A selection of 400 lines of 100,000 characters sends its first 2000 characters, marked, and its
/// true range, and fetches about that much: it used to fetch all 40 MB to keep 2000 characters
/// (17 ms of nvim's loop and 160 MB of peak memory, measured).
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_selection_of_long_lines_fetches_no_more_than_it_sends() {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = std::env::temp_dir().join(format!("nv-w1-long-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("long.txt");
    let line = |i: usize| format!("{i:03}{}", "x".repeat(100_000));
    let body: String = (0..400).map(|i| line(i) + "\n").collect();
    std::fs::write(&file, &body).unwrap();
    let fetched = dir.join("fetched");
    // Counts what every getregion call returns; installed before the snippet loads.
    let count = "lua local g = vim.fn.getregion; _G.fetched = 0; vim.fn.getregion = function(...) \
                 local r = g(...); for _, l in ipairs(r) do _G.fetched = _G.fetched + #l end; return r end";
    let report = format!(
        "lua vim.fn.writefile({{tostring(_G.fetched)}}, '{}')",
        fetched.display()
    );
    for (motion, from) in [("normal! ggVG", 0), ("normal! gg0lvG$", 1)] {
        let mut feed = EditorContextFeed::new().expect("the feed must bind");
        let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
        let context = drive_nvim_with(
            &feed,
            &mut reader,
            &file,
            &["--clean", "--cmd", count],
            &[motion, "sleep 400m", &report],
        )
        .expect("an update must arrive");
        let selection = context.selection.expect("a selection");
        assert_eq!((selection.start_line, selection.end_line), (1, 400), "{motion}");
        let whole: String = body[from..].chars().take(2000).collect();
        assert_eq!(selection.text, format!("{whole}{TRUNCATION_MARKER}"), "{motion}");
        let bytes: usize = std::fs::read_to_string(&fetched).unwrap().trim().parse().unwrap();
        assert!(bytes < 64 * 1024, "{motion}: fetched {bytes} bytes");
        feed.cleanup();
    }
    std::fs::remove_dir_all(dir).unwrap();
}

/// The edit-the-selection key, driven in a real nvim: `commands` run after the feed's snippet loaded, then
/// `doautocmd VimEnter` (the `-c` commands run before that event fires, and the key is put in at it), then
/// `after`. Returns the update nvim sent last.
fn drive_edit_key(
    file: &std::path::Path,
    commands: &[&str],
    after: &[&str],
) -> Option<eitri_core::editor_context::feed::FeedUpdate> {
    let mut feed = EditorContextFeed::new().expect("the feed must bind");
    let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
    let mut all: Vec<&str> = commands.to_vec();
    all.extend(["doautocmd VimEnter", "sleep 100m"]);
    all.extend(after);
    let update = drive_nvim_update(&feed, &mut reader, file, &["--clean"], &all);
    feed.cleanup();
    update
}

fn edit_file(body: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("nv-w1-edit-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("t.txt");
    std::fs::write(&file, body).unwrap();
    (dir, file)
}

const PRESS: &str = r#"lua vim.api.nvim_feedkeys(vim.keycode("\\ae"), "x", false)"#;

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_edit_key_stamps_the_line_that_carries_the_selection_and_leaves_visual_mode_on() {
    let (dir, file) = edit_file("alpha\nbravo\ncharlie\n");
    let update = drive_edit_key(&file, &["normal! gglvj"], &[PRESS]).expect("an update must arrive");
    let selection = update
        .context
        .selection
        .expect("the selection is still live after the key");
    assert_eq!((selection.start_line, selection.end_line), (1, 2));
    let ask = update.context.edit_request.expect("the line says the key was pressed");
    // Milliseconds since the epoch, nvim's clock: after 2026-01-01 and not in the future.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!((1_767_225_600_000..=now + 1000).contains(&ask), "{ask} vs {now}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn without_the_key_no_line_says_it_was_pressed() {
    let (dir, file) = edit_file("alpha\nbravo\n");
    let update = drive_edit_key(&file, &["normal! gglvj"], &[]).expect("an update must arrive");
    assert!(update.context.selection.is_some());
    assert_eq!(update.context.edit_request, None);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_key_follows_the_leader_the_person_set() {
    let (dir, file) = edit_file("alpha\nbravo\n");
    let press = r#"lua vim.api.nvim_feedkeys(vim.keycode("<Space>ae"), "x", false)"#;
    let update = drive_edit_key(&file, &["let mapleader = ' '", "normal! gglvj"], &[press]).expect("an update");
    assert!(update.context.edit_request.is_some(), "<Space>ae pressed");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_key_the_person_already_mapped_is_left_theirs() {
    let (dir, file) = edit_file("alpha\nbravo\n");
    let theirs = r#"lua vim.keymap.set("x", "\\ae", function() vim.g.theirs = 1 end)"#;
    let report = format!(
        "lua vim.fn.writefile({{tostring(vim.g.theirs)}}, '{}')",
        dir.join("theirs").display()
    );
    let update = drive_edit_key(&file, &[theirs, "normal! gglvj"], &[PRESS, &report]).expect("an update");
    assert_eq!(update.context.edit_request, None, "their mapping ran, not ours");
    assert_eq!(std::fs::read_to_string(dir.join("theirs")).unwrap().trim(), "1");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_edit_function_is_there_for_a_key_of_their_own_and_the_default_shadows_no_stock_mapping() {
    let (dir, file) = edit_file("alpha\nbravo\n");
    let report = format!(
        "lua vim.fn.writefile({{tostring(vim.fn.maparg('\\\\ae', 'x')), tostring(vim.fn.maparg('\\\\ae', 'n')), \
         tostring(type(_G.__eitri_edit_selection))}}, '{}')",
        dir.join("report").display()
    );
    drive_edit_key(&file, &["normal! gglvj"], &[&report]).expect("an update");
    let report = std::fs::read_to_string(dir.join("report")).unwrap();
    let lines: Vec<&str> = report.lines().collect();
    assert_eq!(lines.len(), 3, "{report:?}");
    assert_ne!(lines[0], "", "the key is mapped in Visual mode");
    assert_eq!(lines[1], "", "and only there");
    assert_eq!(lines[2], "function");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_torn_down_snippet_takes_its_key_and_its_function_away() {
    let (dir, file) = edit_file("alpha\n");
    let lua = dir.join("snippet.lua");
    std::fs::write(&lua, include_str!("../src/editor_context/nvim_editor_context.lua")).unwrap();
    let report = dir.join("report");
    let status = std::process::Command::new("nvim")
        .args(["--headless", "--clean"])
        .args(["-c", &format!("edit {}", file.display())])
        .args([
            "-c",
            &format!(
                "lua _G.td = loadfile('{}')({{ socket = '{}' }})",
                lua.display(),
                dir.join("none.sock").display()
            ),
        ])
        .args(["-c", "doautocmd VimEnter", "-c", "sleep 100m"])
        .args([
            "-c",
            "lua _G.before = {vim.fn.maparg('\\\\ae', 'x') ~= '', type(_G.__eitri_edit_selection)}",
        ])
        .args(["-c", "lua _G.td()"])
        .args([
            "-c",
            &format!(
                "lua vim.fn.writefile({{tostring(_G.before[1]), _G.before[2], tostring(vim.fn.maparg('\\\\ae', 'x') ~= ''), \
                 type(_G.__eitri_edit_selection)}}, '{}')",
                report.display()
            ),
        ])
        .args(["-c", "qa!"])
        .stdin(std::process::Stdio::null())
        .status()
        .expect("nvim must be on PATH for this test");
    assert!(status.success());
    let report = std::fs::read_to_string(&report).unwrap();
    assert_eq!(report.lines().collect::<Vec<_>>(), ["true", "function", "false", "nil"]);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_leader_that_changes_later_moves_only_the_snippets_own_mapping() {
    let (dir, file) = edit_file("alpha\nbravo\n");
    let theirs = r#"lua vim.keymap.set("x", "\\ae", function() vim.g.theirs = 1 end, { desc = "mine" })"#;
    let report = format!(
        "lua vim.fn.writefile({{tostring(vim.fn.maparg('\\\\ae', 'x', false, true).desc), \
         tostring(vim.fn.maparg(' ae', 'x', false, true).desc)}}, '{}')",
        dir.join("report").display()
    );
    drive_edit_key(
        &file,
        &["normal! gglvj"],
        // nvim takes ten -c commands at most.
        &[
            theirs,
            "let mapleader = ' ' | doautocmd User LazyLoad | sleep 100m",
            &report,
        ],
    )
    .expect("an update");
    let report = std::fs::read_to_string(dir.join("report")).unwrap();
    assert_eq!(
        report.lines().collect::<Vec<_>>(),
        ["mine", "eitri: edit the selection with the agent"],
        "their mapping at the old key stays, and the snippet's is at the new one"
    );
    std::fs::remove_dir_all(dir).unwrap();
}
