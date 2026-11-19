//! Black-box checks of the git shim's classifier: forms it does not recognise are refused, forms it does keep running.
use eitri_core::remote_sync::git_shim::{classify, reads_stdin, Route};
use std::ffi::OsString;
use std::path::Path;

fn route(words: &[&str]) -> Route {
    let args: Vec<OsString> = words.iter().map(OsString::from).collect();
    classify(&args, Path::new("/srv/project"))
}

fn name(route: &Route) -> String {
    let text = format!("{route:?}");
    text.split(|c: char| !c.is_alphanumeric())
        .next()
        .unwrap_or("")
        .to_owned()
}

fn refused(route: &Route) -> bool {
    matches!(name(route).as_str(), "RefuseEditor" | "RefuseUnknown")
}

fn runs(route: &Route) -> bool {
    matches!(name(route).as_str(), "Local" | "Forward")
}

#[test]
fn aliases_and_unlisted_subcommands_are_refused_as_unknown() {
    let mut failures = vec![];
    for words in [
        &["my-alias"][..],
        &["unknown"],
        &["clean"],
        &["clean", "-fd"],
        &["clean", "-n"],
        &["gc"],
        &["fsck"],
        &["bisect", "start"],
        &["submodule", "update"],
        &["filter-branch"],
        &["format-patch", "HEAD~1"],
        &["init"],
        &["clone", "example-url"],
        &["update-index", "--assume-unchanged", "file"],
        &["hash-object", "-w", "file"],
        &["replace", "HEAD", "HEAD~1"],
        &["prune"],
        &["repack"],
        &["maintenance", "run"],
        &["sparse-checkout", "set", "dir"],
        &["Status"],
        &["stat"],
    ] {
        let got = route(words);
        if name(&got) != "RefuseUnknown" {
            failures.push(format!("{words:?}: {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_form_never_seen_is_refused() {
    let mut failures = vec![];
    for words in [
        &["status", "--frobnicate"][..],
        &["log", "--definitely-not-a-git-option"],
        &["diff", "--no-such-flag", "file"],
        &["show", "--made-up=1"],
        &["commit", "--frobnicate", "-m", "message"],
        &["add", "--made-up", "file"],
        &["reset", "--made-up"],
        &["checkout", "--made-up", "topic"],
        &["merge", "--made-up", "topic"],
        &["stash", "push", "--made-up"],
        &["rebase", "--made-up", "main"],
        &["branch", "--made-up"],
        &["tag", "--made-up", "v1"],
        &["restore", "--made-up", "file"],
        &["grep", "--made-up", "word"],
    ] {
        let got = route(words);
        if name(&got) != "RefuseUnknown" {
            failures.push(format!("{words:?}: {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn the_named_bypass_forms_are_refused() {
    let mut failures = vec![];
    for words in [
        &["commit", "--include", "file", "-m", "message", "--edit"][..],
        &["commit", "-p", "-m", "message"],
        &["commit", "--dry-run", "--interactive", "-m", "message"],
        &["revert", "--no-edit", "--edit", "HEAD"],
        &["tag", "-u", "key", "v1"],
        &["tag", "-a", "--edit", "v1"],
        &["notes", "edit"],
        &["notes", "add", "-C", "HEAD"],
        &["grep", "-f", "-", "file"],
        &["blame", "--contents", "/srv/project/input", "file"],
        &["rev-parse", "--parseopt", "--"],
        &["log", "--grep", "x", "--output=out.txt"],
    ] {
        let got = route(words);
        if !refused(&got) {
            failures.push(format!("{words:?}: {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn notes_reedit_refuses_even_with_a_message() {
    for words in [
        &["notes", "add", "-m", "extra", "--reedit-message=HEAD"][..],
        &["notes", "append", "-m", "extra", "--reedit-message", "HEAD"],
        &["notes", "add", "-m", "extra", "-cHEAD"],
    ] {
        assert_eq!(route(words), Route::RefuseEditor, "{words:?}");
    }
}

#[test]
fn editor_read_forms_stay_local() {
    for words in [
        &["ls-files", "--stage", "--others", "--exclude-standard", "--eol", "file"][..],
        &["blame", "--contents", "-", "--", "file"],
        &["blame", "--contents=-", "--", "file"],
        &["show", "-s", "--format=%H", "HEAD"],
        &["log", "--date=iso"],
        &["diff", "--color-words"],
        &["diff", "--color-words=regex"],
        &["status", "--ignore-submodules=all"],
    ] {
        assert!(matches!(route(words), Route::Local { .. }), "{words:?}");
    }
    for cmd in ["status", "diff", "commit", "stash", "worktree"] {
        for help in ["--help", "-h"] {
            assert_eq!(route(&[cmd, help]), Route::Local { extra: vec![] }, "{cmd} {help}");
        }
    }
    assert_eq!(route(&["my-alias", "-h"]), Route::RefuseUnknown);
    for words in [
        &["blame", "--contents", "-", "file"][..],
        &["blame", "--contents=-", "file"],
    ] {
        let args: Vec<_> = words.iter().map(OsString::from).collect();
        assert!(reads_stdin(&args, false), "{words:?}");
    }
}

#[test]
fn implicit_reflog_and_stash_modes_forward() {
    for words in [&["reflog", "main"][..], &["reflog", "-n5", "HEAD"]] {
        assert_eq!(
            route(words),
            Route::Forward {
                whole_tree: false,
                paths: vec![]
            }
        );
    }
    assert_eq!(
        route(&["stash", "--", "path"]),
        Route::Forward {
            whole_tree: true,
            paths: vec!["path".into()]
        }
    );
    assert_eq!(route(&["stash", "path"]), Route::RefuseUnknown);
}

#[test]
fn stash_value_options_are_checked_against_the_mode() {
    for words in [
        &["stash", "pop", "--message=x"][..],
        &["stash", "pop", "--message", "x"],
        &["stash", "pop", "--pathspec-from-file=paths"],
    ] {
        assert_eq!(route(words), Route::RefuseUnknown, "{words:?}");
    }
    assert!(matches!(
        route(&["stash", "push", "--message=x"]),
        Route::Forward { .. }
    ));
}

#[test]
fn commit_include_and_dry_run_do_not_require_an_editor() {
    // `--include` also commits what the index already holds, so every conflict is checked.
    for option in ["-i", "--include"] {
        assert_eq!(
            route(&["commit", option, "-m", "message", "file"]),
            Route::Forward {
                whole_tree: true,
                paths: vec!["file".into()]
            }
        );
    }
    assert!(matches!(
        route(&["commit", "-m", "message", "--dry-run"]),
        Route::Forward { .. }
    ));
    assert_eq!(route(&["commit", "--include", "file"]), Route::RefuseEditor);
}

#[test]
fn branch_contains_with_attached_revision_stays_local() {
    assert_eq!(route(&["branch", "--contains=HEAD"]), Route::Local { extra: vec![] });
}

#[test]
fn log_with_attached_grep_checks_output_path() {
    assert_eq!(
        route(&["log", "--grep=fix", "--output=report"]),
        Route::Forward {
            whole_tree: false,
            paths: vec!["report".into()]
        }
    );
}

#[test]
fn diff_unified_does_not_consume_the_next_option() {
    assert_eq!(route(&["diff", "-U", "--future-option"]), Route::RefuseUnknown);
    for words in [&["diff", "-U", "--stat"][..], &["diff", "-U5", "--stat"]] {
        assert!(matches!(route(words), Route::Local { .. }), "{words:?}");
    }
}

#[test]
fn remote_mode_cannot_come_after_the_option_terminator() {
    assert_eq!(
        route(&["remote", "--", "get-url", "--future-option", "origin"]),
        Route::RefuseUnknown
    );
    assert!(matches!(
        route(&["remote", "get-url", "--all", "origin"]),
        Route::Local { .. }
    ));
}

#[test]
fn config_short_file_option_reads_piped_stdin() {
    for words in [&["config", "-f", "-", "--list"][..], &["config", "-f-", "--list"]] {
        let args: Vec<_> = words.iter().map(OsString::from).collect();
        assert!(matches!(route(words), Route::Forward { .. }), "{words:?}");
        assert!(reads_stdin(&args, false), "{words:?}");
        assert!(!reads_stdin(&args, true), "{words:?}");
    }
}

#[test]
fn grep_stdin_refusal_is_not_an_editor_refusal() {
    for words in [&["grep", "-f", "-", "file"][..], &["grep", "--file=-", "file"]] {
        assert_eq!(route(words), Route::RefuseUnknown, "{words:?}");
    }
}

#[test]
fn tag_local_user_only_needs_an_editor_without_a_message() {
    for option in ["-u", "--local-user"] {
        assert!(matches!(
            route(&["tag", option, "KEY", "-m", "msg", "v1"]),
            Route::Forward { .. }
        ));
        assert_eq!(route(&["tag", option, "KEY", "v1"]), Route::RefuseEditor);
    }
}

#[test]
fn common_forms_still_run() {
    let mut failures = vec![];
    for words in [
        &["status"][..],
        &["status", "-sb"],
        &["status", "--porcelain=v2", "--branch"],
        &["log", "--oneline", "--graph", "--decorate"],
        &["log", "-n", "5", "--stat"],
        &["log", "-p", "--", "file"],
        &["diff", "--stat"],
        &["diff", "--name-only", "HEAD~1"],
        &["diff", "--cached"],
        &["show", "--stat", "HEAD"],
        &["blame", "-L", "1,5", "file"],
        &["ls-files", "--others", "--exclude-standard"],
        &["rev-parse", "--show-toplevel"],
        &["branch", "-vv"],
        &["config", "--get", "user.name"],
        &["add", "file"],
        &["add", "-A"],
        &["commit", "-m", "message"],
        &["commit", "-am", "message"],
        &["checkout", "topic"],
        &["checkout", "-b", "topic"],
        &["switch", "main"],
        &["restore", "file"],
        &["reset", "--hard"],
        &["reset", "HEAD", "--", "file"],
        &["merge", "topic"],
        &["rebase", "main"],
        &["pull"],
        &["cherry-pick", "HEAD"],
        &["stash", "push"],
        &["stash", "pop"],
        &["tag", "v1"],
        &["rm", "file"],
        &["mv", "a", "b"],
    ] {
        let got = route(words);
        if !runs(&got) {
            failures.push(format!("{words:?}: {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn unknown_short_flags_and_abbreviated_long_options_are_refused() {
    let mut failures = vec![];
    for words in [
        &["status", "-Z"][..],
        &["commit", "--amen", "-m", "x"],
        &["log", "--outp=x"],
        &["diff", "--no-ext"],
        &["show", "--textc"],
    ] {
        let got = route(words);
        if !refused(&got) {
            failures.push(format!("{words:?}: {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Global options that choose a program, a config value or another repository must never make a read run on the
/// laptop's replica.
#[test]
fn global_options_that_change_what_runs_never_read_locally() {
    let mut failures = vec![];
    for words in [
        &["--exec-path=/tmp/x", "status"][..],
        &["--config-env=core.fsmonitor=EVIL", "status"],
        &["--namespace=x", "log"],
        &["-c"],
    ] {
        let got = route(words);
        if name(&got) == "Local" {
            failures.push(format!("{words:?}: {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn an_empty_command_line_runs_nothing_from_the_lists() {
    let got = route(&[]);
    assert!(!runs(&got), "{got:?}");
}
