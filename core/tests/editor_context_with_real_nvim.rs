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
//! Run: `cargo test -p neovibe-core --test editor_context_with_real_nvim -- --ignored`

use neovibe_core::editor_context::compose::{compose_turn_text, TRUNCATION_MARKER};
use neovibe_core::editor_context::feed::{EditorContextFeed, EditorContextReader};
use neovibe_core::editor_context::EditorContext;

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
    reader.poll()
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

/// P5-A2: the 400-line cap can drop content that never comes close to `CONTENT_LIMIT` (2000
/// characters) -- 401 one-character lines is 799 characters kept, nowhere near the limit -- and
/// before this fix that drop carried no marker at all, so the model read a partial selection as
/// though it were complete.
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

/// P5-M1: when the line cap applies to a BLOCKWISE selection, the endpoint's column must stay the
/// cursor's own column. Before this fix it was forced to `i32::MAX`, which widens the block to
/// the full line width for every kept line -- text outside the selection reaching the model as
/// "the user selected", with no truncation marker to say anything was wrong at all.
///
/// The verifier's own probe: 2 columns x 500 lines of `abcdefgh`.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_capped_block_selection_sends_only_the_selected_columns() {
    let dir = std::env::temp_dir().join(format!("nv-w1-block-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("wide_lines.txt");

    // The control: 300 lines, columns 1-2, below the cap. Correct even before this fix -- kept so
    // a regression in the ordinary (uncapped) block path fails this test too. `\u{16}` is a
    // literal Ctrl-V byte: `:normal!` reads it as the key that enters blockwise-visual, exactly as
    // it would from a real keypress.
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

    // Above the cap: 500 lines, columns 1-2. Before the fix this sent 8-column "abcdefgh" lines,
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

/// P5-M1, round 2 (the Codex whole-branch review): a capped block keeps the user's DISPLAY columns,
/// not the lower corner's byte column moved up to the cap line. Codex's case: columns 1-2 of
/// `abcdefghijklmnop` over 500 lines, with line 400 reading `\tXYZ`. Round 1 moved the corner to
/// line 400 at byte 2 -- the `X`, at display column 9 -- and sent `abcdefghi` for a selection of
/// `ab`.
///
/// The oracle is nvim itself: after the selection is made, the same nvim writes what its own
/// `getregion` returns for the WHOLE, uncapped block. Every line the snippet sends must be that
/// line of the oracle, exactly -- fewer lines is allowed (and marked), other columns never are.
/// `--clean` keeps the owner's config out of it, so 'virtualedit' and 'selection' are what each
/// case sets and nothing else.
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
