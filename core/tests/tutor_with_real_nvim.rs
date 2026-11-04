//! `:EitriTutor` in a real `nvim --clean --embed --headless`: the install snippet defines the command only after
//! VimEnter and never over one the user has; the command writes the lessons that are missing from their directory
//! (all of them the first time, any deleted since later) once it has checked the directory is the user's alone,
//! has the directory on 'runtimepath' only while the lessons are looked up (a comma in its path included), and
//! opens them in the language nvim's tutor picks, with their checks working, also when the config skips nvim's
//! tutor plugin.
//!
//! Every case is `#[ignore]`d: it needs `nvim` (>= 0.11) on `PATH`, spends no tokens and opens no window.

#[path = "support/embed_client.rs"]
mod embed_client;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use eitri_core::tutor::prefix_text::substitute_checks;
use eitri_core::tutor::{files, install_opts, TutorDir, CHECKS_EN, COMMAND, DEFAULT_PREFIX, FILES, INSTALL_TUTOR_LUA};
use embed_client::Embed;
use rmpv::Value;

struct Case {
    dir: PathBuf,
    lessons: TutorDir,
    nvim: Embed,
}

impl Drop for Case {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const EN: &[(&str, &str)] = &[("LC_ALL", "en_US.UTF-8"), ("LANG", "en_US.UTF-8")];
const ZH: &[(&str, &str)] = &[("LC_ALL", "zh_CN.UTF-8"), ("LANG", "zh_CN.UTF-8")];

fn case(name: &str, args: &[&str], env: &[(&str, &str)]) -> Case {
    case_under_umask(name, args, env, None)
}

fn case_under_umask(name: &str, args: &[&str], env: &[(&str, &str)], umask: Option<u32>) -> Case {
    let dir = std::env::temp_dir().join(format!("tut-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = std::fs::canonicalize(&dir).unwrap();
    let lessons = TutorDir::in_dir(&dir).expect("the lessons are written");
    let nvim = Embed::start_under_umask(&dir, args, env, umask);
    Case { dir, lessons, nvim }
}

fn started(name: &str, args: &[&str], env: &[(&str, &str)]) -> Case {
    let mut c = case(name, args, env);
    c.nvim
        .wait_until("VimEnter", |n| n.eval("v:vim_did_enter").as_i64() == Some(1));
    c
}

fn install(c: &mut Case) -> Value {
    let opts = install_opts(c.lessons.path(), DEFAULT_PREFIX);
    c.nvim.lua(INSTALL_TUTOR_LUA, vec![opts])
}

/// How many entries of 'runtimepath' are the lessons' directory.
fn on_rtp(c: &mut Case) -> i64 {
    let dir = Value::from(c.lessons.path().to_str().unwrap());
    c.nvim
        .lua(
            "local dir = ... local n = 0 for _, p in ipairs(vim.api.nvim_list_runtime_paths()) do if p == dir then n = n + 1 end end return n",
            vec![dir],
        )
        .as_i64()
        .unwrap()
}

/// Runs the command the way a reader does; whether it raised an error is not the test's business here.
fn run_command(c: &mut Case) {
    c.nvim.lua("pcall(vim.cmd, ...)", vec![Value::from(COMMAND)]);
}

/// Every lesson file holds its text, every directory down to them is exactly 0700, every file is the owner's alone.
fn assert_written(c: &Case) {
    let mode = |p: &std::path::Path| std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777;
    for sub in ["", "tutor", "tutor/en", "tutor/zh"] {
        assert_eq!(mode(&c.lessons.path().join(sub)), 0o700, "{sub}/");
    }
    for (rel, text) in files(DEFAULT_PREFIX) {
        let path = c.lessons.path().join(rel);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "{rel}");
        assert_eq!(mode(&path) & 0o077, 0, "{rel}");
    }
}

fn field<'a>(answer: &'a Value, key: &str) -> &'a Value {
    answer
        .as_map()
        .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some(key)))
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("no {key} in {answer:?}"))
}

/// The names in the answer's `skipped`, which an empty Lua table may send as an empty map.
fn skipped(answer: &Value) -> Vec<String> {
    field(answer, "skipped")
        .as_array()
        .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

fn string(v: Value) -> String {
    v.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| panic!("not text: {v:?}"))
}

fn current_file(c: &mut Case) -> String {
    string(c.nvim.eval("expand('%:p')"))
}

fn lesson_file(c: &Case, lang: &str) -> String {
    c.lessons
        .path()
        .join("tutor")
        .join(lang)
        .join("eitri.tutor")
        .to_str()
        .unwrap()
        .to_string()
}

/// `(line, sign)` for each check nvim's tutor drew, or `None` where this nvim draws them some other way.
const MARKS_LUA: &str = r#"
local ns = vim.api.nvim_get_namespaces()['nvim.tutor.mark']
if ns == nil then return vim.NIL end
local out = {}
for _, m in ipairs(vim.api.nvim_buf_get_extmarks(0, ns, 0, -1, { details = true })) do
  out[#out + 1] = { m[2] + 1, vim.trim(m[4].sign_text or '') }
end
return out
"#;

fn marks(c: &mut Case) -> Option<Vec<(u64, String)>> {
    let v = c.nvim.lua(MARKS_LUA, vec![]);
    if v.is_nil() {
        return None;
    }
    Some(
        v.as_array()
            .unwrap()
            .iter()
            .map(|m| {
                let m = m.as_array().unwrap();
                (m[0].as_u64().unwrap(), m[1].as_str().unwrap().to_string())
            })
            .collect(),
    )
}

/// Types the rest of line `n` the way a reader would: the text past what it shares with `want` is replaced, never
/// the whole line, so the check's mark at its start stays.
const FIX_LUA: &str = r#"
local n, want = ...
local have = vim.api.nvim_buf_get_lines(0, n - 1, n, true)[1]
local same = 0
while same < #have and same < #want and have:byte(same + 1) == want:byte(same + 1) do same = same + 1 end
vim.api.nvim_win_set_cursor(0, { n, same })
vim.api.nvim_buf_set_text(0, n - 1, same, n - 1, #have, { want:sub(same + 1) })
vim.cmd('doautocmd <nomodeline> TextChanged')
"#;

fn expect_en() -> Vec<(u64, String)> {
    let json: serde_json::Value = serde_json::from_str(&substitute_checks(CHECKS_EN, DEFAULT_PREFIX)).unwrap();
    let mut out: Vec<(u64, String)> = json["expect"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.parse().unwrap(), v.as_str().unwrap().to_string()))
        .collect();
    out.sort();
    out
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn eitri_tutor_opens_the_english_lessons_and_their_checks_work() {
    let mut c = started("en", &[], EN);
    let answer = install(&mut c);
    assert_eq!(field(&answer, "ok"), &Value::from(true));
    assert!(skipped(&answer).is_empty(), "{answer:?}");
    assert_eq!(c.nvim.eval("exists(':EitriTutor')"), Value::from(2));
    assert_eq!(
        std::fs::read_dir(c.lessons.path()).unwrap().count(),
        0,
        "nothing is written before the command runs"
    );
    assert_eq!(on_rtp(&mut c), 0, "nor put on 'runtimepath'");

    c.nvim.command(COMMAND);
    assert_written(&c);
    assert_eq!(current_file(&mut c), lesson_file(&c, "en"));
    assert_eq!(string(c.nvim.eval("&filetype")), "tutor");
    assert_eq!(
        string(c.nvim.eval("&buftype")),
        "nowrite",
        "the lessons can never be written over"
    );
    assert_eq!(string(c.nvim.eval("getline(1)")), "# Eitri's keys");
    let checks = c.nvim.eval("len(b:tutor_metadata.expect)");
    assert_eq!(
        checks,
        Value::from(expect_en().len() as u64),
        "the .tutor.json was read"
    );

    // nvim 0.11 moved the checks to Lua extmarks; an older nvim draws signs of its own, not read here.
    let lua_checks = c.nvim.lua("return (pcall(require, 'nvim.tutor'))", vec![]);
    let drawn = marks(&mut c);
    assert_eq!(drawn.is_some(), lua_checks == Value::from(true));
    if let Some(drawn) = drawn {
        let want = expect_en();
        let lines: Vec<u64> = drawn.iter().map(|(n, _)| *n).collect();
        assert_eq!(lines, want.iter().map(|(n, _)| *n).collect::<Vec<_>>());
        assert!(
            drawn.iter().all(|(_, s)| s == "✗"),
            "every exercise starts undone: {drawn:?}"
        );
        for (n, text) in &want {
            c.nvim.lua(FIX_LUA, vec![Value::from(*n), Value::from(text.as_str())]);
        }
        let drawn = marks(&mut c).unwrap();
        assert!(
            drawn.iter().all(|(_, s)| s == "✓"),
            "every answer is accepted: {drawn:?}"
        );
    }

    // The directory was on 'runtimepath' only while the lessons were looked up.
    assert_eq!(on_rtp(&mut c), 0, "taken off 'runtimepath' again");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_chinese_locale_gets_the_chinese_lessons() {
    let mut c = started("zh", &[], ZH);
    install(&mut c);
    c.nvim.command(COMMAND);
    assert_eq!(current_file(&mut c), lesson_file(&c, "zh"));
    assert_eq!(string(c.nvim.eval("getline(1)")), "# Eitri 的按键");
    assert_eq!(
        c.nvim.eval("len(b:tutor_metadata.expect)"),
        Value::from(expect_en().len() as u64)
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_command_works_when_the_config_skips_the_tutor_plugin() {
    // LazyVim, among others, turns nvim's tutor plugin off, which leaves no `:Tutor`.
    let mut c = started("noplugin", &["--cmd", "let g:loaded_tutor_mode_plugin = 1"], EN);
    assert_eq!(c.nvim.eval("exists(':Tutor')"), Value::from(0));
    install(&mut c);
    c.nvim.command(COMMAND);
    assert_eq!(current_file(&mut c), lesson_file(&c, "en"));
    assert_eq!(string(c.nvim.eval("&filetype")), "tutor");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_command_of_that_name_the_user_has_is_left_alone() {
    let mut c = started("mine", &[], EN);
    c.nvim.command("command! EitriTutor let g:mine = 1");
    let answer = install(&mut c);
    assert_eq!(field(&answer, "ok"), &Value::from(true));
    assert_eq!(skipped(&answer), [COMMAND]);
    c.nvim.command(COMMAND);
    assert_eq!(c.nvim.eval("g:mine"), Value::from(1), "the user's command still runs");
    assert_eq!(on_rtp(&mut c), 0);
    assert_eq!(
        std::fs::read_dir(c.lessons.path()).unwrap().count(),
        0,
        "nothing written"
    );

    let mut c = started("mine-buffer", &[], EN);
    c.nvim.command("command! -buffer EitriTutor let b:mine = 1");
    let answer = install(&mut c);
    assert_eq!(skipped(&answer), [COMMAND]);
    assert_eq!(c.nvim.eval("exists(':EitriTutor')"), Value::from(2));
    c.nvim.command(COMMAND);
    assert_eq!(c.nvim.eval("b:mine"), Value::from(1));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn installing_again_keeps_one_command_and_runs_leave_runtimepath_as_it_was() {
    let mut c = started("again", &[], EN);
    install(&mut c);
    let again = install(&mut c);
    assert!(
        skipped(&again).is_empty(),
        "its own command is not the user's: {again:?}"
    );
    let before = string(c.nvim.eval("&runtimepath"));
    for _ in 0..2 {
        c.nvim.command(COMMAND);
        assert_eq!(current_file(&mut c), lesson_file(&c, "en"));
        c.nvim.command("enew!");
    }
    assert_eq!(string(c.nvim.eval("&runtimepath")), before);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn files_deleted_after_install_are_written_again_when_the_command_runs() {
    let mut c = started("rewrite", &[], EN);
    install(&mut c);
    c.nvim.command(COMMAND);
    assert_written(&c);
    // The tutor's buffer goes, so the next run reads the file again rather than going back to the loaded buffer.
    c.nvim.command("silent! %bwipeout!");

    // One file aged out of a temp directory.
    std::fs::remove_file(c.lessons.path().join("tutor/en/eitri.tutor")).unwrap();
    c.nvim.command(COMMAND);
    assert_eq!(current_file(&mut c), lesson_file(&c, "en"));
    assert_eq!(string(c.nvim.eval("getline(1)")), "# Eitri's keys");
    assert_written(&c);
    c.nvim.command("silent! %bwipeout!");

    // Everything inside it, and then the directory itself.
    std::fs::remove_dir_all(c.lessons.path().join("tutor")).unwrap();
    c.nvim.command(COMMAND);
    assert_written(&c);
    c.nvim.command("silent! %bwipeout!");
    std::fs::remove_dir_all(c.lessons.path()).unwrap();
    c.nvim.command(COMMAND);
    assert_eq!(current_file(&mut c), lesson_file(&c, "en"));
    assert_written(&c);
    assert_eq!(on_rtp(&mut c), 0);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_lesson_file_cut_short_or_open_to_others_is_replaced_and_a_stale_part_file_goes() {
    let mut c = started("cut", &[], EN);
    install(&mut c);
    c.nvim.command(COMMAND);
    assert_written(&c);
    c.nvim.command("silent! %bwipeout!");

    let en = c.lessons.path().join("tutor/en/eitri.tutor");
    let zh_checks = c.lessons.path().join("tutor/zh/eitri.tutor.json");
    let text = std::fs::read_to_string(&en).unwrap();
    // What a write cut short leaves behind, a stale half of the next one beside it, and a copy others can read.
    std::fs::write(&en, &text[..text.len() / 2]).unwrap();
    std::fs::write(en.with_extension("tutor.part"), "half").unwrap();
    std::fs::set_permissions(&zh_checks, std::fs::Permissions::from_mode(0o644)).unwrap();

    c.nvim.command(COMMAND);
    assert_eq!(current_file(&mut c), lesson_file(&c, "en"));
    assert_written(&c);
    assert!(
        !en.with_extension("tutor.part").exists(),
        "the stale .part file is gone"
    );
    assert_eq!(on_rtp(&mut c), 0);
    c.nvim.command("silent! %bwipeout!");

    // A failed write leaves no lesson file and no .part file: the .part name is taken by a directory, which
    // nothing can clear or create over.
    std::fs::remove_file(&en).unwrap();
    std::fs::create_dir(en.with_extension("tutor.part")).unwrap();
    run_command(&mut c);
    assert!(!en.exists(), "no file where the write failed");
    let said = string(c.nvim.eval("execute('messages')"));
    assert!(said.contains("could not write"), "the command says so: {said:?}");
    assert_eq!(on_rtp(&mut c), 0);
    std::fs::remove_dir(en.with_extension("tutor.part")).unwrap();
    c.nvim.command(COMMAND);
    assert_written(&c);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn under_umask_0277_the_directories_are_exactly_0700_and_the_files_the_owners_alone() {
    let mut c = case_under_umask("narrow", &[], EN, Some(0o277));
    c.nvim
        .wait_until("VimEnter", |n| n.eval("v:vim_did_enter").as_i64() == Some(1));
    install(&mut c);
    for _ in 0..2 {
        c.nvim.command(COMMAND);
        assert_eq!(current_file(&mut c), lesson_file(&c, "en"));
        c.nvim.command("silent! %bwipeout!");
    }
    // The directories exactly 0700 and no file with a group or other bit, as `assert_written` holds.
    assert_written(&c);
    for (rel, _) in FILES {
        let mode = std::fs::symlink_metadata(c.lessons.path().join(rel))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o477,
            0o400,
            "{rel}: the owner can read it and nobody else can do anything"
        );
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_directory_others_can_enter_or_a_link_in_its_place_is_not_used() {
    let mut c = started("loose", &[], EN);
    install(&mut c);
    let before = string(c.nvim.eval("&runtimepath"));

    std::fs::set_permissions(c.lessons.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    run_command(&mut c);
    assert_ne!(current_file(&mut c), lesson_file(&c, "en"));
    let said = string(c.nvim.eval("execute('messages')"));
    assert!(
        said.contains("is not a directory of this user closed to everyone else"),
        "the command says why: {said:?}"
    );
    assert_eq!(
        std::fs::read_dir(c.lessons.path()).unwrap().count(),
        0,
        "nothing written"
    );
    assert_eq!(string(c.nvim.eval("&runtimepath")), before, "nor put on 'runtimepath'");

    // A link where the directory was, to a directory of the user's that is private.
    std::fs::remove_dir(c.lessons.path()).unwrap();
    let elsewhere = c.dir.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(&elsewhere, c.lessons.path()).unwrap();
    run_command(&mut c);
    assert_ne!(current_file(&mut c), lesson_file(&c, "en"));
    assert_eq!(
        std::fs::read_dir(&elsewhere).unwrap().count(),
        0,
        "nothing written through the link"
    );
    assert_eq!(string(c.nvim.eval("&runtimepath")), before);
    std::fs::remove_file(c.lessons.path()).unwrap();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_comma_in_the_directory_is_escaped_on_runtimepath() {
    let mut c = started("a,b", &[], EN);
    assert!(c.lessons.path().to_str().unwrap().contains(','));
    install(&mut c);
    let before = string(c.nvim.eval("&runtimepath"));
    c.nvim.command(COMMAND);
    assert_eq!(current_file(&mut c), lesson_file(&c, "en"));
    assert_eq!(
        string(c.nvim.eval("&runtimepath")),
        before,
        "the escaped entry came off again"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn install_before_vimenter_defines_nothing_and_says_early() {
    // Startup is held in `--cmd`, before a user's config would run; nvim still answers requests inside vim.wait.
    let mut c = case(
        "early",
        &[
            "--cmd",
            "lua vim.g.hold = true",
            "--cmd",
            "lua vim.wait(10000, function() return not vim.g.hold end, 10)",
        ],
        EN,
    );
    assert_eq!(c.nvim.eval("v:vim_did_enter"), Value::from(0));
    let early = install(&mut c);
    assert_eq!(field(&early, "ok"), &Value::from(false));
    assert_eq!(field(&early, "early"), &Value::from(true));
    assert_eq!(c.nvim.eval("exists(':EitriTutor')"), Value::from(0));
    assert_eq!(on_rtp(&mut c), 0, "nothing changes before VimEnter");
    c.nvim.lua("vim.g.hold = false", vec![]);
    c.nvim
        .wait_until("VimEnter", |n| n.eval("v:vim_did_enter").as_i64() == Some(1));
    let done = install(&mut c);
    assert_eq!(field(&done, "ok"), &Value::from(true));
    assert_eq!(c.nvim.eval("exists(':EitriTutor')"), Value::from(2));
}
