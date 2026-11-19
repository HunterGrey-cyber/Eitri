use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Local { extra: Vec<&'static str> },
    Forward { whole_tree: bool, paths: Vec<PathBuf> },
    RefuseEditor,
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

/// Classifies a subcommand and its arguments. Unknown options on conditional read forms forward conservatively.
pub fn classify(args: &[OsString], _root: &Path) -> Route {
    classify_invocation(args, true)
}

pub(super) fn classify_invocation(args: &[OsString], allow_local: bool) -> Route {
    let words: Vec<_> = args.iter().map(|s| s.to_string_lossy()).collect();
    let Some(cmd) = words.first().map(|s| s.as_ref()) else {
        return forward(false, vec![]);
    };
    if cmd == "--" {
        return Route::NotRemote;
    }
    let parsed = CommandOptions::parse(cmd, &words);
    let end = parsed.end;
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
        "commit" => !commit_message || has("--edit") || short('e'),
        "revert" => {
            has("--continue")
                || !(short('n')
                    || ["--no-edit", "--no-commit", "--abort", "--quit", "--skip"]
                        .into_iter()
                        .any(has))
        }
        "merge" | "cherry-pick" => has("--continue") || has("--edit") || short('e'),
        "rebase" => has("--continue") || has("--interactive") || short('i') || has("--edit-todo"),
        "tag" => (short('a') || short('s') || has("--annotate") || has("--sign")) && !message,
        "pull" => has("--edit") || short('e'),
        "notes" => {
            parsed
                .operands
                .first()
                .is_some_and(|&index| matches!(words[index].as_ref(), "add" | "append"))
                && !message
        }
        "add" => short('p') || short('i') || short('e') || has("--patch") || has("--interactive") || has("--edit"),
        "checkout" | "reset" | "restore" => short('p') || has("--patch"),
        "stash" => {
            !options
                .first()
                .is_some_and(|s| !s.starts_with('-') && s != "push" && s != "save")
                && (short('p') || has("--patch"))
        }
        _ => false,
    };
    if interactive {
        return Route::RefuseEditor;
    }
    let output_files = revision_output_files(cmd, args);
    let external = words[1..].iter().take_while(|word| *word != "--").any(|word| {
        let name = word.split('=').next().unwrap_or(word);
        matches!(name, "--ext-diff" | "--textconv" | "--filters")
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
            options.is_empty() || options == ["-v"] || options.first().is_some_and(|s| s == "get-url") && !has("--add")
        }
        "stash" => options.first().is_some_and(|s| s == "list" || s == "show"),
        "reflog" => options.is_empty() || options.first().is_some_and(|s| s == "show"),
        "worktree" => options.first().is_some_and(|s| s == "list"),
        "config" => config_read(options),
        _ => false,
    };
    if local && allow_local && output_files.is_empty() && !external && !grep_pager {
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
    let path_separator = words.get(end).is_some_and(|word| word == "--");
    let mut paths = if cmd == "commit" {
        commit_paths(args, &parsed)
    } else {
        path_arguments(cmd, args, &parsed, path_separator)
    };
    paths.extend(output_files.into_iter().flatten());
    let whole = match cmd {
        "commit" => paths.is_empty() || short('a') || has("--all"),
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
    operands: Vec<usize>,
    end: usize,
}

impl<'a> CommandOptions<'a> {
    fn parse(cmd: &str, words: &'a [Cow<'_, str>]) -> Self {
        let mut parsed = Self {
            long: vec![],
            short: vec![],
            operands: vec![],
            end: words.len(),
        };
        let mut args = words.iter().enumerate().skip(1);
        while let Some((index, arg)) = args.next() {
            if matches!(arg.as_ref(), "--" | "--end-of-options") {
                parsed.end = index;
                parsed.operands.extend(args.map(|(index, _)| index));
                break;
            }
            if arg.starts_with("--") {
                let (name, mut value) = arg.split_once('=').map_or((arg.as_ref(), None), |(n, v)| (n, Some(v)));
                if value.is_none() && long_takes_value(cmd, name) {
                    value = args.next().map(|(_, arg)| arg.as_ref());
                }
                parsed.long.push((name, value));
            } else if let Some(group) = arg.strip_prefix('-').filter(|group| !group.is_empty()) {
                for (offset, flag) in group.char_indices() {
                    parsed.short.push(flag);
                    if short_takes_value(cmd, flag) {
                        if offset + flag.len_utf8() == group.len() && !short_value_optional(cmd, flag) {
                            args.next();
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
            .any(|&flag| !"aqvseiponz".contains(flag) && !short_takes_value("commit", flag))
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

fn short_takes_value(cmd: &str, ch: char) -> bool {
    match cmd {
        "commit" => "mFCcUtSu".contains(ch),
        "rebase" => "CSrsxX".contains(ch),
        "revert" | "cherry-pick" => "mSX".contains(ch),
        "tag" => "mFu".contains(ch),
        "notes" => "mFCc".contains(ch),
        "stash" => ch == 'm',
        "merge" => "mF".contains(ch),
        "restore" => ch == 's',
        "apply" | "am" => "pC".contains(ch),
        "checkout" | "switch" => "bBo".contains(ch),
        "grep" => "efmABC".contains(ch),
        _ => false,
    }
}

fn short_value_optional(cmd: &str, ch: char) -> bool {
    matches!(
        (cmd, ch),
        ("commit", 'S' | 'u') | ("rebase", 'S' | 'r') | ("revert" | "cherry-pick", 'S')
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
            | "unified"
            | "inter-hunk-context"
            | "pathspec-from-file"
    )
}

fn long_takes_value(cmd: &str, option: &str) -> bool {
    if matches!(option, "--file" | "--pathspec-from-file") || option == "--output" && revision_output_option(cmd) {
        return true;
    }
    match cmd {
        "commit" => option.strip_prefix("--").is_some_and(commit_long_value),
        "notes" => matches!(option, "--ref" | "--message" | "--reuse-message" | "--reedit-message"),
        "rebase" => matches!(
            option,
            "--onto" | "--whitespace" | "--empty" | "--exec" | "--strategy" | "--strategy-option" | "--trailer"
        ),
        "revert" | "cherry-pick" => matches!(option, "--mainline" | "--strategy" | "--strategy-option"),
        "tag" | "stash" | "merge" => option == "--message",
        "grep" => matches!(option, "--regexp"),
        "restore" => option == "--source",
        "apply" | "am" => matches!(
            option,
            "--directory" | "--exclude" | "--include" | "--whitespace" | "--patch-format"
        ),
        _ => false,
    }
}

/// Known forwarded forms get immediate EOF unless they consume input; unknown forms stream non-TTY stdin.
pub fn reads_stdin(args: &[OsString], is_tty: bool) -> bool {
    if is_tty {
        return false;
    }
    let Some(parsed) = Invocation::parse(args, Path::new("/")) else {
        return false;
    };
    let words: Vec<_> = parsed.args.iter().map(|s| s.to_string_lossy()).collect();
    let cmd = words.first().map(|s| s.as_ref()).unwrap_or("");
    let mut values = words.iter().skip(1);
    while let Some(option) = values.next() {
        match option.as_ref() {
            "--" | "--end-of-options" => break,
            "--stdin" | "--stdin-paths" | "--index-info" => return true,
            option if cmd == "show-ref" && show_ref_reads_stdin(option) => return true,
            "-F" | "--file" | "--pathspec-from-file" => {
                if values.next().is_some_and(|v| v == "-") {
                    return true;
                }
            }
            "-F-" | "--file=-" | "--pathspec-from-file=-" => return true,
            option if option.starts_with('-') && !option.starts_with("--") => {
                for (offset, ch) in option[1..].char_indices() {
                    if short_takes_value(cmd, ch) {
                        let attached = &option[1 + offset + ch.len_utf8()..];
                        let value = if attached.is_empty() && !short_value_optional(cmd, ch) {
                            values.next().map(|s| s.as_ref())
                        } else {
                            Some(attached)
                        };
                        if ch == 'F' && value == Some("-") {
                            return true;
                        }
                        break;
                    }
                }
            }
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
