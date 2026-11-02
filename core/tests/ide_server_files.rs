//! The judge thread's jobs against a scratch project, each run directly unless the thread itself is the point.
#![cfg(target_os = "linux")]

use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use eitri_core::editor_context::feed::PushFacts;
use eitri_core::editor_context::{EditorContext, Selection};
use eitri_core::editor_tools::tools::diagnostics::{DiagnosticEntry, DiagnosticsAnswer, Severity};
use eitri_core::ide_server::channels::Wake;
use eitri_core::ide_server::files::{diff_with_a_swap, path_of_uri, run_job, spawn};
use eitri_core::ide_server::lines::{join_values, line_values, LineShape};
use eitri_core::ide_server::{
    ContextUpdate, DiagnosticsQuery, DiffRequest, Done, Event, Job, MentionLine, OpenDiffArgs, RpcId, ServerConfig,
    ToolAnswer,
};
use rmpv::Value;
use serde_json::json;

const CONN: u64 = 3;
const INSIDE_ONLY: &str = "Eitri shows diffs only for files inside the project";
const SMALL_REGULAR_ONLY: &str = "Eitri shows diffs only for regular files of at most 4 MiB";
/// How long a job that must not hang gets before the test calls it hung.
const HANG_GUARD: Duration = Duration::from_secs(5);

fn id() -> RpcId {
    RpcId::Num(7)
}

/// A project, a `HOME` beside it and a directory outside both.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new() -> Scratch {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "ide-files-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        for name in ["project/src", "home", "elsewhere"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
        }
        let dir = dir.canonicalize().unwrap();
        // Whatever the umask: a root others can write to is no boundary.
        std::fs::set_permissions(dir.join("project"), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(dir.join("elsewhere/secret"), "private\n").unwrap();
        Scratch { dir }
    }

    fn root(&self) -> PathBuf {
        self.dir.join("project")
    }

    fn at(&self, relative: &str) -> PathBuf {
        self.root().join(relative)
    }

    fn outside(&self, relative: &str) -> PathBuf {
        self.dir.join("elsewhere").join(relative)
    }

    fn write(&self, relative: &str, text: impl AsRef<[u8]>) -> PathBuf {
        let path = self.at(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    fn config(&self) -> ServerConfig {
        ServerConfig {
            root: self.root(),
            home: Some(self.dir.join("home")),
        }
    }

    fn run(&self, job: Job) -> Done {
        run_job(&self.config(), job)
    }

    fn diff(&self, path: &str, contents: &str) -> Done {
        self.run(Job::Diff {
            conn: CONN,
            id: id(),
            args: Box::new(OpenDiffArgs {
                new_file_path: path.to_owned(),
                new_file_contents: contents.to_owned(),
                tab_name: "tab".to_owned(),
            }),
        })
    }

    fn uri(&self, uri: &str) -> Done {
        self.run(Job::Uri {
            conn: CONN,
            id: id(),
            uri: uri.to_owned(),
        })
    }

    fn entries(&self, named_uri: Option<&str>, paths: &[&Path]) -> serde_json::Value {
        let done = self.run(Job::Entries {
            conn: CONN,
            id: id(),
            named_uri: named_uri.map(str::to_owned),
            answer: DiagnosticsAnswer {
                entries: paths.iter().map(|path| entry(path)).collect(),
                modified: Vec::new(),
                left_out: 0,
            },
        });
        match answered(done) {
            ToolAnswer::DiagnosticsText(text) => serde_json::from_str(&text).unwrap(),
            other => panic!("not diagnostics text: {other:?}"),
        }
    }

    /// A selection in a buffer loaded from `file`, recorded as the feed records it: the real path, or the name itself
    /// with nothing on disk.
    fn context(&self, file: &Path) -> serde_json::Value {
        let record = file.canonicalize().unwrap_or_else(|_| file.to_owned());
        self.context_read_from(file, Some(&record.to_string_lossy()))
    }

    fn context_read_from(&self, file: &Path, read_from: Option<&str>) -> serde_json::Value {
        self.context_of(
            file,
            read_from,
            Some(Selection {
                start_line: 2,
                end_line: 2,
                text: "private".to_owned(),
            }),
        )
    }

    fn context_of(&self, file: &Path, read_from: Option<&str>, selection: Option<Selection>) -> serde_json::Value {
        let done = self.run(Job::Context(ContextUpdate {
            context: Some(EditorContext {
                file: file.to_str().unwrap().to_owned(),
                selection,
                edit_request: None,
            }),
            facts: PushFacts {
                cursor_line: Some(2),
                start_col: Some(1),
                end_col: Some(4),
                read_from: read_from.map(str::to_owned),
            },
        }));
        match done {
            Done::Selection(params) => params,
            other => panic!("not a selection: {other:?}"),
        }
    }

    fn mention(&self, file: &str) -> Done {
        self.run(Job::Mention(MentionLine {
            file: file.to_owned(),
            line_start: 4,
            line_end: 9,
        }))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn entry(path: &Path) -> DiagnosticEntry {
    DiagnosticEntry {
        path: path.to_owned(),
        read_from: Some(path.to_owned()),
        line: 1,
        column: 1,
        end_line: 1,
        end_column: 2,
        severity: Severity::Error,
        source: None,
        code: None,
        message: "a message".to_owned(),
        start_character: Some(0),
        end_character: Some(1),
    }
}

/// What the client is told at once. This arm means nvim was never asked.
fn answered(done: Done) -> ToolAnswer {
    match done {
        Done::Answer {
            conn,
            id: answered,
            answer,
        } => {
            assert_eq!(conn, CONN);
            assert_eq!(answered, id());
            answer
        }
        other => panic!("not answered at once: {other:?}"),
    }
}

fn refused(done: Done, sentence: &str) {
    assert_eq!(answered(done), ToolAnswer::Error(sentence.to_owned()));
}

fn opened(done: Done) -> DiffRequest {
    match done {
        Done::Host(Event::OpenDiff { conn, id: asked, diff }) => {
            assert_eq!(conn, CONN);
            assert_eq!(asked, id());
            *diff
        }
        other => panic!("no view asked for: {other:?}"),
    }
}

fn asked_about(done: Done) -> (String, PathBuf) {
    match done {
        Done::Host(Event::Diagnostics {
            conn,
            id: asked,
            query: DiagnosticsQuery::One { uri, path },
        }) => {
            assert_eq!(conn, CONN);
            assert_eq!(asked, id());
            (uri, path)
        }
        other => panic!("not a question for nvim: {other:?}"),
    }
}

fn text_of(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// Runs `work` on a thread of its own, so that a hang fails the test and does not stop the run.
fn unhung<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || sender.send(work()));
    receiver.recv_timeout(HANG_GUARD).expect("it should not hang")
}

fn readable(wake: &Wake) -> bool {
    let mut fds = [libc::pollfd {
        fd: wake.fd(),
        events: libc::POLLIN,
        revents: 0,
    }];
    // SAFETY: `fds` is a valid array of one entry for the call.
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, HANG_GUARD.as_millis() as i32) };
    ready > 0
}

#[test]
fn path_of_uri_takes_the_scheme_off_once_and_decodes_nothing() {
    assert_eq!(path_of_uri("file:///p/a%20b.py"), Some(Path::new("/p/a%20b.py")));
    assert_eq!(path_of_uri("file:///p/a b.py"), Some(Path::new("/p/a b.py")));
    assert_eq!(path_of_uri("file://p/a.py"), None, "a relative path");
    assert_eq!(path_of_uri("file://"), None);
    assert_eq!(path_of_uri("file://file:///p/a.py"), None, "the scheme comes off once");
    assert_eq!(path_of_uri("/p/a.py"), None, "no scheme");
    assert_eq!(path_of_uri("http://p/a.py"), None);
}

#[test]
fn an_outside_uri_answers_an_empty_list() {
    let scratch = Scratch::new();
    let secret = scratch.outside("secret");
    for uri in [
        text_of(&secret),
        "no-scheme".to_owned(),
        "file://relative.py".to_owned(),
    ] {
        assert_eq!(
            answered(scratch.uri(&uri)),
            ToolAnswer::DiagnosticsText("[]".to_owned()),
            "{uri}"
        );
    }
}

#[test]
fn a_uri_is_taken_raw_and_undecoded() {
    let scratch = Scratch::new();
    let named = scratch.write("a%20b.py", "x\n");
    let beside = scratch.write("a b.py", "y\n");
    let uri = text_of(&named);
    assert_eq!(asked_about(scratch.uri(&uri)), (uri.clone(), named));
    assert_ne!(
        asked_about(scratch.uri(&uri)).1,
        beside,
        "`%20` does not name the space"
    );
}

#[test]
fn a_uri_of_an_unsaved_new_buffer_is_a_question_for_nvim() {
    let scratch = Scratch::new();
    let new = scratch.at("src/new.py");
    let uri = text_of(&new);
    assert_eq!(asked_about(scratch.uri(&uri)), (uri, new));
}

#[test]
fn a_path_outside_answers_at_once() {
    let scratch = Scratch::new();
    refused(scratch.diff("/etc/hosts", "x"), INSIDE_ONLY);
    let secret = scratch.outside("secret");
    refused(scratch.diff(secret.to_str().unwrap(), "x"), INSIDE_ONLY);
    let dotdot = scratch.root().join("../elsewhere/secret");
    refused(scratch.diff(dotdot.to_str().unwrap(), "x"), INSIDE_ONLY);
}

#[test]
fn a_link_through_proc_answers_at_once() {
    let scratch = Scratch::new();
    symlink("/proc/self/cwd", scratch.at("d")).unwrap();
    let path = scratch.at("d/x.py");
    refused(scratch.diff(path.to_str().unwrap(), "x"), INSIDE_ONLY);
}

#[test]
fn a_fifo_answers_at_once() {
    let scratch = Scratch::new();
    let fifo = scratch.at("pipe");
    let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: `name` is a valid C string for the call.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let config = scratch.config();
    let job = Job::Diff {
        conn: CONN,
        id: id(),
        args: Box::new(OpenDiffArgs {
            new_file_path: fifo.to_str().unwrap().to_owned(),
            new_file_contents: "x".to_owned(),
            tab_name: "tab".to_owned(),
        }),
    };
    refused(unhung(move || run_job(&config, job)), SMALL_REGULAR_ONLY);
}

#[test]
fn a_directory_answers_at_once() {
    let scratch = Scratch::new();
    let dir = scratch.at("src");
    refused(scratch.diff(dir.to_str().unwrap(), "x"), SMALL_REGULAR_ONLY);
}

#[test]
fn a_5_mib_proposal_answers_at_once() {
    let scratch = Scratch::new();
    let path = scratch.at("src/big.txt");
    let contents = "x".repeat(5 * 1024 * 1024);
    refused(scratch.diff(path.to_str().unwrap(), &contents), SMALL_REGULAR_ONLY);
}

#[test]
fn a_5_mib_file_answers_at_once() {
    let scratch = Scratch::new();
    let path = scratch.write("src/big.txt", "x".repeat(5 * 1024 * 1024));
    refused(scratch.diff(path.to_str().unwrap(), "x"), SMALL_REGULAR_ONLY);
}

#[test]
fn a_file_that_is_not_utf8_answers_at_once() {
    let scratch = Scratch::new();
    let path = scratch.write("src/bin.dat", b"\xff\xfe\n");
    refused(scratch.diff(path.to_str().unwrap(), "x"), SMALL_REGULAR_ONLY);
}

#[test]
fn a_control_character_in_the_path_answers_at_once() {
    let scratch = Scratch::new();
    let path = scratch.at("src/a\nb.py");
    refused(scratch.diff(path.to_str().unwrap(), "x"), SMALL_REGULAR_ONLY);
}

#[test]
fn a_link_to_a_name_with_a_control_character_answers_at_once() {
    let scratch = Scratch::new();
    scratch.write("src/a\u{1b}b.py", "x\n");
    symlink("a\u{1b}b.py", scratch.at("src/plain.py")).unwrap();
    let path = scratch.at("src/plain.py");
    refused(scratch.diff(path.to_str().unwrap(), "x"), SMALL_REGULAR_ONLY);
}

#[test]
fn a_new_file_in_an_existing_directory_has_an_empty_file_side() {
    let scratch = Scratch::new();
    let path = scratch.at("src/new.py");
    let diff = opened(scratch.diff(path.to_str().unwrap(), "one\n"));
    assert_eq!(diff.real_path, path);
    assert_eq!(diff.rel, "src/new.py");
    assert_eq!(diff.tab_name, "tab");
    assert!(diff.file_text.is_empty());
    assert!(diff.file_lines.is_empty());
    assert_eq!(diff.proposal_lines, vec![Value::Binary(b"one".to_vec())]);
    assert!(!path.exists(), "nothing is created for a proposal");
}

#[test]
fn a_new_file_in_a_new_directory_has_an_empty_file_side() {
    let scratch = Scratch::new();
    let path = scratch.at("absent/deeper/new.py");
    let diff = opened(scratch.diff(path.to_str().unwrap(), "one\n"));
    assert_eq!(diff.real_path, path);
    assert_eq!(diff.rel, "absent/deeper/new.py");
    assert!(diff.file_text.is_empty());
    assert!(diff.file_lines.is_empty());
    assert!(!scratch.at("absent").exists(), "nothing is created for a proposal");
}

#[test]
fn a_new_file_through_a_link_leaving_the_root_is_refused() {
    let scratch = Scratch::new();
    symlink(scratch.dir.join("elsewhere"), scratch.at("out")).unwrap();
    let path = scratch.at("out/new.py");
    refused(scratch.diff(path.to_str().unwrap(), "x"), INSIDE_ONLY);
}

#[test]
fn a_link_inside_the_project_shows_the_file_it_reaches() {
    let scratch = Scratch::new();
    let real = scratch.write("src/real.py", "text\n");
    symlink("real.py", scratch.at("src/alias.py")).unwrap();
    let path = scratch.at("src/alias.py");
    let diff = opened(scratch.diff(path.to_str().unwrap(), "new\n"));
    assert_eq!(diff.real_path, real);
    assert_eq!(diff.rel, "src/real.py");
    assert_eq!(diff.file_text, b"text\n");
}

/// The diff of `src/a.py`, with `swap` run between the judgement and the read.
fn diff_swapped(scratch: &Scratch, swap: impl FnOnce()) -> Result<DiffRequest, &'static str> {
    let args = OpenDiffArgs {
        new_file_path: scratch.at("src/a.py").to_str().unwrap().to_owned(),
        new_file_contents: "new\n".to_owned(),
        tab_name: "tab".to_owned(),
    };
    diff_with_a_swap(&scratch.config(), args, swap)
}

#[test]
fn a_file_replaced_by_a_link_after_the_judgement_shows_nothing_of_where_it_leads() {
    let scratch = Scratch::new();
    let path = scratch.write("src/a.py", "text\n");
    let shown = diff_swapped(&scratch, || {
        std::fs::remove_file(&path).unwrap();
        symlink(scratch.outside("secret"), &path).unwrap();
    });
    assert!(shown.is_err(), "{shown:?}");
}

#[test]
fn a_directory_replaced_by_a_link_after_the_judgement_shows_nothing_of_where_it_leads() {
    let scratch = Scratch::new();
    scratch.write("src/a.py", "text\n");
    std::fs::write(scratch.outside("a.py"), "private\n").unwrap();
    let shown = diff_swapped(&scratch, || {
        std::fs::rename(scratch.at("src"), scratch.at("src.old")).unwrap();
        symlink(scratch.dir.join("elsewhere"), scratch.at("src")).unwrap();
    });
    assert!(shown.is_err(), "{shown:?}");
}

#[test]
fn a_file_replaced_by_a_hard_link_after_the_judgement_shows_nothing_of_it() {
    let scratch = Scratch::new();
    let path = scratch.write("src/a.py", "text\n");
    let shown = diff_swapped(&scratch, || {
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(scratch.outside("secret"), &path).unwrap();
    });
    assert!(shown.is_err(), "{shown:?}");
}

#[test]
fn a_file_that_goes_after_the_judgement_is_a_new_file() {
    let scratch = Scratch::new();
    let path = scratch.write("src/a.py", "text\n");
    let diff = diff_swapped(&scratch, || std::fs::remove_file(&path).unwrap()).unwrap();
    assert!(diff.file_text.is_empty());
}

#[test]
fn a_crlf_file_becomes_lf_and_the_proposal_is_untouched() {
    let scratch = Scratch::new();
    let path = scratch.write("src/dos.txt", "one\r\ntwo\r\n");
    for proposal in ["one\r\nthree\r\n", "one\r\nthree", "one\r\nthree\n", "one\nthree\n", ""] {
        let diff = opened(scratch.diff(path.to_str().unwrap(), proposal));
        assert_eq!(diff.file_text, b"one\ntwo\n", "the file as the CLI reads it");
        assert_eq!(
            diff.file_lines,
            line_values(vec![b"one".to_vec(), b"two".to_vec()]),
            "{proposal:?}"
        );
        assert_eq!(
            join_values(&diff.proposal_lines, diff.shape),
            Some(proposal.as_bytes().to_vec()),
            "the lines prebuilt for the view come back as the bytes sent"
        );
    }
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"one\r\ntwo\r\n",
        "the file is only read"
    );
}

#[test]
fn join_answers_the_bytes_as_sent() {
    let scratch = Scratch::new();
    let done = scratch.run(Job::Join {
        conn: CONN,
        id: id(),
        lines: vec![Value::Binary("é".as_bytes().to_vec()), Value::from("b")],
        shape: LineShape { crlf: true, eol: false },
    });
    assert_eq!(answered(done), ToolAnswer::FileSaved("é\r\nb".to_owned()));
}

#[test]
fn not_utf8_answers_an_error() {
    let scratch = Scratch::new();
    let shape = LineShape { crlf: false, eol: true };
    for bad in [Value::Binary(vec![0xff]), Value::Nil] {
        let done = scratch.run(Job::Join {
            conn: CONN,
            id: id(),
            lines: vec![bad],
            shape,
        });
        refused(done, "the edited text is not UTF-8");
    }
}

#[test]
fn pass_keeps_its_place_behind_a_diff() {
    let scratch = Scratch::new();
    let path = scratch.write("src/a.py", "text\n");
    let (jobs, from_worker) = mpsc::channel();
    let (to_worker, done) = mpsc::channel();
    let wake = Arc::new(Wake::new().unwrap());
    let thread = spawn(scratch.config(), from_worker, to_worker, wake.clone()).unwrap();

    jobs.send(Job::Diff {
        conn: CONN,
        id: id(),
        args: Box::new(OpenDiffArgs {
            new_file_path: path.to_str().unwrap().to_owned(),
            new_file_contents: "new\n".to_owned(),
            tab_name: "tab".to_owned(),
        }),
    })
    .unwrap();
    jobs.send(Job::Pass(Event::CloseTab {
        conn: CONN,
        id: RpcId::Num(8),
        tab_name: "tab".to_owned(),
    }))
    .unwrap();
    jobs.send(Job::Pass(Event::CloseAll {
        conn: CONN,
        id: RpcId::Num(9),
    }))
    .unwrap();

    assert!(readable(&wake), "a `Done` wakes the worker");
    opened(done.recv_timeout(HANG_GUARD).unwrap());
    match done.recv_timeout(HANG_GUARD).unwrap() {
        Done::Host(Event::CloseTab { conn, id, tab_name }) => {
            assert_eq!((conn, id, tab_name.as_str()), (CONN, RpcId::Num(8), "tab"));
        }
        other => panic!("not the close: {other:?}"),
    }
    match done.recv_timeout(HANG_GUARD).unwrap() {
        Done::Host(Event::CloseAll { conn, id }) => assert_eq!((conn, id), (CONN, RpcId::Num(9))),
        other => panic!("not the close: {other:?}"),
    }
    drop(jobs);
    unhung(move || thread.join().unwrap());
}

#[test]
fn an_unsaved_new_buffers_entries_stay() {
    let scratch = Scratch::new();
    let new = scratch.at("src/new.py");
    let answer = scratch.entries(None, &[&new]);
    assert_eq!(answer[0]["uri"], json!(text_of(&new)));
    assert_eq!(answer[0]["diagnostics"].as_array().unwrap().len(), 1);
}

#[test]
fn an_unsaved_new_file_can_be_selected() {
    let scratch = Scratch::new();
    let new = scratch.at("src/new.py");
    let params = scratch.context(&new);
    assert_eq!(params["filePath"], json!(new.to_str().unwrap()));
    assert_eq!(params["text"], json!("private"));
}

#[test]
fn entries_of_a_buffer_read_from_outside_are_dropped() {
    let scratch = Scratch::new();
    let file = scratch.write("kept.py", "x\n");
    let read_elsewhere = scratch.write("replaced.py", "x\n");
    let unrecorded = scratch.write("unrecorded.py", "x\n");
    let new_file = scratch.at("new.py");
    let entries = vec![
        DiagnosticEntry {
            read_from: Some(scratch.outside("secret")),
            ..entry(&read_elsewhere)
        },
        DiagnosticEntry {
            read_from: None,
            ..entry(&unrecorded)
        },
        entry(&file),
        DiagnosticEntry {
            read_from: None,
            ..entry(&new_file)
        },
    ];
    let done = scratch.run(Job::Entries {
        conn: CONN,
        id: id(),
        named_uri: None,
        answer: DiagnosticsAnswer {
            entries,
            modified: Vec::new(),
            left_out: 0,
        },
    });
    let ToolAnswer::DiagnosticsText(text) = answered(done) else {
        panic!("not diagnostics text");
    };
    let told: serde_json::Value = serde_json::from_str(&text).unwrap();
    let uris: Vec<&str> = told
        .as_array()
        .unwrap()
        .iter()
        .map(|group| group["uri"].as_str().unwrap())
        .collect();
    assert_eq!(
        uris,
        [
            format!("file://{}", file.display()),
            format!("file://{}", new_file.display())
        ],
        "a project name says nothing of the text a buffer holds"
    );
}

#[test]
fn outside_entries_are_dropped() {
    let scratch = Scratch::new();
    let inside = scratch.write("src/a.py", "x\n");
    symlink(scratch.outside("secret"), scratch.at("out")).unwrap();
    let secret = scratch.outside("secret");
    let through_link = scratch.at("out");
    let answer = scratch.entries(
        None,
        &[
            &secret,
            &inside,
            &through_link,
            Path::new(""),
            Path::new("fugitive://x/a.py"),
        ],
    );
    let groups = answer.as_array().unwrap();
    assert_eq!(groups.len(), 1, "{answer}");
    assert_eq!(groups[0]["uri"], json!(text_of(&inside)));
    assert_eq!(groups[0]["diagnostics"].as_array().unwrap().len(), 1);
}

#[test]
fn the_empty_query_writes_the_resolved_file_url() {
    let scratch = Scratch::new();
    let real = scratch.write("src/real.py", "x\n");
    symlink("real.py", scratch.at("src/alias.py")).unwrap();
    let alias = scratch.at("src/alias.py");
    let answer = scratch.entries(None, &[&alias, &real]);
    let groups = answer.as_array().unwrap();
    assert_eq!(groups.len(), 1, "both names reach one file: {answer}");
    assert_eq!(groups[0]["uri"], json!(text_of(&real)));
    assert_eq!(groups[0]["diagnostics"].as_array().unwrap().len(), 2);
}

#[test]
fn a_named_uri_is_echoed() {
    let scratch = Scratch::new();
    let real = scratch.write("src/real.py", "x\n");
    symlink("real.py", scratch.at("src/alias.py")).unwrap();
    let alias = scratch.at("src/alias.py");
    let uri = text_of(&alias);
    let answer = scratch.entries(Some(&uri), &[&real]);
    assert_eq!(
        answer[0]["uri"],
        json!(uri),
        "the name the CLI asked about, not the file's"
    );
}

#[test]
fn an_outside_selection_gives_the_empty_form() {
    let scratch = Scratch::new();
    assert_eq!(
        scratch.context(&scratch.outside("secret")),
        json!({"selection": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "text": ""})
    );
}

#[test]
fn an_empty_name_gives_the_empty_form() {
    let scratch = Scratch::new();
    let params = scratch.context(Path::new(""));
    assert_eq!(params["text"], json!(""));
    assert!(params.get("filePath").is_none(), "{params}");
}

fn assert_empty_form(params: &serde_json::Value) {
    assert_eq!(
        *params,
        json!({"selection": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "text": ""})
    );
}

#[test]
fn a_selection_in_a_buffer_read_from_outside_gives_the_empty_form() {
    let scratch = Scratch::new();
    let name = scratch.write("replaced.py", "an ordinary file now\n");
    let secret = scratch.outside("secret");
    assert_empty_form(&scratch.context_read_from(&name, Some(secret.to_str().unwrap())));
}

#[test]
fn the_cursor_form_of_a_buffer_read_from_outside_names_no_file() {
    let scratch = Scratch::new();
    let name = scratch.write("replaced.py", "an ordinary file now\n");
    let secret = scratch.outside("secret");
    assert_empty_form(&scratch.context_of(&name, Some(secret.to_str().unwrap()), None));
}

#[test]
fn a_name_on_disk_with_no_record_gives_the_empty_form() {
    let scratch = Scratch::new();
    let name = scratch.write("unrecorded.py", "x\n");
    assert_empty_form(&scratch.context_read_from(&name, None));
    assert_empty_form(&scratch.context_of(&name, None, None));
}

#[test]
fn a_name_with_nothing_on_disk_and_no_record_is_told() {
    let scratch = Scratch::new();
    let params = scratch.context_read_from(&scratch.at("src/new.py"), None);
    assert_eq!(params["text"], json!("private"));
}

/// The feed reads a record that is not UTF-8 with U+FFFD for each bad sequence, and that spelling can lie in the project
/// while the real one does not.
#[test]
fn a_record_the_feed_could_not_read_gives_the_empty_form() {
    let scratch = Scratch::new();
    let name = scratch.write("src/plain.py", "x\n");
    let spelled = scratch.write("src/caf\u{fffd}.py", "x\n");
    assert_empty_form(&scratch.context_read_from(&name, Some(spelled.to_str().unwrap())));
}

#[test]
fn a_record_in_the_project_passes_under_any_name_in_it() {
    let scratch = Scratch::new();
    let name = scratch.write("src/renamed.py", "x\n");
    let record = scratch.at("src/old-name.py");
    let params = scratch.context_read_from(&name, Some(record.to_str().unwrap()));
    assert_eq!(params["filePath"], json!(name.to_str().unwrap()));
    assert_eq!(params["text"], json!("private"));
}

#[test]
fn a_selection_inside_names_the_resolved_file() {
    let scratch = Scratch::new();
    let real = scratch.write("src/real.py", "x\n");
    symlink("real.py", scratch.at("src/alias.py")).unwrap();
    let params = scratch.context(&scratch.at("src/alias.py"));
    assert_eq!(params["filePath"], json!(real.to_str().unwrap()));
}

#[test]
fn a_mention_inside_names_the_resolved_file_and_its_lines() {
    let scratch = Scratch::new();
    let real = scratch.write("src/real.py", "x\n");
    symlink("real.py", scratch.at("src/alias.py")).unwrap();
    let alias = scratch.at("src/alias.py");
    match scratch.mention(alias.to_str().unwrap()) {
        Done::Mention(params) => assert_eq!(
            params,
            json!({"filePath": real.to_str().unwrap(), "lineStart": 4, "lineEnd": 9})
        ),
        other => panic!("not a mention: {other:?}"),
    }
}

#[test]
fn a_mention_outside_sends_nothing_and_says_so() {
    let scratch = Scratch::new();
    let secret = scratch.outside("secret");
    for name in [secret.to_str().unwrap(), "", "term://1:bash"] {
        assert!(
            matches!(scratch.mention(name), Done::Host(Event::MentionOutside)),
            "{name:?}"
        );
    }
}

/// Links in the project to what no client can be told: a name holding an escape and a bell, and one that is not UTF-8.
/// A name holding a control character is refused as given, so only a link gets one this far, and the text of a line or
/// a URI cannot hold the second at all. The sibling is the file a lossy spelling of that name would point at.
fn links_to_unsendable_names(scratch: &Scratch) -> [PathBuf; 2] {
    scratch.write("src/a\u{1b}]0;x\u{7}b.py", "x\n");
    symlink("a\u{1b}]0;x\u{7}b.py", scratch.at("src/escaped.py")).unwrap();
    let latin1 = OsStr::from_bytes(b"caf\xe9.py");
    std::fs::write(scratch.at("src").join(latin1), "x\n").unwrap();
    scratch.write("src/caf\u{fffd}.py", "another file\n");
    symlink(latin1, scratch.at("src/latin1.py")).unwrap();
    [scratch.at("src/escaped.py"), scratch.at("src/latin1.py")]
}

#[test]
fn a_mention_of_a_link_to_an_unsendable_name_sends_nothing_and_says_so() {
    let scratch = Scratch::new();
    for link in links_to_unsendable_names(&scratch) {
        let done = scratch.mention(link.to_str().unwrap());
        assert!(matches!(done, Done::Host(Event::MentionOutside)), "{link:?}: {done:?}");
    }
}

#[test]
fn a_selection_in_a_link_to_an_unsendable_name_gives_the_empty_form() {
    let scratch = Scratch::new();
    for link in links_to_unsendable_names(&scratch) {
        let params = scratch.context(&link);
        assert_eq!(params["text"], json!(""), "{link:?}");
        assert!(params.get("filePath").is_none(), "{link:?}: {params}");
    }
}

/// The feed gives a name that is not UTF-8 with U+FFFD in place of each bad sequence, so the spelling a test passes here
/// is not the buffer's name. That name is a link out of the project, and whatever stands at the spelling says nothing of
/// it: nothing, another file, or a link to a third.
#[test]
fn a_selection_under_a_name_the_feed_could_not_read_gives_the_empty_form() {
    let scratch = Scratch::new();
    let secret = scratch.outside("secret");
    symlink(&secret, scratch.at("src").join(OsStr::from_bytes(b"caf\xe9.py"))).unwrap();
    let spelled = scratch.at("src/caf\u{fffd}.py");
    let empty = |what: &str| {
        let params = scratch.context(&spelled);
        assert_eq!(params["text"], json!(""), "{what}");
        assert!(params.get("filePath").is_none(), "{what}: {params}");
    };
    empty("nothing at the spelling");
    scratch.write("src/caf\u{fffd}.py", "another file\n");
    empty("another file at the spelling");
    std::fs::remove_file(&spelled).unwrap();
    scratch.write("src/plain.py", "x\n");
    symlink("plain.py", &spelled).unwrap();
    empty("a link at the spelling");
}

#[test]
fn entries_of_a_link_to_an_unsendable_name_are_dropped() {
    let scratch = Scratch::new();
    let [escaped, latin1] = links_to_unsendable_names(&scratch);
    let plain = scratch.write("src/plain.py", "x\n");
    let unnamed = scratch.entries(None, &[&escaped, &latin1, &plain]);
    assert_eq!(unnamed.as_array().map(Vec::len), Some(1), "{unnamed}");
    assert_eq!(unnamed[0]["uri"], json!(format!("file://{}", plain.display())));
    assert_eq!(scratch.entries(Some("file:///asked"), &[&escaped, &latin1]), json!([]));
}

#[test]
fn a_uri_of_a_link_to_an_unsendable_name_answers_an_empty_list() {
    let scratch = Scratch::new();
    for link in links_to_unsendable_names(&scratch) {
        let done = scratch.uri(&format!("file://{}", link.to_str().unwrap()));
        match answered(done) {
            ToolAnswer::DiagnosticsText(text) => assert_eq!(text, "[]", "{link:?}"),
            other => panic!("not diagnostics text: {other:?}"),
        }
    }
}

#[test]
fn the_thread_ends_with_its_channel() {
    let scratch = Scratch::new();
    let (jobs, from_worker) = mpsc::channel();
    let (to_worker, _done) = mpsc::channel();
    let thread = spawn(scratch.config(), from_worker, to_worker, Arc::new(Wake::new().unwrap())).unwrap();
    drop(jobs);
    unhung(move || thread.join().unwrap());
}

#[test]
fn the_thread_ends_when_nobody_reads_what_it_finishes() {
    let scratch = Scratch::new();
    let (jobs, from_worker) = mpsc::channel();
    let (to_worker, done) = mpsc::channel();
    let thread = spawn(scratch.config(), from_worker, to_worker, Arc::new(Wake::new().unwrap())).unwrap();
    drop(done);
    jobs.send(Job::Pass(Event::Lost)).unwrap();
    unhung(move || thread.join().unwrap());
}
