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

use eitri_core::editor_context::compose::{compose_turn_text, TRUNCATION_MARKER};
use eitri_core::editor_context::feed::{EditorContextFeed, EditorContextReader};
use eitri_core::editor_context::EditorContext;

fn selection_facts(body: &str, commands: &[&str]) -> eitri_core::editor_context::feed::PushFacts {
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

/// Once it has sent what the editor shows, a snippet with nothing new to say leaves no timer
/// running: an idle nvim is not woken by it (a repeating 150 ms timer woke it about 7 times a
/// second, measured with nvim's own context switches).
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_idle_snippet_leaves_no_timer_running() {
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
