use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use rmpv::Value;
use sha2::{Digest, Sha256};

use super::*;
use crate::keymap::{Action, Keymap};
use crate::nvim_rpc::{Answer, RpcError};

/// What each key the lessons name after the prefix must do in the default table, by its spelling in the lessons
/// (`KeySpec::human`). A key the lessons start naming needs a row here, so a lesson cannot quietly teach a key
/// whose action changed or went away.
const PREFIX_KEYS: &[(&str, &str)] = &[
    ("?", "panel.keymap"),
    ("%", "split.right"),
    ("\"", "split.below"),
    ("z", "zoom"),
    ("x", "module.kill"),
    ("o", "select.next"),
    (";", "select.last"),
    ("Left", "select.left"),
    ("f", "hint"),
    ("c", "tab.new"),
    ("n", "tab.next"),
    ("p", "tab.prev"),
    ("1", "tab.select"),
    ("&", "tab.close"),
    ("e", "module.editor"),
    ("a", "module.agent"),
    ("t", "module.terminal"),
    ("[", "copy-mode"),
    ("Ctrl+b", "send-prefix"),
    ("Ctrl+h", "send-keys"),
    ("Ctrl+j", "send-keys"),
    ("Ctrl+k", "send-keys"),
    ("Ctrl+l", "send-keys"),
    ("Alt+Shift+Space", "float.toggle"),
    ("Alt+_", "scratchpad.move"),
    ("Alt+-", "scratchpad.show"),
    ("s", "workspace.choose"),
    ("L", "workspace.last"),
    ("Alt+w", "container.tabbed"),
];

/// Chords the lessons name on their own that are not the window's: the agent panel's own keys in its message box.
const PANEL_CHORDS: &[&str] = &["Ctrl+u", "Ctrl+y"];

fn lessons() -> [(&'static str, &'static str, &'static str); 2] {
    [("en", LESSONS_EN, CHECKS_EN), ("zh", LESSONS_ZH, CHECKS_ZH)]
}

fn expect_of(checks: &str) -> BTreeMap<usize, String> {
    let json: serde_json::Value = serde_json::from_str(checks).expect("the checks are JSON");
    json["expect"]
        .as_object()
        .expect("an expect table")
        .iter()
        .map(|(line, want)| {
            let line: usize = line.parse().expect("each key is a line number");
            (line, want.as_str().expect("each check is the line's text").to_string())
        })
        .collect()
}

/// The text of every inline code span in `text`, leaving out nvim's own keys and commands (a span followed by
/// `{normal}` or `{vim}`).
fn spans(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('`').collect();
        let mut i = 1;
        while i + 1 < parts.len() {
            let after = parts[i + 1];
            if !(after.starts_with("{normal}") || after.starts_with("{vim}")) {
                out.push(parts[i].to_string());
            }
            i += 2;
        }
    }
    out
}

/// Every key the lessons and their checks name after the prefix, in order.
fn prefix_keys(lessons: &str, checks: &str) -> Vec<String> {
    let mut out: Vec<String> = spans(lessons)
        .into_iter()
        .filter_map(|span| span.strip_prefix("Ctrl+b ").map(str::to_string))
        .collect();
    for want in expect_of(checks).values() {
        if let Some((_, key)) = want.rsplit_once("Ctrl+b ") {
            out.push(key.to_string());
        }
    }
    out
}

/// Every chord the lessons name on its own (no prefix before it).
fn bare_chords(lessons: &str) -> Vec<String> {
    spans(lessons)
        .into_iter()
        .filter(|span| (span.starts_with("Ctrl+") || span.starts_with("Alt+")) && !span.contains(' '))
        .collect()
}

#[test]
fn every_key_the_lessons_name_after_the_prefix_does_what_they_say_in_the_default_table() {
    let defaults = Keymap::defaults();
    assert_eq!(
        defaults.prefix().human(),
        "Ctrl+b",
        "the lessons are written for this prefix"
    );
    let wanted: BTreeMap<&str, &str> = PREFIX_KEYS.iter().copied().collect();
    for (lang, lessons, checks) in lessons() {
        let named = prefix_keys(lessons, checks);
        assert!(
            named.len() > 20,
            "{lang}: the lessons name the window's keys ({named:?})"
        );
        for key in named {
            let Some(want) = wanted.get(key.as_str()) else {
                panic!("{lang}: the lessons name Ctrl+b {key}, which has no row in PREFIX_KEYS");
            };
            let binding = defaults
                .bindings()
                .iter()
                .find(|b| b.key.human() == key)
                .unwrap_or_else(|| panic!("{lang}: Ctrl+b {key} is not in the default key table"));
            assert_eq!(
                binding.action.name(),
                *want,
                "{lang}: Ctrl+b {key} does something else now; change the lessons with it"
            );
            if let Action::SendKeys(sent) = &binding.action {
                assert_eq!(sent.human(), key, "{lang}: Ctrl+b {key} sends that chord itself");
            }
        }
    }
}

#[test]
fn every_chord_the_lessons_name_alone_is_a_moving_key_or_the_panels_own() {
    let roots: Vec<String> = crate::keymap::root::NAV_KEYS
        .iter()
        .map(|(letter, _)| format!("Ctrl+{letter}"))
        .collect();
    for (lang, lessons, _) in lessons() {
        let chords = bare_chords(lessons);
        for nav in &roots {
            assert!(chords.contains(nav), "{lang}: the lessons teach {nav}");
        }
        for chord in chords {
            let is_prefix = chord == Keymap::defaults().prefix().human();
            assert!(
                is_prefix || roots.contains(&chord) || PANEL_CHORDS.contains(&chord.as_str()),
                "{lang}: {chord} is neither the prefix, a moving key nor one of the panel's own chords"
            );
        }
    }
}

#[test]
fn both_languages_name_the_same_keys() {
    let mut en = prefix_keys(LESSONS_EN, CHECKS_EN);
    let mut zh = prefix_keys(LESSONS_ZH, CHECKS_ZH);
    en.sort();
    zh.sort();
    assert_eq!(en, zh, "the translation names other keys than the English lessons");
    let mut en = bare_chords(LESSONS_EN);
    let mut zh = bare_chords(LESSONS_ZH);
    en.sort();
    zh.sort();
    assert_eq!(en, zh);
}

#[test]
fn each_check_points_at_an_exercise_line_of_its_lessons() {
    for (lang, lessons, checks) in lessons() {
        let lines: Vec<&str> = lessons.split('\n').collect();
        let expect = expect_of(checks);
        assert_eq!(expect.len(), 7, "{lang}: seven exercises");
        for (n, want) in &expect {
            let line = lines
                .get(n - 1)
                .unwrap_or_else(|| panic!("{lang}: check line {n} is past the end"));
            assert_ne!(line, want, "{lang}: line {n} is already done, so its ✗ never shows");
            let common = line.chars().zip(want.chars()).take_while(|(a, b)| a == b).count();
            assert!(
                common >= 8,
                "{lang}: line {n} ({line:?}) is not the exercise {want:?}: the lessons moved under the checks"
            );
        }
    }
}

#[test]
fn both_languages_ask_for_the_same_answers() {
    let tail = |want: &String| {
        want.rsplit_once("Ctrl+b ")
            .map_or(want.clone(), |(_, key)| key.to_string())
    };
    let en: Vec<String> = expect_of(CHECKS_EN).values().map(tail).collect();
    let zh: Vec<String> = expect_of(CHECKS_ZH).values().map(tail).collect();
    assert_eq!(en, zh);
}

#[test]
fn the_translation_follows_the_current_english_lessons() {
    let json: serde_json::Value = serde_json::from_str(CHECKS_ZH).unwrap();
    assert_eq!(json["translated-from"]["file"], "en/eitri.tutor");
    let hash = format!("{:x}", Sha256::digest(LESSONS_EN.as_bytes()));
    assert_eq!(
        json["translated-from"]["sha256"].as_str(),
        Some(hash.as_str()),
        "the English lessons changed: bring the Chinese lessons up to date, then set this hash to {hash}"
    );
}

#[test]
fn no_lesson_line_is_wider_than_the_tutor_window_expects() {
    for (lang, lessons, _) in lessons() {
        for (i, line) in lessons.lines().enumerate() {
            let width: usize = line
                .chars()
                .map(|c| {
                    if ('\u{2e80}'..='\u{9fff}').contains(&c) || ('\u{ff00}'..='\u{ffef}').contains(&c) {
                        2
                    } else {
                        1
                    }
                })
                .sum();
            assert!(width <= 80, "{lang}: line {} is {width} columns wide", i + 1);
        }
    }
}

#[test]
fn the_files_are_laid_out_as_nvim_looks_for_tutorials_and_the_snippet_writes_each() {
    let paths: Vec<&str> = FILES.iter().map(|(rel, _)| *rel).collect();
    let mut want = Vec::new();
    for lang in ["en", "zh"] {
        want.push(format!("tutor/{lang}/{TUTORIAL}.tutor"));
        want.push(format!("tutor/{lang}/{TUTORIAL}.tutor.json"));
    }
    assert_eq!(paths, want);
    for rel in paths {
        assert!(
            INSTALL_TUTOR_LUA.contains(&format!("'{rel}',")),
            "the snippet writes only the files it lists itself: {rel}"
        );
    }
}

/// A directory of the test's own under the temp directory, of exactly `mode`.
fn scratch(label: &str, mode: u32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("nv-tutor-test-{label}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir(&dir).unwrap_or_else(|e| panic!("create_dir {dir:?}: {e}"));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
    dir
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

#[test]
fn the_directory_is_made_private_and_empty() {
    let base = scratch("made", 0o755);
    let dir = TutorDir::in_dir(&base).expect("made");
    assert_eq!(dir.path().parent(), Some(base.as_path()));
    assert!(dir
        .path()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with(DIR_PREFIX));
    assert_eq!(mode(dir.path()), 0o700);
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "the files are the command's to write"
    );
    let path = dir.path().to_path_buf();
    dir.cleanup();
    assert!(!path.exists());
    std::fs::remove_dir(&base).unwrap();
}

#[test]
fn a_temp_dir_others_can_rename_entries_in_is_refused_and_nothing_is_made() {
    let tmp = scratch("shared", 0o755);
    for open_mode in [0o777, 0o775, 0o757] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(open_mode)).unwrap();
        let err = TutorDir::in_base(None, &tmp).err().expect("refused");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{open_mode:o}: {err}");
        assert_eq!(
            std::fs::read_dir(&tmp).unwrap().count(),
            0,
            "{open_mode:o}: nothing is made under an unsafe parent"
        );
    }
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o1777)).unwrap();
    }
    let dir = TutorDir::in_base(None, &tmp).expect("a sticky one is safe");
    assert_eq!(dir.path().parent(), Some(tmp.as_path()));
    dir.cleanup();
    std::fs::remove_dir(&tmp).unwrap();
}

#[test]
fn a_private_runtime_dir_is_preferred_and_a_loose_or_relative_one_is_not() {
    let tmp = scratch("tmp", 0o755);
    let runtime = scratch("runtime", 0o700);
    let dir = TutorDir::in_base(Some(&runtime), &tmp).expect("made");
    assert_eq!(dir.path().parent(), Some(runtime.as_path()));
    dir.cleanup();

    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let dir = TutorDir::in_base(Some(&runtime), &tmp).expect("made");
    assert_eq!(
        dir.path().parent(),
        Some(tmp.as_path()),
        "a runtime dir others can enter"
    );
    dir.cleanup();

    let relative = Path::new("relative-runtime-dir");
    assert_eq!(TutorDir::base(Some(relative), &tmp).unwrap(), tmp);
    std::fs::remove_dir(&runtime).unwrap();
    std::fs::remove_dir(&tmp).unwrap();
}

#[test]
fn a_path_with_a_character_runtimepath_does_not_take_literally_is_refused() {
    for &c in SPECIAL_IN_RUNTIMEPATH {
        let base = scratch(&format!("a{}b", char::from(c)), 0o755);
        let err = TutorDir::in_dir(&base).err().expect("refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{:?}: {err}", char::from(c));
        assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0, "nothing is made");
        std::fs::remove_dir(&base).unwrap();
    }
    assert_eq!(SPECIAL_IN_RUNTIMEPATH, b"\\[]*?{}$~");
    // A comma is escaped on 'runtimepath' instead.
    let base = scratch("a,b", 0o755);
    let dir = TutorDir::in_dir(&base).expect("a comma is taken");
    dir.cleanup();
    std::fs::remove_dir(&base).unwrap();
}

/// An editor whose calls the test answers by hand.
#[derive(Default)]
struct FakeEditor {
    target: Cell<Option<u64>>,
    calls: RefCell<Vec<(Value, Answer)>>,
}

impl EditorRpc for FakeEditor {
    fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
        assert_eq!(code, INSTALL_TUTOR_LUA, "only the install snippet is sent");
        assert_eq!(args.len(), 1);
        let (answer, pending) = Pending::pair();
        self.calls.borrow_mut().push((args.into_iter().next().unwrap(), answer));
        pending
    }

    fn target(&self) -> Option<u64> {
        self.target.get()
    }
}

impl FakeEditor {
    fn answer(&self, reply: Result<Value, RpcError>) {
        let (_, answer) = self.calls.borrow_mut().remove(0);
        answer.send(reply);
    }

    fn calls(&self) -> usize {
        self.calls.borrow().len()
    }
}

fn done(skipped: &[&str]) -> Value {
    Value::Map(vec![
        (Value::from("ok"), Value::from(true)),
        (
            Value::from("skipped"),
            Value::Array(skipped.iter().map(|s| Value::from(*s)).collect()),
        ),
    ])
}

fn early() -> Value {
    Value::Map(vec![
        (Value::from("ok"), Value::from(false)),
        (Value::from("early"), Value::from(true)),
    ])
}

fn installer(editor: &Rc<FakeEditor>) -> TutorInstaller {
    let mut tutor = TutorInstaller::with_dir(Box::new(|| TutorDir::in_dir(&std::env::temp_dir()).ok()));
    tutor.set_editor(editor.clone());
    tutor
}

#[test]
fn each_nvim_session_is_installed_once_with_the_lessons_directory() {
    let editor = Rc::new(FakeEditor::default());
    let mut tutor = installer(&editor);
    let now = Instant::now();

    assert_eq!(tutor.editor_session(now), None, "no nvim yet: nothing to ask");
    assert_eq!(editor.calls(), 0);

    editor.target.set(Some(7));
    assert_eq!(tutor.editor_session(now), Some(now + POLL));
    assert_eq!(editor.calls(), 1);
    let dir = tutor.dir().expect("written at the first install").to_path_buf();
    {
        let calls = editor.calls.borrow();
        let opts = &calls[0].0;
        assert_eq!(opts, &install_opts(&dir));
        let dir_arg = opts.as_map().unwrap().iter().find(|(k, _)| k.as_str() == Some("dir"));
        assert_eq!(
            dir_arg.and_then(|(_, v)| v.as_slice()),
            Some(dir.as_os_str().as_encoded_bytes())
        );
        let uid = opts.as_map().unwrap().iter().find(|(k, _)| k.as_str() == Some("uid"));
        assert_eq!(
            uid.and_then(|(_, v)| v.as_u64()),
            Some(u64::from(agent::private_fs::euid()))
        );
        let files = opts.as_map().unwrap().iter().find(|(k, _)| k.as_str() == Some("files"));
        let files = files
            .and_then(|(_, v)| v.as_map())
            .expect("the lessons go with the call");
        assert_eq!(files.len(), FILES.len());
        for (rel, text) in FILES {
            let sent = files
                .iter()
                .find(|(k, _)| k.as_str() == Some(rel))
                .map(|(_, v)| v.as_str());
            assert_eq!(sent, Some(Some(text)), "{rel}");
        }
    }
    // The feeds report again while the call is out: no second call.
    tutor.editor_session(now);
    assert_eq!(editor.calls(), 1);
    assert_eq!(tutor.poll(now + POLL), Some(now + POLL + POLL), "still waiting");

    editor.answer(Ok(done(&[])));
    assert_eq!(tutor.poll(now + POLL), None);
    assert!(tutor.reported().is_empty());

    assert_eq!(tutor.editor_session(now), None, "this session has it");
    assert_eq!(editor.calls(), 0);

    editor.target.set(Some(8));
    tutor.editor_session(now);
    assert_eq!(editor.calls(), 1, "a new nvim session is installed again");
    assert_eq!(tutor.dir(), Some(dir.as_path()), "into the same directory");
    tutor.cleanup();
}

#[test]
fn an_early_answer_is_asked_again_and_given_up_after_a_while() {
    let editor = Rc::new(FakeEditor::default());
    editor.target.set(Some(1));
    let mut tutor = installer(&editor);
    let start = Instant::now();
    tutor.editor_session(start);
    editor.answer(Ok(early()));
    let again = tutor.poll(start).expect("asked again later");
    assert_eq!(again, start + EARLY_AGAIN);
    assert_eq!(tutor.poll(start), Some(again), "not before its time");
    assert_eq!(editor.calls(), 0);
    assert_eq!(tutor.poll(again), Some(again + POLL));
    assert_eq!(editor.calls(), 1, "the second call");

    editor.answer(Ok(early()));
    let late = start + EARLY_GIVE_UP;
    assert_eq!(tutor.poll(late), None, "given up");
    assert_eq!(
        tutor.editor_session(late),
        Some(late + POLL),
        "the next session report starts over"
    );
    editor.answer(Ok(done(&[])));
    assert_eq!(tutor.poll(late), None);
    tutor.cleanup();
}

#[test]
fn a_command_the_user_has_is_reported_once() {
    let editor = Rc::new(FakeEditor::default());
    let mut tutor = installer(&editor);
    let now = Instant::now();
    for session in [1, 2] {
        editor.target.set(Some(session));
        tutor.editor_session(now);
        editor.answer(Ok(done(&[COMMAND])));
        assert_eq!(tutor.poll(now), None);
    }
    assert_eq!(tutor.reported(), [COMMAND.to_string()]);
    tutor.cleanup();
}

#[test]
fn an_unanswered_call_is_given_up_and_a_failed_one_is_not_retried_in_that_session() {
    let editor = Rc::new(FakeEditor::default());
    editor.target.set(Some(1));
    let mut tutor = installer(&editor);
    let now = Instant::now();
    tutor.editor_session(now);
    assert_eq!(tutor.poll(now + ANSWER_WAIT), None, "no answer in time");
    editor.calls.borrow_mut().clear();

    tutor.editor_session(now);
    editor.answer(Err(RpcError::Nvim("E5108: boom".to_string())));
    assert_eq!(tutor.poll(now), None);
    assert_eq!(tutor.editor_session(now), Some(now + POLL), "a later report asks again");
    tutor.cleanup();
}

#[test]
fn without_a_directory_nothing_is_asked_and_the_directory_is_not_tried_again() {
    let editor = Rc::new(FakeEditor::default());
    editor.target.set(Some(1));
    let tries = Rc::new(Cell::new(0));
    let counted = tries.clone();
    let mut tutor = TutorInstaller::with_dir(Box::new(move || {
        counted.set(counted.get() + 1);
        None
    }));
    tutor.set_editor(editor.clone());
    assert_eq!(tutor.editor_session(Instant::now()), None);
    editor.target.set(Some(2));
    assert_eq!(tutor.editor_session(Instant::now()), None);
    assert_eq!(editor.calls(), 0);
    assert_eq!(tries.get(), 1);
}

#[test]
fn the_answer_is_read_in_both_shapes_of_an_empty_list() {
    assert_eq!(read_installed(&done(&[])), Ok(Installed::Done { skipped: vec![] }));
    let empty_map = Value::Map(vec![
        (Value::from("ok"), Value::from(true)),
        (Value::from("skipped"), Value::Map(vec![])),
    ]);
    assert_eq!(read_installed(&empty_map), Ok(Installed::Done { skipped: vec![] }));
    assert_eq!(read_installed(&early()), Ok(Installed::Early));
    assert!(read_installed(&Value::Map(vec![(Value::from("ok"), Value::from(false))])).is_err());
    assert!(read_installed(&Value::Nil).is_err());
}

#[test]
fn the_snippet_keeps_the_editor_snippet_contract() {
    // Options as one argument, nothing read from the environment, and only Lua that LuaJIT and PUC 5.1 run.
    assert!(INSTALL_TUTOR_LUA.contains("local o = ..."));
    assert!(!INSTALL_TUTOR_LUA.contains("vim.env"));
    assert!(!INSTALL_TUTOR_LUA.contains("os.getenv"));
    assert!(!INSTALL_TUTOR_LUA.contains("goto "));
    // The tutor plugin's entry point, so a config that skips the plugin (and so `:Tutor`) still gets the lessons.
    assert!(INSTALL_TUTOR_LUA.contains("pcall(vim.fn['tutor#TutorCmd'], 'eitri')"));
    assert_eq!(TUTORIAL, "eitri");
}
