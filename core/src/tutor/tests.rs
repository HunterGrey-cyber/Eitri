use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use rmpv::Value;
use sha2::{Digest, Sha256};

use super::prefix_text::{substitute, substitute_checks};
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

/// Prefixes the window may have: the stock one, a tmux user's, the two ten-column presets, and two whose spelling
/// holds a character JSON has to escape.
const PREFIXES: &[&str] = &["Ctrl+b", "Ctrl+a", "Ctrl+Space", "Ctrl+Alt+x", "Alt+\"", "Alt+\\"];

/// Both languages' lessons and checks as installed for `prefix`.
fn lessons_for(prefix: &str) -> [(&'static str, String, String); 2] {
    [
        (
            "en",
            substitute(LESSONS_EN, prefix),
            substitute_checks(CHECKS_EN, prefix),
        ),
        (
            "zh",
            substitute(LESSONS_ZH, prefix),
            substitute_checks(CHECKS_ZH, prefix),
        ),
    ]
}

/// The lessons as installed for the stock prefix: what the key table is held to, since it names real keys.
fn lessons() -> [(&'static str, String, String); 2] {
    lessons_for(DEFAULT_PREFIX)
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
        let named = prefix_keys(&lessons, &checks);
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
        let chords = bare_chords(&lessons);
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
    let [(_, en_lessons, en_checks), (_, zh_lessons, zh_checks)] = lessons();
    let mut en = prefix_keys(&en_lessons, &en_checks);
    let mut zh = prefix_keys(&zh_lessons, &zh_checks);
    en.sort();
    zh.sort();
    assert_eq!(en, zh, "the translation names other keys than the English lessons");
    let mut en = bare_chords(&en_lessons);
    let mut zh = bare_chords(&zh_lessons);
    en.sort();
    zh.sort();
    assert_eq!(en, zh);
}

#[test]
fn each_check_points_at_an_exercise_line_of_its_lessons() {
    for prefix in PREFIXES {
        for (lang, lessons, checks) in lessons_for(prefix) {
            let lines: Vec<&str> = lessons.split('\n').collect();
            let expect = expect_of(&checks);
            assert_eq!(expect.len(), 7, "{lang} {prefix}: seven exercises");
            for (n, want) in &expect {
                let line = lines
                    .get(n - 1)
                    .unwrap_or_else(|| panic!("{lang} {prefix}: check line {n} is past the end"));
                assert_ne!(
                    line, want,
                    "{lang} {prefix}: line {n} is already done, so its ✗ never shows"
                );
                let common = line.chars().zip(want.chars()).take_while(|(a, b)| a == b).count();
                assert!(
                    common >= 8,
                    "{lang} {prefix}: line {n} ({line:?}) is not the exercise {want:?}: the lessons moved under the checks"
                );
            }
        }
    }
}

#[test]
fn both_languages_ask_for_the_same_answers() {
    for prefix in PREFIXES {
        let tail = |want: &String| {
            want.rsplit_once(&format!("{prefix} "))
                .map_or(want.clone(), |(_, key)| key.to_string())
        };
        let [(_, _, en), (_, _, zh)] = lessons_for(prefix);
        let en: Vec<String> = expect_of(&en).values().map(tail).collect();
        let zh: Vec<String> = expect_of(&zh).values().map(tail).collect();
        assert_eq!(en, zh, "{prefix}");
    }
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
fn every_line_is_within_80_columns() {
    for prefix in PREFIXES {
        for (lang, lessons, _) in lessons_for(prefix) {
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
                assert!(width <= 80, "{lang} {prefix}: line {} is {width} columns wide", i + 1);
            }
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
    attached: Cell<bool>,
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

    fn attached(&self) -> bool {
        self.attached.get()
    }
}

#[test]
fn a_taken_over_nvim_gets_no_tutor_and_is_not_asked() {
    let editor = Rc::new(FakeEditor::default());
    editor.attached.set(true);
    let mut tutor = installer(&editor);
    editor.target.set(Some(3));
    let now = Instant::now();
    assert_eq!(tutor.editor_session(now), None);
    assert_eq!(tutor.editor_session(now), None, "said once, then nothing");
    assert_eq!(editor.calls(), 0);
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
        assert_eq!(opts, &install_opts(&dir, DEFAULT_PREFIX));
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
        for (rel, text) in super::files(DEFAULT_PREFIX) {
            let sent = files
                .iter()
                .find(|(k, _)| k.as_str() == Some(rel))
                .map(|(_, v)| v.as_str());
            assert_eq!(sent, Some(Some(text.as_str())), "{rel}");
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

/// The `files` map of an install call's options, as path -> text.
fn sent_files(opts: &Value) -> BTreeMap<String, String> {
    let files = opts
        .as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_str() == Some("files"))
        .and_then(|(_, v)| v.as_map())
        .expect("the lessons go with the call");
    files
        .iter()
        .map(|(k, v)| (k.as_str().unwrap().to_string(), v.as_str().unwrap().to_string()))
        .collect()
}

#[test]
fn the_installer_sends_the_stock_prefix_until_it_is_told_another() {
    let editor = Rc::new(FakeEditor::default());
    let mut tutor = installer(&editor);
    editor.target.set(Some(1));
    let now = Instant::now();
    tutor.editor_session(now);
    let first = sent_files(&editor.calls.borrow()[0].0);
    assert_eq!(first["tutor/en/eitri.tutor"], substitute(LESSONS_EN, "Ctrl+b"));
    editor.answer(Ok(done(&[])));
    tutor.poll(now);

    tutor.set_prefix("Ctrl+Space".to_string());
    editor.target.set(Some(2));
    tutor.editor_session(now);
    let second = sent_files(&editor.calls.borrow()[0].0);
    for (rel, text) in super::files("Ctrl+Space") {
        assert_eq!(second[rel], text, "{rel}");
    }
    assert!(second["tutor/zh/eitri.tutor"].contains("`Ctrl+Space ?`"));
    assert!(second["tutor/en/eitri.tutor.json"].contains("Ctrl+Space ?"));
    tutor.cleanup();
}

#[test]
fn an_early_answer_keeps_the_prefix_for_the_retry() {
    let editor = Rc::new(FakeEditor::default());
    editor.target.set(Some(1));
    let mut tutor = installer(&editor);
    tutor.set_prefix("Ctrl+a".to_string());
    let start = Instant::now();
    tutor.editor_session(start);
    editor.answer(Ok(early()));
    let again = tutor.poll(start).expect("asked again later");
    tutor.poll(again);
    let retry = sent_files(&editor.calls.borrow()[0].0);
    assert_eq!(retry["tutor/en/eitri.tutor"], substitute(LESSONS_EN, "Ctrl+a"));
    editor.answer(Ok(done(&[])));
    tutor.poll(again);
    tutor.cleanup();
}

/// What the reader does to an exercise line: types a space and the key after it, or, for the one editing a Lua
/// sample, changes its `b` into `a`.
fn completed(line: &str, want: &str) -> String {
    if line.contains("eitri.keymap.prefix") {
        return line.replace("C-b", "C-a");
    }
    let key = want
        .strip_prefix(line)
        .unwrap_or_else(|| panic!("{want:?} does not continue {line:?}"))
        .strip_prefix(' ')
        .unwrap_or_else(|| panic!("{want:?} is not {line:?}, a space and a key"));
    assert!(!key.is_empty(), "{want:?} adds no key to {line:?}");
    format!("{line} {key}")
}

#[test]
fn completing_each_exercise_produces_its_expect() {
    for prefix in PREFIXES {
        for (lang, lessons, checks) in lessons_for(prefix) {
            let lines: Vec<&str> = lessons.split('\n').collect();
            for (n, want) in expect_of(&checks) {
                let done = completed(lines[n - 1], &want);
                assert_eq!(done, want, "{lang} {prefix}: line {n}");
            }
        }
    }
}

#[test]
fn no_placeholder_survives() {
    assert!(LESSONS_EN.contains(prefix_text::PLACEHOLDER));
    assert!(LESSONS_ZH.contains(prefix_text::PLACEHOLDER));
    for prefix in PREFIXES {
        for (lang, lessons, checks) in lessons_for(prefix) {
            assert!(!lessons.contains(prefix_text::PLACEHOLDER), "{lang} {prefix}");
            assert!(!checks.contains(prefix_text::PLACEHOLDER), "{lang} {prefix}");
        }
    }
}

#[test]
fn every_json_stays_valid() {
    for prefix in PREFIXES {
        for (lang, _, checks) in lessons_for(prefix) {
            let json: serde_json::Value =
                serde_json::from_str(&checks).unwrap_or_else(|e| panic!("{lang} {prefix}: {e}"));
            let source = if lang == "en" { CHECKS_EN } else { CHECKS_ZH };
            let source: serde_json::Value = serde_json::from_str(source).unwrap();
            let lines = |v: &serde_json::Value| v["expect"].as_object().unwrap().keys().cloned().collect::<Vec<_>>();
            assert_eq!(
                lines(&json),
                lines(&source),
                "{lang} {prefix}: the check lines never move"
            );
            assert_eq!(json["translated-from"], source["translated-from"], "{lang} {prefix}");
            for want in json["expect"].as_object().unwrap().values() {
                let want = want.as_str().unwrap();
                assert!(
                    want.contains(prefix) || want.starts_with("eitri.keymap.prefix"),
                    "{lang}: {want:?} names the prefix {prefix}"
                );
            }
        }
    }
}

#[test]
fn substitution_touches_the_placeholder_and_nothing_else() {
    assert_eq!(
        substitute("a {prefix} b {prefix}, Ctrl+b {Prefix}", "X"),
        "a X b X, Ctrl+b {Prefix}"
    );
    assert_eq!(substitute("no key here", "Ctrl+a"), "no key here");
    // The Lua spelling the exercise edits is not the placeholder, and a not-JSON text is left alone.
    let checks = r#"{"expect":{"5":"x: {prefix} ?","9":"eitri.keymap.prefix(\"C-a\")"},"other":"{prefix}"}"#;
    let out: serde_json::Value = serde_json::from_str(&substitute_checks(checks, "Alt+\"")).unwrap();
    assert_eq!(out["expect"]["5"], "x: Alt+\" ?");
    assert_eq!(out["expect"]["9"], "eitri.keymap.prefix(\"C-a\")");
    assert_eq!(out["other"], "{prefix}");
    assert_eq!(substitute_checks("not json {prefix}", "X"), "not json {prefix}");
}

/// The lessons of today (before the placeholders), a word to a line.
const STOCK_WORDS_EN: &str = include_str!("lessons/en/stock-words.txt");
const STOCK_WORDS_ZH: &str = include_str!("lessons/zh/stock-words.txt");

/// The passages the conversion rewrote on purpose, as (what the stock lessons said, what they say now), each
/// with whitespace normalised (the Chinese lessons without any, since a wrap there falls between characters).
const REWRITTEN_EN: &[(&str, &str)] = &[
    (
        "The keys are written for the default setup, with `Ctrl+b` as the prefix. If Eitri took another prefix \
         from your tmux config or your init.lua, press yours wherever this tutorial says Ctrl+b. Lesson 7 says \
         where those come from. If your tmux config also rebinds keys after the prefix, `Ctrl+b ?` shows what \
         they do here.",
        "The keys are written with `Ctrl+b` as the prefix, the one Eitri has now (the default prefix is \
         `Ctrl+b`). Lesson 7 says where another one comes from. This tutorial teaches the stock keys after the \
         prefix: if your tmux config rebinds some of them, `Ctrl+b ?` shows what they do here.",
    ),
    ("AND CTRL+B TWICE", "AND THE PREFIX TWICE"),
    (
        "to send Ctrl+b to the program: in a shell it moves the cursor back one character. Claude Code",
        "to send Ctrl+b to the program. For Ctrl+b, a shell moves the cursor back one character, and Claude Code",
    ),
    (
        "nvim gets Ctrl+b and goes a page back to this lesson.",
        "nvim gets Ctrl+b; for Ctrl+b that goes a page back to this lesson.",
    ),
    (
        "The line below sets the prefix to Ctrl+b.",
        "The sample line below sets a prefix to Ctrl+b.",
    ),
];
const REWRITTEN_ZH: &[(&str, &str)] = &[
    (
        "按键按默认设置来写，前缀是`Ctrl+b`。如果Eitri从你的tmux配置或init.lua里拿到了别的前缀，本教程写Ctrl+b的地方就\
         按你自己的。第7课讲它们从哪里来。如果你的tmux配置还改了前缀之后的键，`Ctrl+b?`会显示它们在这里做什么。",
        "按键按你现在的前缀`Ctrl+b`来写（默认前缀是`Ctrl+b`）。别的前缀从哪里来，第7课会讲。本教程讲的是前缀之后的默认键：\
         如果你的tmux配置改了其中一些，`Ctrl+b?`会显示它们在这里做什么。",
    ),
    ("以及连按两次CTRL+B", "以及连按两次前缀"),
    (
        "发给程序：在shell里，它让光标后退一个字符。在这个终端里",
        "发给程序。对Ctrl+b来说，shell里它让光标后退一个字符，在这个终端里",
    ),
    ("nvim收到Ctrl+b，翻回这一课。", "nvim收到Ctrl+b；对Ctrl+b来说，它翻回这一课。"),
    ("下面这行把前缀设成Ctrl+b。", "下面这行示例把一个前缀设成Ctrl+b。"),
];

fn normalised(lang: &str, text: &str) -> String {
    if lang == "en" {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        text.split_whitespace().collect()
    }
}

#[test]
fn the_default_prefix_changes_no_word() {
    let [(_, en, _), (_, zh, _)] = lessons();
    for (lang, now, stock, rewritten) in [
        ("en", en, STOCK_WORDS_EN, REWRITTEN_EN),
        ("zh", zh, STOCK_WORDS_ZH, REWRITTEN_ZH),
    ] {
        let mut want = normalised(lang, stock);
        for (old, new) in rewritten {
            assert_eq!(
                want.matches(old).count(),
                1,
                "{lang}: the stock lessons hold {old:?} once"
            );
            want = want.replace(old, new);
        }
        let now = normalised(lang, &now);
        if want != now {
            let at = want.chars().zip(now.chars()).take_while(|(a, b)| a == b).count();
            let around = |s: &str| s.chars().skip(at.saturating_sub(30)).take(80).collect::<String>();
            panic!(
                "{lang}: the lessons differ from today's at char {at}: wanted {:?}, got {:?}",
                around(&want),
                around(&now)
            );
        }
    }
}

/// The fenced sample of lesson 7.
fn sample_block(lessons: &str) -> String {
    let start = lessons.find("~~~ lua").expect("a fenced sample");
    let end = lessons[start..].find("\n~~~\n").expect("its end") + start;
    lessons[start..end].to_string()
}

#[test]
fn the_literal_sample_block_keeps_ctrl_b() {
    for (lang, source) in [("en", LESSONS_EN), ("zh", LESSONS_ZH)] {
        let block = sample_block(source);
        assert!(!block.contains(prefix_text::PLACEHOLDER), "{lang}");
        assert!(block.contains("k.del(\"prefix\", \"C-b\")"), "{lang}");
        for prefix in PREFIXES {
            assert_eq!(sample_block(&substitute(source, prefix)), block, "{lang} {prefix}");
        }
    }
    let en = sample_block(LESSONS_EN);
    assert!(en.contains("-- Ctrl+b after the prefix sent Ctrl+b"), "{en}");
}

#[test]
fn lesson_262_stays_a_lua_edit() {
    for prefix in PREFIXES {
        for (lang, lessons, checks) in lessons_for(prefix) {
            let expect = expect_of(&checks);
            let (n, want) = expect.iter().next_back().expect("the last exercise");
            assert_eq!(want, "eitri.keymap.prefix(\"C-a\")", "{lang} {prefix}");
            assert_eq!(
                lessons.split('\n').nth(n - 1),
                Some("eitri.keymap.prefix(\"C-b\")"),
                "{lang} {prefix}"
            );
        }
    }
    assert!(normalised("en", LESSONS_EN).contains("The sample line below sets a prefix to Ctrl+b. Change it to Ctrl+a"));
    assert!(normalised("zh", LESSONS_ZH).contains("下面这行示例把一个前缀设成Ctrl+b。把它改成Ctrl+a"));
}

#[test]
fn the_first_lesson_says_it_teaches_the_stock_keys() {
    let section = |text: &str, from: &str, to: &str| {
        let start = text.find(from).expect(from);
        let end = text[start..].find(to).expect(to) + start;
        text[start..end].to_string()
    };
    let en = section(LESSONS_EN, "# Lesson 0", "# Lesson 1");
    assert!(normalised("en", &en).contains("This tutorial teaches the stock keys after the prefix"));
    let zh = section(LESSONS_ZH, "# 第 0 课", "# 第 1 课");
    assert!(normalised("zh", &zh).contains("本教程讲的是前缀之后的默认键"));
}
