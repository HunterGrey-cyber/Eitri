//! The judge thread: path judgements, file reads and whole-text conversions, in order.
//!
//! Whatever a client's request may do to the disk, or cost a whole text's copy, runs here: off the worker, so frames and
//! pings keep moving while a path hangs, and off the GTK thread, which must not block. One job at a time, so a close
//! keeps its place behind the `openDiff` before it.
//!
//! A diff's path goes by where an edit would land ([`agent::judge_edit_path`]), so a file that does not exist yet can be
//! shown. Every other name is a buffer's ([`paths::name_inside`]), so an unsaved new file counts as it does for a tab.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use agent::{judge_edit_path, PathJudgement};
use serde_json::json;

use super::channels::Wake;
use super::diagnostics;
use super::lines::{join_values, line_values, split_lines};
use super::selection::selection_params;
use super::types::{
    ContextUpdate, DiagnosticsQuery, DiffRequest, Done, Event, Job, MentionLine, OpenDiffArgs, ServerConfig, ToolAnswer,
};
use crate::agent_bridge::{path_holds_control, ProposedChange};
use crate::editor_tools::paths;
use crate::editor_tools::tools::diagnostics::DiagnosticsAnswer;
use crate::scratch::{proposal_texts, PROPOSAL_MAX_BYTES};

const FILE_SCHEME: &str = "file://";
const NO_DIAGNOSTICS: &str = "[]";
const ONLY_INSIDE: &str = "Eitri shows diffs only for files inside the project";
const ONLY_SMALL_REGULAR: &str = "Eitri shows diffs only for regular files of at most 4 MiB";
const NOT_UTF8: &str = "the edited text is not UTF-8";

/// The judge thread. It ends when `jobs` closes, or when nobody is left to read what it finishes.
pub fn spawn(config: ServerConfig, jobs: Receiver<Job>, done: Sender<Done>, wake: Arc<Wake>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        while let Ok(job) = jobs.recv() {
            if done.send(run_job(&config, job)).is_err() {
                return;
            }
            wake.signal();
        }
    })
}

/// One job, from the thread's loop: what a test calls directly.
pub fn run_job(config: &ServerConfig, job: Job) -> Done {
    match job {
        Job::Uri { conn, id, uri } => match path_of_uri(&uri).and_then(|path| inside(config, path)) {
            Some(path) => Done::Host(Event::Diagnostics {
                conn,
                id,
                query: DiagnosticsQuery::One { uri, path },
            }),
            None => Done::Answer {
                conn,
                id,
                answer: ToolAnswer::DiagnosticsText(NO_DIAGNOSTICS.to_owned()),
            },
        },
        Job::Diff { conn, id, args } => match diff_request(config, *args) {
            Ok(diff) => Done::Host(Event::OpenDiff {
                conn,
                id,
                diff: Box::new(diff),
            }),
            Err(sentence) => Done::Answer {
                conn,
                id,
                answer: ToolAnswer::Error(sentence.to_owned()),
            },
        },
        Job::Entries {
            conn,
            id,
            named_uri,
            answer,
        } => Done::Answer {
            conn,
            id,
            answer: ToolAnswer::DiagnosticsText(entries_text(config, named_uri, answer)),
        },
        Job::Join { conn, id, lines, shape } => {
            let text = join_values(&lines, shape).and_then(|bytes| String::from_utf8(bytes).ok());
            Done::Answer {
                conn,
                id,
                answer: text.map_or_else(|| ToolAnswer::Error(NOT_UTF8.to_owned()), ToolAnswer::FileSaved),
            }
        }
        Job::Pass(event) => Done::Host(event),
        Job::Context(ContextUpdate { context, facts }) => {
            let file = context
                .as_ref()
                .and_then(|context| inside(config, Path::new(&context.file)));
            Done::Selection(selection_params(context.as_ref(), &facts, file.as_deref()))
        }
        Job::Mention(MentionLine {
            file,
            line_start,
            line_end,
        }) => match inside(config, Path::new(&file)) {
            Some(path) => Done::Mention(json!({
                "filePath": path.to_string_lossy(),
                "lineStart": line_start,
                "lineEnd": line_end,
            })),
            None => Done::Host(Event::MentionOutside),
        },
    }
}

/// The path a `getDiagnostics` URI names. The CLI writes `file://${path}` with nothing encoded and compares what comes
/// back with that text, so `file://` comes off once and nothing is decoded. A relative path is `None`.
pub fn path_of_uri(uri: &str) -> Option<&Path> {
    let path = Path::new(uri.strip_prefix(FILE_SCHEME)?);
    path.is_absolute().then_some(path)
}

/// Where a buffer's name leads when that is in the project and a client can be told it. A name that is not absolute
/// (none at all, a terminal's, a plugin's URL) is no file of the project, though it would read as a new file under the
/// root. The name judged says nothing of where a link leads, and that is what goes out: a control character there would
/// reach the person's prompt, and a name that is not UTF-8 could only be written as some other file's.
fn inside(config: &ServerConfig, name: &Path) -> Option<PathBuf> {
    name.is_absolute()
        .then(|| paths::name_inside(&config.root, config.home.as_deref(), name))
        .flatten()
        .filter(|path| path.to_str().is_some_and(|text| !path_holds_control(text)))
}

/// The answer's JSON: the entries of buffers inside the project, each path judged once. A name the client asked about
/// is echoed, since it compares that text; otherwise each entry goes under its file's URL, which is the client's key.
fn entries_text(config: &ServerConfig, named_uri: Option<String>, answer: DiagnosticsAnswer) -> String {
    let mut judged: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
    let mut kept = Vec::with_capacity(answer.entries.len());
    for entry in answer.entries {
        let resolved = judged
            .entry(entry.path.clone())
            .or_insert_with_key(|name| inside(config, name));
        if let Some(path) = resolved {
            let uri = named_uri
                .clone()
                .unwrap_or_else(|| format!("{FILE_SCHEME}{}", path.to_string_lossy()));
            kept.push((uri, entry));
        }
    }
    diagnostics::shape(&kept)
}

/// What a proposal view needs, or the sentence that refuses it. The worker refuses an oversized contents before it gets
/// here; this holds for a direct caller. The file side is the file as the CLI reads it, LF only; the proposal side keeps
/// its own line ends, so an untouched accept gives its bytes back.
fn diff_request(config: &ServerConfig, args: OpenDiffArgs) -> Result<DiffRequest, &'static str> {
    let OpenDiffArgs {
        new_file_path,
        new_file_contents,
        tab_name,
    } = args;
    if path_holds_control(&new_file_path) || new_file_contents.len() as u64 > PROPOSAL_MAX_BYTES {
        return Err(ONLY_SMALL_REGULAR);
    }
    let judgement = judge_edit_path(&config.root, config.home.as_deref(), Path::new(&new_file_path));
    let PathJudgement::Inside(real_path) = judgement else {
        return Err(ONLY_INSIDE);
    };
    let rel = real_path
        .strip_prefix(&config.root)
        .map_err(|_| ONLY_INSIDE)?
        .to_string_lossy()
        .into_owned();
    // The path a link leads to was not in what the client sent, and this name reaches the view's winbars.
    if path_holds_control(&rel) {
        return Err(ONLY_SMALL_REGULAR);
    }
    let change = ProposedChange::Content {
        content: new_file_contents,
    };
    let (base, proposal) = proposal_texts(&real_path, &change).map_err(|_| ONLY_SMALL_REGULAR)?;
    let file_text = base.replace("\r\n", "\n").into_bytes();
    let (file_lines, _) = split_lines(&file_text);
    let (proposal_lines, shape) = split_lines(proposal.as_bytes());
    Ok(DiffRequest {
        tab_name,
        real_path,
        rel,
        file_text,
        file_lines: line_values(file_lines),
        proposal_lines: line_values(proposal_lines),
        shape,
    })
}
