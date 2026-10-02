//! D7 (keymap/tabs spec §4.1): "Always allow `git log *` in this project". Claude Code's own rule
//! syntax, `Bash(git log *)` -- "note the space before `*`, which is required for prefix matching"
//! (`bin:`, 2.1.282) -- and nothing else in v1.
//!
//! **A rule never widens a fail-closed check.** It is consulted only after
//! `permission_policy::classify_permission_request` has said "ask", and only when that ask carries
//! one of the two reasons in [`crate::permission_policy::REPLACEABLE_BY_A_RULE`]. Both are reached in
//! `classify_bash` after every syntax, path, symlink, link-following, `cd`, `git --output` and
//! `find -exec` check has already passed -- so a rule can only ever replace "this first word is not
//! on the read-only list", never "this command could do something the words do not show".

use std::path::Path;

use serde_json::Value;

use crate::permission_policy::{classify_permission_request, REPLACEABLE_BY_A_RULE};

/// One `Bash(<words> *)` rule. `words` is never empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixRule {
    words: Vec<String>,
}

impl PrefixRule {
    /// Claude Code's syntax, prefix form only. Whitespace inside is normalized; anything else is
    /// `None` -- the rules file's reader logs and skips it, which fails toward more cards.
    ///
    /// A rule whose first word is a program that runs another program (`env *`, `sh *`,
    /// `timeout 5 *`, or an assignment such as `LC_ALL=C *`) is `None` too, even though the syntax is fine: it would allow whatever program
    /// follows, so the card never offers one, and a hand-edited rules file must not add one either.
    pub fn parse(text: &str) -> Option<Self> {
        let inner = text.trim().strip_prefix("Bash(")?.strip_suffix(')')?;
        let inner = inner.trim_end();
        let body = inner.strip_suffix(" *")?;
        let words: Vec<String> = body.split_whitespace().map(str::to_string).collect();
        if words.is_empty() || words.iter().any(|w| w.contains('*')) {
            return None;
        }
        if runs_another_program(&words[0]) {
            return None;
        }
        Some(PrefixRule { words })
    }

    pub fn to_rule_string(&self) -> String {
        format!("Bash({} *)", self.words.join(" "))
    }

    /// What the card's button names: `git log *`.
    pub fn display(&self) -> String {
        format!("{} *", self.words.join(" "))
    }

    pub fn words(&self) -> &[String] {
        &self.words
    }

    /// Whole leading words, never a character prefix: `cargo test *` is not `cargo testx`.
    pub fn matches_command(&self, command: &str) -> bool {
        let mut words = command.split_whitespace();
        self.words.iter().all(|w| words.next() == Some(w.as_str()))
    }
}

/// A project's rules. Order is insertion order; a duplicate is never stored twice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrefixRules(Vec<PrefixRule>);

impl PrefixRules {
    pub fn new(rules: Vec<PrefixRule>) -> Self {
        rules
            .into_iter()
            .fold(PrefixRules::default(), |set, rule| set.with(rule))
    }
    pub fn rules(&self) -> &[PrefixRule] {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn with(&self, rule: PrefixRule) -> PrefixRules {
        let mut rules = self.0.clone();
        if !rules.contains(&rule) {
            rules.push(rule);
        }
        PrefixRules(rules)
    }
    /// `Bash` with a string `command` that some rule matches. Nothing else can match.
    pub fn matches(&self, tool_name: &str, input: &Value) -> bool {
        self.matching_rule(tool_name, input).is_some()
    }

    /// The first rule [`Self::matches`] would match on, so a transcript row can name the rule that
    /// answered it (v1 polish F18) rather than only stderr saying "a prefix rule".
    pub fn matching_rule(&self, tool_name: &str, input: &Value) -> Option<&PrefixRule> {
        if tool_name != "Bash" {
            return None;
        }
        let Some(Value::String(command)) = input.get("command") else {
            return None;
        };
        self.0.iter().find(|rule| rule.matches_command(command))
    }
}

/// Programs whose job is to run another program named later on the line. A prefix rule over one
/// (`timeout 5 *`, `sudo *`, `bash *`) allows whatever program follows, so none is ever suggested.
const WRAPPERS: &[&str] = &[
    "bash", "busybox", "chroot", "chrt", "command", "dash", "doas", "env", "eval", "exec", "fish", "flock", "ionice",
    "ltrace", "nice", "nohup", "runuser", "setsid", "sh", "stdbuf", "strace", "su", "sudo", "taskset", "time",
    "timeout", "unbuffer", "watch", "xargs", "zsh",
];

/// `env`, `/usr/bin/env` and `./env` all name a program that runs whatever follows, so the word's
/// last path component is what is compared.
///
/// A first word holding `=` is refused too: the shell reads `LC_ALL=C rm -rf src` (and bash's
/// `A+=x cmd`, `a[0]=x cmd`) as an assignment followed by the program that really runs, the same as
/// `env LC_ALL=C rm -rf src`, so a rule over it would allow whatever program follows. Any `=` rather
/// than only a valid name before it: a program whose own name holds `=` is rare enough that losing
/// its offer costs one more card, not a gap.
fn runs_another_program(first: &str) -> bool {
    first.contains('=') || WRAPPERS.contains(&first.rsplit('/').next().unwrap_or(first))
}

fn is_bare_word(word: &str) -> bool {
    word.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The first word, plus the second when it is a bare word (`^[A-Za-z0-9][A-Za-z0-9._-]*$`), so
/// `git log --oneline` suggests `git log *` and `uname -a` suggests `uname *` (spec §4.1, ruling 14).
///
/// Two narrowings of ruling 14, both toward fewer offers (the card's own `a` still answers the one
/// call), from the phase 3 whole-branch review:
/// - a first word in [`WRAPPERS`] (by name or by path, `/usr/bin/timeout`) suggests nothing:
///   `timeout 5 cargo test` offered `timeout 5 *`, which would then allow `timeout 5 rm -rf src`;
///   nor does a first word that is an assignment (`LC_ALL=C cargo test`), for the same reason;
/// - a second word that is not bare (an option) with a bare word anywhere after it suggests
///   nothing: the first word alone would span every subcommand the option was hiding, so
///   `git -C sub log` offered `git *`, which would then allow `git push --force`. With nothing bare
///   after the options (`uname -a`), the first word alone is still offered.
pub fn suggest(command: &str) -> Option<PrefixRule> {
    let words: Vec<&str> = command.split_whitespace().collect();
    let first = *words.first()?;
    if runs_another_program(first) {
        return None;
    }
    let mut rule = vec![first.to_string()];
    if let Some(second) = words.get(1) {
        if is_bare_word(second) {
            rule.push(second.to_string());
        } else if words[2..].iter().any(|w| is_bare_word(w)) {
            return None;
        }
    }
    Some(PrefixRule { words: rule })
}

/// The rule the card offers, or `None` when no rule could take effect: only a `Bash` call the
/// classifier refused for one of the two replaceable reasons (ruling 16). A compound command, a path
/// outside, a symlink out, `cd`, `git --output`... get no offer, because a rule would change nothing.
pub fn offer(tool_name: &str, input: &Value, project_root: &Path) -> Option<PrefixRule> {
    if tool_name != "Bash" {
        return None;
    }
    let classification = classify_permission_request(tool_name, input, project_root);
    if !classification.needs_a_human() || !REPLACEABLE_BY_A_RULE.contains(&classification.reason) {
        return None;
    }
    let Some(Value::String(command)) = input.get("command") else {
        return None;
    };
    suggest(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_rule_round_trips_claude_codes_own_syntax() {
        let rule = PrefixRule::parse("Bash(git log *)").expect("Claude Code's syntax parses");
        assert_eq!(rule.to_rule_string(), "Bash(git log *)");
        assert_eq!(rule.display(), "git log *");
        assert_eq!(
            PrefixRule::parse("Bash( cargo   test  *)").unwrap().to_rule_string(),
            "Bash(cargo test *)"
        );
    }

    #[test]
    fn anything_but_a_bash_prefix_rule_is_not_a_rule() {
        for text in [
            "Bash(git log)",   // exact form: not in v1
            "Bash(git log*)",  // no space before *: not a prefix rule in Claude Code either
            "Bash(*)",         // no words
            "Read(src/*)",     // another tool
            "Bash(git * log)", // a star anywhere but last
            "Bash(git log *",  // unclosed
            "",
        ] {
            assert_eq!(PrefixRule::parse(text), None, "{text:?}");
        }
    }

    /// A rules file is plain text a person may edit, so `parse` must refuse what the card would
    /// never offer: a rule whose first word runs another program allows whatever program follows.
    #[test]
    fn a_rule_over_a_program_that_runs_another_is_never_read() {
        for text in [
            "Bash(env *)",
            "Bash(sh *)",
            "Bash(timeout *)",
            "Bash(timeout 5 *)",
            "Bash(sudo rm *)",
            "Bash(/usr/bin/env *)",
            "Bash(nohup cargo *)",
            "Bash(LC_ALL=C *)",
            "Bash(FOO=1 env *)",
            "Bash(LC_ALL=C rm *)",
            "Bash(A+=x *)",
        ] {
            assert_eq!(PrefixRule::parse(text), None, "{text:?}");
        }
        for wrapper in WRAPPERS {
            let text = format!("Bash({wrapper} *)");
            assert_eq!(PrefixRule::parse(&text), None, "{text:?}");
        }
        for text in [
            "Bash(git log *)",
            "Bash(cargo env *)",
            "Bash(envsubst *)",
            "Bash(git log --format=%H *)",
        ] {
            assert!(PrefixRule::parse(text).is_some(), "{text:?}");
        }
    }

    #[test]
    fn a_rule_matches_whole_leading_words_only() {
        let rule = PrefixRule::parse("Bash(cargo test *)").unwrap();
        assert!(rule.matches_command("cargo test"));
        assert!(rule.matches_command("cargo test --lib"));
        assert!(rule.matches_command("  cargo   test  -p agent"));
        assert!(!rule.matches_command("cargo testx"));
        assert!(!rule.matches_command("cargo"));
        assert!(!rule.matches_command("cargo build"));
    }

    #[test]
    fn the_suggestion_is_the_first_word_and_a_bare_second_word() {
        assert_eq!(suggest("git log --oneline").unwrap().display(), "git log *");
        assert_eq!(suggest("cargo test --lib").unwrap().display(), "cargo test *");
        assert_eq!(
            suggest("uname -a").unwrap().display(),
            "uname *",
            "a second word starting with - is not taken"
        );
        assert_eq!(suggest("make").unwrap().display(), "make *");
        assert_eq!(suggest("npm run build").unwrap().display(), "npm run *");
        assert_eq!(
            suggest("cp a.txt b.txt").unwrap().display(),
            "cp a.txt *",
            "a.txt is a bare word"
        );
        assert_eq!(suggest("   ").map(|r| r.display()), None);
    }

    /// Review finding (phase 3): the suggestion could span far more than the approved command. An
    /// option ahead of the subcommand (`git -C sub log`) left only the first word, so the offer was
    /// `git *` -- which would then allow `git push --force` in the tree. A wrapper (`timeout 5 cargo
    /// test`) offered `timeout 5 *`, which would then allow `timeout 5 rm -rf src`. Neither is
    /// offered now; the card's own `a` still answers the one call.
    #[test]
    fn no_rule_is_offered_that_would_span_every_subcommand_or_any_program() {
        let root = std::env::temp_dir().join(format!("agent-permission-rules-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        // Canonical, as a session's root is: the policy cards a root that does not resolve to itself
        // (macOS's `$TMPDIR` is under `/var -> private/var`).
        let root = root.canonicalize().unwrap();
        let bash = |command: &str| offer("Bash", &json!({ "command": command }), &root).map(|r| r.display());

        // The shapes the review reproduced, each still a card the policy refused for a
        // replaceable reason -- so without this check they WOULD be offered.
        for command in ["timeout 5 cargo test", "rm -rf sub"] {
            let c = classify_permission_request("Bash", &json!({ "command": command }), &root);
            assert!(
                c.needs_a_human() && REPLACEABLE_BY_A_RULE.contains(&c.reason),
                "{command}: {c:?}"
            );
        }
        // `git -C sub log` is guarded twice since the round-3 follow-up (2026-09-28): the policy's
        // own reason for a leading git global option is no longer one a rule replaces, AND the
        // suggestion still offers nothing, each checked on its own.
        let c = classify_permission_request("Bash", &json!({ "command": "git -C sub log" }), &root);
        assert!(c.needs_a_human() && !REPLACEABLE_BY_A_RULE.contains(&c.reason), "{c:?}");
        assert_eq!(suggest("git -C sub log"), None, "an option hides the subcommand");
        assert_eq!(bash("git -C sub log"), None, "an option hides the subcommand");
        assert_eq!(bash("timeout 5 cargo test"), None, "a wrapper runs whatever follows it");
        assert_eq!(
            suggest("/usr/bin/timeout 5 cargo test"),
            None,
            "a wrapper named by its path"
        );
        assert_eq!(suggest("/usr/bin/env cargo test"), None, "a wrapper named by its path");
        assert_eq!(bash("rm -rf sub"), None, "`rm *` is not what approving one rm meant");
        // A leading assignment runs whatever program follows it, as `env` does. Each of these is a
        // card for a replaceable reason, so only the suggestion stands between it and a rule.
        for command in ["LC_ALL=C", "LC_ALL=C -x", "LC_ALL=C rm -rf sub", "FOO=1 env rm -rf sub"] {
            let c = classify_permission_request("Bash", &json!({ "command": command }), &root);
            if c.needs_a_human() && REPLACEABLE_BY_A_RULE.contains(&c.reason) {
                assert_eq!(bash(command), None, "{command}: an assignment runs whatever follows");
            }
            assert_eq!(suggest(command), None, "{command}: an assignment runs whatever follows");
        }
        for command in ["LC_ALL=C", "LC_ALL=C -x"] {
            let c = classify_permission_request("Bash", &json!({ "command": command }), &root);
            assert!(
                c.needs_a_human() && REPLACEABLE_BY_A_RULE.contains(&c.reason),
                "{command}: {c:?}"
            );
        }
        for wrapper in [
            "nice", "nohup", "sudo", "env", "bash", "sh", "xargs", "command", "exec", "time",
        ] {
            assert_eq!(suggest(&format!("{wrapper} cargo test")), None, "{wrapper}");
        }

        // Unchanged: a bare subcommand, and an option with nothing after it.
        assert_eq!(bash("git push origin main").as_deref(), Some("git push *"));
        assert_eq!(bash("cargo test --lib").as_deref(), Some("cargo test *"));
        assert_eq!(suggest("uname -a").map(|r| r.display()).as_deref(), Some("uname *"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_rule_set_matches_only_bash_calls_and_deduplicates() {
        let rules = PrefixRules::default()
            .with(PrefixRule::parse("Bash(npm ci *)").unwrap())
            .with(PrefixRule::parse("Bash(npm ci *)").unwrap());
        assert_eq!(rules.rules().len(), 1);
        assert!(rules.matches("Bash", &json!({ "command": "npm ci" })));
        assert!(!rules.matches("Read", &json!({ "command": "npm ci" })));
        assert!(!rules.matches("Bash", &json!({ "command": ["npm", "ci"] })));
        assert!(!PrefixRules::default().matches("Bash", &json!({ "command": "npm ci" })));
        let two = PrefixRules::new(vec![
            PrefixRule::parse("Bash(git log *)").unwrap(),
            PrefixRule::parse("Bash(npm *)").unwrap(),
        ]);
        assert_eq!(
            two.matching_rule("Bash", &json!({ "command": "npm ci" }))
                .map(PrefixRule::to_rule_string),
            Some("Bash(npm *)".to_string())
        );
        assert_eq!(two.matching_rule("Bash", &json!({ "command": "ls" })), None);
    }
}
