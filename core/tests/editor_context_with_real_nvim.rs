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
    let mut command = std::process::Command::new("nvim");
    command.arg("--headless");
    for arg in feed.nvim_args() {
        command.arg(arg);
    }
    for (key, value) in feed.child_env() {
        command.env(key, value);
    }
    let status = command
        .args(["-c", &format!("edit {}", file.display())])
        .args(["-c", normal_command])
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
