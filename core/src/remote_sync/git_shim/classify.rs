use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Local { extra: Vec<&'static str> },
    Forward { whole_tree: bool, paths: Vec<PathBuf> },
    RefuseEditor,
    RefuseUnknown,
    NotRemote,
}

#[derive(Clone, Debug)]
pub(super) struct Invocation {
    pub cwd: PathBuf,
    pub globals: Vec<OsString>,
    pub args: Vec<OsString>,
    pub foreign: bool,
    pub icase_pathspecs: bool,
    pub git_dir: Option<PathBuf>,
    pub work_tree: Option<PathBuf>,
}

impl Invocation {
    pub fn parse(args: &[OsString], cwd: &Path) -> Option<Self> {
        let mut parsed = Self {
            cwd: cwd.into(),
            globals: vec![],
            args: vec![],
            foreign: false,
            icase_pathspecs: false,
            git_dir: None,
            work_tree: None,
        };
        let mut i = 0;
        while i < args.len() {
            let arg = args[i].to_string_lossy();
            if arg == "-C" {
                i += 1;
                let dir = args.get(i)?;
                if !dir.is_empty() {
                    let target = parsed.cwd.join(dir);
                    std::fs::metadata(target.join(".")).ok()?;
                    parsed.cwd = target.canonicalize().ok()?;
                }
            } else if arg == "--" {
                return None;
            } else if arg == "--git-dir"
                || arg == "--work-tree"
                || arg.starts_with("--git-dir=")
                || arg.starts_with("--work-tree=")
            {
                let (name, value) = if let Some((name, _)) = arg.split_once('=') {
                    let value = OsStr::from_bytes(&args[i].as_bytes()[name.len() + 1..]);
                    (name.to_owned(), Some(value.to_owned()))
                } else {
                    i += 1;
                    (arg.into_owned(), args.get(i).cloned())
                };
                parsed.globals.push(name.clone().into());
                if let Some(value) = value {
                    if name == "--git-dir" {
                        parsed.git_dir = Some(PathBuf::from(&value));
                    } else {
                        parsed.work_tree = Some(PathBuf::from(&value));
                    }
                    parsed.globals.push(value);
                } else {
                    parsed.foreign = true;
                }
            } else if global_takes_separate_value(&arg) {
                parsed.globals.push(args[i].clone());
                i += 1;
                if let Some(value) = args.get(i) {
                    parsed.globals.push(value.clone());
                } else {
                    parsed.foreign = true;
                }
                if arg != "-c" {
                    parsed.foreign = true;
                }
            } else if arg == "--bare"
                || arg.split_once('=').is_some_and(|(name, _)| {
                    matches!(
                        name,
                        "--config-env" | "--namespace" | "--super-prefix" | "--attr-source" | "--exec-path"
                    )
                })
            {
                parsed.foreign = true;
                parsed.globals.push(args[i].clone());
            } else if matches!(
                arg.as_ref(),
                "--no-pager"
                    | "--paginate"
                    | "-p"
                    | "-P"
                    | "--literal-pathspecs"
                    | "--no-literal-pathspecs"
                    | "--glob-pathspecs"
                    | "--noglob-pathspecs"
                    | "--icase-pathspecs"
                    | "--no-optional-locks"
                    | "--no-replace-objects"
                    | "--no-lazy-fetch"
                    | "--no-advice"
            ) || arg.starts_with("-c") && arg.len() > 2
            {
                parsed.icase_pathspecs |= arg == "--icase-pathspecs";
                parsed.globals.push(args[i].clone());
            } else {
                // Git's version, help and path/listing queries end global parsing before any later -C.
                break;
            }
            i += 1;
        }
        parsed.git_dir = parsed.git_dir.map(|path| parsed.cwd.join(path));
        parsed.work_tree = parsed.work_tree.map(|path| parsed.cwd.join(path));
        parsed.args = args.get(i..).unwrap_or_default().to_vec();
        Some(parsed)
    }

    pub fn for_project(mut self, root: &Path) -> Self {
        let mut globals = Vec::new();
        let mut values = self.globals.into_iter();
        while let Some(arg) = values.next() {
            if arg == "--git-dir" || arg == "--work-tree" {
                let value = values.next();
                let expected = if arg == "--git-dir" {
                    root.join(".git")
                } else {
                    root.to_path_buf()
                };
                let equal = value.as_ref().is_some_and(|value| {
                    self.cwd
                        .join(value)
                        .canonicalize()
                        .ok()
                        .zip(expected.canonicalize().ok())
                        .is_some_and(|(a, b)| a == b)
                });
                if !equal {
                    self.foreign = true;
                    globals.push(arg);
                    if let Some(value) = value {
                        globals.push(value);
                    }
                }
            } else {
                let takes_value = global_takes_separate_value(&arg.to_string_lossy());
                globals.push(arg);
                if takes_value {
                    if let Some(value) = values.next() {
                        globals.push(value);
                    }
                }
            }
        }
        self.globals = globals;
        self
    }

    pub fn forwarded_args(&self) -> Vec<OsString> {
        self.globals.iter().chain(&self.args).cloned().collect()
    }
}

fn global_takes_separate_value(arg: &str) -> bool {
    matches!(
        arg,
        "-c" | "--config-env" | "--namespace" | "--super-prefix" | "--attr-source" | "--shallow-file"
    )
}

pub(super) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Classifies the Git arguments for a mirrored repository.
pub fn classify(args: &[OsString], root: &Path) -> Route {
    Invocation::parse(args, root).map_or(Route::NotRemote, |parsed| {
        let parsed = parsed.for_project(root);
        classify_invocation(&parsed.args, !parsed.foreign)
    })
}

pub(super) fn classify_invocation(args: &[OsString], allow_local: bool) -> Route {
    let words: Vec<_> = args.iter().map(|s| s.to_string_lossy()).collect();
    let Some(cmd) = words.first().map(|s| s.as_ref()) else {
        return Route::RefuseUnknown;
    };
    if words.len() == 2
        && matches!(words[1].as_ref(), "--help" | "-h")
        && (allowed_form(cmd, None, 0).is_some() || cmd == "worktree")
    {
        return if allow_local {
            Route::Local { extra: vec![] }
        } else {
            forward(false, vec![])
        };
    }
    let parsed = CommandOptions::parse(cmd, &words);
    let first_operand = parsed.operands.first().copied();
    let end = parsed.end;
    let path_separator = words.get(end).is_some_and(|word| word == "--");
    if cmd == "remote" && first_operand.is_some_and(|index| index > end) {
        return Route::RefuseUnknown;
    }
    // Stash and reflog select a subcommand only when it immediately follows the command name.
    let mode = first_operand
        .filter(|&index| !matches!(cmd, "stash" | "reflog") || index == 1 && index < end)
        .map(|index| words[index].as_ref());
    let operands_before_end = parsed.operands.partition_point(|&index| index < end);
    let Some((short_flags, long_flags)) = allowed_form(cmd, mode, operands_before_end) else {
        return Route::RefuseUnknown;
    };
    if parsed.invalid
        || cmd == "config" && !config_operation_allowed(mode, &parsed)
        || parsed.short.iter().any(|&flag| !short_flags.contains(flag))
        || parsed.long.iter().any(|&(name, value)| {
            let takes_value = long_takes_value(cmd, name);
            ((!takes_value || matches!(cmd, "stash" | "reflog"))
                && !long_flags
                    .split_ascii_whitespace()
                    .any(|flag| name.strip_prefix("--") == Some(flag)))
                || (takes_value && value.is_none())
                || (value.is_some() && !takes_value && !long_accepts_attached_value(cmd, name))
        })
    {
        return Route::RefuseUnknown;
    }
    if cmd == "grep" && (parsed.short_values.contains(&('f', "-")) || parsed.long.contains(&("--file", Some("-"))))
        || matches!(cmd, "blame" | "annotate")
            && parsed
                .long
                .iter()
                .any(|&(name, value)| name == "--contents" && value != Some("-"))
    {
        return Route::RefuseUnknown;
    }
    let options = &words[1..end];
    let trailing_operands = end + 1 < words.len();
    let has = |flag: &str| parsed.long.iter().any(|(name, _)| *name == flag);
    let short = |flag: char| parsed.short.contains(&flag);
    let message = has("--message") || has("--file") || short('m') || short('F');
    let plain_fixup = parsed
        .long
        .iter()
        .rev()
        .find(|(name, _)| *name == "--fixup")
        .and_then(|(_, value)| *value)
        .is_some_and(|value| !value.starts_with("amend:") && !value.starts_with("reword:"));
    let commit_message = message || has("--reuse-message") || short('C') || has("--no-edit") || plain_fixup;
    let interactive = match cmd {
        "commit" => {
            !commit_message && !has("--dry-run")
                || "ep".chars().any(short)
                || ["--edit", "--interactive", "--patch"].into_iter().any(has)
        }
        "revert" => {
            has("--continue")
                || has("--edit")
                || short('e')
                || !(short('n')
                    || ["--no-edit", "--no-commit", "--abort", "--quit", "--skip"]
                        .into_iter()
                        .any(has))
        }
        "merge" | "cherry-pick" => has("--continue") || has("--edit") || short('e'),
        "rebase" => has("--continue") || has("--interactive") || short('i') || has("--edit-todo"),
        "tag" => {
            has("--edit")
                || ("asu".chars().any(short) || ["--annotate", "--sign", "--local-user"].into_iter().any(has))
                    && !message
        }
        "pull" => {
            has("--edit")
                || short('e')
                || parsed
                    .long
                    .iter()
                    .any(|&(name, value)| name == "--rebase" && matches!(value, Some("interactive" | "i")))
                || parsed
                    .short_values
                    .iter()
                    .any(|&(flag, value)| flag == 'r' && matches!(value, "interactive" | "i"))
        }
        "notes" => {
            mode == Some("edit")
                || matches!(mode, Some("add" | "append")) && (!message || short('c') || has("--reedit-message"))
        }
        "config" => short('e') || has("--edit") || mode == Some("edit"),
        "add" => short('p') || short('i') || short('e') || has("--patch") || has("--interactive") || has("--edit"),
        "checkout" | "reset" | "restore" => short('p') || has("--patch"),
        "stash" => !matches!(mode, Some("show" | "list")) && (short('p') || has("--patch")),
        _ => false,
    };
    if interactive {
        return Route::RefuseEditor;
    }
    // Implicit stash push requires a double hyphen before any pathspecs.
    if cmd == "stash" && mode.is_none() && (operands_before_end > 0 || !path_separator && !parsed.operands.is_empty()) {
        return Route::RefuseUnknown;
    }
    let output_files = revision_output_files(cmd, args);
    if cmd == "log" && parsed.separate_grep && (has("--output") || !output_files.is_empty()) {
        return Route::RefuseUnknown;
    }
    let external = words[1..].iter().take_while(|word| *word != "--").any(|word| {
        let name = word.split('=').next().unwrap_or(word);
        matches!(
            name,
            "--ext-diff" | "--textconv" | "--filters" | "--filter" | "--textcon"
        )
    });
    let grep_pager = cmd == "grep"
        && (short('O')
            || parsed
                .long
                .iter()
                .any(|(name, _)| name.len() > 2 && "--open-files-in-pager".starts_with(name)));
    let local = match cmd {
        "status" | "diff" | "diff-files" | "diff-index" | "diff-tree" | "log" | "shortlog" | "show" | "blame"
        | "annotate" | "grep" | "rev-parse" | "rev-list" | "ls-files" | "ls-tree" | "for-each-ref" | "show-ref"
        | "merge-base" | "name-rev" | "describe" | "check-ignore" | "check-attr" | "var" | "--version" | "help" => true,
        "cat-file" => cat_file_read(options),
        "symbolic-ref" => {
            parsed.operands.len() == 1
                && options
                    .iter()
                    .all(|s| !s.starts_with('-') || matches!(s.as_ref(), "--short" | "--quiet" | "-q"))
        }
        "branch" => {
            options.first().is_some_and(|s| s == "--list") && options[1..].iter().all(|s| !s.starts_with('-'))
                || (!trailing_operands
                    || options
                        .iter()
                        .any(|s| !matches!(s.as_ref(), "-v" | "-vv" | "--verbose")))
                    && options.iter().all(|s| {
                        matches!(
                            s.as_ref(),
                            "-l" | "--list"
                                | "-a"
                                | "--all"
                                | "-r"
                                | "--remotes"
                                | "-v"
                                | "-vv"
                                | "--verbose"
                                | "--show-current"
                        ) || s.starts_with("--contains=")
                    })
                || options.first().is_some_and(|s| s == "--contains")
                    && (options.len() == 1 || options.len() == 2 && !options[1].starts_with('-'))
        }
        "tag" => {
            options.is_empty() && parsed.operands.is_empty()
                || options.first().is_some_and(|s| s == "-l" || s == "--list")
                    && options[1..].iter().all(|s| !s.starts_with('-'))
        }
        "remote" => {
            (options.is_empty() || options == ["-v"]) && parsed.operands.is_empty()
                || options.first().is_some_and(|s| s == "get-url")
        }
        "stash" => options.first().is_some_and(|s| s == "list" || s == "show"),
        "reflog" => options.is_empty() && parsed.operands.is_empty() || options.first().is_some_and(|s| s == "show"),
        "worktree" => options.first().is_some_and(|s| s == "list"),
        "config" => config_read(options),
        _ => false,
    };
    if local && allow_local && !has("--output") && output_files.is_empty() && !external && !grep_pager {
        let diff = matches!(
            cmd,
            "diff"
                | "diff-files"
                | "diff-index"
                | "diff-tree"
                | "log"
                | "show"
                | "blame"
                | "annotate"
                | "grep"
                | "reflog"
        ) || cmd == "stash" && options.first().is_some_and(|s| s == "show" || s == "list");
        return Route::Local {
            extra: if diff {
                vec!["--no-ext-diff", "--no-textconv"]
            } else {
                vec![]
            },
        };
    }
    let mut paths = if cmd == "commit" {
        commit_paths(args, &parsed)
    } else {
        path_arguments(cmd, args, &parsed, path_separator)
    };
    paths.extend(output_files.into_iter().flatten());
    let whole = match cmd {
        // `--include` commits what the index already holds as well as the named paths.
        "commit" => paths.is_empty() || short('a') || has("--all") || short('i') || has("--include"),
        "add" => paths.is_empty() || paths.iter().any(|p| p == Path::new(".")),
        "stash" => true,
        "checkout" => paths.is_empty(),
        "switch" => true,
        "restore" => paths.is_empty(),
        "reset" => !path_separator || has("--hard") || has("--merge") || has("--keep") || paths.is_empty(),
        "merge" | "rebase" | "pull" | "cherry-pick" | "revert" | "am" => true,
        "apply" => !has("--cached"),
        "rm" | "mv" => paths.is_empty(),
        _ => false,
    };
    forward(whole || has("--pathspec-from-file"), paths)
}

fn forward(whole_tree: bool, paths: Vec<PathBuf>) -> Route {
    Route::Forward { whole_tree, paths }
}

fn allowed_form(cmd: &str, mode: Option<&str>, operands_before_end: usize) -> Option<(&'static str, &'static str)> {
    const DIFF: &str = "patch no-patch stat numstat shortstat name-only name-status compact-summary summary raw \
        patch-with-stat patch-with-raw check exit-code quiet color no-color unified inter-hunk-context \
        no-ext-diff ext-diff no-textconv textconv find-renames no-renames find-copies full-index binary \
        cached staged no-index relative no-relative ignore-space-change ignore-all-space ignore-space-at-eol \
        ignore-blank-lines word-diff color-words no-prefix output";
    const HISTORY: &str = "oneline graph decorate no-decorate all branches tags remotes first-parent \
        no-merges merges reverse topo-order date-order author-date-order follow walk-reflogs format pretty \
        patch no-patch stat numstat shortstat name-only name-status summary raw color no-color abbrev-commit \
        no-abbrev-commit left-right cherry-pick boundary parents children count objects no-object-names \
        no-ext-diff ext-diff no-textconv textconv find-renames no-renames full-index binary \
        output grep author committer since until max-count";
    Some(match cmd {
        "status" => ("sbzuv", "short branch porcelain long ahead-behind no-ahead-behind show-stash untracked-files ignored ignore-submodules null verbose"),
        "diff" | "diff-files" | "diff-index" | "diff-tree" => ("psqzMCRwbuU", DIFF),
        "log" | "rev-list" => ("pnqzMCRwbuU", HISTORY),
        "show" => ("psnqzMCRwbuU", HISTORY),
        "shortlog" => ("snecw", "summary numbered email committer all no-merges merges format pretty"),
        "blame" | "annotate" => ("LwlsefpMChi", "porcelain line-porcelain incremental show-name show-number root reverse first-parent ignore-whitespace no-ext-diff no-textconv"),
        "grep" => ("einIlLwFEPGhzHcqaAvfBomCO", "cached no-index untracked exclude-standard no-exclude-standard recurse-submodules \
            ignore-case word-regexp invert-match full-name line-number column files-with-matches files-without-match \
            count quiet null name-only fixed-strings basic-regexp extended-regexp perl-regexp text textconv no-textconv \
            break heading color no-color open-files-in-pager open and or not all-match"),
        "rev-parse" => ("q", "verify quiet short symbolic symbolic-full-name abbrev-ref show-toplevel show-prefix \
            show-cdup show-object-format show-superproject-working-tree git-dir absolute-git-dir git-common-dir \
            is-inside-work-tree is-inside-git-dir is-bare-repository is-shallow-repository local-env-vars \
            sq no-revs no-flags flags revs-only default"),
        "ls-files" => ("cdmosutvzfikxX", "cached deleted modified others ignored stage unmerged killed directory \
            no-empty-directory exclude-standard full-name error-unmatch deduplicate sparse resolve-undo eol"),
        "ls-tree" => ("drtlz", "name-only name-status object-only full-name full-tree long abbrev"),
        "for-each-ref" => ("", "shell perl python tcl ignore-case omit-empty contains no-contains merged no-merged"),
        "show-ref" => ("qds", "head heads branches tags verify quiet dereference hash abbrev exists exclude-existing"),
        "merge-base" => ("a", "all octopus independent is-ancestor fork-point"),
        "name-rev" => ("", "name-only tags all always stdin annotate-stdin undefined no-undefined"),
        "describe" => ("", "all tags contains dirty broken long first-parent always exact-match debug abbrev"),
        "check-ignore" => ("vnqz", "verbose non-matching quiet stdin no-index"),
        "check-attr" => ("az", "all cached stdin"),
        "var" => ("l", ""),
        "--version" if mode.is_none() => ("", "build-options"),
        "help" => ("agimw", "all guides config man info web verbose no-verbose"),
        "cat-file" => ("tsepzZ", "batch batch-check batch-command batch-all-objects buffer follow-symlinks unordered \
            allow-unknown-type filters filter textconv textcon use-mailmap mailmap"),
        "add" => ("uAfvNnpei", "update all no-all force verbose intent-to-add dry-run patch edit interactive \
            ignore-errors ignore-missing sparse refresh renormalize pathspec-file-nul"),
        "am" => ("pC3qsk", "3way no-3way quiet signoff keep keep-non-patch continue skip abort quit show-current-patch"),
        "apply" => ("pCRzv", "cached index check stat numstat summary apply reverse reject recount allow-empty \
            ignore-space-change ignore-whitespace verbose"),
        "branch" => ("larvdfDmMcCq", "list all remotes verbose show-current contains no-contains merged no-merged \
            delete force move copy quiet track no-track unset-upstream"),
        "checkout" => ("bBoqftp", "quiet force detach track no-track ours theirs merge patch recurse-submodules \
            no-recurse-submodules pathspec-file-nul"),
        "switch" => ("cCqftm", "quiet force detach track no-track merge discard-changes guess no-guess \
            recurse-submodules no-recurse-submodules"),
        "cherry-pick" | "revert" => ("emSXnxs", "continue skip abort quit edit no-edit no-commit signoff \
            no-signoff gpg-sign no-gpg-sign allow-empty allow-empty-message ff"),
        "commit" => ("mFCcUSutavqsenipsoz", "quiet verbose reset-author signoff no-signoff edit no-edit status \
            no-status gpg-sign no-gpg-sign all include interactive patch only verify no-verify dry-run short branch \
            ahead-behind no-ahead-behind porcelain long null amend no-post-rewrite untracked-files pathspec-file-nul \
            allow-empty allow-empty-message unified inter-hunk-context"),
        "config" => ("ezlf", "get get-all get-regexp get-urlmatch list add replace-all unset unset-all rename-section \
            remove-section edit null local global system worktree includes no-includes fixed-value bool int path"),
        "merge" => ("mFqevsXS", "continue abort quit edit no-edit ff no-ff ff-only squash no-squash commit no-commit \
            log no-log stat no-stat quiet verbose signoff no-signoff gpg-sign no-gpg-sign allow-unrelated-histories"),
        "mv" => ("fknv", "force dry-run verbose"),
        "notes" if mode.is_none_or(|mode| matches!(mode, "list" | "add" | "append" | "edit" | "show" | "copy" | "remove" | "prune" | "get-ref")) =>
            ("mFCcf", "force allow-empty ignore-missing stdin dry-run"),
        "pull" => ("eqvfrS", "edit no-edit rebase no-rebase ff no-ff ff-only squash no-squash commit no-commit \
            autostash no-autostash quiet verbose force tags no-tags prune no-prune gpg-sign no-gpg-sign"),
        "rebase" => ("CSrsxXi", "continue skip abort quit interactive edit-todo root rebase-merges no-rebase-merges \
            autosquash no-autosquash autostash no-autostash keep-base update-refs no-update-refs apply merge \
            force-rebase gpg-sign no-gpg-sign"),
        "remote" => match mode {
            None => ("", ""),
            Some("get-url") => ("", "push all"),
            Some("add") => ("ftm", "fetch tags no-tags mirror"),
            Some("set-url") => ("", "push add delete"),
            Some("rename" | "remove" | "rm") => ("", ""),
            Some("set-head") => ("ad", "auto delete"),
            Some("set-branches") => ("", "add"),
            Some("show") => ("n", ""),
            Some("prune") => ("n", "dry-run"),
            Some("update") => ("p", "prune no-prune"),
            _ => return None,
        },
        "reflog" => match mode {
            Some("show") => ("pnqzMCRwbuU", HISTORY),
            Some("list" | "exists") => ("", ""),
            Some("expire" | "delete") => ("n", "dry-run verbose all single-worktree rewrite updateref stale-fix"),
            Some("drop") => ("", "all single-worktree"),
            // The default show form accepts a ref. Write is a separate operation even without its arguments.
            mode if mode != Some("write") && operands_before_end <= 1 => ("pnqzMCRwbuU", HISTORY),
            _ => return None,
        },
        "reset" => ("pqNU", "soft mixed hard merge keep quiet patch intent-to-add pathspec-file-nul recurse-submodules no-recurse-submodules"),
        "restore" => ("sSWpq", "staged worktree ours theirs merge patch quiet overlay no-overlay pathspec-file-nul"),
        "rm" => ("frnq", "force cached ignore-unmatch dry-run quiet sparse"),
        "stash" => match mode {
            Some("show" | "list") => ("psqzMCRwbuU", DIFF),
            None | Some("push") => ("pmuaqkS", "patch message include-untracked all quiet keep-index \
                no-keep-index staged pathspec-from-file pathspec-file-nul"),
            Some("save") => ("pmuaqkS", "patch message include-untracked all quiet keep-index no-keep-index staged"),
            Some("apply" | "pop") => ("q", "quiet index"),
            Some("drop") => ("q", "quiet"),
            Some("clear" | "branch" | "create" | "store") => ("mq", "message quiet"),
            Some(_) => return None,
        },
        "symbolic-ref" => ("qdm", "short quiet delete recurse no-recurse"),
        "tag" => ("lasumFdfvn", "list annotate sign no-sign local-user delete force verify contains no-contains \
            merged no-merged edit create-reflog"),
        "worktree" if mode == Some("list") => ("vz", "porcelain verbose"),
        "fetch" => ("qvfpPtkn", "all append atomic dry-run force keep prune prune-tags no-prune tags no-tags verbose quiet"),
        "push" => ("qvfun", "all atomic delete dry-run follow-tags force force-with-lease force-if-includes \
            mirror no-verify porcelain prune quiet set-upstream tags verbose"),
        _ => return None,
    })
}

fn takes_revision_options(cmd: &str) -> bool {
    matches!(
        cmd,
        "log" | "show" | "rev-list" | "shortlog" | "reflog" | "blame" | "annotate"
    )
}

fn long_accepts_attached_value(cmd: &str, option: &str) -> bool {
    match (cmd, option) {
        ("status", "--porcelain" | "--untracked-files" | "--ignored" | "--ignore-submodules")
        | ("commit", "--gpg-sign" | "--untracked-files" | "--porcelain" | "--unified" | "--inter-hunk-context")
        | ("merge" | "rebase" | "revert" | "cherry-pick" | "pull", "--gpg-sign")
        | ("pull", "--rebase")
        | ("rebase", "--rebase-merges")
        | ("merge", "--log")
        | ("am", "--show-current-patch")
        | ("describe", "--dirty" | "--broken" | "--abbrev")
        | ("show-ref", "--exclude-existing" | "--hash" | "--abbrev")
        | ("rev-parse", "--short" | "--show-object-format" | "--abbrev-ref")
        | ("ls-tree", "--abbrev")
        | ("branch" | "switch" | "checkout", "--track")
        | ("branch", "--contains")
        | ("remote", "--mirror")
        | ("cat-file", "--batch" | "--batch-check" | "--batch-command")
        | ("grep", "--color" | "--open-files-in-pager" | "--open")
        | ("push", "--force-with-lease") => true,
        _ => {
            revision_output_option(cmd)
                && matches!(
                    option,
                    "--color"
                        | "--decorate"
                        | "--format"
                        | "--pretty"
                        | "--find-renames"
                        | "--find-copies"
                        | "--relative"
                        | "--word-diff"
                        | "--color-words"
                        | "--unified"
                        | "--inter-hunk-context"
                )
        }
    }
}

fn config_operation_allowed(mode: Option<&str>, options: &CommandOptions<'_>) -> bool {
    let Some(word) = mode else {
        return true;
    };
    if matches!(
        word,
        "list" | "get" | "set" | "unset" | "rename-section" | "remove-section" | "edit"
    ) || options.long.iter().any(|&(name, _)| {
        matches!(
            name,
            "--get-regexp" | "--get-urlmatch" | "--rename-section" | "--remove-section"
        )
    }) {
        return true;
    }
    let Some((section, rest)) = word.split_once('.') else {
        return false;
    };
    let key = rest.rsplit('.').next().unwrap_or_default();
    let key_char = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'-';
    !word.contains('\n')
        && !section.is_empty()
        && section.bytes().all(key_char)
        && key.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && key.bytes().all(key_char)
}

fn config_read(words: &[std::borrow::Cow<'_, str>]) -> bool {
    let mut action = None;
    let mut values = 0;
    for word in words {
        match word.as_ref() {
            "--get" | "--get-all" | "--get-regexp" | "--list" if action.is_none() => action = Some(word.as_ref()),
            "--null" | "--local" => {}
            s if s.starts_with('-') => return false,
            _ => values += 1,
        }
    }
    match action {
        Some("--list") => values == 0,
        Some("--get" | "--get-all") => values == 1,
        Some("--get-regexp") => (1..=2).contains(&values),
        _ => false,
    }
}

struct CommandOptions<'a> {
    long: Vec<(&'a str, Option<&'a str>)>,
    short: Vec<char>,
    short_values: Vec<(char, &'a str)>,
    operands: Vec<usize>,
    end: usize,
    invalid: bool,
    separate_grep: bool,
}

impl<'a> CommandOptions<'a> {
    fn parse(cmd: &str, words: &'a [Cow<'_, str>]) -> Self {
        let first_word = words.get(1).map(Cow::as_ref);
        let mut parsed = Self {
            long: vec![],
            short: vec![],
            short_values: vec![],
            operands: vec![],
            end: words.len(),
            invalid: false,
            separate_grep: false,
        };
        let mut args = words.iter().enumerate().skip(1);
        while let Some((index, arg)) = args.next() {
            if matches!(arg.as_ref(), "--" | "--end-of-options") {
                parsed.end = index;
                parsed.operands.extend(args.map(|(index, _)| index));
                break;
            }
            if cmd == "remote" && parsed.operands.is_empty() && arg.starts_with('-') {
                parsed.invalid |= !matches!(arg.as_ref(), "--verbose" | "--no-verbose")
                    && !arg
                        .strip_prefix('-')
                        .is_some_and(|flags| !flags.is_empty() && flags.bytes().all(|flag| flag == b'v'));
                continue;
            }
            if arg.starts_with("--") {
                let (name, mut value) = arg.split_once('=').map_or((arg.as_ref(), None), |(n, v)| (n, Some(v)));
                if value.is_none() && long_takes_value(cmd, name) {
                    parsed.separate_grep |= name == "--grep";
                    value = args.next().map(|(_, arg)| arg.as_ref());
                }
                parsed.long.push((name, value));
            } else if let Some(group) = arg.strip_prefix('-').filter(|group| !group.is_empty()) {
                if takes_revision_options(cmd)
                    && !reflog_maintenance_mode(cmd, first_word)
                    && group.bytes().all(|byte| byte.is_ascii_digit())
                {
                    continue;
                }
                for (offset, flag) in group.char_indices() {
                    parsed.short.push(flag);
                    if short_takes_value(cmd, flag, first_word) {
                        let tail = &group[offset + flag.len_utf8()..];
                        if !tail.is_empty() {
                            parsed.short_values.push((flag, tail));
                        } else if !short_value_optional(cmd, flag) {
                            if let Some((_, value)) = args.next() {
                                parsed.short_values.push((flag, value));
                            } else {
                                parsed.invalid = true;
                            }
                        }
                        break;
                    }
                }
            } else {
                parsed.operands.push(index);
            }
        }
        parsed
    }
}

fn commit_paths(args: &[OsString], options: &CommandOptions<'_>) -> Vec<PathBuf> {
    // An unrecognized option may take a separate value, so its following operand cannot narrow the conflict check.
    if options.long.iter().any(|&(name, _)| !commit_option_known(name))
        || options
            .short
            .iter()
            .any(|&flag| !"aqvseiponz".contains(flag) && !short_takes_value("commit", flag, None))
    {
        return vec![];
    }
    options
        .operands
        .iter()
        .map(|&index| PathBuf::from(&args[index]))
        .collect()
}

fn commit_option_known(name: &str) -> bool {
    let name = name.strip_prefix("--no-").unwrap_or(name.trim_start_matches("--"));
    commit_long_value(name)
        || matches!(
            name,
            "quiet"
                | "verbose"
                | "reset-author"
                | "signoff"
                | "edit"
                | "status"
                | "gpg-sign"
                | "all"
                | "include"
                | "interactive"
                | "patch"
                | "only"
                | "verify"
                | "dry-run"
                | "short"
                | "branch"
                | "ahead-behind"
                | "porcelain"
                | "long"
                | "null"
                | "amend"
                | "post-rewrite"
                | "untracked-files"
                | "pathspec-file-nul"
                | "allow-empty"
                | "allow-empty-message"
        )
}

fn path_arguments(cmd: &str, args: &[OsString], parsed: &CommandOptions<'_>, path_separator: bool) -> Vec<PathBuf> {
    if matches!(
        cmd,
        "push" | "fetch" | "tag" | "symbolic-ref" | "branch" | "remote" | "config" | "help"
    ) {
        return vec![];
    }
    // These commands accept paths on both sides. Other commands need a separator to distinguish paths from revisions.
    if !matches!(cmd, "add" | "rm" | "mv" | "restore") {
        return if path_separator {
            args[parsed.end + 1..].iter().map(PathBuf::from).collect()
        } else {
            vec![]
        };
    }
    let mut skip = false;
    for arg in &args[1..parsed.end] {
        if skip {
            skip = false;
            continue;
        }
        let s = arg.to_string_lossy();
        if cmd == "restore" && matches!(s.as_ref(), "--source" | "-s") {
            skip = true;
            continue;
        }
        if s.starts_with('-') && (cmd != "add" || s != "-") {
            let known = match cmd {
                "add" => {
                    matches!(
                        s.as_ref(),
                        "--update"
                            | "--all"
                            | "--force"
                            | "--verbose"
                            | "--intent-to-add"
                            | "--ignore-errors"
                            | "--ignore-missing"
                            | "--sparse"
                    ) || s[1..].chars().all(|flag| matches!(flag, 'u' | 'A' | 'f' | 'v' | 'N'))
                }
                "restore" => {
                    matches!(
                        s.as_ref(),
                        "--staged" | "--worktree" | "-S" | "-W" | "--ours" | "--theirs"
                    ) || s.starts_with("--source=")
                }
                "rm" => matches!(
                    s.as_ref(),
                    "-f" | "--force" | "-r" | "--cached" | "--ignore-unmatch" | "-n" | "--dry-run" | "-q" | "--quiet"
                ),
                "mv" => matches!(
                    s.as_ref(),
                    "-f" | "--force" | "-k" | "-n" | "--dry-run" | "-v" | "--verbose"
                ),
                _ => false,
            };
            if !known {
                return vec![];
            }
        }
    }
    parsed
        .operands
        .iter()
        .map(|&index| PathBuf::from(&args[index]))
        .collect()
}

fn reflog_maintenance_mode(cmd: &str, first_word: Option<&str>) -> bool {
    cmd == "reflog" && matches!(first_word, Some("expire" | "delete" | "drop"))
}

fn short_takes_value(cmd: &str, ch: char, first_word: Option<&str>) -> bool {
    match cmd {
        "commit" => "mFCcUtSu".contains(ch),
        "rebase" => "CSrsxX".contains(ch),
        "revert" | "cherry-pick" => "mSX".contains(ch),
        "tag" => "mFu".contains(ch),
        "notes" => "mFCc".contains(ch),
        "stash" => ch == 'm',
        "merge" => "mFsXS".contains(ch),
        "pull" => "rS".contains(ch),
        "restore" => ch == 's',
        "apply" | "am" => "pC".contains(ch),
        "checkout" => "bB".contains(ch),
        "switch" => "cC".contains(ch),
        "grep" => "efmABCO".contains(ch),
        "blame" | "annotate" => "LMC".contains(ch),
        "reflog" if reflog_maintenance_mode(cmd, first_word) => false,
        "log" | "show" | "rev-list" | "reflog" => "nUMC".contains(ch),
        "diff" | "diff-files" | "diff-index" | "diff-tree" | "reset" => "UMC".contains(ch),
        "status" => ch == 'u',
        "ls-files" => "xX".contains(ch),
        "show-ref" => ch == 's',
        "config" => ch == 'f',
        "symbolic-ref" => ch == 'm',
        "remote" => "tm".contains(ch),
        _ => false,
    }
}

fn short_value_optional(cmd: &str, ch: char) -> bool {
    matches!(
        (cmd, ch),
        ("commit", 'S' | 'u')
            | ("rebase" | "pull", 'S' | 'r')
            | ("revert" | "cherry-pick" | "merge", 'S')
            | ("status", 'u')
            | ("grep", 'O')
            | (
                "diff" | "diff-files" | "diff-index" | "diff-tree" | "log" | "show" | "rev-list" | "reflog",
                'U'
            )
            | (
                "blame"
                    | "annotate"
                    | "log"
                    | "show"
                    | "rev-list"
                    | "reflog"
                    | "diff"
                    | "diff-files"
                    | "diff-index"
                    | "diff-tree",
                'M' | 'C'
            )
            | ("show-ref", 's')
    )
}

fn revision_output_option(cmd: &str) -> bool {
    matches!(
        cmd,
        "diff"
            | "log"
            | "show"
            | "diff-files"
            | "diff-index"
            | "diff-tree"
            | "reflog"
            | "stash"
            | "rev-list"
            | "shortlog"
            | "blame"
            | "annotate"
    )
}

fn revision_output_files(cmd: &str, args: &[OsString]) -> Vec<Option<PathBuf>> {
    if !revision_output_option(cmd) {
        return vec![];
    }
    // Revision parsing can open output files before rejecting unknown arguments. Shortlog and blame's
    // option parser can also consume a bare `--` as a value before reaching a later `--output`.
    let mut files = vec![];
    let parses_options = matches!(cmd, "shortlog" | "blame" | "annotate");
    for (index, arg) in args.iter().enumerate().skip(1) {
        if arg == "--" {
            let previous = &args[index - 1];
            let could_be_value = parses_options
                && previous.as_bytes().starts_with(b"-")
                && !previous.as_bytes().contains(&b'=')
                && previous != "--end-of-options";
            if !could_be_value {
                break;
            }
        } else if arg == "--output" {
            files.push(args.get(index + 1).map(PathBuf::from));
        } else if let Some(path) = arg.as_bytes().strip_prefix(b"--output=") {
            files.push(Some(PathBuf::from(OsStr::from_bytes(path))));
        }
    }
    files
}

fn cat_file_read(options: &[std::borrow::Cow<'_, str>]) -> bool {
    options.iter().all(|s| {
        !s.starts_with('-')
            || matches!(
                s.as_ref(),
                "-t" | "-s"
                    | "-e"
                    | "-p"
                    | "--batch"
                    | "--batch-check"
                    | "--batch-command"
                    | "--batch-all-objects"
                    | "--buffer"
                    | "--follow-symlinks"
                    | "--unordered"
                    | "--allow-unknown-type"
                    | "--path"
                    | "-z"
                    | "-Z"
            )
            || s.starts_with("--batch=")
            || s.starts_with("--batch-check=")
            || s.starts_with("--batch-command=")
    })
}

fn commit_long_value(name: &str) -> bool {
    matches!(
        name,
        "message"
            | "file"
            | "author"
            | "date"
            | "trailer"
            | "cleanup"
            | "template"
            | "reuse-message"
            | "reedit-message"
            | "fixup"
            | "squash"
            | "pathspec-from-file"
    )
}

fn long_takes_value(cmd: &str, option: &str) -> bool {
    if option == "--output" && revision_output_option(cmd)
        || option == "--pathspec-from-file"
            && matches!(
                cmd,
                "add" | "commit" | "checkout" | "reset" | "restore" | "rm" | "stash"
            )
        || takes_revision_options(cmd)
            && matches!(
                option,
                "--grep" | "--author" | "--committer" | "--since" | "--until" | "--max-count" | "--skip"
            )
            && !matches!((cmd, option), ("reflog", "--skip") | ("shortlog", "--committer"))
    {
        return true;
    }
    match cmd {
        "commit" => option.strip_prefix("--").is_some_and(commit_long_value),
        "notes" => matches!(
            option,
            "--ref" | "--message" | "--file" | "--reuse-message" | "--reedit-message"
        ),
        "rebase" => matches!(
            option,
            "--onto" | "--whitespace" | "--empty" | "--exec" | "--strategy" | "--strategy-option" | "--trailer"
        ),
        "revert" | "cherry-pick" => matches!(option, "--mainline" | "--strategy" | "--strategy-option"),
        "tag" => matches!(option, "--message" | "--file" | "--local-user"),
        "log" => option == "--date",
        "blame" | "annotate" => option == "--contents",
        "stash" => option == "--message",
        "merge" => matches!(option, "--message" | "--file" | "--strategy" | "--strategy-option"),
        "grep" => matches!(
            option,
            "--regexp"
                | "--file"
                | "--max-depth"
                | "--threads"
                | "--max-count"
                | "--before-context"
                | "--after-context"
                | "--context"
        ),
        "restore" => option == "--source",
        "config" => matches!(option, "--file" | "--blob" | "--type"),
        "cat-file" => option == "--path",
        "ls-files" => matches!(
            option,
            "--exclude" | "--exclude-from" | "--exclude-per-directory" | "--with-tree" | "--format"
        ),
        "for-each-ref" => matches!(option, "--format" | "--sort" | "--count"),
        "reset" => option == "--unified" || option == "--inter-hunk-context",
        "symbolic-ref" => option == "--reason",
        "branch" => matches!(option, "--set-upstream-to" | "--sort" | "--format"),
        "switch" => matches!(option, "--create" | "--force-create"),
        "describe" => matches!(option, "--match" | "--exclude" | "--candidates"),
        "apply" | "am" => {
            matches!(option, "--directory" | "--exclude" | "--include" | "--whitespace")
                || cmd == "am" && option == "--patch-format"
        }
        _ => false,
    }
}

/// Known forwarded forms get immediate EOF unless they consume input.
pub fn reads_stdin(args: &[OsString], is_tty: bool) -> bool {
    if is_tty {
        return false;
    }
    let Some(parsed) = Invocation::parse(args, Path::new("/")) else {
        return false;
    };
    let words: Vec<_> = parsed.args.iter().map(|s| s.to_string_lossy()).collect();
    let cmd = words.first().map(|s| s.as_ref()).unwrap_or("");
    let first_word = words.get(1).map(Cow::as_ref);
    let mut values = words.iter().skip(1);
    while let Some(option) = values.next() {
        match option.as_ref() {
            "--" | "--end-of-options" => break,
            "--stdin" | "--stdin-paths" | "--index-info" => return true,
            "--annotate-stdin" if cmd == "name-rev" => return true,
            option if cmd == "show-ref" && show_ref_reads_stdin(option) => return true,
            "-F" | "--file" | "--pathspec-from-file" => {
                if values.next().is_some_and(|v| v == "-") {
                    return true;
                }
            }
            "-F-" | "--file=-" | "--pathspec-from-file=-" => return true,
            option if option.starts_with('-') && !option.starts_with("--") => {
                for (offset, ch) in option[1..].char_indices() {
                    if short_takes_value(cmd, ch, first_word) {
                        let attached = &option[1 + offset + ch.len_utf8()..];
                        let value = if attached.is_empty() && !short_value_optional(cmd, ch) {
                            values.next().map(|s| s.as_ref())
                        } else {
                            Some(attached)
                        };
                        if (ch == 'F' || (cmd, ch) == ("config", 'f')) && value == Some("-") {
                            return true;
                        }
                        break;
                    }
                }
            }
            "--contents" if matches!(cmd, "blame" | "annotate") => {
                if values.next().is_some_and(|v| v == "-") {
                    return true;
                }
            }
            "--contents=-" if matches!(cmd, "blame" | "annotate") => return true,
            option if long_takes_value(cmd, option) => {
                values.next();
            }
            _ => {}
        }
    }
    match cmd {
        "apply" | "am" => {
            let mut skip = false;
            let mut files = vec![];
            let mut after_separator = false;
            for word in words.iter().skip(1) {
                if after_separator {
                    files.push(word.as_ref());
                    continue;
                }
                if word == "--" {
                    after_separator = true;
                    continue;
                }
                if skip {
                    skip = false;
                    continue;
                }
                if matches!(
                    word.as_ref(),
                    "--directory" | "--exclude" | "--include" | "--whitespace" | "-p" | "-C" | "--patch-format"
                ) {
                    skip = true;
                } else if word == "-" || !word.starts_with('-') {
                    files.push(word.as_ref());
                }
            }
            files.is_empty() || files.contains(&"-")
        }
        "add" | "commit" | "reset" | "checkout" | "switch" | "restore" | "rm" | "mv" | "merge" | "rebase"
        | "revert" | "cherry-pick" | "stash" | "push" | "fetch" | "pull" | "tag" | "branch" | "config" | "remote"
        | "worktree" | "notes" | "hash-object" | "update-index" | "status" | "diff" | "diff-files" | "diff-index"
        | "diff-tree" | "log" | "show" | "blame" | "annotate" | "grep" | "rev-parse" | "rev-list" | "ls-files"
        | "ls-tree" | "for-each-ref" | "show-ref" | "merge-base" | "name-rev" | "describe" | "check-ignore"
        | "check-attr" | "var" | "help" | "reflog" | "symbolic-ref" => false,
        // The server resolves the revisions. With none selected, shortlog reads the piped log.
        "shortlog" => true,
        "cat-file" => words.iter().any(|s| s.starts_with("--batch")),
        _ => true,
    }
}

fn show_ref_reads_stdin(option: &str) -> bool {
    let name = option.split('=').next().unwrap_or(option);
    name.starts_with("--exc") && "--exclude-existing".starts_with(name)
}
