//! Two processes, as two windows of one project, writing the prompt history and the permission rules at
//! once: every prompt either one appended and every rule either one was told it saved is in the file
//! afterwards. The children are this test binary run again with one test selected.

use std::path::Path;
use std::process::{Command, Stdio};

use eitri_core::{permission_store, prompt_history};

const CHILD: &str = "EITRI_TEST_STORE_WRITES_CHILD";
const PROMPTS_PER_CHILD: usize = 200;
const RULES_PER_CHILD: usize = 40;

fn project() -> &'static Path {
    Path::new("/projects/two-windows")
}

fn prompt(tag: &str, i: usize) -> String {
    format!("prompt {i} from window {tag}")
}

fn rule(tag: &str, i: usize) -> String {
    format!("Bash(tool-{tag}-{i} *)")
}

/// How many times a child repeats a write the other one's lock refused before it fails the test. Only a
/// window that never lets go runs out of them: each history write already tries for about two seconds,
/// and each rule save four times.
const REPEATS: usize = 10_000;

/// `write` again while the other window's lock refuses it, at most [`REPEATS`] times.
fn repeated<T>(what: &str, mut write: impl FnMut() -> std::io::Result<T>) {
    for _ in 0..REPEATS {
        match write() {
            Ok(_) => return,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("{what}: {e}"),
        }
    }
    panic!("{what}: refused {REPEATS} times by the other window's lock");
}

/// One window's writes, prompts and rules interleaved. A save refused because the other window held the
/// lock for every quick try is the card left up; the user presses it again, so this does too.
fn write_as(root: &Path, tag: &str) {
    let history = root.join("eitri/history");
    let rules = root.join("eitri/permissions");
    for i in 0..PROMPTS_PER_CHILD {
        repeated("a prompt", || {
            prompt_history::append(&history, project(), &[prompt(tag, i)])
        });
        if i % (PROMPTS_PER_CHILD / RULES_PER_CHILD) == 0 {
            let r = agent::PrefixRule::parse(&rule(tag, i / (PROMPTS_PER_CHILD / RULES_PER_CHILD))).unwrap();
            repeated("a rule", || permission_store::add(&rules, project(), &r));
        }
    }
}

#[test]
fn two_windows_writing_at_once_lose_no_prompt_and_no_saved_rule() {
    if let Ok(spec) = std::env::var(CHILD) {
        let (root, tag) = spec.split_once('|').expect("<root>|<tag>");
        write_as(Path::new(root), tag);
        return;
    }
    let root = std::env::temp_dir().join(format!("eitri-store-2proc-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(root.join("eitri")).unwrap();
    // Each child's output goes to a file beside the stores. Every write either child makes is tried a
    // bounded number of times (`REPEATS` rounds of bounded tries), so both end whatever the other does.
    let spawn = |tag: &str| {
        let out = std::fs::File::create(root.join(format!("{tag}.out"))).unwrap();
        Command::new(std::env::current_exe().unwrap())
            .args([
                "two_windows_writing_at_once_lose_no_prompt_and_no_saved_rule",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, format!("{}|{tag}", root.display()))
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .stdin(Stdio::null())
            .spawn()
            .unwrap()
    };
    let children = [("a", spawn("a")), ("b", spawn("b"))];
    for (tag, mut child) in children {
        let status = child.wait().unwrap();
        let text = std::fs::read_to_string(root.join(format!("{tag}.out"))).unwrap();
        assert!(
            status.success() && text.contains("1 passed"),
            "{tag}: {status:?}\n{text}"
        );
    }

    let prompt_history::Loaded::Entries(entries) = prompt_history::load(&root.join("eitri/history"), project()) else {
        panic!("the history is not usable");
    };
    let permission_store::LoadedRules::Rules(rules) =
        permission_store::load(&root.join("eitri/permissions"), project())
    else {
        panic!("the rules file is not usable");
    };
    let saved: Vec<String> = rules.rules().iter().map(|r| r.to_rule_string()).collect();
    for tag in ["a", "b"] {
        let mine: Vec<String> = entries
            .iter()
            .filter(|e| e.ends_with(&format!("window {tag}")))
            .cloned()
            .collect();
        let expected: Vec<String> = (0..PROMPTS_PER_CHILD).map(|i| prompt(tag, i)).collect();
        let lost: Vec<&String> = expected.iter().filter(|p| !mine.contains(p)).collect();
        assert!(
            lost.is_empty(),
            "window {tag} lost {} of its {PROMPTS_PER_CHILD} prompts ({} entries in all), the first: {:?}",
            lost.len(),
            entries.len(),
            lost.iter().take(5).collect::<Vec<_>>()
        );
        assert_eq!(mine, expected, "window {tag}'s prompts, in its own order");
        let lost_rules: Vec<String> = (0..RULES_PER_CHILD)
            .map(|i| rule(tag, i))
            .filter(|r| !saved.contains(r))
            .collect();
        assert!(lost_rules.is_empty(), "window {tag} lost saved rules: {lost_rules:?}");
    }
    assert_eq!(entries.len(), 2 * PROMPTS_PER_CHILD);
    assert_eq!(saved.len(), 2 * RULES_PER_CHILD);
    let _ = std::fs::remove_dir_all(&root);
}
