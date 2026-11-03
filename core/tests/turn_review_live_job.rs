//! Live hunks use a real turn baseline and the current file, without writing the project.

use std::os::unix::fs::{symlink, DirBuilderExt};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use agent::{AgentDomainEvent, TurnOutcome};
use eitri_core::turn_review::{LiveHunks, LiveSkip, ReviewOptions, TurnReview, TurnState};

struct Project {
    scratch: PathBuf,
    root: PathBuf,
    review: TurnReview,
}

impl Project {
    fn new(files: &[(&str, &[u8])], options: ReviewOptions) -> Self {
        let scratch = std::env::temp_dir().join(format!("eitri-live-job-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new().mode(0o700).create(&scratch).unwrap();
        let root = scratch.join("project");
        std::fs::create_dir(&root).unwrap();
        for (path, bytes) in files {
            std::fs::write(root.join(path), bytes).unwrap();
        }
        let review = TurnReview::with_options(Some(scratch.join("review")), &root, options);
        Self { scratch, root, review }
    }

    fn start(&mut self) {
        self.review
            .observe(7, "s1", &AgentDomainEvent::TurnStarted { turn_id: "t1".into() }, 1);
    }

    fn base(&mut self) {
        self.start();
        self.wait(|review| review.turns("s1").first().is_some_and(|r| r.base.commit().is_some()));
    }

    fn wait(&mut self, ready: impl Fn(&TurnReview) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            self.review.poll();
            if ready(&self.review) {
                return;
            }
            assert!(Instant::now() < deadline, "the snapshot worker finishes");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn write(&self, path: &str, bytes: &[u8]) {
        std::fs::write(self.root.join(path), bytes).unwrap();
    }

    fn live(&self, path: &str) -> Result<LiveHunks, LiveSkip> {
        self.review.live_hunks_job("s1", "t1", path).run().unwrap()
    }

    fn end(&mut self) {
        self.review.observe(
            7,
            "s1",
            &AgentDomainEvent::TurnCompleted {
                turn_id: "t1".into(),
                outcome: TurnOutcome::Completed,
                result_text: String::new(),
                stop_reason: None,
                usage: None,
                detail: Default::default(),
            },
            2,
        );
        self.wait(|review| review.turns("s1").first().is_some_and(|r| r.state() == TurnState::Ok));
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut absent_since = None;
        while Instant::now() < deadline {
            if self.scratch.exists() {
                let _ = std::fs::remove_dir_all(&self.scratch);
                absent_since = None;
            } else if absent_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_millis(100) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn options() -> ReviewOptions {
    ReviewOptions {
        excludes: Some(None),
        trim_every: Duration::MAX,
        ..ReviewOptions::default()
    }
}

#[test]
fn an_edit_is_diffed_against_the_turns_base() {
    let mut p = Project::new(&[("a", b"one\ntwo\nthree\n")], options());
    p.base();
    p.write("a", b"one\nchanged\nthree\n");
    let first = p.live("a").unwrap();
    assert_eq!(first.path, "a");
    assert_eq!(first.hunks.len(), 1);
    assert_eq!(first.hunks[0].old_lines.concat(), b"one\ntwo\nthree\n");
    assert_eq!(first.hunks[0].new_lines.concat(), b"one\nchanged\nthree\n");
    p.write("a", b"again\nchanged\nthree\n");
    let next = p.live("a").unwrap();
    assert_eq!(next.hunks[0].old_lines.concat(), b"one\ntwo\nthree\n");
    assert_eq!(next.hunks[0].new_lines.concat(), b"again\nchanged\nthree\n");
}

#[test]
fn a_new_file_is_all_added() {
    let mut p = Project::new(&[], options());
    p.base();
    p.write("new", b"one\ntwo\n");
    let live = p.live("new").unwrap();
    assert_eq!(live.hunks.len(), 1);
    assert_eq!(live.hunks[0].old_len, 0);
    assert!(live.hunks[0].old_lines.is_empty());
    assert_eq!(live.hunks[0].new_lines.concat(), b"one\ntwo\n");
}

#[test]
fn an_ignored_new_file_is_skipped_as_ignored() {
    let mut p = Project::new(&[(".gitignore", b"ignored\n")], options());
    std::fs::create_dir_all(p.root.join(".git/info")).unwrap();
    std::fs::write(p.root.join(".git/info/exclude"), "excluded\n").unwrap();
    // Initialise a project repository so its excludes are copied into the shadow.
    let mut init = eitri_core::turn_review::git::init_bare(&p.root.join(".git"));
    assert!(init.output().unwrap().status.success());
    let mut config = eitri_core::turn_review::git::command(&p.root.join(".git"), &p.root, None, None);
    assert!(config
        .args(["config", "core.bare", "false"])
        .output()
        .unwrap()
        .status
        .success());
    p.base();
    for path in ["ignored", "excluded"] {
        p.write(path, b"secret\n");
        assert_eq!(p.live(path), Err(LiveSkip::Ignored));
    }
}

#[test]
fn a_file_the_base_ignored_is_not_new_after_the_turn_changes_an_ignore_file() {
    let mut p = Project::new(&[(".gitignore", b".env\n"), (".env", b"SECRET=1\n")], options());
    std::fs::create_dir(p.root.join("sub")).unwrap();
    p.base();
    p.write(".gitignore", b"");
    p.write(".env", b"SECRET=2\n");
    assert_eq!(p.live(".env"), Err(LiveSkip::Ignored), "the base's rules kept it out");
    p.write(".gitignore", b".env\n");
    p.write("sub/.gitignore", b"other\n");
    p.write("sub/new", b"one\n");
    assert_eq!(
        p.live("sub/new"),
        Err(LiveSkip::Ignored),
        "an ignore file made in the turn"
    );
    p.write("plain", b"two\n");
    assert_eq!(
        p.live("plain").unwrap().hunks[0].new_lines.concat(),
        b"two\n",
        "unchanged ignore files above it: new"
    );
}

#[test]
fn an_ignored_name_that_starts_with_a_colon_is_still_ignored() {
    let mut p = Project::new(&[(".gitignore", b":secret\n")], options());
    p.base();
    p.write(":secret", b"hidden\n");
    assert_eq!(p.live(":secret"), Err(LiveSkip::Ignored));
    p.write(":shown", b"shown\n");
    assert_eq!(p.live(":shown").unwrap().hunks[0].new_lines.concat(), b"shown\n");
}

fn nested_repository(dir: &std::path::Path, commit: bool) {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    if commit {
        git(&["add", "."]);
        git(&["commit", "-qm", "c"]);
    }
}

#[test]
fn a_file_inside_a_nested_repository_is_not_drawn_as_new() {
    let mut p = Project::new(&[("top", b"t\n")], options());
    std::fs::create_dir(p.root.join("vendor")).unwrap();
    p.write("vendor/lib.rs", b"one\n");
    // Committed before the turn: the shadow holds `vendor` as a single gitlink entry.
    nested_repository(&p.root.join("vendor"), true);
    p.base();
    p.write("vendor/lib.rs", b"two\n");
    p.write("vendor/added.rs", b"new\n");
    assert_eq!(p.live("vendor/lib.rs"), Err(LiveSkip::Ignored));
    assert_eq!(p.live("vendor/added.rs"), Err(LiveSkip::Ignored));
    // A repository made during the turn, and one without a commit, are nested too.
    std::fs::create_dir(p.root.join("made")).unwrap();
    nested_repository(&p.root.join("made"), false);
    p.write("made/a.rs", b"a\n");
    assert_eq!(p.live("made/a.rs"), Err(LiveSkip::Ignored));
    // A plain new directory is still a new file.
    std::fs::create_dir(p.root.join("plain")).unwrap();
    p.write("plain/a.rs", b"a\n");
    assert_eq!(p.live("plain/a.rs").unwrap().hunks[0].new_lines.concat(), b"a\n");
}

// A file name that is not valid UTF-8 cannot exist on macOS (APFS refuses it).
#[cfg(target_os = "linux")]
#[test]
fn a_name_that_reads_like_another_files_is_skipped_as_ambiguous() {
    use std::os::unix::ffi::OsStrExt;
    let mut p = Project::new(&[("plain", b"p\n")], options());
    p.base();
    std::fs::write(
        p.root.join(std::ffi::OsStr::from_bytes(b"file-\xFF.txt")),
        "raw bytes name\n",
    )
    .unwrap();
    std::fs::write(p.root.join("file-\u{FFFD}.txt"), "real replacement character\n").unwrap();
    assert_eq!(p.live("file-\u{FFFD}.txt"), Err(LiveSkip::Ambiguous));
    p.write("plain", b"changed\n");
    assert!(p.live("plain").is_ok());
}

#[test]
fn a_file_left_out_of_the_base_for_size_is_skipped() {
    let mut opts = options();
    opts.limits.max_file_bytes = 16;
    let mut p = Project::new(&[("a", b"this was too large to capture\n")], opts);
    p.base();
    p.write("a", b"small\n");
    assert_eq!(p.live("a"), Err(LiveSkip::LeftOut));
}

#[test]
fn a_late_base_is_skipped() {
    let mut p = Project::new(&[("a", b"one\n")], options());
    p.start();
    p.review.observe(
        7,
        "s1",
        &AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "write-1".into(),
            name: "Write".into(),
            input: serde_json::json!({}),
        },
        2,
    );
    p.wait(|review| review.turns("s1").first().is_some_and(|r| r.base.commit().is_some()));
    p.write("a", b"two\n");
    assert_eq!(p.live("a"), Err(LiveSkip::Late));
}

#[test]
fn no_base_yet_is_skipped() {
    let mut p = Project::new(&[("a", b"one\n")], options());
    assert_eq!(p.live("a"), Err(LiveSkip::NoBase));
    p.start();
    let pending = p.review.live_hunks_job("s1", "t1", "a");
    p.wait(|review| review.turns("s1").first().is_some_and(|r| r.base.commit().is_some()));
    assert_eq!(pending.run().unwrap(), Err(LiveSkip::NoBase));
    assert_eq!(
        p.review.live_hunks_job("s1", "missing", "a").run().unwrap(),
        Err(LiveSkip::NoBase)
    );
}

#[test]
fn binary_and_over_cap_are_skipped() {
    let mut p = Project::new(&[("a", b"one\n"), ("binary", b"\0base")], options());
    p.base();
    p.write("a", b"two\0\n");
    assert_eq!(p.live("a"), Err(LiveSkip::Binary));
    p.write("binary", b"text now\n");
    assert_eq!(p.live("binary"), Err(LiveSkip::Binary));
    p.write(
        "a",
        "added\n"
            .repeat(eitri_core::turn_review::MAX_OVERVIEW_DIFF_LINES + 1)
            .as_bytes(),
    );
    assert_eq!(p.live("a"), Err(LiveSkip::OverCap));
    p.write("a", &vec![b'a'; 8 * 1024 * 1024 + 1]);
    assert_eq!(p.live("a"), Err(LiveSkip::TooLarge));
}

#[test]
fn a_link_and_a_fifo_are_not_read() {
    let mut p = Project::new(&[("a", b"one\n")], options());
    p.base();
    symlink(p.root.join("a"), p.root.join("link")).unwrap();
    let fifo = std::ffi::CString::new(p.root.join("fifo").as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: the path is a valid C string and mkfifo reads it only during the call.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    for path in ["link", "fifo"] {
        assert_eq!(p.live(path), Err(LiveSkip::NotAFile));
    }
    symlink(&p.scratch, p.root.join("outside")).unwrap();
    assert_eq!(p.live("outside/a"), Err(LiveSkip::NotAFile));
    assert_eq!(p.live("../a"), Err(LiveSkip::NotAFile));
}

#[test]
fn read_lines_counts_the_lines_read() {
    let mut p = Project::new(&[("a", b"base\n")], options());
    p.base();
    for (bytes, count) in [(b"a\nb\n".as_slice(), 2), (b"a\nb", 2), (b"\n", 1), (b"", 0)] {
        p.write("a", bytes);
        assert_eq!(p.live("a").unwrap().read_lines, count);
    }
    p.write("a", b"base\n");
    assert_eq!(p.live("a"), Err(LiveSkip::Unchanged));
}

#[test]
fn a_crlf_file_and_one_without_a_final_newline_give_exact_sides() {
    for (before, after) in [
        (b"one\r\ntwo\r\n".as_slice(), b"one\r\nchanged\r\n".as_slice()),
        (b"one\ntwo", b"one\nchanged"),
    ] {
        let mut p = Project::new(&[("a", before)], options());
        p.base();
        p.write("a", after);
        let live = p.live("a").unwrap();
        assert_eq!(live.hunks[0].old_lines.concat(), before);
        assert_eq!(live.hunks[0].new_lines.concat(), after);
    }
}

#[test]
fn the_settle_job_gives_the_end_snapshots_line_count() {
    for (after, count) in [(b"one\ntwo\n".as_slice(), 2), (b"one\ntwo", 2), (b"", 0)] {
        let mut p = Project::new(&[("a", b"base\n")], options());
        p.base();
        p.write("a", after);
        p.end();
        p.write("a", b"later\ncontents\nelsewhere\n");
        let settled = p.review.live_settle_job("s1", 1, "a").run().unwrap();
        assert_eq!(settled.end_lines, count);
        assert!(!settled.exact.binary);
        let hunks = settled.exact.hunks.unwrap();
        assert_eq!(hunks[0].old_lines.concat(), b"base\n");
        assert_eq!(hunks[0].new_lines.concat(), after);
    }
}
