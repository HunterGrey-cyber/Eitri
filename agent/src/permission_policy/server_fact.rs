//! The gate split at the seam between the disk and the person.
//!
//! The classifier answers from two kinds of input. Some comes from the disk: whether the project root is a boundary,
//! where each path in the call resolves, the repository git would use, Eitri's own places. The rest comes from the
//! person: the tab's mode and the prefix rules they saved. When the agent runs on another machine, the disk is that
//! machine's and the person is at this one. So the classifier comes in two halves, which together answer exactly
//! what the local classifier answers:
//!
//! - [`gate_fact`], the disk half, runs where the files are. It is the local classifier with no saved rules and no
//!   mode, given its home directory, identity, the agent's `GIT_*` flag and Eitri's places as values, and it reads
//!   nothing of the environment of the process it runs in.
//! - [`classify_with_fact`] and [`rule_that_allows_with_fact`], the policy half, run where the person is. They take
//!   the disk half's answer as an input and read no file, identity or environment: Manual's edit card, a saved rule,
//!   then the disk's answer, in the order the local classifier asks them. What needs no disk they decide themselves:
//!   a tool decided by its name or an input that is not an object is answered here whatever the fact says, and the
//!   places' card is joined with the classification here as the local classifier joins them. A fact the disk half
//!   could not have given (a reason only the policy half or a decision by name gives, a verdict its reason never
//!   comes with, an allow or a reason a rule replaces at a root that is no boundary, a places card with a reason the
//!   places' check never gives) is a card, and a places card is a card whatever verdict it arrived with: a fact can
//!   make the answer stricter than the local classifier's, never looser.
//!
//! [`request_digest`] names the request a fact is about, so a fact is never applied to another request.
//! [`known_reason`] maps a reason received as text back onto this build's own fixed label.

use super::{
    allow, ask, classify_by_name, classify_with_place_card, eitri_places_card, place_card_first, Classification,
    EitriPlaces, Surroundings, ALLOW_REASONS, ALL_REASONS, EDIT_TOOLS, MAX_WORK_PER_CLASSIFICATION,
    REASON_ALLOWED_BY_A_PROJECT_RULE, REASON_CLI_ALWAYS_ASKS, REASON_EITRI_OWN_PLACES, REASON_EITRI_PLACE_UNKNOWN,
    REASON_INPUT_NOT_AN_OBJECT, REASON_MANUAL_ASKS_BEFORE_EVERY_EDIT, REASON_TOOL_CHANGES_NOTHING,
    REASON_TOOL_NOT_IN_THE_TABLE, REPLACEABLE_BY_A_RULE,
};
use crate::permission_rules::PrefixRules;
use crate::project_boundary::{checked_root, Identity};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Why a request is a card when the server's path check gave a reason this build does not know.
pub const REASON_SERVER_FACT_UNKNOWN: &str = "the server's path check gave a reason this Eitri does not know";
/// Why a request is a card when the server's path check gave no answer for it.
pub const REASON_SERVER_FACT_MISSING: &str = "the server's path check has not answered";
/// Why a request is a card when the server's path check answered differently right before an automatic answer.
pub const REASON_SERVER_FACT_CHANGED: &str = "the server's path check changed its answer before Eitri could answer";
/// Why a request is a card when the server's path check gave an answer the classifier never gives: a reason only
/// the policy half gives, a verdict its reason never comes with, or an allow, or a reason a saved rule replaces, at a
/// root it says is no boundary.
pub const REASON_SERVER_FACT_IMPOSSIBLE: &str = "the server's path check gave an answer it could not have given";

/// What the disk gives about one gate request where its files are: the disk half's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GateFact {
    /// The project root is a boundary ([`crate::project_boundary`]), with the home directory and identity given.
    pub root_is_boundary: bool,
    /// The places' check alone ([`eitri_places_card`]).
    pub places_card: Option<Classification>,
    /// The classifier with no saved rules and no mode, the places' check included.
    pub classification: Classification,
}

/// What, beyond the call and the root, the disk half judges against, as values: the home directory, who the root
/// must be a boundary against, and whether the agent's environment names any of
/// [`GIT_LOCATION_VARIABLES`](super::GIT_LOCATION_VARIABLES).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactSurroundings {
    pub home: Option<PathBuf>,
    pub identity: Identity,
    pub git_location_from_environment: bool,
}

/// The disk half, run where the files are: the root's boundary, the places' check and the classification. The
/// places' check and the classification each run in a work budget of their own, as they do locally; the root's
/// check resolves the root once and looks at it with `lstat`. Reads the disk under `root` and the places, and
/// nothing of this process's environment.
pub fn gate_fact(
    tool_name: &str,
    input: &Value,
    root: &Path,
    around: &FactSurroundings,
    places: &EitriPlaces,
) -> GateFact {
    let places_card = eitri_places_card(tool_name, input, root, places);
    GateFact {
        root_is_boundary: checked_root(root, around.home.as_deref(), &around.identity).is_ok(),
        places_card,
        classification: classify_with_place_card(tool_name, input, root, places_card, &around.as_surroundings()),
    }
}

impl FactSurroundings {
    /// The classifier's own form, with the whole work budget.
    fn as_surroundings(&self) -> Surroundings<'_> {
        Surroundings {
            home: self.home.as_deref(),
            who: &self.identity,
            git_location_from_environment: self.git_location_from_environment,
            work_budget: MAX_WORK_PER_CLASSIFICATION,
        }
    }
}

/// The policy half, run where the person is: the answer the local classifier gives a Manual tab (`manual`) or an
/// Auto tab with these `rules`, when the disk gave `fact`. Reads no file, identity or environment.
pub fn classify_with_fact(
    tool_name: &str,
    input: &Value,
    fact: &GateFact,
    rules: &PrefixRules,
    manual: bool,
) -> Classification {
    if manual && EDIT_TOOLS.contains(&tool_name) {
        return ask(REASON_MANUAL_ASKS_BEFORE_EVERY_EDIT);
    }
    if rule_that_allows_with_fact(tool_name, input, fact, rules).is_some() {
        return allow(REASON_ALLOWED_BY_A_PROJECT_RULE);
    }
    disk_answer(tool_name, input, fact)
}

/// The rule [`classify_with_fact`] answers with, in Claude Code's syntax, for the row's note: `Some` exactly when
/// its reason is [`REASON_ALLOWED_BY_A_PROJECT_RULE`]. A rule replaces only a reason in [`REPLACEABLE_BY_A_RULE`];
/// `matching_rule` matches only a call that carries nothing but its command. Reads no file, identity or environment.
pub fn rule_that_allows_with_fact(
    tool_name: &str,
    input: &Value,
    fact: &GateFact,
    rules: &PrefixRules,
) -> Option<String> {
    let rule = rules.matching_rule(tool_name, input)?;
    let disk = disk_answer(tool_name, input, fact);
    (disk.needs_a_human() && REPLACEABLE_BY_A_RULE.contains(&disk.reason)).then(|| rule.to_rule_string())
}

/// What the local classifier with no rules and no mode answers when the disk gave `fact`: the decision by name made
/// here, else the fact's classification as the disk half could have given it, joined with the places' card as the
/// local classifier joins them. A fact the disk half gives already holds that join, so joining again changes only
/// one it could not have given.
fn disk_answer(tool_name: &str, input: &Value, fact: &GateFact) -> Classification {
    // A places card says the call reaches a place, so it is a card whatever verdict it arrived with; one with a
    // reason the places' check never gives is a card no rule replaces.
    let place_card = fact.places_card.map(|card| match card.reason {
        REASON_EITRI_OWN_PLACES | REASON_EITRI_PLACE_UNKNOWN => ask(card.reason),
        _ => ask(REASON_SERVER_FACT_IMPOSSIBLE),
    });
    place_card_first(place_card, || {
        classify_by_name(tool_name, input).unwrap_or_else(|| judged_by_its_paths(fact))
    })
}

/// The reasons of the decisions made by the tool's name or the input's shape alone ([`classify_by_name`]): a call
/// judged by its paths never has one.
const DECIDED_BY_NAME: [&str; 4] = [
    REASON_INPUT_NOT_AN_OBJECT,
    REASON_TOOL_CHANGES_NOTHING,
    REASON_CLI_ALWAYS_ASKS,
    REASON_TOOL_NOT_IN_THE_TABLE,
];

/// The fact's classification of a call judged by its paths, or a card when the disk half could not have given it:
/// a reason only the policy half or a decision by name gives, a verdict the reason never comes with, or, at a root
/// that is no boundary, an allow or a reason a saved rule replaces (every path-judged call cards there, for a reason
/// no rule replaces).
fn judged_by_its_paths(fact: &GateFact) -> Classification {
    let classification = fact.classification;
    let carded = classification.needs_a_human();
    let fits_its_reason = carded != ALLOW_REASONS.contains(&classification.reason)
        && !POLICY_HALF_REASONS.contains(&classification.reason)
        && !DECIDED_BY_NAME.contains(&classification.reason);
    let fits_the_root = fact.root_is_boundary || (carded && !REPLACEABLE_BY_A_RULE.contains(&classification.reason));
    if fits_its_reason && fits_the_root {
        classification
    } else {
        ask(REASON_SERVER_FACT_IMPOSSIBLE)
    }
}

/// What a fact is about: SHA-256 over a fixed tag, then the session id, the permission id, the tool name, the
/// request's input exactly as its event carried it, and the project root, each preceded by its length (eight bytes,
/// little-endian) so no two different requests give the same bytes.
pub fn request_digest(
    session_id: &str,
    permission_id: &str,
    tool_name: &str,
    input_json: &[u8],
    root: &Path,
) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"eitri-gate-request\0");
    for part in [
        session_id.as_bytes(),
        permission_id.as_bytes(),
        tool_name.as_bytes(),
        input_json,
        root.as_os_str().as_bytes(),
    ] {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part);
    }
    hash.finalize().into()
}

/// The reasons the policy half adds; the disk half never gives one, so one received from it is not known.
const POLICY_HALF_REASONS: [&str; 2] = [REASON_ALLOWED_BY_A_PROJECT_RULE, REASON_MANUAL_ASKS_BEFORE_EVERY_EDIT];

/// This build's own label for a reason the disk half sent as text, or `None` for one it does not know. The two the
/// policy half adds (a saved rule's allow, Manual's edit card) are not known here: the disk half never gives them.
pub fn known_reason(text: &str) -> Option<&'static str> {
    if POLICY_HALF_REASONS.contains(&text) {
        return None;
    }
    ALL_REASONS.iter().copied().find(|reason| *reason == text)
}

#[cfg(test)]
mod tests {
    use super::super::{
        classify_in_manual, classify_in_manual_around, classify_with_rules, classify_with_rules_around,
        rule_that_allows, rule_that_allows_around, EitriPlaceInputs, PermissionVerdict, GIT_LOCATION_VARIABLES,
        REASON_NOT_ON_READ_ONLY_LIST, REASON_PATH_RESOLVES_INSIDE, REASON_PATH_RESOLVES_OUTSIDE,
        REASON_READ_ONLY_COMMAND,
    };
    use super::*;
    use crate::permission_rules::PrefixRule;
    use crate::project_boundary::NotABoundary;
    use crate::test_tmp::TmpDir;
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::os::unix::fs::{symlink, PermissionsExt};

    /// Every `(tool, input)` the classifier's own tests pass it, and the places' tests, with `{root}`, `{outside}`,
    /// `{home}` and `{tmp}` standing for the directories the scene makes. One JSON array per line.
    const CORPUS: &str = include_str!("server_fact_corpus.jsonl");

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn link(path: &Path, target: impl AsRef<Path>) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        symlink(target, path).unwrap();
    }

    /// What git's own discovery accepts as a repository: a `HEAD` naming a ref, `objects/` and `refs/`.
    fn git_dir(dir: &Path) {
        std::fs::create_dir_all(dir.join("objects")).unwrap();
        std::fs::create_dir_all(dir.join("refs/heads")).unwrap();
        write(&dir.join("HEAD"), "ref: refs/heads/main\n");
    }

    /// A root holding what the corpus names: links that stay, leave, loop, dangle or pass through `/proc`, hard
    /// links in and out, protected names, a directory nothing may search.
    fn rich_root(root: &Path, outside: &Path, with_git: bool) {
        std::fs::create_dir_all(root).unwrap();
        chmod(root, 0o700);
        if with_git {
            git_dir(&root.join(".git"));
        }
        for (name, contents) in [
            ("src/main.rs", "fn main() {}\n"),
            ("README.md", "readme\n"),
            ("a.txt", "a\n"),
            ("original.txt", "v1\n"),
            ("one.rs", "x\n"),
            ("config/npm-user.conf", "registry=x\n"),
            ("docs/page.md", "page\n"),
            ("docs/nb.ipynb", "{}\n"),
            ("page.md", "page\n"),
            ("nb.ipynb", "{}\n"),
            ("sub/deeper/page.md", "page\n"),
            ("real/target.rs", "x\n"),
            ("real/deep/file.rs", "x\n"),
            ("~/.ssh/known_hosts", "host\n"),
            ("my dir/page.md", "page\n"),
            ("a~b/page.md", "page\n"),
            ("-n", "hello\n"),
            ("paths", "/x/secret\0"),
            ("chain/f", "x\n"),
            ("editor-config/.bashrc", "x\n"),
        ] {
            write(&root.join(name), contents);
        }
        std::fs::create_dir_all(root.join("conf/eitri")).unwrap();
        std::fs::hard_link(root.join("original.txt"), root.join("linked.txt")).unwrap();
        std::fs::hard_link(root.join("one.rs"), root.join("two.rs")).unwrap();
        std::fs::hard_link(outside.join("secret"), root.join("hard-out.txt")).unwrap();
        for (name, target) in [
            ("escape", outside.to_path_buf()),
            ("out", outside.to_path_buf()),
            ("sub2", outside.to_path_buf()),
            ("escape.txt", outside.join("secret")),
            ("out_file", outside.join("secret")),
            ("a/f", outside.join("secret")),
            (".npmrc", root.join("config/npm-user.conf")),
            (".vscode", root.join("editor-config")),
            (
                "dangling",
                PathBuf::from("/nonexistent-target-for-permission-policy-tests"),
            ),
            ("hooks", PathBuf::from(".git/hooks")),
            ("gitlink", PathBuf::from("src/../.git")),
            ("docs-link", PathBuf::from("docs")),
            ("d", PathBuf::from("sub/deeper")),
            ("dir", PathBuf::from("real")),
            ("alias.rs", PathBuf::from("real/target.rs")),
            ("inner", root.join("src")),
            ("absolute-in-root", root.join("docs")),
            ("kernel", PathBuf::from("/proc/self/cwd/..")),
            (
                "via-proc-self-root",
                PathBuf::from(format!("/proc/self/root{}", root.display())),
            ),
            ("chain/h0", PathBuf::from("h1")),
            ("chain/h1", PathBuf::from("f")),
            ("loop_a", PathBuf::from("loop_b")),
            ("loop_b", PathBuf::from("loop_a")),
            ("cfg", root.join("editor-config/.bashrc")),
        ] {
            link(&root.join(name), target);
        }
        std::fs::create_dir_all(root.join("locked/inside")).unwrap();
        chmod(&root.join("locked"), 0o000);
    }

    /// Every directory the equivalence runs over, and what it is for.
    struct Scene {
        base: TmpDir,
        outside: PathBuf,
        home: PathBuf,
        /// The corpus's roots: a boundary; the same root reached through a link; one its group may write to; one
        /// whose `.git` is a file naming a repository outside it.
        corpus_roots: Vec<PathBuf>,
        /// Roots made for one reason each (a repository's configuration, hooks or layout; the root's own state).
        reason_roots: Vec<PathBuf>,
        /// Paths whose mode this scene took away, given back before the directory is removed.
        locked: Vec<PathBuf>,
    }

    impl Drop for Scene {
        fn drop(&mut self) {
            for path in &self.locked {
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
            }
        }
    }

    impl Scene {
        fn new() -> Scene {
            let base = TmpDir::new("agent-gate-fact", "scene");
            let at = base.canonicalize().unwrap();
            let outside = at.join("outside");
            write(&outside.join("secret"), "private\n");
            git_dir(&outside.join("repo/.git"));
            let home = at.join("home");
            std::fs::create_dir_all(home.join(".config/eitri")).unwrap();
            let loop_home = at.join("loop-home");
            link(&loop_home.join(".config/eitri"), loop_home.join(".config/eitri"));
            std::fs::create_dir_all(at.join("helper-state")).unwrap();

            let plain = at.join("plain");
            rich_root(&plain, &outside, true);
            let through_a_link = at.join("through-a-link");
            link(&through_a_link, &plain);
            let group = at.join("group");
            rich_root(&group, &outside, true);
            chmod(&group, 0o770);
            let gitfile = at.join("gitfile");
            rich_root(&gitfile, &outside, false);
            write(
                &gitfile.join(".git"),
                &format!("gitdir: {}\n", outside.join("repo/.git").display()),
            );

            let locked = vec![plain.join("locked"), group.join("locked"), gitfile.join("locked")];
            Scene {
                base,
                outside,
                home,
                corpus_roots: vec![plain, through_a_link, group, gitfile],
                reason_roots: Vec::new(),
                locked,
            }
        }

        /// The scene with the roots made for one reason each, some of them thousands of directories: only the checks
        /// over the whole scene use them.
        fn with_reason_roots() -> Scene {
            let mut scene = Scene::new();
            let at = scene.at();
            let mut reason_roots = Vec::new();
            let mut repo = |name: &str, layout: &dyn Fn(&Path)| {
                let root = at.join(name);
                std::fs::create_dir_all(root.join("src")).unwrap();
                write(&root.join("src/main.rs"), "fn main() {}\n");
                chmod(&root, 0o700);
                git_dir(&root.join(".git"));
                layout(&root.join(".git"));
                reason_roots.push(root);
            };
            repo("worktree-out", &|git| {
                write(&git.join("config"), "[core]\n\tworktree = /\n")
            });
            repo("fsmonitor", &|git| {
                write(&git.join("config"), "[core]\n\tfsmonitor = ./fsm.sh\n")
            });
            repo("ext-url", &|git| {
                write(&git.join("config"), "[remote \"o\"]\n\turl = ext::sh -c x\n")
            });
            repo("bad-config", &|git| write(&git.join("config"), "[core\n"));
            repo("hookspath", &|git| {
                write(&git.join("config"), "[core]\n\thooksPath = /\n")
            });
            repo("alternates", &|git| {
                write(&git.join("objects/info/alternates"), "/elsewhere\n")
            });
            repo("hook", &|git| {
                write(&git.join("hooks/post-index-change"), "#!/bin/sh\n")
            });
            repo("git-escape", &|git| link(&git.join("escape"), "/etc"));
            repo("git-deep", &|git| {
                std::fs::create_dir_all(git.join(vec!["d"; 130].join("/"))).unwrap();
            });
            repo("git-links", &|git| {
                write(&git.join("description"), "x\n");
                for i in 0..65 {
                    link(&git.join(format!("l/{i}")), "../description");
                }
            });
            repo("git-modules", &|git| {
                for i in 0..1025 {
                    std::fs::create_dir_all(git.join(format!("modules/m{i}"))).unwrap();
                }
            });
            // More directories than the work budget can list, far fewer than the git directory's own limit.
            repo("git-busy", &|git| {
                for i in 0..12_500 {
                    std::fs::create_dir_all(git.join(format!("busy/{}/{}", i / 100, i))).unwrap();
                }
            });
            repo("git-unreadable", &|git| {
                std::fs::create_dir_all(git.join("locked")).unwrap()
            });
            scene.locked.push(at.join("git-unreadable/.git/locked"));
            chmod(&at.join("git-unreadable/.git/locked"), 0o000);

            let above = at.join("above");
            git_dir(&above.join(".git"));
            write(&above.join("project/src/main.rs"), "fn main() {}\n");
            chmod(&above.join("project"), 0o700);
            reason_roots.push(above.join("project"));
            let git_itself = at.join("git-itself");
            git_dir(&git_itself);
            chmod(&git_itself, 0o700);
            reason_roots.push(git_itself);
            let empty_git = at.join("empty-git");
            std::fs::create_dir_all(empty_git.join(".git")).unwrap();
            chmod(&empty_git, 0o700);
            reason_roots.push(empty_git);
            let open = at.join("open");
            std::fs::create_dir_all(open.join("src")).unwrap();
            chmod(&open, 0o777);
            reason_roots.push(open);
            // `/dev` for the kernel-tree root: macOS has it too, where `/proc` does not exist and would only be
            // unresolvable.
            reason_roots.extend([at.join("missing"), PathBuf::from("/"), PathBuf::from("/dev")]);
            scene.reason_roots = reason_roots;
            scene
        }

        fn at(&self) -> PathBuf {
            self.base.canonicalize().unwrap()
        }

        /// The places a test classifier guards: a home of the scene's and the helper's state directory.
        fn places(&self) -> EitriPlaces {
            EitriPlaces::from_values(&EitriPlaceInputs {
                home: Some(self.home.as_os_str()),
                extra: &[self.at().join("helper-state")],
                ..EitriPlaceInputs::default()
            })
        }

        /// Places one of which is a link to itself, so none can be spelled.
        fn unknown_places(&self) -> EitriPlaces {
            let home = self.at().join("loop-home");
            EitriPlaces::from_values(&EitriPlaceInputs {
                home: Some(home.as_os_str()),
                ..EitriPlaceInputs::default()
            })
        }

        /// The corpus with this scene's directories in place of its placeholders, and the cases no line can hold.
        fn corpus(&self, root: &Path) -> Vec<(String, Value)> {
            let swap = |text: &str| {
                text.replace("{root}", &root.display().to_string())
                    .replace("{outside}", &self.outside.display().to_string())
                    .replace("{home}", &self.home.display().to_string())
                    .replace("{tmp}", &self.at().display().to_string())
            };
            let mut cases: Vec<(String, Value)> = CORPUS
                .lines()
                .map(|line| {
                    let (tool, input): (String, Value) = serde_json::from_str(line).unwrap();
                    (tool, swap_strings(input, &swap))
                })
                .collect();
            let outside_name = self.outside.file_name().unwrap().to_string_lossy().into_owned();
            cases.extend([
                (
                    "Bash".into(),
                    json!({ "command": format!("echo {}", "a".repeat(10_000)) }),
                ),
                (
                    "Bash".into(),
                    json!({ "command": format!("echo {}", "a".repeat(9_900)) }),
                ),
                ("Bash".into(), json!({ "command": "  " })),
                ("Bash".into(), json!({ "command": "touch notes.txt" })),
                ("Bash".into(), json!({ "command": "git commit" })),
                ("Bash".into(), json!({ "command": "git log --oneline" })),
                ("Bash".into(), json!({ "command": "git status" })),
                ("Bash".into(), json!({ "command": "ls", "sandbox": "off" })),
                ("Read".into(), json!({ "file_path": "n".repeat(300) })),
                (
                    "Read".into(),
                    json!({ "file_path": format!("../{outside_name}/secret") }),
                ),
                (
                    "Write".into(),
                    json!({ "file_path": format!("{}/new.txt", "n".repeat(300)), "content": "" }),
                ),
                (
                    "Write".into(),
                    json!({ "file_path": format!("nx/{}f.txt", "d/".repeat(256)), "content": "" }),
                ),
                ("Write".into(), json!({ "file_path": "locked/inside/x", "content": "" })),
                ("Write".into(), json!({ "file_path": "cfg", "content": "" })),
                (
                    "Write".into(),
                    json!({ "file_path": self.home.join(".config/eitri/init.lua").display().to_string() }),
                ),
                ("mcp__fs__write_file".into(), json!({ "values": vec![0; 10_001] })),
            ]);
            cases
        }
    }

    /// `value` with `swap` applied to every string in it, keys included.
    fn swap_strings(value: Value, swap: &dyn Fn(&str) -> String) -> Value {
        match value {
            Value::String(text) => Value::String(swap(&text)),
            Value::Array(items) => Value::Array(items.into_iter().map(|item| swap_strings(item, swap)).collect()),
            Value::Object(fields) => Value::Object(
                fields
                    .into_iter()
                    .map(|(k, v)| (swap(&k), swap_strings(v, swap)))
                    .collect(),
            ),
            other => other,
        }
    }

    /// A few calls that reach a root's boundary, its repository and its places, for the roots made for one reason.
    fn reason_cases() -> Vec<(String, Value)> {
        vec![
            ("Read".into(), json!({ "file_path": "src/main.rs" })),
            ("Write".into(), json!({ "file_path": "src/new.rs", "content": "" })),
            ("Bash".into(), json!({ "command": "git status" })),
            ("Bash".into(), json!({ "command": "git log" })),
            ("Bash".into(), json!({ "command": "touch x" })),
            ("Bash".into(), json!({ "command": "ls" })),
            ("TodoWrite".into(), json!({})),
        ]
    }

    fn rule_sets() -> Vec<PrefixRules> {
        let with = |rules: &[&str]| {
            rules.iter().fold(PrefixRules::default(), |set, rule| {
                set.with(PrefixRule::parse(rule).unwrap())
            })
        };
        vec![
            PrefixRules::default(),
            with(&["Bash(git log *)"]),
            with(&["Bash(touch *)", "Bash(git commit *)", "Bash(cat *)", "Bash(cp *)"]),
        ]
    }

    /// The surroundings this test process has, with the scene's home: `HOME` is where the places and the
    /// home-or-above check look, so the scene names its own.
    fn here(home: &Path) -> FactSurroundings {
        FactSurroundings {
            home: Some(home.to_path_buf()),
            identity: Identity::current(),
            git_location_from_environment: false,
        }
    }

    /// One check of every (case, root, surroundings, places) the scene gives, with the reasons the local classifier
    /// gave; every disagreement, listed.
    fn over_the_scene(
        check: &dyn Fn(
            &str,
            &Value,
            &Path,
            &FactSurroundings,
            &EitriPlaces,
            &mut BTreeSet<&'static str>,
        ) -> Vec<String>,
    ) -> (Vec<String>, BTreeSet<&'static str>) {
        let scene = Scene::with_reason_roots();
        let places = scene.places();
        let unknown = scene.unknown_places();
        let around = here(&scene.home);
        let another_user = FactSurroundings {
            identity: Identity {
                uid: around.identity.uid + 1,
                ..around.identity
            },
            ..around.clone()
        };
        let not_private = FactSurroundings {
            identity: Identity {
                private_group: false,
                ..around.identity
            },
            ..around.clone()
        };
        let git_from_environment = FactSurroundings {
            git_location_from_environment: true,
            ..around.clone()
        };
        let mut seen = BTreeSet::new();
        let mut wrong = Vec::new();
        for root in &scene.corpus_roots {
            for (tool, input) in scene.corpus(root) {
                wrong.extend(check(&tool, &input, root, &around, &places, &mut seen));
            }
        }
        let plain = &scene.corpus_roots[0];
        let group = &scene.corpus_roots[2];
        let home_root = scene.home.parent().unwrap().to_path_buf();
        let mut runs: Vec<(&Path, &FactSurroundings, &EitriPlaces)> = scene
            .reason_roots
            .iter()
            .map(|root| (root.as_path(), &around, &places))
            .collect();
        runs.extend([
            (plain.as_path(), &another_user, &places),
            (group.as_path(), &not_private, &places),
            (plain.as_path(), &git_from_environment, &places),
            (plain.as_path(), &around, &unknown),
            (home_root.as_path(), &around, &places),
        ]);
        for (root, around, places) in runs {
            for (tool, input) in reason_cases() {
                wrong.extend(check(&tool, &input, root, around, places, &mut seen));
            }
        }
        (wrong, seen)
    }

    /// `fact` as the laptop has it once it has crossed the wire: each verdict a flag, each reason text mapped back by
    /// [`known_reason`]. Every reason the disk half gives is one the laptop knows, with the verdict it belongs to.
    fn over_the_wire(fact: &GateFact) -> GateFact {
        let carried = |sent: Classification| {
            let reason = known_reason(&sent.reason.to_string())
                .unwrap_or_else(|| panic!("the disk half gave a reason the laptop does not know: {sent:?}"));
            assert_eq!(
                ALLOW_REASONS.contains(&reason),
                !sent.needs_a_human(),
                "a verdict its reason does not come with: {sent:?}"
            );
            let verdict = if sent.needs_a_human() {
                PermissionVerdict::AskTheUser
            } else {
                PermissionVerdict::AllowWithoutAsking
            };
            Classification { verdict, reason }
        };
        GateFact {
            root_is_boundary: fact.root_is_boundary,
            places_card: fact.places_card.map(carried),
            classification: carried(fact.classification),
        }
    }

    /// The two halves together answer what the local classifier answers, in Auto and in Manual, with every rule
    /// set, over the classifier's own test corpus and roots made to give every reason it has; the disk half's answer
    /// crosses the wire as text on the way.
    #[test]
    fn the_two_halves_equal_the_local_classifier() {
        let rule_sets = rule_sets();
        let (wrong, seen) = over_the_scene(&|tool, input, root, around, places, seen| {
            let fact = over_the_wire(&gate_fact(tool, input, root, around, places));
            let mut wrong = Vec::new();
            for rules in &rule_sets {
                for manual in [false, true] {
                    let local = if manual {
                        classify_in_manual_around(tool, input, root, rules, places, &around.as_surroundings())
                    } else {
                        classify_with_rules_around(tool, input, root, rules, places, &around.as_surroundings())
                    };
                    seen.insert(local.reason);
                    let split = classify_with_fact(tool, input, &fact, rules, manual);
                    if split != local {
                        wrong.push(format!(
                            "{tool} {input} at {root:?} (manual {manual}, {rules:?}): {split:?} against {local:?}"
                        ));
                    }
                }
            }
            wrong
        });
        assert!(
            wrong.is_empty(),
            "{} disagreements, first: {:#?}",
            wrong.len(),
            &wrong[..wrong.len().min(20)]
        );
        let missing: Vec<&str> = ALL_REASONS
            .iter()
            .copied()
            .filter(|reason| !seen.contains(reason))
            .collect();
        assert!(missing.is_empty(), "no case gave these reasons: {missing:#?}");
    }

    /// The same, through the public functions, in this process's own surroundings and places.
    #[test]
    fn the_two_halves_equal_the_public_classifier_in_this_process() {
        let scene = Scene::new();
        let around = FactSurroundings {
            home: std::env::var_os("HOME").map(PathBuf::from),
            identity: Identity::current(),
            git_location_from_environment: GIT_LOCATION_VARIABLES.iter().any(|v| std::env::var_os(v).is_some()),
        };
        let places = EitriPlaces::from_process();
        let mut wrong = Vec::new();
        for root in &scene.corpus_roots[..2] {
            for (tool, input) in scene.corpus(root) {
                let fact = gate_fact(&tool, &input, root, &around, &places);
                for rules in &rule_sets() {
                    let pairs = [
                        (
                            classify_with_fact(&tool, &input, &fact, rules, false),
                            classify_with_rules(&tool, &input, root, rules),
                        ),
                        (
                            classify_with_fact(&tool, &input, &fact, rules, true),
                            classify_in_manual(&tool, &input, root, rules),
                        ),
                    ];
                    for (split, local) in pairs {
                        if split != local {
                            wrong.push(format!("{tool} {input} at {root:?}: {split:?} against {local:?}"));
                        }
                    }
                    let (split, local) = (
                        rule_that_allows_with_fact(&tool, &input, &fact, rules),
                        rule_that_allows(&tool, &input, root, rules),
                    );
                    if split != local {
                        wrong.push(format!(
                            "rule for {tool} {input} at {root:?}: {split:?} against {local:?}"
                        ));
                    }
                }
            }
        }
        assert!(
            wrong.is_empty(),
            "{} disagreements, first: {:#?}",
            wrong.len(),
            &wrong[..wrong.len().min(20)]
        );
    }

    #[test]
    fn rule_that_allows_with_fact_agrees_with_rule_that_allows() {
        let rule_sets = rule_sets();
        let (wrong, _) = over_the_scene(&|tool, input, root, around, places, _| {
            if !matches!(tool, "Bash" | "Monitor") {
                return Vec::new();
            }
            let fact = over_the_wire(&gate_fact(tool, input, root, around, places));
            let mut wrong = Vec::new();
            for rules in &rule_sets {
                let local = rule_that_allows_around(tool, input, root, rules, places, &around.as_surroundings());
                let split = rule_that_allows_with_fact(tool, input, &fact, rules);
                let allowed_by_a_rule =
                    classify_with_fact(tool, input, &fact, rules, false).reason == REASON_ALLOWED_BY_A_PROJECT_RULE;
                if split != local || split.is_some() != allowed_by_a_rule {
                    wrong.push(format!(
                        "{tool} {input} at {root:?} ({rules:?}): {split:?} against {local:?}"
                    ));
                }
            }
            wrong
        });
        assert!(
            wrong.is_empty(),
            "{} disagreements, first: {:#?}",
            wrong.len(),
            &wrong[..wrong.len().min(20)]
        );
    }

    /// A fact the disk half could not have given is made stricter, never looser.
    #[test]
    fn a_fact_the_disk_half_could_not_give_cards() {
        let rules = PrefixRules::default().with(PrefixRule::parse("Bash(touch *)").unwrap());
        let touch = json!({ "command": "touch x" });
        let allows = Classification {
            verdict: PermissionVerdict::AllowWithoutAsking,
            reason: super::super::REASON_READ_ONLY_COMMAND,
        };
        for reason in [REASON_EITRI_OWN_PLACES, REASON_EITRI_PLACE_UNKNOWN] {
            let fact = GateFact {
                root_is_boundary: true,
                places_card: Some(Classification {
                    verdict: PermissionVerdict::AllowWithoutAsking,
                    reason,
                }),
                classification: allows,
            };
            let got = classify_with_fact("Bash", &touch, &fact, &rules, false);
            assert_eq!(got.verdict, PermissionVerdict::AskTheUser, "{reason}");
            assert_eq!(got.reason, reason);
            assert_eq!(rule_that_allows_with_fact("Bash", &touch, &fact, &rules), None);
        }
    }

    /// Callees a scanned body may name without their own body being scanned: constructors and methods of the
    /// standard library and `serde_json` that work on values in memory, and `classify`, the closure
    /// [`place_card_first`] takes, which the policy half gives as a body of its own, scanned with it.
    const VALUE_CALLS: &[&str] = &[
        "Some",
        "Ok",
        "Err",
        "String",
        "Bool",
        "contains",
        "filter",
        "map",
        "then",
        "is_some",
        "unwrap_or_else",
        "is_object",
        "as_object",
        "iter",
        "copied",
        "find",
        "all",
        "get",
        "as_str",
        "next",
        "join",
        "split",
        "is_empty",
        "classify",
    ];

    /// Macros the policy half may use: each works on values in memory.
    const VALUE_MACROS: &[&str] = &["matches", "format", "vec", "assert", "debug_assert", "unreachable"];

    /// The files the policy half's functions live in, and those of the saved rules it consults.
    const POLICY_FILES: &[&str] = &[
        "permission_policy/server_fact.rs",
        "permission_policy.rs",
        "permission_rules.rs",
    ];

    /// Every function reachable from `entries` through the calls their bodies name, each found in one of
    /// [`POLICY_FILES`] and scanned in turn, or the first callee that is neither such a function nor in
    /// [`VALUE_CALLS`]. A name defined in one of the files is that function, scanned, even where it is also a value
    /// call. Literals are blanked first, so a word before a `(` in a format string is no call. A macro outside
    /// [`VALUE_MACROS`], a call this scan cannot name (the result of a call, an index, generic arguments not after
    /// `::`) and a function named as a value after `::` each fail it.
    fn reachable(
        files: &std::collections::BTreeMap<String, String>,
        entries: &[&str],
    ) -> Result<BTreeSet<String>, String> {
        let mut scanned = BTreeSet::new();
        let mut queue: Vec<String> = entries.iter().map(|name| name.to_string()).collect();
        while let Some(name) = queue.pop() {
            if !scanned.insert(name.clone()) {
                continue;
            }
            let needle = format!("fn {name}(");
            let defined: Vec<&str> = POLICY_FILES
                .iter()
                .copied()
                .filter(|file| files[*file].contains(&needle))
                .collect();
            let [file] = defined[..] else {
                return Err(format!(
                    "{name} is defined in {defined:?}, not in exactly one scanned file"
                ));
            };
            let body = crate::wire_guard::fn_body(&files[file], &name)?;
            let code = crate::wire_guard::blank_literals(&body);
            let bytes = code.as_bytes();
            let ident_before = |end: usize| {
                let mut start = end;
                while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
                    start -= 1;
                }
                &code[start..end]
            };
            // A function named as a value (`.map(path::name)`) is a call this scan cannot follow.
            for (at, _) in code.match_indices("::") {
                let rest = code[at + 2..].trim_start();
                let word: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                let after = rest[word.len()..].trim_start();
                if word.starts_with(|c: char| c.is_ascii_lowercase())
                    && !["(", "::", "<", "!", "{"].iter().any(|next| after.starts_with(next))
                {
                    return Err(format!("{name} names {word} as a value, which is not followed"));
                }
            }
            for (at, _) in code.match_indices('(') {
                let mut end = at;
                while end > 0 && bytes[end - 1].is_ascii_whitespace() {
                    end -= 1;
                }
                let callee = match bytes.get(end.wrapping_sub(1)) {
                    // A macro: only those that read nothing.
                    Some(b'!') => {
                        let mac = ident_before(end - 1);
                        if !VALUE_MACROS.contains(&mac) {
                            return Err(format!("{name} calls the macro {mac}!, which is not scanned"));
                        }
                        continue;
                    }
                    // A match arm's or a return type's arrow before a tuple.
                    Some(b'>') if matches!(bytes.get(end.wrapping_sub(2)), Some(b'=' | b'-')) => continue,
                    // A call with its generic arguments named (`name::<T>(`): the name before them.
                    Some(b'>') => {
                        let mut depth = 0usize;
                        let mut open = None;
                        for back in (0..end).rev() {
                            match bytes[back] {
                                b'>' => depth += 1,
                                b'<' => {
                                    depth -= 1;
                                    if depth == 0 {
                                        open = Some(back);
                                        break;
                                    }
                                }
                                _ => {}
                            }
                        }
                        match open.filter(|open| code[..*open].ends_with("::")) {
                            Some(open) => ident_before(open - 2),
                            None => return Err(format!("{name} calls something this scan cannot name")),
                        }
                    }
                    // The result of a call or an index called in turn.
                    Some(b')' | b']') => return Err(format!("{name} calls a value, which is not followed")),
                    _ => ident_before(end),
                };
                if callee.is_empty() {
                    if bytes
                        .get(end.wrapping_sub(1))
                        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
                    {
                        return Err(format!("{name} calls something this scan cannot name"));
                    }
                    continue;
                }
                if callee.starts_with(|c: char| c.is_ascii_digit()) {
                    return Err(format!("{name} calls {callee}, which is not scanned"));
                }
                // A function of the scanned files is scanned, even one sharing its name with a value call.
                let needle = format!("fn {callee}(");
                if POLICY_FILES.iter().any(|file| files[*file].contains(&needle)) {
                    queue.push(callee.to_string());
                } else if !VALUE_CALLS.contains(&callee) {
                    return Err(format!("{name} calls {callee}, which is not scanned"));
                }
            }
        }
        Ok(scanned)
    }

    /// The scan fails on what it cannot follow rather than passing over it.
    #[test]
    fn the_reach_scan_fails_on_what_it_cannot_follow() {
        let files_with = |body: &str| -> std::collections::BTreeMap<String, String> {
            POLICY_FILES
                .iter()
                .map(|file| (file.to_string(), String::new()))
                .chain([(
                    "permission_rules.rs".to_string(),
                    format!("fn entry(x: &str) -> bool {{ {body} }} fn helper(x: &str) -> bool {{ x.is_empty() }}"),
                )])
                .collect()
        };
        let reach = |body: &str| reachable(&files_with(body), &["entry"]);
        assert!(reach("helper(x)").unwrap().contains("helper"));
        assert!(reach("match x.is_empty() { true => (helper(x)), false => (x.is_empty()) }").is_ok());
        assert!(reach("let t = format!(\"read_to_string({x})\"); t.is_empty()").is_ok());
        assert!(reach("let t = r#\"open(\"#; t.is_empty()").is_ok());
        for (refused, why) in [
            ("std::fs::read(x).is_some()", "calls read"),
            ("std::env::var_os(x).is_some()", "calls var_os"),
            ("include_str!(x).is_empty()", "the macro include_str"),
            ("std::fs::read::<String>(x).is_some()", "calls read"),
            ("[x].iter().map(std::fs::read).is_some()", "names read as a value"),
            ("let f = helper; (f)(x)", "calls a value"),
            ("helpers[0](x)", "calls a value"),
            ("let n = x.is_empty() > (true); n", "cannot name"),
        ] {
            let error = reach(refused).expect_err(refused);
            assert!(error.contains(why), "{refused}: {error}");
        }
    }

    /// The policy half calls nothing that could read a file, the identity or the environment: every function it
    /// reaches is scanned, and each names only those and calls on values in memory.
    #[test]
    fn classify_with_fact_reads_nothing() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let files = crate::wire_guard::production_code(&root);
        let reached = reachable(
            &files,
            &["classify_with_fact", "rule_that_allows_with_fact", "known_reason"],
        )
        .unwrap();
        for expected in [
            "disk_answer",
            "judged_by_its_paths",
            "place_card_first",
            "classify_by_name",
            "matching_rule",
        ] {
            assert!(reached.contains(expected), "{expected} was not reached: {reached:?}");
        }
        // Not vacuous: from the disk half, the scan meets a call it does not know.
        let disk = reachable(&files, &["gate_fact"]).unwrap_err();
        assert!(disk.contains("calls"), "{disk}");
    }

    #[test]
    fn every_reason_constant_is_known() {
        for reason in ALL_REASONS {
            if POLICY_HALF_REASONS.contains(reason) {
                assert_eq!(known_reason(reason), None, "{reason}");
            } else {
                assert_eq!(known_reason(reason), Some(*reason));
                assert_eq!(known_reason(&reason.to_string()), Some(*reason));
            }
        }
        for refusal in NotABoundary::ALL {
            assert_eq!(known_reason(refusal.reason()), Some(refusal.reason()), "{refusal:?}");
        }
        for reason in super::super::REPLACEABLE_BY_A_RULE.iter().chain(&POLICY_HALF_REASONS) {
            assert!(ALL_REASONS.contains(reason), "{reason}");
        }
        for unknown in [
            REASON_SERVER_FACT_UNKNOWN,
            REASON_SERVER_FACT_MISSING,
            REASON_SERVER_FACT_CHANGED,
            REASON_SERVER_FACT_IMPOSSIBLE,
            "",
            "the path resolves inside the project root ",
        ] {
            assert_eq!(known_reason(unknown), None, "{unknown:?}");
        }
        let distinct: BTreeSet<&str> = ALL_REASONS.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            ALL_REASONS.len(),
            "a reason is listed twice, or two share their words"
        );

        // Every named reason of the classifier's files is in the list, by name.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let files = crate::wire_guard::production_code(&root);
        let policy = &files["permission_policy.rs"];
        let start = policy.find("const ALL_REASONS").expect("ALL_REASONS");
        let list = &policy[start..start + policy[start..].find("];").unwrap()];
        let mut names = 0;
        for file in [
            "permission_policy.rs",
            "permission_policy/eitri_places.rs",
            "project_boundary.rs",
        ] {
            let code = &files[file];
            for (at, _) in code.match_indices("const REASON_") {
                let name: String = code[at + "const ".len()..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                names += 1;
                assert!(
                    list.contains(&format!("{name},")),
                    "{file}: {name} is not in ALL_REASONS"
                );
            }
        }
        assert_eq!(
            names,
            ALL_REASONS.len(),
            "ALL_REASONS lists a reason none of the files names"
        );
    }

    /// Every reason the classifier gives is a named constant: no `ask`, `allow` or `reason:` takes a literal, and
    /// `NotABoundary::reason` spells none.
    #[test]
    fn no_reason_is_an_inline_literal() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let files = crate::wire_guard::production_code(&root);
        let mut offenders = Vec::new();
        let mut calls = 0;
        for file in [
            "permission_policy.rs",
            "permission_policy/eitri_places.rs",
            "permission_policy/server_fact.rs",
            "project_boundary.rs",
        ] {
            let code = &files[file];
            for word in ["ask(", "allow(", "reason:"] {
                for (at, _) in code.match_indices(word) {
                    let before = code[..at].chars().next_back();
                    if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                        continue;
                    }
                    calls += 1;
                    let after = code[at + word.len()..].trim_start();
                    if after.starts_with('"') || after.starts_with("r\"") || after.starts_with("r#") {
                        offenders.push(format!("{file}: {}", &code[at..(at + 80).min(code.len())]));
                    }
                }
            }
        }
        assert!(calls > 60, "the scan saw only {calls} calls");
        let reason = crate::wire_guard::fn_body(&files["project_boundary.rs"], "reason").unwrap();
        assert!(!reason.contains('"'), "NotABoundary::reason spells a reason: {reason}");
        assert!(offenders.is_empty(), "reasons written as literals: {offenders:#?}");
    }

    #[test]
    fn request_digest_changes_with_every_part() {
        let root = Path::new("/srv/project");
        let base = request_digest("s1", "p1", "Read", br#"{"file_path":"a"}"#, root);
        assert_eq!(base, request_digest("s1", "p1", "Read", br#"{"file_path":"a"}"#, root));
        let changed = [
            request_digest("s2", "p1", "Read", br#"{"file_path":"a"}"#, root),
            request_digest("s1", "p2", "Read", br#"{"file_path":"a"}"#, root),
            request_digest("s1", "p1", "Write", br#"{"file_path":"a"}"#, root),
            request_digest("s1", "p1", "Read", br#"{"file_path":"b"}"#, root),
            request_digest("s1", "p1", "Read", br#"{"file_path": "a"}"#, root),
            request_digest("s1", "p1", "Read", br#"{"file_path":"a"}"#, Path::new("/srv/other")),
            // A byte moved from one part to the next is another request.
            request_digest("s1p", "1", "Read", br#"{"file_path":"a"}"#, root),
            request_digest("s1", "p1R", "ead", br#"{"file_path":"a"}"#, root),
        ];
        let mut all: BTreeSet<[u8; 32]> = changed.iter().copied().collect();
        all.insert(base);
        assert_eq!(all.len(), changed.len() + 1, "two different requests share a digest");
        assert_ne!(
            request_digest("a\0b", "c", "Read", b"{}", root),
            request_digest("a", "b\0c", "Read", b"{}", root)
        );
    }

    #[test]
    fn extra_places_card_a_write_there() {
        let scene = Scene::new();
        let state = scene.at().join("helper-state");
        let root = &scene.corpus_roots[0];
        let around = here(&scene.home);
        let without = EitriPlaces::from_values(&EitriPlaceInputs {
            home: Some(scene.home.as_os_str()),
            ..EitriPlaceInputs::default()
        });
        let relative = [PathBuf::from("helper-state")];
        let ignored = EitriPlaces::from_values(&EitriPlaceInputs {
            home: Some(scene.home.as_os_str()),
            extra: &relative,
            ..EitriPlaceInputs::default()
        });
        assert_eq!(ignored, without, "a relative extra directory names no place");
        let rules = PrefixRules::default().with(PrefixRule::parse("Bash(cp *)").unwrap());
        let journal = state.join("journal").display().to_string();
        for (tool, input) in [
            ("Write", json!({ "file_path": journal.clone(), "content": "" })),
            ("Bash", json!({ "command": format!("cp notes.txt {journal}") })),
        ] {
            assert_eq!(
                gate_fact(tool, &input, root, &around, &without).places_card,
                None,
                "{tool}"
            );
            let fact = gate_fact(tool, &input, root, &around, &scene.places());
            assert_eq!(
                fact.places_card.map(|card| card.reason),
                Some(REASON_EITRI_OWN_PLACES),
                "{tool}"
            );
            for manual in [false, true] {
                let got = classify_with_fact(tool, &input, &fact, &rules, manual);
                assert!(got.needs_a_human(), "{tool}");
                if !(manual && tool == "Write") {
                    assert_eq!(got.reason, REASON_EITRI_OWN_PLACES, "{tool}");
                }
            }
            assert_eq!(rule_that_allows_with_fact(tool, &input, &fact, &rules), None);
        }
        // The same command without the extra place is one the saved rule answers.
        let input = json!({ "command": format!("cp notes.txt {journal}") });
        let fact = gate_fact("Bash", &input, root, &around, &without);
        assert_eq!(fact.places_card, None);
    }

    /// A place inside the root: the call a saved rule answers without the place is a card with it, no rule answering
    /// it, locally and through the two halves alike.
    #[test]
    fn an_extra_place_inside_the_root_cards_what_a_rule_answers() {
        let scene = Scene::new();
        let root = &scene.corpus_roots[0];
        write(&root.join("notes.txt"), "notes\n");
        write(&root.join("state/journal"), "");
        let around = here(&scene.home);
        let surroundings = around.as_surroundings();
        let without = EitriPlaces::from_values(&EitriPlaceInputs {
            home: Some(scene.home.as_os_str()),
            ..EitriPlaceInputs::default()
        });
        let extra = [root.join("state")];
        let with = EitriPlaces::from_values(&EitriPlaceInputs {
            home: Some(scene.home.as_os_str()),
            extra: &extra,
            ..EitriPlaceInputs::default()
        });
        let rules = PrefixRules::default().with(PrefixRule::parse("Bash(cp *)").unwrap());
        let input = json!({ "command": "cp notes.txt state/journal" });
        let by_the_rule = allow(REASON_ALLOWED_BY_A_PROJECT_RULE);

        let fact = gate_fact("Bash", &input, root, &around, &without);
        assert_eq!(fact.places_card, None);
        assert_eq!(fact.classification, ask(REASON_NOT_ON_READ_ONLY_LIST));
        assert_eq!(classify_with_fact("Bash", &input, &fact, &rules, false), by_the_rule);
        assert_eq!(
            classify_with_rules_around("Bash", &input, root, &rules, &without, &surroundings),
            by_the_rule
        );
        assert_eq!(
            rule_that_allows_with_fact("Bash", &input, &fact, &rules),
            Some("Bash(cp *)".to_string())
        );

        let fact = gate_fact("Bash", &input, root, &around, &with);
        assert_eq!(fact.places_card, Some(ask(REASON_EITRI_OWN_PLACES)));
        for manual in [false, true] {
            assert_eq!(
                classify_with_fact("Bash", &input, &fact, &rules, manual),
                ask(REASON_EITRI_OWN_PLACES)
            );
        }
        assert_eq!(
            classify_with_rules_around("Bash", &input, root, &rules, &with, &surroundings),
            ask(REASON_EITRI_OWN_PLACES)
        );
        assert_eq!(rule_that_allows_with_fact("Bash", &input, &fact, &rules), None);
    }

    /// A places card and a classification no disk half gives together are joined as the local classifier joins
    /// them: a write into the places cards first, a place that cannot be spelled takes the place of an allow and of a
    /// card a rule replaces, and no saved rule answers either.
    #[test]
    fn a_places_card_joins_the_classification_as_locally() {
        let rules = PrefixRules::default().with(PrefixRule::parse("Bash(touch *)").unwrap());
        let touch = json!({ "command": "touch x" });
        let other = ask(REASON_PATH_RESOLVES_OUTSIDE);
        for place in [REASON_EITRI_OWN_PLACES, REASON_EITRI_PLACE_UNKNOWN] {
            for verdict in [PermissionVerdict::AskTheUser, PermissionVerdict::AllowWithoutAsking] {
                for (classification, expected) in [
                    (allow(REASON_READ_ONLY_COMMAND), place),
                    (ask(REASON_NOT_ON_READ_ONLY_LIST), place),
                    (
                        other,
                        if place == REASON_EITRI_OWN_PLACES {
                            place
                        } else {
                            other.reason
                        },
                    ),
                ] {
                    let fact = GateFact {
                        root_is_boundary: true,
                        places_card: Some(Classification { verdict, reason: place }),
                        classification,
                    };
                    for manual in [false, true] {
                        assert_eq!(
                            classify_with_fact("Bash", &touch, &fact, &rules, manual),
                            ask(expected),
                            "{fact:?}, manual {manual}"
                        );
                    }
                    assert_eq!(
                        rule_that_allows_with_fact("Bash", &touch, &fact, &rules),
                        None,
                        "{fact:?}"
                    );
                }
            }
        }
        // A places card with a reason the places' check never gives is a card no rule replaces, whatever the
        // classification beside it.
        for (verdict, reason) in [
            (PermissionVerdict::AskTheUser, REASON_NOT_ON_READ_ONLY_LIST),
            (PermissionVerdict::AllowWithoutAsking, REASON_ALLOWED_BY_A_PROJECT_RULE),
            (PermissionVerdict::AllowWithoutAsking, REASON_READ_ONLY_COMMAND),
        ] {
            for classification in [allow(REASON_READ_ONLY_COMMAND), ask(REASON_NOT_ON_READ_ONLY_LIST)] {
                let fact = GateFact {
                    root_is_boundary: true,
                    places_card: Some(Classification { verdict, reason }),
                    classification,
                };
                for manual in [false, true] {
                    assert_eq!(
                        classify_with_fact("Bash", &touch, &fact, &rules, manual),
                        ask(REASON_SERVER_FACT_IMPOSSIBLE),
                        "{fact:?}, manual {manual}"
                    );
                }
                assert_eq!(
                    rule_that_allows_with_fact("Bash", &touch, &fact, &rules),
                    None,
                    "{fact:?}"
                );
            }
        }
    }

    /// What the classifier decides by the tool's name, or because the input is not an object, is decided here
    /// whatever the fact says, and joined with the places' card as locally.
    #[test]
    fn a_decision_by_name_is_made_here() {
        let inside = allow(REASON_PATH_RESOLVES_INSIDE);
        let outside = ask(REASON_PATH_RESOLVES_OUTSIDE);
        let rules = PrefixRules::default().with(PrefixRule::parse("Bash(ls *)").unwrap());
        for (tool, input, expected) in [
            (
                "nvim_exec",
                json!({ "command": "!rm -rf ~" }),
                ask(REASON_TOOL_NOT_IN_THE_TABLE),
            ),
            ("WebFetch", json!({ "prompt": "x" }), ask(REASON_CLI_ALWAYS_ASKS)),
            ("Bash", json!("ls"), ask(REASON_INPUT_NOT_AN_OBJECT)),
            ("Read", json!(["src/main.rs"]), ask(REASON_INPUT_NOT_AN_OBJECT)),
            ("TodoWrite", json!({}), allow(REASON_TOOL_CHANGES_NOTHING)),
        ] {
            for root_is_boundary in [true, false] {
                for classification in [inside, outside] {
                    let fact = GateFact {
                        root_is_boundary,
                        places_card: None,
                        classification,
                    };
                    for manual in [false, true] {
                        assert_eq!(
                            classify_with_fact(tool, &input, &fact, &rules, manual),
                            expected,
                            "{tool} {fact:?}"
                        );
                    }
                    assert_eq!(rule_that_allows_with_fact(tool, &input, &fact, &rules), None);
                }
            }
            let own = GateFact {
                root_is_boundary: true,
                places_card: Some(ask(REASON_EITRI_OWN_PLACES)),
                classification: inside,
            };
            assert_eq!(
                classify_with_fact(tool, &input, &own, &rules, false),
                ask(REASON_EITRI_OWN_PLACES),
                "{tool}"
            );
            let unknown = GateFact {
                places_card: Some(ask(REASON_EITRI_PLACE_UNKNOWN)),
                ..own
            };
            let joined = if expected.needs_a_human() {
                expected
            } else {
                ask(REASON_EITRI_PLACE_UNKNOWN)
            };
            assert_eq!(
                classify_with_fact(tool, &input, &unknown, &rules, false),
                joined,
                "{tool}"
            );
        }
    }

    /// A fact the disk half could not have given for a call judged by its paths is a card no rule answers: a reason
    /// only the policy half gives, a verdict its reason never comes with, or an allow or a reason a rule replaces at a
    /// root that is no boundary.
    #[test]
    fn an_impossible_fact_cards() {
        let rules = PrefixRules::default().with(PrefixRule::parse("Bash(touch *)").unwrap());
        let touch = json!({ "command": "touch x" });
        let read = json!({ "file_path": "src/main.rs" });
        let flipped = |sent: Classification| Classification {
            verdict: if sent.needs_a_human() {
                PermissionVerdict::AllowWithoutAsking
            } else {
                PermissionVerdict::AskTheUser
            },
            reason: sent.reason,
        };
        for (root_is_boundary, classification) in [
            (true, flipped(ask(REASON_PATH_RESOLVES_OUTSIDE))),
            (true, flipped(ask(NotABoundary::NotYours.reason()))),
            (true, flipped(ask(REASON_NOT_ON_READ_ONLY_LIST))),
            (true, flipped(allow(REASON_PATH_RESOLVES_INSIDE))),
            (true, allow(REASON_ALLOWED_BY_A_PROJECT_RULE)),
            (false, allow(REASON_PATH_RESOLVES_INSIDE)),
            (false, allow(REASON_READ_ONLY_COMMAND)),
            (false, ask(REASON_NOT_ON_READ_ONLY_LIST)),
            // A decision by name on a call judged by its paths.
            (true, allow(REASON_TOOL_CHANGES_NOTHING)),
            (true, ask(REASON_INPUT_NOT_AN_OBJECT)),
            (true, ask(REASON_CLI_ALWAYS_ASKS)),
            (true, ask(REASON_TOOL_NOT_IN_THE_TABLE)),
        ] {
            let fact = GateFact {
                root_is_boundary,
                places_card: None,
                classification,
            };
            for (tool, input) in [("Read", &read), ("Bash", &touch)] {
                for manual in [false, true] {
                    assert_eq!(
                        classify_with_fact(tool, input, &fact, &rules, manual),
                        ask(REASON_SERVER_FACT_IMPOSSIBLE),
                        "{tool} {fact:?}"
                    );
                }
                assert_eq!(rule_that_allows_with_fact(tool, input, &fact, &rules), None);
            }
        }
        // What the disk half gives at a root that is no boundary stands.
        let refused = ask(NotABoundary::OthersCanWrite.reason());
        let fact = GateFact {
            root_is_boundary: false,
            places_card: None,
            classification: refused,
        };
        assert_eq!(classify_with_fact("Bash", &touch, &fact, &rules, false), refused);
        assert_eq!(rule_that_allows_with_fact("Bash", &touch, &fact, &rules), None);
    }

    /// Each reason comes with one verdict: every `allow` names a reason of [`ALLOW_REASONS`], no `ask` names one, and
    /// neither is named other than in a call. A reason passed in a variable is covered by `over_the_wire`, which checks
    /// every verdict the corpus produces against [`ALLOW_REASONS`].
    #[test]
    fn every_reason_comes_with_one_verdict() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let files = crate::wire_guard::production_code(&root);
        let policy = &files["permission_policy.rs"];
        let start = policy.find("const ALLOW_REASONS").expect("ALLOW_REASONS");
        let list = &policy[start..start + policy[start..].find("];").unwrap()];
        let listed: BTreeSet<&str> = list
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .filter(|word| word.starts_with("REASON_"))
            .collect();
        assert_eq!(listed.len(), ALLOW_REASONS.len(), "{list}");
        let mut allows = 0;
        for file in [
            "permission_policy.rs",
            "permission_policy/eitri_places.rs",
            "permission_policy/server_fact.rs",
            "project_boundary.rs",
        ] {
            let code = &crate::wire_guard::blank_literals(&files[file]);
            // Named other than in a call (`.map(ask)`), a verdict's reason could not be read here.
            let mut without_uses = code.clone();
            for span in crate::wire_guard::use_item_spans(code) {
                without_uses.replace_range(span.clone(), &" ".repeat(span.len()));
            }
            for word in ["ask", "allow"] {
                for (at, _) in without_uses.match_indices(word) {
                    let (before, after) = (&without_uses[..at], &without_uses[at + word.len()..]);
                    let ident = |c: char| c.is_alphanumeric() || c == '_';
                    if before.ends_with(ident) || after.starts_with(ident) || before.ends_with("fn ") {
                        continue;
                    }
                    assert!(
                        after.trim_start().starts_with('('),
                        "{file}: {word} is named other than in a call"
                    );
                }
            }
            for word in ["ask(", "allow("] {
                for (at, _) in code.match_indices(word) {
                    let before = code[..at].chars().next_back();
                    if before.is_some_and(|c| c.is_alphanumeric() || c == '_') || code[..at].ends_with("fn ") {
                        continue;
                    }
                    let argument: String = code[at + word.len()..]
                        .trim_start()
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if word == "allow(" {
                        allows += 1;
                        assert!(listed.contains(argument.as_str()), "{file}: allow({argument}");
                    } else {
                        assert!(!listed.contains(argument.as_str()), "{file}: ask({argument}");
                    }
                }
            }
        }
        assert!(allows >= ALLOW_REASONS.len(), "the scan saw only {allows} allows");
    }
}
