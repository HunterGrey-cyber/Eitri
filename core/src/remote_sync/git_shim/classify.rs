use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Local { extra: Vec<&'static str> },
    Forward { whole_tree: bool, paths: Vec<PathBuf> },
    RefuseEditor,
    NotRemote,
}

#[derive(Debug)]
pub(super) struct Invocation {
    pub cwd: PathBuf,
    pub globals: Vec<OsString>,
    pub args: Vec<OsString>,
    pub foreign: bool,
    pub git_dir: Option<PathBuf>,
    pub work_tree: Option<PathBuf>,
}

impl Invocation {
    pub fn parse(args: &[OsString], cwd: &Path, root: Option<&Path>) -> Self {
        let mut parsed = Self {
            cwd: cwd.into(),
            globals: vec![],
            args: vec![],
            foreign: false,
            git_dir: None,
            work_tree: None,
        };
        let mut i = 0;
        while i < args.len() {
            let arg = args[i].to_string_lossy();
            if arg == "-C" {
                i += 1;
                if let Some(dir) = args.get(i) {
                    parsed.cwd = parsed.cwd.join(dir);
                    parsed.cwd = parsed.cwd.canonicalize().unwrap_or_else(|_| normalize(&parsed.cwd));
                } else {
                    parsed.foreign = true;
                }
            } else if arg == "--git-dir"
                || arg == "--work-tree"
                || arg.starts_with("--git-dir=")
                || arg.starts_with("--work-tree=")
            {
                let (name, value) = if let Some((name, value)) = arg.split_once('=') {
                    (name.to_owned(), Some(OsString::from(value)))
                } else {
                    i += 1;
                    (arg.into_owned(), args.get(i).cloned())
                };
                parsed.globals.push(name.clone().into());
                if let Some(value) = value {
                    if name == "--git-dir" {
                        parsed.git_dir = Some(parsed.cwd.join(&value));
                    } else {
                        parsed.work_tree = Some(parsed.cwd.join(&value));
                    }
                    parsed.globals.push(value);
                } else {
                    parsed.foreign = true;
                }
            } else if matches!(
                arg.as_ref(),
                "-c" | "--config-env" | "--namespace" | "--super-prefix" | "--attr-source" | "--list-cmds"
            ) {
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
            } else if arg.starts_with("--config-env=") || arg.starts_with("--exec-path") || arg == "--bare" {
                parsed.foreign = true;
                parsed.globals.push(args[i].clone());
            } else if matches!(
                arg.as_ref(),
                "--no-pager"
                    | "--paginate"
                    | "-p"
                    | "-P"
                    | "--literal-pathspecs"
                    | "--glob-pathspecs"
                    | "--noglob-pathspecs"
                    | "--icase-pathspecs"
                    | "--no-optional-locks"
            ) || arg.starts_with("-c") && arg.len() > 2
            {
                parsed.globals.push(args[i].clone());
            } else {
                break;
            }
            i += 1;
        }
        if let Some(root) = root {
            let mut globals = Vec::new();
            let mut values = parsed.globals.into_iter();
            while let Some(arg) = values.next() {
                if arg == "--git-dir" || arg == "--work-tree" {
                    let value = values.next();
                    let expected = if arg == "--git-dir" {
                        root.join(".git")
                    } else {
                        root.to_path_buf()
                    };
                    let equal = value.as_ref().is_some_and(|value| {
                        parsed
                            .cwd
                            .join(value)
                            .canonicalize()
                            .ok()
                            .zip(expected.canonicalize().ok())
                            .is_some_and(|(a, b)| a == b)
                    });
                    if !equal {
                        parsed.foreign = true;
                        globals.push(arg);
                        if let Some(value) = value {
                            globals.push(value);
                        }
                    }
                } else {
                    let takes_value = arg == "-c"
                        || arg == "--config-env"
                        || arg == "--namespace"
                        || arg == "--super-prefix"
                        || arg == "--attr-source"
                        || arg == "--list-cmds";
                    globals.push(arg);
                    if takes_value {
                        if let Some(value) = values.next() {
                            globals.push(value);
                        }
                    }
                }
            }
            parsed.globals = globals;
        }
        parsed.args = args.get(i..).unwrap_or_default().to_vec();
        parsed
    }

    pub fn forwarded_args(&self) -> Vec<OsString> {
        self.globals.iter().chain(&self.args).cloned().collect()
    }
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
    let words: Vec<_> = args.iter().map(|s| s.to_string_lossy()).collect();
    let Some(cmd) = words.first().map(|s| s.as_ref()) else {
        return forward(true, vec![]);
    };
    let end = words.iter().position(|s| s == "--").unwrap_or(words.len());
    let options = &words[1..end];
    let has = |flag: &str| options.iter().any(|s| s == flag || s.starts_with(&format!("{flag}=")));
    let short = |flag: char| {
        options.iter().any(|s| {
            if !s.starts_with('-') || s.starts_with("--") {
                return false;
            }
            for ch in s[1..].chars() {
                if ch == flag {
                    return true;
                }
                if short_takes_value(cmd, ch) {
                    break;
                }
            }
            false
        })
    };
    let message = has("--message") || has("--file") || short('m') || short('F');
    let commit_message =
        message || has("--reuse-message") || short('C') || has("--no-edit") || has("--fixup") || has("--squash");
    let interactive = match cmd {
        "commit" => !commit_message || has("--edit") || short('e'),
        "revert" => !has("--no-edit") && !has("--abort") && !has("--quit"),
        "merge" | "cherry-pick" => has("--continue") || has("--edit") || short('e'),
        "rebase" => has("--continue") || has("--interactive") || short('i') || has("--edit-todo"),
        "tag" => (short('a') || short('s') || has("--annotate") || has("--sign")) && !message,
        "pull" => has("--edit") || short('e'),
        "notes" => options.first().is_some_and(|s| s == "add" || s == "append") && !message,
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
    if options.iter().any(|s| {
        matches!(s.as_ref(), "--ext-diff" | "--textconv" | "--filters")
            || s.starts_with("--ext-diff=")
            || s.starts_with("--textconv=")
            || s.starts_with("--filters=")
    }) {
        return forward(true, vec![]);
    }
    if matches!(
        cmd,
        "diff" | "log" | "show" | "diff-files" | "diff-index" | "diff-tree" | "reflog" | "stash"
    ) && options.iter().any(|s| s.starts_with("--output"))
        || cmd == "grep"
            && (short('O')
                || options.iter().any(|s| {
                    let option = s.split('=').next().unwrap();
                    option.len() > 2 && "--open-files-in-pager".starts_with(option)
                }))
    {
        return forward(true, vec![]);
    }
    let local = match cmd {
        "status" | "diff" | "diff-files" | "diff-index" | "diff-tree" | "log" | "shortlog" | "show" | "blame"
        | "annotate" | "grep" | "rev-parse" | "rev-list" | "ls-files" | "ls-tree" | "for-each-ref" | "show-ref"
        | "merge-base" | "name-rev" | "describe" | "check-ignore" | "check-attr" | "var" | "--version" => true,
        "help" => options == ["-a"],
        "cat-file" => cat_file_read(options),
        "symbolic-ref" => {
            options.iter().filter(|s| !s.starts_with('-')).count() == 1
                && options
                    .iter()
                    .all(|s| !s.starts_with('-') || matches!(s.as_ref(), "--short" | "--quiet" | "-q"))
        }
        "branch" => {
            options.first().is_some_and(|s| s == "--list") && options[1..].iter().all(|s| !s.starts_with('-'))
                || options.is_empty()
                || options.iter().all(|s| {
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
            options.is_empty()
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
    if local {
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
    // Options with values are consumed before collecting paths, so a message or revision is never mistaken for a file.
    let paths = path_arguments(cmd, args);
    let whole = match cmd {
        "commit" => paths.is_empty() || short('a') || has("--all"),
        "add" => paths.is_empty() || paths.iter().any(|p| p == Path::new(".")) || short('A') || has("--all"),
        "stash" => true,
        "checkout" => !args.iter().any(|s| s == "--"),
        "switch" => true,
        "restore" => paths.is_empty(),
        "reset" => {
            !args.iter().any(|s| s == "--") || has("--hard") || has("--merge") || has("--keep") || paths.is_empty()
        }
        "merge" | "rebase" | "pull" | "cherry-pick" | "revert" | "am" => true,
        "apply" => !has("--cached"),
        "rm" | "mv" => paths.is_empty(),
        _ => true,
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

fn path_arguments(cmd: &str, args: &[OsString]) -> Vec<PathBuf> {
    if let Some(i) = args.iter().position(|s| s == "--") {
        return args[i + 1..].iter().map(PathBuf::from).collect();
    }
    if matches!(
        cmd,
        "apply" | "am" | "stash" | "switch" | "merge" | "rebase" | "pull" | "cherry-pick" | "revert"
    ) {
        return vec![];
    }
    // Only unambiguous path commands accept implicit paths. Other commands require an explicit separator.
    if !matches!(cmd, "add" | "rm" | "mv" | "restore") {
        return vec![];
    }
    let mut paths = vec![];
    let mut skip = false;
    for arg in &args[1..] {
        if skip {
            skip = false;
            continue;
        }
        let s = arg.to_string_lossy();
        if cmd == "restore" && matches!(s.as_ref(), "--source" | "-s") {
            skip = true;
            continue;
        }
        if s.starts_with('-') {
            let known = match cmd {
                "add" => matches!(
                    s.as_ref(),
                    "-u" | "--update"
                        | "-A"
                        | "--all"
                        | "-f"
                        | "--force"
                        | "-v"
                        | "--verbose"
                        | "-N"
                        | "--intent-to-add"
                        | "--ignore-errors"
                        | "--ignore-missing"
                        | "--sparse"
                ),
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
        } else {
            paths.push(PathBuf::from(arg));
        }
    }
    paths
}

fn short_takes_value(cmd: &str, ch: char) -> bool {
    match cmd {
        "commit" => "mFCcUt".contains(ch),
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

fn stdin_long_takes_value(cmd: &str, option: &str) -> bool {
    match cmd {
        "commit" => matches!(
            option,
            "--message"
                | "--author"
                | "--date"
                | "--trailer"
                | "--cleanup"
                | "--template"
                | "--reuse-message"
                | "--reedit-message"
                | "--fixup"
                | "--squash"
        ),
        "tag" | "notes" | "stash" | "merge" => matches!(option, "--message"),
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
    let parsed = Invocation::parse(args, Path::new("/"), None);
    let words: Vec<_> = parsed.args.iter().map(|s| s.to_string_lossy()).collect();
    let end = words.iter().position(|s| s == "--").unwrap_or(words.len());
    let options = &words[..end];
    let cmd = words.first().map(|s| s.as_ref()).unwrap_or("");
    let mut values = options.iter().skip(1);
    while let Some(option) = values.next() {
        match option.as_ref() {
            "--stdin" | "--stdin-paths" | "--index-info" => return true,
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
                        let value = if attached.is_empty() {
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
            option if stdin_long_takes_value(cmd, option) => {
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
        | "diff-tree" | "log" | "shortlog" | "show" | "blame" | "annotate" | "grep" | "rev-parse" | "rev-list"
        | "ls-files" | "ls-tree" | "for-each-ref" | "show-ref" | "merge-base" | "name-rev" | "describe"
        | "check-ignore" | "check-attr" | "var" | "help" | "reflog" | "symbolic-ref" => false,
        "cat-file" => words.iter().any(|s| s.starts_with("--batch")),
        _ => true,
    }
}
