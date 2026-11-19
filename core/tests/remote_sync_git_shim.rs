mod remote_sync {
    mod git_shim {
        use eitri_core::remote_project::{RemoteProject, SyncRecord};
        use eitri_core::remote_sync::engine::{Conflict, EntryKind, FlushOutcome, Role, SessionStatus, SyncState};
        use eitri_core::remote_sync::git_shim::forward::{Conflicts, Services};
        use eitri_core::remote_sync::git_shim::{classify, run_with, Route, ShimEnv};
        use eitri_core::remote_sync::status_schema::{ConflictEntry, StatusFile, STATUS_VERSION};
        use eitri_core::remote_sync::SyncKey;
        use std::ffi::OsString;
        use std::io::{Read, Write};
        use std::path::{Path, PathBuf};
        use std::process::Command;
        use std::time::Duration;

        fn os(words: &[&str]) -> Vec<OsString> {
            words.iter().map(OsString::from).collect()
        }
        struct Repo {
            base: PathBuf,
            env: ShimEnv,
        }
        impl Repo {
            fn new() -> Self {
                let base =
                    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("git-shim-{}", uuid::Uuid::new_v4().simple()));
                let root = base.join("project");
                std::fs::create_dir_all(root.join("sub")).unwrap();
                let git =
                    eitri_core::remote_sync::shim_dir::locate_real_git(&std::env::var_os("PATH").unwrap()).unwrap();
                let run = |args: &[&str]| {
                    let result = Command::new(&git)
                        .args(args)
                        .current_dir(&root)
                        .env("HOME", &base)
                        .env("GIT_CONFIG_NOSYSTEM", "1")
                        .output()
                        .unwrap();
                    assert!(
                        result.status.success(),
                        "{args:?}: {}",
                        String::from_utf8_lossy(&result.stderr)
                    );
                };
                run(&["init", "-b", "main"]);
                for (key, value) in [
                    ("gc.auto", "0"),
                    ("maintenance.auto", "false"),
                    ("user.name", "Example"),
                    ("user.email", "example@example.invalid"),
                ] {
                    run(&["config", key, value]);
                }
                std::fs::write(root.join("file"), "first\n").unwrap();
                std::fs::write(root.join(".gitignore"), "ignored\n").unwrap();
                run(&["add", "."]);
                run(&["commit", "-m", "initial"]);
                run(&["tag", "v1"]);
                std::fs::write(root.join("file"), "first\nsecond\n").unwrap();
                run(&["stash", "push", "-m", "snapshot"]);
                run(&["remote", "add", "origin", "https://example.invalid/project"]);
                let mut project = RemoteProject {
                    version: 1,
                    host: "example-host".into(),
                    host_key_sha256: "SHA256:example".into(),
                    root: root.clone(),
                    helper_version: "0.4.0-example".into(),
                    sync: None,
                    created_at: 0,
                    queued_stops: vec![],
                };
                project.set_sync_record(&SyncRecord {
                    engine: "mutagen".into(),
                    tree_session: Some("recorded-tree".into()),
                    git_session: Some("recorded-git".into()),
                    host_key_sha256: project.host_key_sha256.clone(),
                    ignore_digest: String::new(),
                    excludes_added: vec![],
                    excludes_removed: vec![],
                    tripwire_allowed: vec![],
                    paused: None,
                    copied_back: vec![],
                    git_ready: true,
                });
                let env = ShimEnv {
                    cwd: root,
                    state_home: base.join("state"),
                    real_git: git,
                    exe: "/usr/bin/eitri".into(),
                    projects: vec![project],
                    flush_cap: Duration::from_millis(500),
                    mutagen: None,
                    git_environment: vec![],
                    unreadable_records: vec![],
                    enabled: true,
                };
                Self { base, env }
            }
            fn invoke(&self, args: &[&str], fake: &mut Fake) -> (i32, String, String) {
                invoke(&self.env, &os(args), fake, b"")
            }
        }
        impl Drop for Repo {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.base);
            }
        }
        fn status() -> StatusFile {
            StatusFile {
                version: STATUS_VERSION,
                host: "example-host".into(),
                phase: "ready".into(),
                git_ready: true,
                git_pct: 100,
                cycle_running: false,
                conflicts: vec![],
                problems: vec![],
                copied_back: vec![],
                paused: None,
                written_unix_ms: 0,
            }
        }
        struct Fake {
            status: Option<StatusFile>,
            calls: Vec<String>,
            outcome: FlushOutcome,
            post_outcome: Option<FlushOutcome>,
            engine_state: SyncState,
            engine_conflicts: Option<Vec<String>>,
            conflicts_error: Option<String>,
            excluded_conflicts: u64,
            connect_error: Option<String>,
            exec_cwd: Option<PathBuf>,
            result: Result<i32, String>,
            input: Vec<u8>,
            pauses: usize,
            clear_cycle: bool,
            disk: bool,
            now: std::time::Instant,
        }
        impl Default for Fake {
            fn default() -> Self {
                Self {
                    status: Some(status()),
                    calls: vec![],
                    outcome: FlushOutcome::Flushed,
                    post_outcome: None,
                    engine_state: SyncState::Watching,
                    engine_conflicts: None,
                    conflicts_error: None,
                    excluded_conflicts: 0,
                    connect_error: None,
                    exec_cwd: None,
                    result: Ok(0),
                    input: vec![],
                    pauses: 0,
                    clear_cycle: true,
                    disk: false,
                    now: std::time::Instant::now(),
                }
            }
        }
        impl Services for Fake {
            fn connect(&mut self, _: &RemoteProject) -> Result<(), String> {
                self.connect_error.clone().map_or(Ok(()), Err)
            }
            fn conflicts(&mut self, _: &str) -> Result<Conflicts, String> {
                if let Some(error) = &self.conflicts_error {
                    return Err(error.clone());
                }
                let paths = self.engine_conflicts.clone().unwrap_or_else(|| {
                    self.status
                        .as_ref()
                        .unwrap()
                        .conflicts
                        .iter()
                        .map(|c| c.path.clone())
                        .collect()
                });
                let mut status = SessionStatus::disconnected("recorded-tree", Role::Tree, "fake");
                status.state = self.engine_state.clone();
                status.conflicts = paths
                    .into_iter()
                    .map(|path| Conflict {
                        path,
                        laptop: EntryKind::File,
                        server: EntryKind::File,
                    })
                    .collect();
                status.excluded_conflicts = self.excluded_conflicts;
                Ok(status.into())
            }
            fn status(&mut self, env: &ShimEnv, project: &RemoteProject) -> Option<StatusFile> {
                if self.disk {
                    let path = env
                        .state_home
                        .join("eitri/remote/sync")
                        .join(SyncKey::new(&project.host_key_sha256, &project.root).as_str())
                        .join("status.json");
                    return std::fs::read(path)
                        .ok()
                        .and_then(|bytes| eitri_core::remote_sync::status_schema::read_status(&bytes).ok());
                }
                self.status.clone()
            }
            fn now(&self) -> std::time::Instant {
                self.now
            }
            fn pause(&mut self, duration: Duration) {
                self.now += duration;
                self.pauses += 1;
                if self.clear_cycle {
                    self.status.as_mut().unwrap().cycle_running = false;
                }
            }
            fn flush(&mut self, session: &str, cap: Duration) -> FlushOutcome {
                assert_eq!(cap, Duration::from_millis(500));
                self.calls.push(format!("flush {session}"));
                if self.calls.iter().any(|c| c == "exec") {
                    self.post_outcome.clone().unwrap_or(self.outcome.clone())
                } else {
                    self.outcome.clone()
                }
            }
            fn exec(
                &mut self,
                _: &RemoteProject,
                cwd: &Path,
                args: &[OsString],
                mut input: Box<dyn Read + Send>,
                stdout: &mut dyn Write,
                stderr: &mut dyn Write,
            ) -> Result<i32, String> {
                assert!(cwd.is_absolute());
                assert!(!args.is_empty());
                self.exec_cwd = Some(cwd.to_owned());
                self.calls.push("exec".into());
                input.read_to_end(&mut self.input).unwrap();
                stdout.write_all(b"server stdout\n").unwrap();
                stderr.write_all(b"server stderr\n").unwrap();
                self.result.clone()
            }
        }
        fn invoke(env: &ShimEnv, args: &[OsString], fake: &mut Fake, input: &[u8]) -> (i32, String, String) {
            let (mut out, mut err) = (vec![], vec![]);
            let code = run_with(
                args,
                env,
                fake,
                Box::new(std::io::Cursor::new(input.to_vec())),
                &mut out,
                &mut err,
            );
            (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
        }
        fn route(words: &[&str]) -> Route {
            classify(&os(words), Path::new("/srv/project"))
        }
        fn conflict(fake: &mut Fake) {
            fake.status.as_mut().unwrap().conflicts.push(ConflictEntry {
                path: "file".into(),
                laptop_kind: "file".into(),
                server_kind: "file".into(),
            });
        }

        #[test]
        fn read_only_forms_route_local_with_their_flags() {
            let repo = Repo::new();
            let table: &[&[&str]] = &[
                &["status", "--porcelain"],
                &["diff", "--", "file"],
                &["diff", "file"],
                &["log", "-p", "file"],
                &["show", "HEAD", "file"],
                &["diff-files", "--", "file"],
                &["diff-index", "HEAD", "--", "file"],
                &["diff-tree", "HEAD"],
                &["log", "-p", "-1"],
                &["shortlog", "HEAD"],
                &["show", "HEAD"],
                &["blame", "file"],
                &["annotate", "file"],
                &["grep", "first"],
                &["rev-parse", "HEAD"],
                &["rev-list", "HEAD"],
                &["ls-files"],
                &["ls-tree", "HEAD"],
                &["cat-file", "-p", "HEAD"],
                &["for-each-ref"],
                &["show-ref"],
                &["merge-base", "HEAD", "HEAD"],
                &["name-rev", "HEAD"],
                &["describe", "--tags"],
                &["symbolic-ref", "HEAD"],
                &["check-ignore", "ignored"],
                &["check-attr", "-a", "file"],
                &["branch"],
                &["branch", "-l"],
                &["branch", "--list"],
                &["branch", "-a"],
                &["branch", "-r"],
                &["branch", "-v"],
                &["branch", "--show-current"],
                &["branch", "--contains", "HEAD"],
                &["tag"],
                &["tag", "-l"],
                &["tag", "--list"],
                &["remote"],
                &["remote", "-v"],
                &["remote", "get-url", "origin"],
                &["stash", "list"],
                &["stash", "show"],
                &["stash", "show", "-p"],
                &["stash", "list", "-p"],
                &["reflog"],
                &["reflog", "show"],
                &["worktree", "list"],
                &["config", "--local", "--get", "user.name"],
                &["--version"],
                &["config", "--get-all", "user.name"],
                &["config", "--get-regexp", "^user"],
                &["config", "--list", "--null", "--local"],
                &["var", "GIT_AUTHOR_IDENT"],
                &["help", "-a"],
            ];
            assert!(table.len() >= 45);
            let mut failures = vec![];
            for words in table {
                let Route::Local { extra } = route(words) else {
                    panic!("{words:?}");
                };
                let diff = matches!(
                    words[0],
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
                ) || words[0] == "stash";
                assert_eq!(
                    extra,
                    if diff {
                        vec!["--no-ext-diff", "--no-textconv"]
                    } else {
                        vec![]
                    },
                    "{words:?}"
                );
                let mut fake = Fake::default();
                let (code, _, err) = repo.invoke(words, &mut fake);
                if code != 0 {
                    failures.push(format!("{words:?}: exit {code}: {err}"));
                }
                assert!(fake.calls.is_empty(), "{words:?}: {err}");
            }
            assert!(failures.is_empty(), "{}", failures.join("\n"));
        }
        #[test]
        fn gitsigns_style_git_dir_reads_route_local() {
            let repo = Repo::new();
            let root = &repo.env.cwd;
            let args = vec![
                format!("--git-dir={}", root.join(".git").display()).into(),
                format!("--work-tree={}", root.display()).into(),
                "diff".into(),
                "--".into(),
                "file".into(),
            ];
            let mut fake = Fake::default();
            assert_eq!(invoke(&repo.env, &args, &mut fake, b"").0, 0);
            assert!(fake.calls.is_empty());
        }
        #[test]
        fn foreign_git_dir_forwards() {
            let repo = Repo::new();
            let args = vec![
                OsString::from("--git-dir=/srv/other/.git"),
                format!("--work-tree={}", repo.env.cwd.display()).into(),
                "status".into(),
            ];
            let mut fake = Fake::default();
            assert_eq!(invoke(&repo.env, &args, &mut fake, b"").0, 0);
            assert!(fake.calls.contains(&"exec".into()));
        }
        #[test]
        fn unknown_and_alias_route_forward() {
            for cmd in ["unknown", "my-alias"] {
                assert_eq!(
                    route(&[cmd]),
                    Route::Forward {
                        whole_tree: false,
                        paths: vec![],
                    }
                );
            }
        }
        #[test]
        fn branch_operands_after_separator_forward_unless_listing() {
            for words in [&["branch", "--", "topic"][..], &["branch", "-v", "--", "topic"]] {
                assert!(matches!(route(words), Route::Forward { .. }), "{words:?}");
            }
            for words in [&["branch", "--"][..], &["branch", "--list", "--", "topic"]] {
                assert!(matches!(route(words), Route::Local { .. }), "{words:?}");
            }
        }
        #[test]
        fn leading_separator_reports_git_usage_without_forwarding() {
            assert_eq!(route(&["--"]), Route::NotRemote);
            let repo = Repo::new();
            let mut fake = Fake::default();
            let (code, _, err) = repo.invoke(&["--"], &mut fake);
            assert_eq!(code, 129, "{err}");
            assert!(fake.calls.is_empty());
        }
        #[test]
        fn no_replace_objects_before_dash_c_still_selects_mirror() {
            let mut repo = Repo::new();
            let root = std::mem::replace(&mut repo.env.cwd, repo.base.clone());
            let mut fake = Fake::default();
            let (code, _, err) = repo.invoke(&["--no-replace-objects", "-C", "project", "add", "file"], &mut fake);
            assert_eq!(code, 0, "{err}");
            assert_eq!(fake.exec_cwd, Some(root));
        }
        #[test]
        fn relative_git_dir_before_dash_c_still_selects_mirror() {
            let mut repo = Repo::new();
            let root = std::mem::replace(&mut repo.env.cwd, repo.base.clone());
            for words in [
                &["--git-dir=.git", "-C", "project", "branch", "topic"][..],
                &["--git-dir", ".git", "-C", "project", "branch", "topic"],
            ] {
                let mut fake = Fake::default();
                let (code, _, err) = repo.invoke(words, &mut fake);
                assert_eq!(code, 0, "{err}");
                assert_eq!(fake.exec_cwd.as_ref(), Some(&root), "{words:?}");
            }
        }
        #[test]
        fn failed_dash_c_stops_before_later_directory_changes() {
            let repo = Repo::new();
            let mut fake = Fake::default();
            for dir in ["missing", "file"] {
                let (code, _, err) = repo.invoke(&["-C", dir, "-C", "..", "add", "file"], &mut fake);
                assert_eq!(code, 128, "{err}");
                assert!(err.contains(dir), "{err}");
                assert!(fake.calls.is_empty());
            }
        }
        #[test]
        fn global_options_are_skipped_and_dash_c_is_applied() {
            let mut repo = Repo::new();
            repo.env.cwd = repo.base.clone();
            let mut fake = Fake::default();
            let (code, out, err) = repo.invoke(
                &[
                    "-C",
                    "project",
                    "-C",
                    "sub",
                    "-c",
                    "color.ui=never",
                    "--no-pager",
                    "rev-parse",
                    "--show-prefix",
                ],
                &mut fake,
            );
            assert_eq!(code, 0, "{err}");
            assert_eq!(out, "sub/\n");
            assert!(fake.calls.is_empty());
            let (code, _, err) = repo.invoke(
                &[
                    "-C",
                    "project",
                    "-c",
                    "core.fsmonitor=/does-not-exist",
                    "status",
                    "--porcelain",
                ],
                &mut fake,
            );
            assert_eq!(code, 0, "{err}");
        }
        #[test]
        fn caller_textconv_or_ext_diff_forwards() {
            for words in [
                &["diff", "--ext-diff"][..],
                &["log", "--textconv"],
                &["cat-file", "--filters", "HEAD:file"],
            ] {
                assert!(matches!(route(words), Route::Forward { .. }));
            }
        }
        #[test]
        fn editor_needing_forms_are_refused_with_the_line() {
            let repo = Repo::new();
            let table: &[&[&str]] = &[
                &["commit"],
                &["commit", "--amend"],
                &["commit", "--edit", "-m", "message"],
                &["revert", "HEAD"],
                &["merge", "--continue"],
                &["cherry-pick", "--continue"],
                &["revert", "--continue"],
                &["rebase", "--continue"],
                &["rebase", "-i"],
                &["rebase", "--interactive"],
                &["rebase", "--edit-todo"],
                &["tag", "-a", "v2"],
                &["tag", "-s", "v2"],
                &["merge", "--edit", "topic"],
                &["pull", "--edit"],
                &["notes", "add"],
                &["notes", "append"],
                &["add", "-p"],
                &["add", "-i"],
                &["add", "-e"],
                &["checkout", "-p"],
                &["reset", "-p"],
                &["stash", "-p"],
            ];
            assert!(table.len() >= 20);
            for words in table {
                assert_eq!(route(words), Route::RefuseEditor, "{words:?}");
                let mut fake = Fake::default();
                let (code, _, err) = repo.invoke(words, &mut fake);
                assert_eq!(code, 1);
                let line = if words[0] == "revert" {
                    "run git revert from the terminal (it runs on example-host)"
                } else {
                    "commit from the terminal (it runs on example-host) or with -m"
                };
                assert_eq!(err.trim_end(), format!("Eitri: {line}"));
                assert!(fake.calls.is_empty());
            }
            for words in [
                &["commit", "-m", "message"][..],
                &["commit", "-F", "-"],
                &["commit", "-C", "HEAD"],
                &["commit", "--amend", "--no-edit"],
                &["revert", "--no-edit", "HEAD"],
                &["tag", "-a", "v2", "-m", "message"],
            ] {
                assert!(matches!(route(words), Route::Forward { .. }), "{words:?}");
            }
        }
        #[test]
        fn commit_autosquash_only_skips_editor_with_a_complete_message() {
            for words in [
                &["commit", "--squash", "HEAD"][..],
                &["commit", "--fixup=amend:HEAD"],
                &["commit", "--fixup", "reword:HEAD"],
            ] {
                assert_eq!(route(words), Route::RefuseEditor, "{words:?}");
            }
            for words in [
                &["commit", "--fixup", "HEAD"][..],
                &["commit", "--fixup=HEAD"],
                &["commit", "--fixup=amend:HEAD", "--no-edit"],
                &["commit", "--squash=HEAD", "-m", "message"],
            ] {
                assert!(matches!(route(words), Route::Forward { .. }), "{words:?}");
            }
        }

        #[test]
        fn commit_message_values_are_not_editor_flags() {
            for words in [
                &["commit", "-m", "-e"][..],
                &["commit", "-sm", "--edit"],
                &["commit", "--message", "--edit"],
                &["commit", "-m", "--"],
            ] {
                assert!(matches!(route(words), Route::Forward { .. }), "{words:?}");
            }
            assert_eq!(route(&["commit", "-m", "--", "-e"]), Route::RefuseEditor);
        }

        #[test]
        fn commit_and_rebase_short_option_values_are_not_flags() {
            for words in [
                &["commit", "-m", "x", "-Sdeadbeef"][..],
                &["rebase", "-Xtheirs", "main"],
                &["rebase", "-X", "-i", "main"],
                &["rebase", "-srecursive", "main"],
                &["rebase", "-xprintf example", "main"],
                &["rebase", "-Skeyid", "main"],
                &["rebase", "-rrebase-cousins", "main"],
            ] {
                assert!(matches!(route(words), Route::Forward { .. }), "{words:?}");
            }
            for words in [
                &["commit", "-Smykey"][..],
                &["commit", "-m", "x", "-S", "-e"],
                &["rebase", "-Xtheirs", "-i", "main"],
            ] {
                assert_eq!(route(words), Route::RefuseEditor, "{words:?}");
            }
        }

        #[test]
        fn notes_find_the_editor_form_after_the_ref_option() {
            for words in [
                &["notes", "--ref=review", "add", "HEAD"][..],
                &["notes", "--ref", "review", "append", "HEAD"],
            ] {
                assert_eq!(route(words), Route::RefuseEditor, "{words:?}");
            }
            for words in [
                &["notes", "--ref=review", "add", "-m", "message", "HEAD"][..],
                &["notes", "--ref", "review", "append", "-F", "-", "HEAD"],
                &["notes", "--ref=review", "show", "HEAD"],
            ] {
                assert!(matches!(route(words), Route::Forward { .. }), "{words:?}");
            }
        }

        #[test]
        fn help_topics_route_local() {
            for words in [&["help"][..], &["help", "-a"], &["help", "status"]] {
                assert_eq!(route(words), Route::Local { extra: vec![] }, "{words:?}");
            }
            let repo = Repo::new();
            let mut fake = Fake::default();
            conflict(&mut fake);
            let (code, out, err) = repo.invoke(
                &[
                    "-c",
                    "man.viewer=example",
                    "-c",
                    "man.example.cmd=printf '%s\\n'",
                    "help",
                    "--man",
                    "status",
                ],
                &mut fake,
            );
            assert_eq!(code, 0, "{err}");
            assert_eq!(out, "git-status\n");
            assert!(fake.calls.is_empty());
        }

        #[test]
        fn revert_no_commit_and_skip_forward_without_an_editor() {
            for words in [
                &["revert", "--no-commit", "HEAD"][..],
                &["revert", "-n", "HEAD"],
                &["revert", "--skip"],
            ] {
                assert_eq!(
                    route(words),
                    Route::Forward {
                        whole_tree: true,
                        paths: vec![],
                    },
                    "{words:?}"
                );
            }
            assert_eq!(route(&["revert", "-m", "1", "HEAD"]), Route::RefuseEditor);
        }

        #[test]
        fn whole_tree_set_is_by_subcommand() {
            for words in [
                &["commit", "-m", "message"][..],
                &["commit", "-am", "message"],
                &["add", "-A"],
                &["add", "."],
                &["add", "-u"],
                &["stash", "push"],
                &["checkout", "topic"],
                &["switch", "main"],
                &["restore"],
                &["reset", "--hard"],
                &["reset", "--merge"],
                &["reset", "--keep"],
                &["merge", "topic"],
                &["rebase", "main"],
                &["pull"],
                &["cherry-pick", "HEAD"],
                &["revert", "--no-edit", "HEAD"],
                &["am"],
                &["apply"],
            ] {
                assert!(
                    matches!(route(words), Route::Forward { whole_tree: true, .. }),
                    "{words:?}"
                );
            }
            for words in [
                &["add", "file"][..],
                &["add", "-u", "file"],
                &["commit", "-m", "message", "--", "file"],
                &["checkout", "--", "file"],
                &["restore", "file"],
                &["apply", "--cached"],
            ] {
                assert!(
                    matches!(route(words), Route::Forward { whole_tree: false, .. }),
                    "{words:?}"
                );
            }
        }
        #[test]
        fn config_write_forms_forward() {
            for words in [
                &["config", "user.name", "Changed"][..],
                &["config", "--unset", "user.name"],
                &["config", "--add", "a.b", "x"],
                &["config", "--global", "--get", "a.b"],
                &["config", "--get", "--file", "/srv/other", "a.b"],
            ] {
                assert!(matches!(route(words), Route::Forward { .. }));
            }
        }
        #[test]
        fn outside_a_remote_root_the_real_git_runs_unchanged() {
            let mut repo = Repo::new();
            repo.env.projects.clear();
            let args = os(&["-c", "alias.example=rev-parse HEAD", "example"]);
            let mut fake = Fake::default();
            let (code, out, err) = invoke(&repo.env, &args, &mut fake, b"");
            let plain = Command::new(&repo.env.real_git)
                .args(&args)
                .current_dir(&repo.env.cwd)
                .output()
                .unwrap();
            assert_eq!(code, plain.status.code().unwrap(), "{err}");
            assert_eq!(out.as_bytes(), plain.stdout);
            assert!(fake.calls.is_empty());
        }
        #[test]
        fn history_arriving_refuses_reads_and_forwards_writes() {
            let repo = Repo::new();
            let mut fake = Fake::default();
            fake.status.as_mut().unwrap().git_ready = false;
            fake.status.as_mut().unwrap().git_pct = 42;
            let (code, _, err) = repo.invoke(&["status"], &mut fake);
            assert_eq!(code, 128);
            assert!(err.contains("is still arriving from example-host (42 %)"));
            assert!(fake.calls.is_empty());
            assert_eq!(repo.invoke(&["add", "file"], &mut fake).0, 0);
            assert!(fake.calls.contains(&"exec".into()));
        }
        #[test]
        fn reads_wait_briefly_for_an_applying_cycle() {
            let repo = Repo::new();
            let mut fake = Fake::default();
            fake.status.as_mut().unwrap().cycle_running = true;
            assert_eq!(repo.invoke(&["status", "--porcelain"], &mut fake).0, 0);
            assert_eq!(fake.pauses, 1);
            assert!(fake.calls.is_empty());
        }
        #[test]
        fn an_applying_cycle_stops_waiting_at_the_budget() {
            let repo = Repo::new();
            let mut fake = Fake {
                clear_cycle: false,
                ..Default::default()
            };
            fake.status.as_mut().unwrap().cycle_running = true;
            let start = fake.now;
            assert_eq!(repo.invoke(&["status"], &mut fake).0, 0);
            assert_eq!(fake.now - start, Duration::from_secs(1));
            assert_eq!(fake.pauses, 50);
        }

        #[test]
        fn local_cat_file_streams_stdin_and_returns_real_output() {
            let repo = Repo::new();
            let mut fake = Fake::default();
            let (code, out, err) = invoke(
                &repo.env,
                &os(&["cat-file", "--batch-check"]),
                &mut fake,
                b"HEAD:file\n",
            );
            assert_eq!(code, 0, "{err}");
            assert!(out.ends_with(" blob 6\n"), "{out}");
            assert!(fake.calls.is_empty());
        }

        #[test]
        fn disk_status_uses_the_shared_private_file_reader() {
            struct Disk(Fake);
            impl Services for Disk {
                fn conflicts(&mut self, name: &str) -> Result<Conflicts, String> {
                    self.0.conflicts(name)
                }
                fn flush(&mut self, name: &str, cap: Duration) -> FlushOutcome {
                    self.0.flush(name, cap)
                }
                fn exec(
                    &mut self,
                    project: &RemoteProject,
                    cwd: &Path,
                    args: &[OsString],
                    input: Box<dyn Read + Send>,
                    out: &mut dyn Write,
                    err: &mut dyn Write,
                ) -> Result<i32, String> {
                    self.0.exec(project, cwd, args, input, out, err)
                }
            }
            let repo = Repo::new();
            let project = &repo.env.projects[0];
            let path = repo
                .env
                .state_home
                .join("eitri/remote/sync")
                .join(SyncKey::new(&project.host_key_sha256, &project.root).as_str())
                .join("status.json");
            agent::private_fs::create_private_dir_all(path.parent().unwrap(), &repo.env.state_home).unwrap();
            let mut status = status();
            status.git_ready = false;
            status.git_pct = 17;
            agent::private_fs::write_private(&path, &serde_json::to_vec(&status).unwrap()).unwrap();
            let mut disk = Disk(Fake::default());
            let (mut out, mut err) = (vec![], vec![]);
            assert_eq!(
                run_with(
                    &os(&["status"]),
                    &repo.env,
                    &mut disk,
                    Box::new(std::io::empty()),
                    &mut out,
                    &mut err
                ),
                128
            );
            assert!(String::from_utf8(err).unwrap().contains("(17 %)"));
            std::fs::remove_file(&path).unwrap();
            let (mut out, mut err) = (vec![], vec![]);
            assert_eq!(
                run_with(
                    &os(&["add", "file"]),
                    &repo.env,
                    &mut disk,
                    Box::new(std::io::empty()),
                    &mut out,
                    &mut err
                ),
                1
            );
            assert!(String::from_utf8(err).unwrap().contains("is unknown; nothing was run"));
            assert!(disk.0.calls.is_empty());
        }

        #[test]
        fn forward_flushes_before_and_both_after() {
            let repo = Repo::new();
            let mut fake = Fake::default();
            assert_eq!(repo.invoke(&["add", "file"], &mut fake).0, 0);
            assert_eq!(
                fake.calls,
                [
                    "flush recorded-tree",
                    "exec",
                    "flush recorded-tree",
                    "flush recorded-git"
                ]
            );
        }
        #[test]
        fn forward_refuses_on_a_named_conflict() {
            let mut repo = Repo::new();
            let mut fake = Fake::default();
            conflict(&mut fake);
            repo.env.cwd.push("sub");
            let (code, _, err) = repo.invoke(&["add", "../file"], &mut fake);
            assert_eq!(code, 1);
            assert!(err.contains(
                "these files differ between this laptop and example-host: file; settle them with :EitriSync"
            ));
            assert_eq!(fake.calls, ["flush recorded-tree"]);
            fake.calls.clear();
            assert_eq!(repo.invoke(&["add", "another"], &mut fake).0, 0);
            assert!(fake.calls.contains(&"exec".into()));
        }
        #[test]
        fn add_all_with_explicit_path_ignores_unrelated_conflicts() {
            let repo = Repo::new();
            let mut fake = Fake::default();
            conflict(&mut fake);
            for flag in ["-A", "--all", "-Af"] {
                let (code, _, err) = repo.invoke(&["add", flag, "--", "clean.txt"], &mut fake);
                assert_eq!(code, 0, "{err}");
                assert!(fake.calls.iter().any(|call| call == "exec"));
                fake.calls.clear();
            }
        }
        #[test]
        fn add_checks_conflicts_on_both_sides_of_separator() {
            assert_eq!(
                route(&["add", "-", "--", "--edit"]),
                Route::Forward {
                    whole_tree: false,
                    paths: vec!["-".into(), "--edit".into()],
                }
            );
            let repo = Repo::new();
            for paths in [["file", "--", "clean.txt"], ["clean.txt", "--", "file"]] {
                let mut fake = Fake::default();
                conflict(&mut fake);
                let (code, _, err) = repo.invoke(&["add", paths[0], paths[1], paths[2]], &mut fake);
                assert_eq!(code, 1, "{err}");
                assert!(err.contains(": file;"), "{err}");
                assert_eq!(fake.calls, ["flush recorded-tree"]);
            }
        }
        #[test]
        fn forward_refuses_reset_when_conflict_status_is_unreadable() {
            let repo = Repo::new();
            let mut fake = Fake {
                conflicts_error: Some("daemon socket absent".into()),
                ..Default::default()
            };
            let (code, _, err) = repo.invoke(&["reset", "--hard"], &mut fake);
            assert_eq!(code, 1);
            assert!(err.contains("sync state"), "{err}");
            assert!(
                err.contains("is unknown; nothing was run (daemon socket absent)"),
                "{err}"
            );
            assert_eq!(fake.calls, ["flush recorded-tree"]);
        }
        #[test]
        fn forward_refuses_when_sync_conflicts_are_not_current() {
            let repo = Repo::new();
            for (state, label) in [
                (SyncState::Disconnected("endpoint disconnected".into()), "disconnected"),
                (SyncState::Connecting, "connecting"),
                (SyncState::Halted("halted-on-root-emptied".into()), "halted"),
                (SyncState::Paused, "paused"),
                (SyncState::Other("future-state".into()), "in an unknown state"),
                (SyncState::Scanning, "scanning"),
            ] {
                for args in [["reset", "--hard"], ["add", "file"], ["apply", "--cached"]] {
                    let mut fake = Fake {
                        engine_state: state.clone(),
                        engine_conflicts: Some(vec![]),
                        ..Default::default()
                    };
                    let (code, out, err) = repo.invoke(&args, &mut fake);
                    assert_eq!(code, 1, "{state:?}, {args:?}: {err}");
                    assert_eq!(
                        err,
                        format!("Eitri: sync with example-host is {label}; nothing was run\n")
                    );
                    assert!(out.is_empty());
                    assert_eq!(fake.calls, ["flush recorded-tree"]);
                }
            }
        }
        #[test]
        fn forward_runs_reset_without_conflicts_in_current_sync_states() {
            let repo = Repo::new();
            for state in [SyncState::Watching, SyncState::Staging, SyncState::Applying] {
                let mut fake = Fake {
                    engine_state: state.clone(),
                    ..Default::default()
                };
                let (code, out, err) = repo.invoke(&["reset", "--hard"], &mut fake);
                assert_eq!(code, 0, "{state:?}: {err}");
                assert_eq!(out, "server stdout\n");
                assert_eq!(
                    fake.calls,
                    [
                        "flush recorded-tree",
                        "exec",
                        "flush recorded-tree",
                        "flush recorded-git"
                    ]
                );
            }
        }
        #[test]
        fn forward_refuses_icase_restore_on_matching_conflicts() {
            let repo = Repo::new();
            let mut fake = Fake {
                engine_conflicts: Some(vec!["Foo.txt".into(), "Parent/Child.txt".into()]),
                ..Default::default()
            };
            for (path, conflict) in [("foo.txt", "Foo.txt"), ("parent", "Parent/Child.txt")] {
                fake.calls.clear();
                let (code, _, err) = repo.invoke(&["--icase-pathspecs", "restore", "--", path], &mut fake);
                assert_eq!(code, 1);
                assert!(err.contains(conflict), "{err}");
                assert_eq!(fake.calls, ["flush recorded-tree"]);
            }
            assert_eq!(repo.invoke(&["restore", "--", "foo.txt"], &mut fake).0, 0);
            assert_eq!(
                repo.invoke(&["--icase-pathspecs", "restore", "--", "parental"], &mut fake)
                    .0,
                0
            );
        }
        #[test]
        fn whole_tree_forms_refuse_on_any_conflict() {
            let repo = Repo::new();
            for words in [
                &["commit", "-m", "message"][..],
                &["add", "-A"],
                &["merge", "topic"],
                &["apply"],
            ] {
                let mut fake = Fake::default();
                conflict(&mut fake);
                assert_eq!(repo.invoke(words, &mut fake).0, 1);
                assert_eq!(fake.calls, ["flush recorded-tree"]);
            }
        }
        #[test]
        fn non_whole_tree_commands_forward_with_unrelated_conflicts() {
            let repo = Repo::new();
            for words in [
                &["push"][..],
                &["push", "--", "file"],
                &["fetch"],
                &["tag", "v2"],
                &["branch", "-d", "topic"],
                &["remote", "add", "upstream", "/srv/project"],
                &["config", "user.name"],
                &["diff", "--ext-diff"],
            ] {
                let mut fake = Fake::default();
                conflict(&mut fake);
                let (code, _, err) = repo.invoke(words, &mut fake);
                assert_eq!(code, 0, "{words:?}: {err}");
                assert_eq!(
                    fake.calls,
                    [
                        "flush recorded-tree",
                        "exec",
                        "flush recorded-tree",
                        "flush recorded-git"
                    ]
                );
            }
            let mut fake = Fake::default();
            conflict(&mut fake);
            let (code, _, err) = repo.invoke(&["diff", "--ext-diff", "--", "file"], &mut fake);
            assert_eq!(code, 1, "{err}");
            assert!(err.contains(": file;"), "{err}");
            assert_eq!(fake.calls, ["flush recorded-tree"]);

            std::fs::rename(repo.env.cwd.join(".git"), repo.base.join("elsewhere")).unwrap();
            let mut fake = Fake::default();
            conflict(&mut fake);
            let (code, _, err) = repo.invoke(&["status"], &mut fake);
            assert_eq!(code, 0, "{err}");
            assert_eq!(
                fake.calls,
                [
                    "flush recorded-tree",
                    "exec",
                    "flush recorded-tree",
                    "flush recorded-git"
                ]
            );
        }

        #[test]
        fn implicit_commit_paths_limit_conflict_checks() {
            let repo = Repo::new();
            for path in ["clean.txt", "file"] {
                let words = [
                    "commit",
                    "--author",
                    "Example <example@example.invalid>",
                    "-m",
                    "message",
                    path,
                ];
                assert_eq!(
                    route(&words),
                    Route::Forward {
                        whole_tree: false,
                        paths: vec![path.into()],
                    }
                );
                let mut fake = Fake::default();
                conflict(&mut fake);
                let (code, _, err) = repo.invoke(&words, &mut fake);
                assert_eq!(code, i32::from(path == "file"), "{words:?}: {err}");
                assert_eq!(fake.calls.iter().any(|call| call == "exec"), path == "clean.txt");
            }
            assert_eq!(
                route(&["commit", "-m", "message", "-S", "clean.txt", "--", "file"]),
                Route::Forward {
                    whole_tree: false,
                    paths: vec!["clean.txt".into(), "file".into()],
                }
            );
        }

        #[test]
        fn revision_output_files_are_forwarded() {
            let repo = Repo::new();
            for (command, operand) in [
                ("rev-list", "HEAD"),
                ("shortlog", "HEAD"),
                ("blame", "file"),
                ("annotate", "file"),
            ] {
                for words in [
                    &[command, "--output=report", operand][..],
                    &[command, "--output", "report", operand],
                ] {
                    assert_eq!(
                        route(words),
                        Route::Forward {
                            whole_tree: false,
                            paths: vec!["report".into()],
                        },
                        "{words:?}"
                    );
                    let mut fake = Fake::default();
                    conflict(&mut fake);
                    let (code, _, err) = repo.invoke(words, &mut fake);
                    assert_eq!(code, 0, "{words:?}: {err}");
                    assert!(fake.calls.iter().any(|call| call == "exec"));
                    assert!(!repo.env.cwd.join("report").exists());
                }
            }
            for words in [
                &["rev-list", "--file", "--output=report", "HEAD"][..],
                &["shortlog", "--end-of-options", "--output=report", "HEAD"],
                &["blame", "--grep", "--", "--output=report", "file"],
            ] {
                assert!(matches!(route(words), Route::Forward { .. }), "{words:?}");
                let mut fake = Fake::default();
                let (code, _, err) = repo.invoke(words, &mut fake);
                assert_eq!(code, 0, "{words:?}: {err}");
                assert!(fake.calls.iter().any(|call| call == "exec"));
                assert!(!repo.env.cwd.join("report").exists());
            }
            assert!(matches!(
                route(&["blame", "HEAD", "--", "--output=report"]),
                Route::Local { .. }
            ));
            let mut fake = Fake::default();
            conflict(&mut fake);
            let (code, _, err) = repo.invoke(&["rev-list", "--output=file", "HEAD"], &mut fake);
            assert_eq!(code, 1, "{err}");
            assert!(err.contains(": file;"), "{err}");
            assert_eq!(fake.calls, ["flush recorded-tree"]);
        }

        #[test]
        fn path_reads_still_run_during_a_conflict() {
            let repo = Repo::new();
            let mut fake = Fake::default();
            conflict(&mut fake);
            let (code, _, err) = repo.invoke(&["diff", "--", "file"], &mut fake);
            assert_eq!(code, 0, "{err}");
            assert!(fake.calls.is_empty());
        }
        #[test]
        fn forward_flush_timeout_runs_nothing() {
            let repo = Repo::new();
            for outcome in [FlushOutcome::TimedOut, FlushOutcome::Failed("session is paused".into())] {
                let mut fake = Fake {
                    outcome,
                    ..Default::default()
                };
                let (code, _, err) = repo.invoke(&["add", "file"], &mut fake);
                assert_eq!(code, 128);
                assert!(err.contains("your last save has not reached example-host; nothing was run"));
                assert_eq!(fake.calls, ["flush recorded-tree"]);
            }
        }
        #[test]
        fn forward_with_lost_link_reports_unknown_outcome_and_never_retries() {
            let repo = Repo::new();
            let mut fake = Fake {
                result: Err("the outcome on example-host is unknown; check with `git status` in the terminal".into()),
                ..Default::default()
            };
            let (code, _, err) = repo.invoke(&["commit", "-m", "message"], &mut fake);
            assert_eq!(code, 128);
            assert!(err.contains("the outcome on example-host is unknown; check with `git status` in the terminal"));
            assert_eq!(fake.calls.iter().filter(|c| *c == "exec").count(), 1);
        }
        #[test]
        fn forward_passes_stdin_and_exit_status() {
            let repo = Repo::new();
            let mut fake = Fake {
                result: Ok(1),
                ..Default::default()
            };
            let patch = b"diff --git a/file b/file\n--- a/file\n+++ b/file\n@@ -1 +1 @@\n-first\n+second\n";
            let (code, out, err) = invoke(&repo.env, &os(&["apply", "--cached"]), &mut fake, patch);
            assert_eq!(code, 1);
            assert_eq!(fake.input, patch);
            assert_eq!(out, "server stdout\n");
            assert_eq!(err, "server stderr\n");
        }
        #[test]
        fn offline_local_copy_refuses_writes() {
            let repo = Repo::new();
            let mut fake = Fake {
                status: None,
                connect_error: Some(
                    "git writes in this copy go to example-host, which cannot be reached; nothing was run".into(),
                ),
                ..Default::default()
            };
            let (code, _, err) = repo.invoke(&["add", "file"], &mut fake);
            assert_eq!(code, 1);
            assert!(
                err.contains("git writes in this copy go to example-host, which cannot be reached; nothing was run")
            );
            assert!(fake.calls.is_empty());
            assert_eq!(repo.invoke(&["status"], &mut fake).0, 0);
        }
        #[test]
        fn missing_or_unreadable_status_keeps_reads_and_refuses_writes() {
            let repo = Repo::new();
            let project = &repo.env.projects[0];
            let path = repo
                .env
                .state_home
                .join("eitri/remote/sync")
                .join(SyncKey::new(&project.host_key_sha256, &project.root).as_str())
                .join("status.json");
            for bytes in [None, Some(b"{}".as_slice()), Some(b"{\"version\":2}".as_slice())] {
                if let Some(bytes) = bytes {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(&path, bytes).unwrap();
                }
                let mut fake = Fake {
                    disk: true,
                    ..Default::default()
                };
                assert_eq!(repo.invoke(&["status"], &mut fake).0, 0);
                let (code, _, err) = repo.invoke(&["add", "file"], &mut fake);
                assert_eq!(code, 1);
                assert!(err.contains("is unknown; nothing was run"));
                assert!(fake.calls.is_empty());
            }
        }
        #[test]
        fn missing_or_indirect_history_forwards_every_read() {
            let repo = Repo::new();
            let gitdir = repo.env.cwd.join(".git");
            std::fs::rename(&gitdir, repo.base.join("elsewhere")).unwrap();
            for linked in [false, true] {
                if linked {
                    std::fs::remove_file(&gitdir).unwrap();
                    std::os::unix::fs::symlink(repo.base.join("elsewhere"), &gitdir).unwrap();
                } else {
                    std::fs::write(&gitdir, "gitdir: ../elsewhere\n").unwrap();
                }
                let mut fake = Fake::default();
                assert_eq!(repo.invoke(&["status"], &mut fake).0, 0);
                assert!(fake.calls.contains(&"exec".into()));
            }
        }

        fn assert_unmirrored_read_forwards_stdin(words: &[&str], input: &[u8]) {
            let repo = Repo::new();
            std::fs::rename(repo.env.cwd.join(".git"), repo.base.join("elsewhere")).unwrap();
            let mut fake = Fake::default();
            let (code, _, err) = invoke(&repo.env, &os(words), &mut fake, input);
            assert_eq!(code, 0, "{words:?}: {err}");
            assert_eq!(fake.input, input, "{words:?}");
        }

        #[test]
        fn unmirrored_shortlog_preserves_piped_log() {
            assert_unmirrored_read_forwards_stdin(&["shortlog", "-sn"], b"Author: Example\n\n    Change\n");
            assert!(!eitri_core::remote_sync::git_shim::reads_stdin(
                &os(&["shortlog", "-sn"]),
                true
            ));
        }

        #[test]
        fn unmirrored_show_ref_preserves_exclude_existing_input() {
            for option in ["--exclude-existing", "--exclude-existing=refs/heads/"] {
                assert_unmirrored_read_forwards_stdin(&["show-ref", option], b"refs/heads/new\n");
            }
            assert!(!eitri_core::remote_sync::git_shim::reads_stdin(
                &os(&["show-ref", "--heads"]),
                false
            ));
        }

        #[test]
        fn forwarded_non_input_forms_close_before_reading_a_never_eof_input() {
            struct NeverEof;
            impl Read for NeverEof {
                fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                    panic!("stdin must never be read");
                }
            }
            let repo = Repo::new();
            let mut fake = Fake::default();
            assert_eq!(
                run_with(
                    &os(&["commit", "-mrelease"]),
                    &repo.env,
                    &mut fake,
                    Box::new(NeverEof),
                    &mut vec![],
                    &mut vec![]
                ),
                0
            );
            assert!(fake.input.is_empty());
            for words in [
                &["apply"][..],
                &["am", "-"],
                &["commit", "-F", "-"],
                &["commit", "-m", "x", "--pathspec-from-file", "-"],
                &["stash", "push", "--pathspec-from-file", "-"],
                &["reset", "--pathspec-from-file", "-"],
                &["commit", "-s", "-F", "-"],
                &["commit", "-e", "-F", "-"],
                &["tag", "-s", "-F", "-", "v2"],
                &["commit", "-sF-"],
                &["hash-object", "--stdin"],
                &["update-index", "--index-info"],
                &["unknown"],
            ] {
                assert!(
                    eitri_core::remote_sync::git_shim::reads_stdin(&os(words), false),
                    "{words:?}"
                );
                assert!(
                    !eitri_core::remote_sync::git_shim::reads_stdin(&os(words), true),
                    "{words:?}"
                );
            }
            for words in [
                &["apply", "patch.diff"][..],
                &["am", "patch.mbox"],
                &["commit", "-mrelease"],
                &["commit", "-m", "--stdin"],
                &["commit", "-t", "--stdin"],
                &["commit", "-sm", "--stdin"],
                &["grep", "-e", "--stdin"],
                &["grep", "--regexp", "--stdin"],
                &["restore", "-s", "--stdin"],
                &["commit", "-m", "-F", "--", "-"],
                &["apply", "--", "--stdin"],
                &["push"],
                &["diff"],
            ] {
                assert!(
                    !eitri_core::remote_sync::git_shim::reads_stdin(&os(words), false),
                    "{words:?}"
                );
            }
        }

        #[test]
        fn forwarded_pathspec_and_signoff_forms_preserve_stdin() {
            let repo = Repo::new();
            for words in [
                &["commit", "-m", "x", "--pathspec-from-file", "-"][..],
                &["stash", "push", "--pathspec-from-file", "-"],
                &["reset", "--pathspec-from-file", "-"],
                &["commit", "-s", "-F", "-"],
                &["tag", "-s", "-F", "-", "v2"],
                &["unknown"],
                &["example-alias"],
            ] {
                let mut fake = Fake::default();
                let (code, _, err) = invoke(&repo.env, &os(words), &mut fake, b"file\n");
                assert_eq!(code, 0, "{words:?}: {err}");
                assert_eq!(fake.input, b"file\n", "{words:?}");
                assert_eq!(fake.calls.iter().filter(|c| *c == "exec").count(), 1);
            }
        }

        #[test]
        fn excluded_conflicts_refuse_even_unrelated_paths_and_name_known_conflicts() {
            let repo = Repo::new();
            for known in [vec![], vec!["elsewhere".to_owned()]] {
                let mut fake = Fake {
                    engine_conflicts: Some(known.clone()),
                    excluded_conflicts: 1,
                    ..Default::default()
                };
                let (code, _, err) = repo.invoke(&["add", "file"], &mut fake);
                assert_eq!(code, 1);
                assert!(err.contains("more conflicts than Mutagen lists"), "{err}");
                assert!(err.contains(":EitriSync"));
                if !known.is_empty() {
                    assert!(err.contains("elsewhere"), "{err}");
                }
                assert_eq!(fake.calls, ["flush recorded-tree"]);
            }
        }

        #[test]
        fn non_utf8_forward_refusal_exits_one() {
            use std::os::unix::ffi::OsStringExt;
            let repo = Repo::new();
            let mut fake = Fake {
                result: Err("remote git argument is not UTF-8; nothing was run".into()),
                ..Default::default()
            };
            let (code, _, err) = invoke(
                &repo.env,
                &["add".into(), OsString::from_vec(vec![255])],
                &mut fake,
                b"",
            );
            assert_eq!(code, 1, "{err}");
            assert!(err.contains("not UTF-8; nothing was run"));
        }

        #[test]
        fn post_flush_timeout_keeps_the_servers_exit_code() {
            let repo = Repo::new();
            for code in [0, 1, 17] {
                let mut fake = Fake {
                    result: Ok(code),
                    post_outcome: Some(FlushOutcome::TimedOut),
                    ..Default::default()
                };
                let (actual, _, err) = repo.invoke(&["add", "file"], &mut fake);
                assert_eq!(actual, code);
                assert_eq!(err.matches("sync after git").count(), 2);
            }
        }

        #[test]
        fn flush_checks_the_engine_conflicts_even_when_disk_status_is_stale() {
            let repo = Repo::new();
            let mut fake = Fake {
                engine_conflicts: Some(vec!["file".into()]),
                ..Default::default()
            };
            assert!(fake.status.as_ref().unwrap().conflicts.is_empty());
            let (code, _, err) = repo.invoke(&["add", "file"], &mut fake);
            assert_eq!(code, 1);
            assert!(err.contains("file; settle them"));
            assert!(!fake.calls.contains(&"exec".into()));
        }

        #[test]
        fn commit_value_options_and_reset_revision_cannot_hide_conflicts() {
            let repo = Repo::new();
            for words in [
                &["commit", "--author", "A <a@example.invalid>", "-m", "m"][..],
                &["commit", "--date", "yesterday", "-m", "m"],
                &["commit", "--trailer", "Name: value", "-m", "m"],
                &["commit", "--cleanup", "strip", "-m", "m"],
                &["commit", "-t", "template", "-m", "m"],
                &["commit", "--template", "template", "-m", "m"],
                &["reset", "--soft", "HEAD"],
            ] {
                let mut fake = Fake::default();
                conflict(&mut fake);
                assert_eq!(repo.invoke(words, &mut fake).0, 1, "{words:?}");
                assert!(!fake.calls.contains(&"exec".into()));
            }
        }

        #[test]
        fn explicit_repository_and_environment_route_from_outside_and_refuse_moving_variables() {
            let mut repo = Repo::new();
            let root = repo.env.cwd.clone();
            repo.env.cwd = repo.base.clone();
            let mut fake = Fake::default();
            let args = vec![
                format!("--git-dir={}", root.join(".git").display()).into(),
                "add".into(),
                "file".into(),
            ];
            assert_eq!(invoke(&repo.env, &args, &mut fake, b"").0, 0);
            assert_eq!(fake.exec_cwd.as_ref(), Some(&root));
            repo.env.git_environment = vec![
                ("GIT_DIR".into(), root.join(".git").into_os_string()),
                ("GIT_WORK_TREE".into(), root.clone().into_os_string()),
            ];
            let mut fake = Fake::default();
            assert_eq!(repo.invoke(&["add", "file"], &mut fake).0, 0);
            assert_eq!(fake.exec_cwd.as_ref(), Some(&root));
            for key in [
                "GIT_INDEX_FILE",
                "GIT_COMMON_DIR",
                "GIT_OBJECT_DIRECTORY",
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                "GIT_NAMESPACE",
                "GIT_CONFIG_COUNT",
                "GIT_CONFIG_PARAMETERS",
                "GIT_CONFIG_KEY_0",
                "GIT_CONFIG_VALUE_0",
            ] {
                repo.env.git_environment.push((key.into(), "other".into()));
                let mut fake = Fake::default();
                let (code, _, err) = repo.invoke(&["add", "file"], &mut fake);
                assert_eq!(code, 128, "{key}");
                assert!(err.contains(key) && err.contains("nothing was run"));
                assert!(fake.calls.is_empty());
                repo.env.git_environment.pop();
            }
            repo.env
                .git_environment
                .push(("GIT_WORK_TREE".into(), "/srv/other".into()));
            // Use a single mismatched work-tree value, as in a real environment.
            repo.env.git_environment.remove(1);
            assert!(repo
                .invoke(&["add", "file"], &mut Fake::default())
                .2
                .contains("GIT_WORK_TREE"));
        }

        #[test]
        fn moving_environment_only_refuses_forwards_and_allows_config_locations() {
            let mut repo = Repo::new();
            for key in [
                "GIT_INDEX_FILE",
                "GIT_OBJECT_DIRECTORY",
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                "GIT_COMMON_DIR",
                "GIT_NAMESPACE",
                "GIT_CONFIG_PARAMETERS",
                "GIT_CONFIG_COUNT",
                "GIT_CONFIG_KEY_0",
                "GIT_CONFIG_VALUE_0",
            ] {
                repo.env.git_environment = vec![(key.into(), "other".into())];
                let mut fake = Fake::default();
                let (code, _, err) = repo.invoke(&["status", "--porcelain"], &mut fake);
                assert_eq!(code, 0, "{key}: {err}");
                assert!(fake.calls.is_empty());
                let (code, _, err) = repo.invoke(&["add", "file"], &mut fake);
                assert_eq!(code, 128, "{key}: {err}");
                assert!(err.contains(key) && err.contains("nothing was run"));
                assert!(fake.calls.is_empty());
            }
            for key in ["GIT_CONFIG_NOSYSTEM", "GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM"] {
                repo.env.git_environment = vec![(key.into(), "other".into())];
                let mut fake = Fake::default();
                let (code, _, err) = repo.invoke(&["add", "file"], &mut fake);
                assert_eq!(code, 0, "{key}: {err}");
                assert!(fake.calls.iter().any(|call| call == "exec"));
            }
        }

        #[test]
        fn explicit_dotfiles_repository_inside_a_remote_root_runs_untouched() {
            let repo = Repo::new();
            let args = os(&["--git-dir=/srv/dotfiles.git", "--work-tree=/srv/dotfiles", "status"]);
            let mut fake = Fake::default();
            let (code, _, err) = invoke(&repo.env, &args, &mut fake, b"");
            assert_eq!(code, 128);
            assert!(!err.contains("Eitri:"));
            assert!(fake.calls.is_empty());
        }

        #[test]
        fn canonical_matching_keeps_the_records_spelling_in_exec_cwd() {
            let mut repo = Repo::new();
            let parent = repo.base.join("linked-parent");
            std::os::unix::fs::symlink(&repo.base, &parent).unwrap();
            repo.env.projects[0].root = parent.join("project");
            repo.env.cwd.push("sub");
            let mut fake = Fake::default();
            assert_eq!(repo.invoke(&["add", "other"], &mut fake).0, 0);
            assert_eq!(fake.exec_cwd, Some(parent.join("project/sub")));
        }

        #[test]
        fn short_option_values_are_not_editor_flags() {
            for words in [&["commit", "-mrelease"][..], &["checkout", "-bpatch"]] {
                assert!(matches!(route(words), Route::Forward { .. }), "{words:?}");
            }
            assert!(matches!(route(&["stash", "show", "--patch"]), Route::Local { .. }));
            assert!(matches!(route(&["stash", "list", "-p"]), Route::Local { .. }));
        }

        #[test]
        fn program_and_output_options_forward_instead_of_running_locally() {
            for words in [
                &["diff", "--output=result"][..],
                &["log", "--output", "result"],
                &["show", "--output=result"],
                &["grep", "-O", "first"],
                &["grep", "--open-files-in-pager", "first"],
                &["grep", "--open", "first"],
                &["branch", "--contains", "--delete"],
                &["cat-file", "--filter", "HEAD:file"],
                &["cat-file", "--textcon", "HEAD:file"],
                &["cat-file", "--unknown", "HEAD"],
            ] {
                assert!(matches!(route(words), Route::Forward { .. }), "{words:?}");
            }
            assert!(
                matches!(route(&["reflog"]), Route::Local { extra } if extra == ["--no-ext-diff", "--no-textconv"])
            );
        }

        #[test]
        fn unreadable_records_only_refuse_their_own_ancestors() {
            let mut repo = Repo::new();
            let key = eitri_core::remote_project::project_key(&repo.env.cwd);
            repo.env
                .unreadable_records
                .push(repo.env.state_home.join(format!("{key}.json")));
            repo.env.projects.clear();
            assert!(repo
                .invoke(&["status"], &mut Fake::default())
                .2
                .contains("unreadable; nothing was run"));
            repo.env.cwd = repo.base.clone();
            let (code, _, err) = repo.invoke(&["--version"], &mut Fake::default());
            assert_eq!(code, 0, "{err}");
        }

        #[test]
        fn absent_status_uses_recorded_history_readiness() {
            let mut repo = Repo::new();
            let mut record = repo.env.projects[0].sync_record().unwrap().unwrap();
            record.git_ready = false;
            repo.env.projects[0].set_sync_record(&record);
            let mut fake = Fake {
                status: None,
                ..Default::default()
            };
            let (code, _, err) = repo.invoke(&["status"], &mut fake);
            assert_eq!(code, 128);
            assert!(err.contains("still arriving"));
        }

        #[test]
        fn grep_value_options_keep_overrides_before_the_pattern() {
            let repo = Repo::new();
            for words in [
                &["grep", "-e", "first", "--", "file"][..],
                &["grep", "-f", "patterns", "--", "file"],
                &["grep", "--max-depth=2", "--threads", "1", "first"],
                &["grep", "-e", "first", "--end-of-options", "file"],
            ] {
                std::fs::write(repo.env.cwd.join("patterns"), "first\n").unwrap();
                let mut fake = Fake::default();
                let (code, out, err) = repo.invoke(words, &mut fake);
                assert_eq!(code, 0, "{words:?}: {err}");
                assert!(out.contains("first"));
                assert!(fake.calls.is_empty());
            }
        }

        #[test]
        fn work_tree_only_override_cannot_move_a_replica_write() {
            let mut repo = Repo::new();
            repo.env.git_environment = vec![("GIT_WORK_TREE".into(), repo.base.join("other").into_os_string())];
            let mut fake = Fake::default();
            let (code, _, err) = repo.invoke(&["add", "file"], &mut fake);
            assert_eq!(code, 128);
            assert!(err.contains("GIT_WORK_TREE") && err.contains("nothing was run"));
            assert!(fake.calls.is_empty());
        }

        #[test]
        fn repository_targets_resolve_parent_components_before_matching() {
            let repo = Repo::new();
            let args = vec![
                format!("--git-dir={}/../other/.git", repo.env.cwd.display()).into(),
                format!("--work-tree={}/../other", repo.env.cwd.display()).into(),
                "status".into(),
            ];
            let mut fake = Fake::default();
            let (code, _, err) = invoke(&repo.env, &args, &mut fake, b"");
            assert_eq!(code, 128);
            assert!(!err.contains("Eitri:"));
            assert!(fake.calls.is_empty());
        }
    }
}
