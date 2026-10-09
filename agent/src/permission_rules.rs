//! "Always allow `git log *` in this project". Claude Code's own rule
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

use crate::permission_policy::{bash_input_is_only_a_command, classify_permission_request, REPLACEABLE_BY_A_RULE};

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
        Self::parse_or_refusal(text).ok()
    }

    /// [`Self::parse`], saying why a text is not a rule, so the rules file's reader can log the
    /// reason that applies rather than one that fits only some of them.
    pub fn parse_or_refusal(text: &str) -> Result<Self, RuleRefusal> {
        let body = text
            .trim()
            .strip_prefix("Bash(")
            .and_then(|rest| rest.strip_suffix(')'))
            .map(str::trim_end)
            .and_then(|inner| inner.strip_suffix(" *"))
            .ok_or(RuleRefusal::NotAPrefixRule)?;
        let words: Vec<String> = ascii_words(body).map(str::to_string).collect();
        if words.is_empty() || words.iter().any(|w| w.contains('*')) {
            return Err(RuleRefusal::NotAPrefixRule);
        }
        if words.iter().any(|w| !is_ascii_graphic_word(w)) {
            return Err(RuleRefusal::NotPrintableAscii);
        }
        if runs_another_program(&words[0]) {
            return Err(RuleRefusal::RunsAnotherProgram);
        }
        Ok(PrefixRule { words })
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
    ///
    /// A command holding a newline or a carriage return matches nothing: to the shell a newline
    /// ends one command and starts the next, so `git status<LF>rm -rf ~` would otherwise match
    /// `git status *` and run a second program the rule never named.
    pub fn matches_command(&self, command: &str) -> bool {
        if command.contains(['\n', '\r']) {
            return false;
        }
        let mut words = ascii_words(command);
        self.words.iter().all(|w| words.next() == Some(w.as_str()))
    }
}

/// Why a text is not a rule ([`PrefixRule::parse_or_refusal`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleRefusal {
    /// Not `Bash(<words> *)`: another tool, the exact form, no words, or a `*` anywhere but last.
    NotAPrefixRule,
    /// A word holding anything but printable ASCII (a non-ASCII letter or space, a control
    /// character), so the words shown are not certainly the words the shell runs.
    NotPrintableAscii,
    /// The first word runs another program (`env`, `sh`, `timeout`, an assignment such as
    /// `LC_ALL=C`), so the rule would allow whatever program follows.
    RunsAnotherProgram,
}

impl RuleRefusal {
    /// One line for a log, naming why the text was refused.
    pub fn describe(self) -> &'static str {
        match self {
            RuleRefusal::NotAPrefixRule => "not a Bash(<words> *) prefix rule",
            RuleRefusal::NotPrintableAscii => {
                "a word holds a character other than printable ASCII, so the rule may not name the command that runs"
            }
            RuleRefusal::RunsAnotherProgram => {
                "its first word runs another program (env, sh, timeout, an assignment...), so it would allow any program"
            }
        }
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
    /// answered it rather than only stderr saying "a prefix rule".
    ///
    /// A call carrying more than its command (`dangerouslyDisableSandbox: true`, an unknown key)
    /// matches no rule: the rule was approved for the command's words, not for whatever else the
    /// call asks for.
    pub fn matching_rule(&self, tool_name: &str, input: &Value) -> Option<&PrefixRule> {
        if tool_name != "Bash" || !bash_input_is_only_a_command(input) {
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

/// The words of one simple command as the shell splits them: at space and tab only. A newline is
/// not a word break to the shell but the end of the command, so it is never split at here: a rule
/// holding one is refused as not printable ASCII, a suggestion is never made from a command holding
/// one, and [`PrefixRule::matches_command`] refuses such a command outright. A no-break or other
/// Unicode space, a carriage return or a form feed belongs to the word it sits in (the shell runs
/// `git<NBSP>status` as one program that does not exist), so a rule built or matched over a wider
/// split would name a different command than the one that runs.
fn ascii_words(text: &str) -> impl Iterator<Item = &str> {
    text.split([' ', '\t']).filter(|w| !w.is_empty())
}

/// A word a rule may name: printable ASCII only, so what the card shows is what the rule means.
fn is_ascii_graphic_word(word: &str) -> bool {
    word.chars().all(|c| c.is_ascii_graphic())
}

fn is_bare_word(word: &str) -> bool {
    word.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Suggests the first word plus a bare second word (`^[A-Za-z0-9][A-Za-z0-9._-]*$`).
/// `git log --oneline` suggests `git log *`; `uname -a` suggests `uname *`.
/// A first word in [`WRAPPERS`], by name or path, or containing an assignment suggests
/// nothing: the prefix could allow arbitrary commands after the wrapper or assignment.
/// If the second word is not bare but a later word is, no prefix is offered:
/// `git -C sub log` must not suggest `git *`, which would also allow `git push --force`.
/// With no bare words after the options, the first word alone is still offered.
pub fn suggest(command: &str) -> Option<PrefixRule> {
    // A newline starts another command, which a rule built from the first would then not name.
    if command.contains(['\n', '\r']) {
        return None;
    }
    let words: Vec<&str> = ascii_words(command).collect();
    let first = *words.first()?;
    if runs_another_program(first) || !is_ascii_graphic_word(first) {
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
/// classifier refused for one of the two replaceable reasons. A compound command, a path
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
    fn only_ascii_whitespace_splits_words() {
        // The shell reads `git<NBSP>status` as one word, so no rule may be built from it or match it.
        assert_eq!(suggest("git\u{a0}status foo"), None);
        assert_eq!(suggest("ls\u{2003}-la"), None);
        assert_eq!(suggest("git\rstatus x"), None);
        assert_eq!(suggest("git\u{c}status x"), None);
        assert_eq!(PrefixRule::parse("Bash(git\u{a0}status *)"), None);
        let rule = PrefixRule::parse("Bash(git status *)").unwrap();
        assert!(!rule.matches_command("git\u{a0}status foo"));
        assert!(!rule.matches_command("git\u{3000}status"));
        assert!(!rule.matches_command("git\rstatus foo"));
        assert!(!rule.matches_command("git\u{c}status foo"));
        assert!(rule.matches_command("git \t status foo"));
        assert_eq!(suggest("git status foo").unwrap().display(), "git status *");
    }

    /// To the shell a newline ends one command and starts another, so a command holding one is
    /// never matched by a rule over its first line, nor makes a suggestion, and a rule holding one
    /// is not a rule. Before, words were split at a newline as if it were a blank, and
    /// `git status<LF>rm -rf ~` matched `git status *`.
    #[test]
    fn a_newline_ends_the_command_and_no_rule_matches_across_it() {
        let rule = PrefixRule::parse("Bash(git status *)").unwrap();
        assert!(!rule.matches_command("git status\nrm -rf ~"));
        assert!(!rule.matches_command("git status\n"));
        assert!(!rule.matches_command("git\nstatus"));
        assert!(!rule.matches_command("git status\r\nrm -rf ~"));
        assert!(!rule.matches_command("git status --short\r"));
        assert!(rule.matches_command("git status --short"));
        let rules = PrefixRules::new(vec![rule]);
        assert_eq!(
            rules.matching_rule("Bash", &json!({ "command": "git status\nrm -rf ~" })),
            None
        );
        assert_eq!(suggest("git status\nrm -rf ~"), None);
        assert_eq!(suggest("make\n"), None);
        assert_eq!(
            PrefixRule::parse_or_refusal("Bash(git\nstatus *)"),
            Err(RuleRefusal::NotPrintableAscii)
        );
    }

    /// Each refusal says which rule of the format it broke, so the rules file's log names the
    /// right reason.
    #[test]
    fn a_refused_rule_says_why() {
        for (text, why) in [
            ("Read(src/*)", RuleRefusal::NotAPrefixRule),
            ("Bash(git log)", RuleRefusal::NotAPrefixRule),
            ("Bash(git * log *)", RuleRefusal::NotAPrefixRule),
            ("Bash(git st\u{e4}tus *)", RuleRefusal::NotPrintableAscii),
            ("Bash(git\u{a0}status *)", RuleRefusal::NotPrintableAscii),
            ("Bash(env *)", RuleRefusal::RunsAnotherProgram),
            ("Bash(LC_ALL=C rm *)", RuleRefusal::RunsAnotherProgram),
        ] {
            assert_eq!(PrefixRule::parse_or_refusal(text), Err(why), "{text:?}");
            assert_eq!(PrefixRule::parse(text), None, "{text:?}");
        }
        assert!(PrefixRule::parse_or_refusal("Bash(git log *)").is_ok());
    }

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

    /// Do not suggest prefixes that authorize more than the approved command's subcommand or
    /// program. `git -C sub log` must not offer `git *`, which would also allow `git push
    /// --force`. `timeout 5 cargo test` must not offer `timeout 5 *`, which could run any
    /// program. The card's `a` still approves only this call.
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
        // `git -C sub log` has two independent guards: the global-option reason cannot be
        // replaced by a saved rule, and no prefix rule is suggested. Check both.
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
