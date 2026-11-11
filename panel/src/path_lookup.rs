//! Exact opens and bounded suffix searches. Listing and checking real paths happen only on the request's worker.
use super::*;
use eitri_core::path_complete::{find_by_suffix, Stopped, SuffixLimits, SuffixLookup};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

/// Listings ask for a narrower suffix beyond the fourteen single-letter choices.
const PATH_PICK_MAX: usize = 14;
const MAX_FILES: usize = 200_000;
const DEADLINE: Duration = Duration::from_secs(1);
const MAX_RUNNING: usize = 4;
const MAX_LIVE_WORKERS: usize = 8;
const OFFER_LIFETIME: Duration = Duration::from_secs(60);
const MAX_OFFERS: usize = 64;

struct Request {
    line: Option<u32>,
    expect: Option<String>,
    view: ViewId,
    request_id: String,
    tab: TabId,
    wanted: String,
    started: Instant,
    /// The page's interaction generation when it asked, if it sent one.
    interaction: Option<u64>,
}
#[derive(Debug)]
struct FoundPath {
    name: String,
    canonical: PathBuf,
}
enum Answer {
    One(FoundPath),
    Ready(PathBuf, Option<String>),
    Choices(Vec<FoundPath>, Option<String>),
}
struct Offer {
    view: ViewId,
    request_id: String,
    tab: TabId,
    root: PathBuf,
    paths: Vec<FoundPath>,
    at: Instant,
}
pub(super) struct PathLookup {
    running: Vec<(Request, mpsc::Receiver<Result<Answer, String>>)>,
    offers: Vec<Offer>,
    abandon_after: Duration,
    limits: SuffixLimits,
    live_workers: Arc<AtomicUsize>,
}
impl Default for PathLookup {
    fn default() -> Self {
        Self {
            running: Vec::new(),
            offers: Vec::new(),
            abandon_after: Duration::from_secs(5),
            limits: SuffixLimits {
                max_files: MAX_FILES,
                deadline: DEADLINE,
            },
            live_workers: Arc::default(),
        }
    }
}

impl PathLookup {
    pub(super) fn is_running(&self) -> bool {
        !self.running.is_empty()
    }
}

/// The count covers abandoned workers too, until their closures return or unwind.
struct LiveWorker(Arc<AtomicUsize>);
impl Drop for LiveWorker {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn checked_path(root: &Path, name: &str, expected: Option<&Path>) -> Result<FoundPath, String> {
    let missing = || format!("no such file: {name}");
    if eitri_core::path_complete::suffix_wanted(name).is_none() {
        return Err(missing());
    }
    let canonical_root = root.canonicalize().map_err(|_| missing())?;
    let joined = root.join(name);
    let real = joined.canonicalize().map_err(|_| missing())?;
    if !real.starts_with(&canonical_root) {
        return Err(format!("refused to open {name}: it leads outside the project"));
    }
    if !real.is_file() {
        return Err(format!("refused to open {name}: not a regular file"));
    }
    if expected.is_some_and(|expected| real != expected) {
        return Err(format!("refused to open {name}: its target changed"));
    }
    Ok(FoundPath {
        name: name.to_owned(),
        canonical: real,
    })
}

fn lookup(root: &Path, wanted: &str, limits: SuffixLimits) -> Result<Answer, String> {
    checked_answer(root, wanted, find_by_suffix(root, wanted, limits))
}

fn checked_answer(root: &Path, wanted: &str, found: SuffixLookup) -> Result<Answer, String> {
    checked_answer_with(wanted, found, |name| checked_path(root, name, None))
}

fn checked_answer_with(
    wanted: &str,
    found: SuffixLookup,
    mut check: impl FnMut(&str) -> Result<FoundPath, String>,
) -> Result<Answer, String> {
    let count = found.matches.len();
    let mut paths = Vec::new();
    let mut refused = None;
    for name in found.matches {
        match check(&name) {
            Ok(path) => paths.push(path),
            Err(why) => {
                refused.get_or_insert(why);
            }
        }
        if paths.len() > PATH_PICK_MAX {
            break;
        }
    }
    let stopped = found.stopped.map(|why| match why {
        Stopped::Files(n) => format!("stopped looking after {n} files"),
        Stopped::Time => "stopped looking after 1 s".into(),
    });
    match paths.len() {
        0 => Err(refused.unwrap_or_else(|| {
            format!(
                "no such file: {wanted}{}",
                stopped.map(|why| format!(" ({why})")).unwrap_or_default()
            )
        })),
        1 if stopped.is_none() => Ok(Answer::One(paths.pop().unwrap())),
        n if n <= PATH_PICK_MAX => Ok(Answer::Choices(paths, stopped)),
        _ => Err(match stopped {
            Some(why) => format!("{count}+ files end with {wanted} ({why}); narrow it"),
            None => format!("{count} files end with {wanted}; narrow it"),
        }),
    }
}

fn start_worker(
    state: &mut AgentPanelState,
    request: Request,
    work: impl FnOnce() -> Result<Answer, String> + Send + 'static,
) -> Result<(), String> {
    if state.path_lookup.live_workers.load(Ordering::SeqCst) >= MAX_LIVE_WORKERS {
        return Err("8 path lookups are still waiting on the file system; try again later".into());
    }
    if state.path_lookup.running.len() >= MAX_RUNNING {
        return Err("still looking for another path; try again in a moment".into());
    }
    let wake = state.wake.as_ref().map(crate::panel_wake::PanelWake::waker);
    let (tx, rx) = mpsc::channel();
    let live_worker = LiveWorker(state.path_lookup.live_workers.clone());
    live_worker.0.fetch_add(1, Ordering::SeqCst);
    std::thread::Builder::new()
        .name("path-lookup".into())
        .spawn(move || {
            let _live_worker = live_worker;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .unwrap_or_else(|_| Err("path lookup failed; try again".into()));
            let _ = tx.send(result);
            if let Some(wake) = wake {
                wake();
            }
        })
        .map_err(|why| format!("could not start path lookup: {why}"))?;
    state.path_lookup.running.push((request, rx));
    Ok(())
}

pub(super) fn check_path_length(path: &str) -> Result<(), String> {
    if path.len() > 4096 {
        return Err("path is longer than 4096 bytes".into());
    }
    Ok(())
}

pub(super) fn start(
    state: &mut AgentPanelState,
    path: String,
    line: Option<u32>,
    expect: Option<String>,
    view: ViewId,
    request_id: String,
    tab: TabId,
    interaction: Option<u64>,
) -> Result<(), String> {
    check_path_length(&path)?;
    let root = state.project_dir.clone();
    let limits = state.path_lookup.limits;
    let request = Request {
        line,
        expect: expect.clone(),
        view,
        request_id,
        tab,
        wanted: path.clone(),
        started: Instant::now(),
        interaction,
    };
    start_worker(state, request, move || {
        match super::links::open_path_exact(&root, &path) {
            Ok(resolved) => {
                let note = line_note(&resolved, &path, line, expect.as_deref());
                Ok(Answer::Ready(resolved, note))
            }
            Err(super::links::OpenPathRefusal::Missing)
                if eitri_core::path_complete::suffix_wanted(&path).is_some() =>
            {
                lookup(&root, &path, limits)
            }
            Err(why) => Err(why.message(&path)),
        }
    })
}

/// The one regular file inside the canonical project root that `path` names as `gf` resolves it: the path itself,
/// else the only file whose name ends with it. A directory, a FIFO, a device, a path leading outside the project or a
/// suffix several files share is none. Listing and resolving touch the file system: call it on a worker.
pub(super) fn resolve_inside(root: &Path, path: &str) -> Option<PathBuf> {
    let canonical_root = root.canonicalize().ok()?;
    match super::links::open_path_exact(root, path) {
        Ok(resolved) => {
            let real = resolved.canonicalize().ok()?;
            (real.starts_with(&canonical_root) && real.is_file()).then_some(real)
        }
        Err(super::links::OpenPathRefusal::Missing) if eitri_core::path_complete::suffix_wanted(path).is_some() => {
            let limits = SuffixLimits {
                max_files: MAX_FILES,
                deadline: DEADLINE,
            };
            match lookup(root, path, limits) {
                Ok(Answer::One(found)) => Some(found.canonical),
                _ => None,
            }
        }
        Err(_) => None,
    }
}

/// An offered name retains its checked identity until chosen or expired. Choosing one consumes the
/// request's whole offer so unused entries do not accumulate.
pub(super) fn pick(
    state: &mut AgentPanelState,
    path: &str,
    line: Option<u32>,
    expect: Option<String>,
    view: ViewId,
    request_id: &str,
    tab: TabId,
    interaction: Option<u64>,
) -> Option<Result<(), String>> {
    state
        .path_lookup
        .offers
        .retain(|offer| offer.at.elapsed() < OFFER_LIFETIME);
    let index = state.path_lookup.offers.iter().rposition(|offer| {
        offer.view == view && offer.tab == tab && offer.paths.iter().any(|found| found.name == path)
    })?;
    let offer = state.path_lookup.offers.remove(index);
    let found = offer.paths.into_iter().find(|found| found.name == path).unwrap();
    let request = Request {
        line,
        expect: expect.clone(),
        view,
        request_id: request_id.to_owned(),
        tab,
        wanted: path.to_owned(),
        started: Instant::now(),
        interaction,
    };
    Some(start_worker(state, request, move || {
        final_check(&offer.root, found, line, expect.as_deref())
    }))
}

fn final_check(root: &Path, found: FoundPath, line: Option<u32>, expect: Option<&str>) -> Result<Answer, String> {
    checked_path(root, &found.name, Some(&found.canonical))?;
    let note = line_note(&found.canonical, &found.name, line, expect);
    Ok(Answer::Ready(root.join(found.name), note))
}

/// Only a bounded regular disk file is evidence about a citation; loaded buffers may have unsaved edits.
fn line_note(path: &Path, name: &str, line: Option<u32>, expect: Option<&str>) -> Option<String> {
    use std::io::Read;
    let line = line.filter(|line| *line > 0)?;
    let expect = expect.filter(|name| eitri_core::agent_bridge::valid_symbol_name(name))?;
    let canonical = path.canonicalize().ok()?;
    if !std::fs::metadata(&canonical).ok()?.is_file() {
        return None;
    }
    // Judged again on the handle, opened without waiting: a FIFO put in its place since the look above would
    // otherwise hold this worker until a writer came.
    let file = eitri_core::scratch::open_regular_file(&canonical)?;
    let mut bytes = Vec::new();
    file.take(1_000_000).read_to_end(&mut bytes).ok()?;
    // The cap may cut the requested line before its name. That is no evidence it changed.
    if bytes.len() == 1_000_000 && bytes.iter().filter(|byte| **byte == b'\n').count() < line as usize {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    if text
        .lines()
        .nth((line - 1) as usize)
        .is_some_and(|text| text.contains(expect))
    {
        return None;
    }
    Some(format!(
        "{name}:{line} no longer has {expect} (the file changed since the reply)"
    ))
}

fn active(state: &AgentPanelState, request: &Request) -> bool {
    state.tabs.get(request.tab).is_some()
        && state.tabs.view_of(request.tab) == request.view
        && if request.view.is_base() {
            state.tabs.active() == request.tab
        } else {
            state.tabs.tab_in(request.view) == Some(request.tab)
        }
}

/// Whether the page is still where it asked: its `panel_keys` mirror carries the generation the request did. A
/// page that has moved on since (typed, left BROWSE, lost the keys) is not given an editor that takes them.
fn still_wanted(state: &AgentPanelState, request: &Request) -> bool {
    request
        .interaction
        .is_none_or(|asked| state.page(request.view).and_then(|page| page.interaction.get()) == Some(asked))
}

pub(super) fn poll(state: &mut AgentPanelState, out: &mut Outbox) {
    state
        .path_lookup
        .offers
        .retain(|offer| offer.at.elapsed() < OFFER_LIFETIME);
    let mut i = 0;
    while i < state.path_lookup.running.len() {
        let (request, receiver) = &state.path_lookup.running[i];
        let result = if request.started.elapsed() >= state.path_lookup.abandon_after {
            Err(format!(
                "gave up looking for {} after {}; the file system did not answer",
                request.wanted,
                if state.path_lookup.abandon_after >= Duration::from_secs(1) {
                    format!("{} s", state.path_lookup.abandon_after.as_secs())
                } else {
                    format!("{} ms", state.path_lookup.abandon_after.as_millis())
                }
            ))
        } else {
            match receiver.try_recv() {
                Ok(answer) => answer,
                Err(mpsc::TryRecvError::Empty) => {
                    i += 1;
                    continue;
                }
                Err(mpsc::TryRecvError::Disconnected) => Err("path lookup ended before an answer arrived".into()),
            }
        };
        let (request, _) = state.path_lookup.running.swap_remove(i);
        if !active(state, &request) {
            // A successful empty answer settles the page's request without an error flash or an editor open.
            out.send(
                request.view,
                serialize_command_result_for_js(&request.request_id, Ok(())),
            );
            continue;
        }
        match result {
            Ok(Answer::One(found)) => {
                // Re-check on another worker after delivery, keeping filesystem calls off the panel tick.
                // The request retains its original start time so this phase shares the abandonment bound.
                let root = state.project_dir.clone();
                let view = request.view;
                let id = request.request_id.clone();
                let line = request.line;
                let expect = request.expect.clone();
                if let Err(why) = start_worker(state, request, move || {
                    final_check(&root, found, line, expect.as_deref())
                }) {
                    out.send(view, serialize_command_result_for_js(&id, Err(&why)));
                }
            }
            Ok(Answer::Ready(..)) if !still_wanted(state, &request) => {
                out.send(
                    request.view,
                    serialize_command_result_for_js(&request.request_id, Ok(())),
                );
            }
            Ok(Answer::Ready(path, note)) => {
                if let Some(note) = note {
                    // An envelope of its own, before the editor's answer, which alone settles the request.
                    out.send(
                        request.view,
                        eitri_core::agent_bridge::serialize_open_path_note_for_js(&request.request_id, &note),
                    );
                }
                out.effect(Effect::AskEditor {
                    request: eitri_core::scratch::open_request(&path, request.line),
                    then: EditorThen::Answer {
                        view: request.view,
                        request_id: request.request_id,
                    },
                });
            }
            Ok(Answer::Choices(paths, stopped)) => {
                let names = paths.iter().map(|path| path.name.clone()).collect::<Vec<_>>();
                out.send(
                    request.view,
                    eitri_core::agent_bridge::serialize_path_choices_for_js(
                        &request.request_id,
                        request.tab,
                        &names,
                        request.line,
                        stopped.as_deref(),
                    ),
                );
                let offers = &mut state.path_lookup.offers;
                offers.retain(|offer| offer.view != request.view || offer.request_id != request.request_id);
                while offers.len() >= MAX_OFFERS
                    || offers.iter().map(|offer| offer.paths.len()).sum::<usize>() + paths.len() > MAX_FILES
                {
                    if offers.is_empty() {
                        break;
                    }
                    offers.remove(0);
                }
                offers.push(Offer {
                    view: request.view,
                    request_id: request.request_id,
                    tab: request.tab,
                    root: state.project_dir.clone(),
                    paths,
                    at: Instant::now(),
                });
            }
            Err(why) => out.send(
                request.view,
                serialize_command_result_for_js(&request.request_id, Err(&why)),
            ),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;

    #[test]
    fn path_lookup_expect_checks_disk_on_exact_suffix_and_picked_paths() {
        for mode in ["exact", "suffix", "pick"] {
            for present in [false, true] {
                let root = Project::new();
                root.write("src/mesh.go");
                std::fs::write(
                    root.0.join("src/mesh.go"),
                    if present {
                        "header\nfn Name() {}\n"
                    } else {
                        "header\nchanged\n"
                    },
                )
                .unwrap();
                if mode == "pick" {
                    root.write("test/mesh.go");
                }
                let (handle, page, _) = handle_with_recording_page();
                handle.state.borrow_mut().project_dir = root.0.clone();
                let opened = record_opens(&handle);
                let wanted = if mode == "exact" { "src/mesh.go" } else { "mesh.go" };
                handle.page_message(
                    ViewId::BASE,
                    &serde_json::json!({
                        "type": "open_path", "request_id": "find", "path": wanted, "line": 2, "expect": "Name",
                    })
                    .to_string(),
                );
                if mode == "pick" {
                    tick_handle_until(&handle, "choices before the expected-name check", || {
                        response(&page).is_some()
                    });
                    assert_eq!(response(&page).unwrap()["kind"], "path_choices");
                    page.sent.borrow_mut().clear();
                    handle.page_message(ViewId::BASE, &serde_json::json!({
                        "type": "open_path", "request_id": "find", "path": "src/mesh.go", "line": 2, "expect": "Name",
                    }).to_string());
                }
                tick_handle_until(&handle, "expected-name open", || !opened.borrow().is_empty());
                assert_eq!(opened.borrow().len(), 1, "a changed line still jumps");
                let notes = page
                    .sent
                    .borrow()
                    .iter()
                    .map(|p| json_of(p))
                    .filter(|p| p["kind"] == "open_path_note")
                    .filter_map(|p| p["note"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>();
                assert!(
                    page.sent
                        .borrow()
                        .iter()
                        .all(|p| json_of(p).get("note").is_none() || json_of(p)["kind"] == "open_path_note"),
                    "no other answer carries the note"
                );
                if present {
                    assert!(notes.is_empty());
                } else {
                    assert_eq!(
                        notes,
                        ["src/mesh.go:2 no longer has Name (the file changed since the reply)"]
                    );
                }
            }
        }
    }

    #[test]
    fn path_lookup_expect_never_calls_a_line_changed_when_the_read_cap_cuts_it() {
        let root = Project::new();
        root.write("mesh.go");
        let path = root.0.join("mesh.go");
        std::fs::write(&path, format!("{}Name", "x".repeat(1_000_000))).unwrap();
        assert_eq!(line_note(&path, "mesh.go", Some(1), Some("Name")), None);
        std::fs::write(&path, format!("changed\n{}Name", "x".repeat(1_000_000))).unwrap();
        assert!(line_note(&path, "mesh.go", Some(1), Some("Name")).is_some());
        assert_eq!(line_note(&path, "mesh.go", Some(2), Some("Name")), None);
    }

    #[test]
    fn path_lookup_expect_ignores_non_files_and_invalid_names() {
        let root = Project::new();
        assert_eq!(line_note(&root.0, "dir", Some(1), Some("Name")), None);
        root.write("mesh.go");
        assert_eq!(
            line_note(&root.0.join("mesh.go"), "mesh.go", Some(1), Some("bad name")),
            None
        );
        assert_eq!(line_note(&root.0.join("mesh.go"), "mesh.go", None, Some("Name")), None);
    }

    #[test]
    fn path_lookup_refuses_oversized_paths_before_starting_work() {
        for path in ["a".repeat(4097), "é".repeat(2049), format!("/{}", "a".repeat(4096))] {
            let (handle, page, _) = handle_with_recording_page();
            ask(&handle, &path);
            let answer = response(&page).expect("an oversized open is refused synchronously");
            assert_eq!(answer["ok"], false);
            assert_eq!(answer["error"], "path is longer than 4096 bytes");
            let mut state = handle.state.borrow_mut();
            let tab = state.tabs.active();
            assert_eq!(
                start(&mut state, path, None, None, ViewId::BASE, "long".into(), tab, None),
                Err("path is longer than 4096 bytes".into())
            );
            assert!(!state.path_lookup.is_running());
            assert_eq!(state.path_lookup.live_workers.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn path_lookup_length_limit_counts_bytes_and_accepts_the_boundary() {
        assert_eq!(check_path_length(&"a".repeat(4096)), Ok(()));
        assert_eq!(check_path_length(&"é".repeat(2048)), Ok(()));
        assert_eq!(check_path_length(""), Ok(()));
        assert!(check_path_length(&"é".repeat(2049)).is_err());
    }

    struct Project(PathBuf);
    impl Project {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("path-lookup-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn write(&self, name: &str) {
            let path = self.0.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
    }
    impl Drop for Project {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn ask(handle: &AgentPanelHandle, path: &str) {
        handle.page_message(
            ViewId::BASE,
            &serde_json::json!({
                "type": "open_path", "request_id": "find", "path": path, "line": 12,
            })
            .to_string(),
        );
    }
    fn response(page: &RecordingPage) -> Option<serde_json::Value> {
        page.sent
            .borrow()
            .iter()
            .map(|p| json_of(p))
            .find(|v| v["requestId"] == "find")
    }
    #[test]
    fn path_lookup_exact_open_uses_worker_and_missing_falls_back() {
        let root = Project::new();
        root.write("exact.rs");
        root.write("src/fallback.rs");
        for (wanted, expected) in [
            ("exact.rs", "exact.rs"),
            ("src", "src"),
            ("fallback.rs", "src/fallback.rs"),
        ] {
            let (handle, page, _) = handle_with_recording_page();
            handle.state.borrow_mut().project_dir = root.0.clone();
            let opened = record_opens(&handle);
            ask(&handle, wanted);
            assert_eq!(handle.state.borrow().path_lookup.running.len(), 1);
            assert!(opened.borrow().is_empty(), "only the tick delivers worker results");
            assert!(response(&page).is_none());
            tick_handle_until(&handle, "worker open", || response(&page).is_some());
            assert_eq!(response(&page).unwrap()["ok"], true);
            assert_eq!(
                *opened.borrow(),
                vec![eitri_core::scratch::open_request(&root.0.join(expected), Some(12))]
            );
        }
    }
    #[test]
    fn path_lookup_opens_a_single_suffix_with_its_project_name_and_line() {
        let root = Project::new();
        root.write("src/deep/a.rs");
        let (handle, page, _) = handle_with_recording_page();
        let opened = Rc::new(RefCell::new(Vec::new()));
        {
            let mut state = handle.state.borrow_mut();
            state.project_dir = root.0.clone();
            let opened = opened.clone();
            state.editor_request_hook = Some(Rc::new(move |request| {
                opened.borrow_mut().push(request.clone());
                Ok(None)
            }));
        }
        ask(&handle, "a.rs");
        tick_handle_until(&handle, "suffix open", || response(&page).is_some());
        assert_eq!(
            *opened.borrow(),
            vec![eitri_core::scratch::open_request(
                &root.0.join("src/deep/a.rs"),
                Some(12)
            )]
        );
        assert_eq!(response(&page).unwrap()["ok"], true);
    }
    #[test]
    fn path_lookup_several_matches_answer_the_request_with_choices() {
        let root = Project::new();
        root.write("src/a.rs");
        root.write("test/a.rs");
        let (handle, page, _) = handle_with_recording_page();
        handle.state.borrow_mut().project_dir = root.0.clone();
        let tab = handle.state.borrow().tabs.active();
        ask(&handle, "a.rs");
        tick_handle_until(&handle, "suffix choices", || response(&page).is_some());
        assert_eq!(
            response(&page).unwrap(),
            serde_json::json!({
                "kind": "path_choices", "requestId": "find", "tab": tab.0,
                "paths": [{"path": "src/a.rs", "line": 12}, {"path": "test/a.rs", "line": 12}], "stopped": null,
            })
        );
    }
    #[test]
    fn path_lookup_none_answers_with_the_original_request_id() {
        let root = Project::new();
        let (handle, page, _) = handle_with_recording_page();
        handle.state.borrow_mut().project_dir = root.0.clone();
        ask(&handle, "missing.rs");
        tick_handle_until(&handle, "missing suffix", || response(&page).is_some());
        let answer = response(&page).unwrap();
        assert_eq!(answer["kind"], "command_result");
        assert_eq!(answer["ok"], false);
        assert_eq!(answer["error"], "no such file: missing.rs");
    }
    #[test]
    fn path_lookup_refuses_a_suffix_symlink_outside_the_project() {
        let root = Project::new();
        let outside = Project::new();
        outside.write("x.rs");
        std::fs::create_dir(root.0.join("link")).unwrap();
        std::os::unix::fs::symlink(outside.0.join("x.rs"), root.0.join("link/x.rs")).unwrap();
        let (handle, page, _) = handle_with_recording_page();
        handle.state.borrow_mut().project_dir = root.0.clone();
        ask(&handle, "x.rs");
        tick_handle_until(&handle, "outside refusal", || response(&page).is_some());
        assert_eq!(
            response(&page).unwrap()["error"],
            "refused to open link/x.rs: it leads outside the project"
        );
    }
    #[test]
    fn path_lookup_invalid_requests_refuse_controls_before_worker_check() {
        let root = Project::new();
        for path in ["../x.rs", "a/../b.rs", "bad\nname.rs", "bad\u{85}.rs", "bad\u{202e}.rs"] {
            let (handle, page, _) = handle_with_recording_page();
            handle.state.borrow_mut().project_dir = root.0.clone();
            ask(&handle, path);
            let control = eitri_core::agent_bridge::path_holds_control(path);
            if control {
                assert!(handle.state.borrow().path_lookup.running.is_empty());
                assert!(response(&page).is_some(), "controls are refused synchronously");
            } else {
                tick_handle_until(&handle, "missing exact path", || response(&page).is_some());
            }
            let answer = response(&page).unwrap();
            assert_eq!(answer["ok"], false);
            assert!(answer["error"].as_str().unwrap().contains(if control {
                "control character"
            } else {
                "no such file"
            }));
        }
    }
    #[test]
    fn path_lookup_too_many_matches_asks_for_a_narrower_name() {
        let root = Project::new();
        for i in 0..15 {
            root.write(&format!("dir{i}/a.rs"));
        }
        assert!(
            matches!(lookup(&root.0, "a.rs", SuffixLimits { max_files: MAX_FILES, deadline: DEADLINE }), Err(why) if why == "15 files end with a.rs; narrow it")
        );
    }
    #[test]
    fn path_lookup_stopped_twenty_matches_answer_with_a_count_and_no_choices() {
        let root = Project::new();
        let names: Vec<_> = (0..20).map(|i| format!("dir{i}/a.rs")).collect();
        for name in &names {
            root.write(name);
        }
        let (handle, page, _) = handle_with_recording_page();
        let answer = checked_answer(
            &root.0,
            "a.rs",
            SuffixLookup {
                matches: names,
                stopped: Some(Stopped::Files(20)),
            },
        );
        let (tx, rx) = mpsc::channel();
        tx.send(answer).unwrap();
        {
            let mut state = handle.state.borrow_mut();
            let req = request(&state);
            state.path_lookup.running.push((req, rx));
        }
        handle.tick();
        let answer = response(&page).unwrap();
        assert_eq!(answer["kind"], "command_result");
        assert_eq!(answer["ok"], false);
        assert_eq!(
            answer["error"],
            "20+ files end with a.rs (stopped looking after 20 files); narrow it"
        );
        assert!(handle.state.borrow().path_lookup.offers.is_empty());
        assert!(page.sent.borrow().iter().all(|p| kind_of(p) != "path_choices"));
    }
    #[test]
    fn path_lookup_checks_only_until_fifteen_matches_pass() {
        for stopped in [false, true] {
            for refused in [0, 3] {
                let mut checks = 0;
                let result = checked_answer_with(
                    "a.rs",
                    SuffixLookup {
                        matches: (0..3000).map(|i| format!("dir{i}/a.rs")).collect(),
                        stopped: stopped.then_some(Stopped::Time),
                    },
                    |name| {
                        checks += 1;
                        if checks <= refused {
                            Err("refused".into())
                        } else {
                            Ok(FoundPath {
                                name: name.into(),
                                canonical: PathBuf::from(name),
                            })
                        }
                    },
                );
                let expected = if stopped {
                    "3000+ files end with a.rs (stopped looking after 1 s); narrow it"
                } else {
                    "3000 files end with a.rs; narrow it"
                };
                assert!(matches!(result, Err(why) if why == expected));
                assert_eq!(checks, refused + PATH_PICK_MAX + 1);
            }
        }
    }
    #[test]
    fn path_lookup_fewer_than_fifteen_valid_matches_keep_their_order() {
        let mut checks = 0;
        let result = checked_answer_with(
            "a.rs",
            SuffixLookup {
                matches: (0..20).map(|i| format!("dir{i}/a.rs")).collect(),
                stopped: Some(Stopped::Time),
            },
            |name| {
                checks += 1;
                if checks <= 6 {
                    Err("refused".into())
                } else {
                    Ok(FoundPath {
                        name: name.into(),
                        canonical: PathBuf::from(name),
                    })
                }
            },
        );
        let Ok(Answer::Choices(paths, Some(_))) = result else {
            panic!("fourteen valid matches must remain choices");
        };
        assert_eq!(checks, 20);
        assert_eq!(paths.len(), PATH_PICK_MAX);
        assert_eq!(
            paths.iter().map(|path| path.name.clone()).collect::<Vec<_>>(),
            (6..20).map(|i| format!("dir{i}/a.rs")).collect::<Vec<_>>()
        );
    }
    #[test]
    fn path_lookup_caps_workers_without_touching_the_disk() {
        let (handle, page, _) = handle_with_recording_page();
        let mut keep_senders = Vec::new();
        for _ in 0..MAX_RUNNING {
            let (tx, rx) = mpsc::channel();
            keep_senders.push(tx);
            handle.state.borrow_mut().path_lookup.running.push((
                Request {
                    line: None,
                    expect: None,
                    view: ViewId::BASE,
                    request_id: "held".into(),
                    tab: TabId(1),
                    wanted: "held".into(),
                    started: Instant::now(),
                    interaction: None,
                },
                rx,
            ));
        }
        ask(&handle, "a.rs");
        assert_eq!(
            response(&page).unwrap()["error"],
            "still looking for another path; try again in a moment"
        );
    }
    #[test]
    fn path_lookup_choices_name_the_requesting_views_tab() {
        let root = Project::new();
        root.write("src/a.rs");
        root.write("test/a.rs");
        let (handle, base, second) = handle_with_two_pages();
        let tab = {
            let mut state = handle.state.borrow_mut();
            state.project_dir = root.0.clone();
            state.tabs.open_in(AgentId::Claude, view(2)).unwrap()
        };
        handle.page_message(
            view(2),
            &serde_json::json!({
                "type": "open_path", "request_id": "find", "path": "a.rs",
            })
            .to_string(),
        );
        tick_handle_until(&handle, "second view's suffix choices", || response(&second).is_some());
        let answer = response(&second).unwrap();
        assert_eq!(answer["kind"], "path_choices");
        assert_eq!(answer["tab"], tab.0);
        assert_eq!(answer["paths"][0]["line"], serde_json::Value::Null);
        assert!(response(&base).is_none());
    }
    fn request(state: &AgentPanelState) -> Request {
        Request {
            line: Some(12),
            expect: None,
            view: ViewId::BASE,
            request_id: "find".into(),
            tab: state.tabs.active(),
            wanted: "a.rs".into(),
            started: Instant::now(),
            interaction: None,
        }
    }
    #[test]
    fn path_lookup_tick_polls_until_the_request_is_answered() {
        use crate::panel_wake::NextTick;
        let (handle, page) = settled_panel();
        let (tx, rx) = mpsc::channel();
        {
            let mut state = handle.state.borrow_mut();
            let req = request(&state);
            state.path_lookup.running.push((req, rx));
        }
        let at = Instant::now();
        assert_eq!(handle.run_tick(at), NextTick::At(at + AgentPanelHandle::TICK));
        assert!(response(&page).is_none());
        tx.send(Err("finished".into())).unwrap();
        let at = at + AgentPanelHandle::TICK;
        assert_eq!(handle.run_tick(at), NextTick::At(at + AgentPanelHandle::TICK));
        assert_eq!(response(&page).unwrap()["error"], "finished");
        assert_eq!(handle.run_tick(at + AgentPanelHandle::TICK), NextTick::OnWake);
    }
    #[test]
    fn path_lookup_tick_abandons_a_lookup_on_an_otherwise_idle_panel() {
        use crate::panel_wake::NextTick;
        let (handle, page) = settled_panel();
        let (_tx, rx) = mpsc::channel();
        {
            let mut state = handle.state.borrow_mut();
            // Keep the receiver unanswered without depending on worker scheduling or a slow disk.
            state.path_lookup.abandon_after = Duration::from_secs(60);
            let req = request(&state);
            state.path_lookup.running.push((req, rx));
        }
        let at = Instant::now();
        let NextTick::At(due) = handle.run_tick(at) else {
            panic!("an unanswered lookup must schedule its abandonment check");
        };
        assert!(response(&page).is_none());
        {
            let mut state = handle.state.borrow_mut();
            state.path_lookup.abandon_after = Duration::from_millis(17);
            state.path_lookup.running[0].0.started = Instant::now() - Duration::from_millis(18);
        }
        assert_eq!(handle.run_tick(due), NextTick::At(due + AgentPanelHandle::TICK));
        assert_eq!(
            response(&page).unwrap()["error"],
            "gave up looking for a.rs after 17 ms; the file system did not answer"
        );
        assert!(handle.state.borrow().path_lookup.running.is_empty());
        assert_eq!(handle.run_tick(due + AgentPanelHandle::TICK), NextTick::OnWake);
    }
    fn wait_for_workers(live: &AtomicUsize) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while live.load(Ordering::SeqCst) != 0 {
            assert!(Instant::now() < deadline, "released lookup workers did not exit");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn path_lookup_counts_abandoned_live_workers_and_refuses_new_lookups_and_picks() {
        let root = Project::new();
        root.write("src/a.rs");
        let (handle, page) = settled_panel();
        let mut releases = Vec::new();
        let live = handle.state.borrow().path_lookup.live_workers.clone();
        for _ in 0..MAX_LIVE_WORKERS {
            let (release, blocked) = mpsc::channel::<()>();
            releases.push(release);
            let mut state = handle.state.borrow_mut();
            state.path_lookup.abandon_after = Duration::ZERO;
            let req = request(&state);
            start_worker(&mut state, req, move || {
                let _ = blocked.recv();
                Err("released".into())
            })
            .unwrap();
            poll(&mut state, &mut Outbox::default());
            assert!(state.path_lookup.running.is_empty());
        }
        assert_eq!(live.load(Ordering::SeqCst), MAX_LIVE_WORKERS);
        ask(&handle, "a.rs");
        const FULL: &str = "8 path lookups are still waiting on the file system; try again later";
        assert_eq!(response(&page).unwrap()["error"], FULL);
        {
            let mut state = handle.state.borrow_mut();
            let tab = state.tabs.active();
            state.path_lookup.offers.push(Offer {
                view: ViewId::BASE,
                request_id: "offer".into(),
                tab,
                root: root.0.clone(),
                paths: vec![checked_path(&root.0, "src/a.rs", None).unwrap()],
                at: Instant::now(),
            });
            assert_eq!(
                pick(&mut state, "src/a.rs", None, None, ViewId::BASE, "pick", tab, None),
                Some(Err(FULL.into()))
            );
            assert!(state.path_lookup.running.is_empty());
            assert!(state.path_lookup.offers.is_empty());
        }
        assert_eq!(live.load(Ordering::SeqCst), MAX_LIVE_WORKERS);
        drop(releases);
        wait_for_workers(&live);
        let mut state = handle.state.borrow_mut();
        let req = request(&state);
        start_worker(&mut state, req, || Err("free slot".into())).unwrap();
        drop(state);
        wait_for_workers(&live);
    }
    #[test]
    fn path_lookup_a_new_interaction_cancels_a_held_open_even_back_in_browse() {
        for changed in [false, true] {
            let (handle, page) = settled_panel();
            let opened = record_opens(&handle);
            let mirror = |mode: &str, interaction: u64| {
                handle.page_message(
                    ViewId::BASE,
                    &serde_json::json!({
                        "type": "panel_keys", "request_id": "keys", "mode": mode, "interaction": interaction,
                    })
                    .to_string(),
                );
            };
            mirror("browse", 1);
            let (release, blocked) = mpsc::channel::<()>();
            let live = handle.state.borrow().path_lookup.live_workers.clone();
            {
                let mut state = handle.state.borrow_mut();
                let req = Request {
                    interaction: Some(1),
                    ..request(&state)
                };
                start_worker(&mut state, req, move || {
                    blocked.recv().unwrap();
                    Ok(Answer::Ready(PathBuf::from("/project/a.rs"), None))
                })
                .unwrap();
            }
            handle.tick();
            assert!(opened.borrow().is_empty(), "the filesystem is held");
            if changed {
                mirror("input", 2);
                mirror("browse", 3);
            }
            release.send(()).unwrap();
            wait_for_workers(&live);
            page.sent.borrow_mut().clear();
            handle.tick();
            assert_eq!(opened.borrow().len(), usize::from(!changed));
            assert!(handle.state.borrow().path_lookup.running.is_empty());
            assert_eq!(response(&page).unwrap()["ok"], true);
        }
    }
    #[test]
    fn path_lookup_open_path_carries_its_interaction_to_the_editor_check() {
        for (asked, mirrored, opens) in [(4, 4, true), (4, 5, false)] {
            let root = Project::new();
            root.write("src/a.rs");
            let (handle, page, _) = handle_with_recording_page();
            handle.state.borrow_mut().project_dir = root.0.clone();
            let opened = record_opens(&handle);
            handle.page_message(
                ViewId::BASE,
                &serde_json::json!({
                    "type": "panel_keys", "request_id": "keys", "mode": "browse", "interaction": mirrored,
                })
                .to_string(),
            );
            handle.page_message(
                ViewId::BASE,
                &serde_json::json!({
                    "type": "open_path", "request_id": "find", "path": "src/a.rs", "interaction": asked,
                })
                .to_string(),
            );
            tick_handle_until(&handle, "the open's answer", || {
                !opened.borrow().is_empty() || response(&page).is_some()
            });
            assert_eq!(!opened.borrow().is_empty(), opens, "asked {asked}, mirrored {mirrored}");
            if !opens {
                assert_eq!(response(&page).unwrap()["ok"], true);
            }
        }
    }
    #[test]
    fn path_lookup_panicking_worker_releases_its_live_count() {
        let (handle, page) = settled_panel();
        let live = handle.state.borrow().path_lookup.live_workers.clone();
        {
            let mut state = handle.state.borrow_mut();
            let req = request(&state);
            start_worker(&mut state, req, || panic!("worker failed")).unwrap();
        }
        wait_for_workers(&live);
        handle.tick();
        assert_eq!(response(&page).unwrap()["error"], "path lookup failed; try again");
    }
    fn record_opens(handle: &AgentPanelHandle) -> Rc<RefCell<Vec<eitri_core::scratch::ScratchRequest>>> {
        let opened = Rc::new(RefCell::new(Vec::new()));
        let sink = opened.clone();
        handle.state.borrow_mut().editor_request_hook = Some(Rc::new(move |value| {
            sink.borrow_mut().push(value.clone());
            Ok(None)
        }));
        opened
    }
    #[test]
    fn path_lookup_stopped_single_match_is_a_choice_with_the_bound() {
        let root = Project::new();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&root.0)
            .status()
            .unwrap()
            .success());
        root.write("a/a.rs");
        root.write("z/z.rs");
        let (handle, page, _) = handle_with_recording_page();
        {
            let mut state = handle.state.borrow_mut();
            state.project_dir = root.0.clone();
            state.path_lookup.limits.max_files = 1;
        }
        let opened = record_opens(&handle);
        ask(&handle, "a.rs");
        tick_handle_until(&handle, "stopped choice", || response(&page).is_some());
        let answer = response(&page).unwrap();
        assert_eq!(answer["kind"], "path_choices");
        assert_eq!(answer["paths"].as_array().unwrap().len(), 1);
        assert_eq!(answer["stopped"], "stopped looking after 1 files");
        assert!(opened.borrow().is_empty());
    }
    #[test]
    fn path_lookup_abandons_blocked_workers_and_reuses_the_slot() {
        let (handle, page, _) = handle_with_recording_page();
        let (release, blocked) = mpsc::channel();
        {
            let mut state = handle.state.borrow_mut();
            state.path_lookup.abandon_after = Duration::from_millis(1);
            let mut req = request(&state);
            req.started = Instant::now() - Duration::from_millis(2);
            start_worker(&mut state, req, move || {
                blocked.recv().unwrap();
                Err("released".into())
            })
            .unwrap();
        }
        handle.tick();
        assert_eq!(
            response(&page).unwrap()["error"],
            "gave up looking for a.rs after 1 ms; the file system did not answer"
        );
        assert!(handle.state.borrow().path_lookup.running.is_empty());
        let mut state = handle.state.borrow_mut();
        let req = request(&state);
        start_worker(&mut state, req, || Err("new worker".into())).unwrap();
        release.send(()).unwrap();
    }
    #[test]
    fn path_lookup_pick_rechecks_containment_and_regular_file() {
        for replacement in ["outside", "fifo", "directory", "inside"] {
            let root = Project::new();
            let outside = Project::new();
            root.write("src/a.rs");
            root.write("test/a.rs");
            outside.write("a.rs");
            let (handle, page, _) = handle_with_recording_page();
            handle.state.borrow_mut().project_dir = root.0.clone();
            let opened = record_opens(&handle);
            ask(&handle, "a.rs");
            tick_handle_until(&handle, "offer choices", || response(&page).is_some());
            assert_eq!(response(&page).unwrap()["kind"], "path_choices");
            std::fs::remove_file(root.0.join("src/a.rs")).unwrap();
            match replacement {
                "outside" => std::os::unix::fs::symlink(outside.0.join("a.rs"), root.0.join("src/a.rs")).unwrap(),
                "fifo" => assert!(std::process::Command::new("mkfifo")
                    .arg(root.0.join("src/a.rs"))
                    .status()
                    .unwrap()
                    .success()),
                "directory" => std::fs::create_dir(root.0.join("src/a.rs")).unwrap(),
                "inside" => std::os::unix::fs::symlink(root.0.join("test/a.rs"), root.0.join("src/a.rs")).unwrap(),
                _ => unreachable!(),
            }
            page.sent.borrow_mut().clear();
            ask(&handle, "src/a.rs");
            tick_handle_until(&handle, "pick refusal", || response(&page).is_some());
            let answer = response(&page).unwrap();
            assert_eq!(answer["ok"], false, "{replacement}: {answer}");
            assert!(answer["error"].as_str().unwrap().contains(match replacement {
                "outside" => "outside the project",
                "inside" => "target changed",
                _ => "not a regular file",
            }));
            assert!(opened.borrow().is_empty());
            assert!(handle.state.borrow().path_lookup.offers.is_empty());
        }
    }
    #[test]
    fn path_lookup_rechecks_single_identity_after_worker_delivery() {
        let root = Project::new();
        let outside = Project::new();
        root.write("src/a.rs");
        outside.write("a.rs");
        let found = checked_path(&root.0, "src/a.rs", None).unwrap();
        std::fs::remove_file(root.0.join("src/a.rs")).unwrap();
        std::os::unix::fs::symlink(outside.0.join("a.rs"), root.0.join("src/a.rs")).unwrap();
        let (handle, page, _) = handle_with_recording_page();
        handle.state.borrow_mut().project_dir = root.0.clone();
        let opened = record_opens(&handle);
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(Answer::One(found))).unwrap();
        let mut state = handle.state.borrow_mut();
        let req = request(&state);
        state.path_lookup.running.push((req, rx));
        drop(state);
        tick_handle_until(&handle, "single target recheck", || response(&page).is_some());
        assert!(response(&page).unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("outside the project"));
        assert!(opened.borrow().is_empty());
    }
    #[test]
    fn path_lookup_late_single_never_opens_after_tab_switch_or_close() {
        for close in [false, true] {
            for ready in [false, true] {
                let root = Project::new();
                root.write("src/a.rs");
                let (handle, page, _) = handle_with_recording_page();
                let opened = record_opens(&handle);
                let (tx, rx) = mpsc::channel();
                let mut state = handle.state.borrow_mut();
                state.project_dir = root.0.clone();
                let req = request(&state);
                let old = req.tab;
                state.path_lookup.running.push((req, rx));
                state.tabs.open(AgentId::Claude);
                if close {
                    state.tabs.remove(old);
                }
                drop(state);
                tx.send(Ok(if ready {
                    Answer::Ready(root.0.join("src/a.rs"), None)
                } else {
                    Answer::One(checked_path(&root.0, "src/a.rs", None).unwrap())
                }))
                .unwrap();
                tick_handle_until(&handle, "late single ignored", || response(&page).is_some());
                assert_eq!(response(&page).unwrap()["ok"], true);
                assert!(opened.borrow().is_empty());
                assert!(handle.state.borrow().path_lookup.running.is_empty());
            }
        }
    }
    #[test]
    fn path_lookup_git_tracked_outside_symlink_is_refused() {
        let root = Project::new();
        let outside = Project::new();
        outside.write("a.rs");
        std::fs::create_dir(root.0.join("src")).unwrap();
        std::os::unix::fs::symlink(outside.0.join("a.rs"), root.0.join("src/a.rs")).unwrap();
        for args in [&["init", "-q"][..], &["add", "src/a.rs"][..]] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(&root.0)
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        let (handle, page, _) = handle_with_recording_page();
        handle.state.borrow_mut().project_dir = root.0.clone();
        let opened = record_opens(&handle);
        ask(&handle, "a.rs");
        tick_handle_until(&handle, "git symlink refused", || response(&page).is_some());
        assert_eq!(
            response(&page).unwrap()["error"],
            "refused to open src/a.rs: it leads outside the project"
        );
        assert!(opened.borrow().is_empty());
    }
    #[test]
    fn path_lookup_time_stopped_match_is_a_choice_with_a_reason() {
        let root = Project::new();
        root.write("src/a.rs");
        let answer = checked_answer(
            &root.0,
            "a.rs",
            SuffixLookup {
                matches: vec!["src/a.rs".into()],
                stopped: Some(Stopped::Time),
            },
        )
        .unwrap_or_else(|why| panic!("{why}"));
        assert!(matches!(answer, Answer::Choices(..)));
        let (handle, page, _) = handle_with_recording_page();
        handle.state.borrow_mut().project_dir = root.0.clone();
        let opened = record_opens(&handle);
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(answer)).unwrap();
        let mut state = handle.state.borrow_mut();
        let req = request(&state);
        state.path_lookup.running.push((req, rx));
        drop(state);
        tick_handle_until(&handle, "time-stopped choice", || response(&page).is_some());
        assert_eq!(response(&page).unwrap()["stopped"], "stopped looking after 1 s");
        assert!(opened.borrow().is_empty());
    }
    #[test]
    fn path_lookup_offers_expire_and_are_bounded_per_view_and_request() {
        let root = Project::new();
        root.write("src/a.rs");
        let (handle, _, _) = handle_with_recording_page();
        let mut state = handle.state.borrow_mut();
        state.project_dir = root.0.clone();
        for i in 0..MAX_OFFERS + 2 {
            let (tx, rx) = mpsc::channel();
            tx.send(Ok(Answer::Choices(
                vec![checked_path(&root.0, "src/a.rs", None).unwrap()],
                None,
            )))
            .unwrap();
            let mut req = request(&state);
            req.request_id = i.to_string();
            state.path_lookup.running.push((req, rx));
            poll(&mut state, &mut Outbox::default());
        }
        assert_eq!(state.path_lookup.offers.len(), MAX_OFFERS);
        assert_eq!(state.path_lookup.offers[0].request_id, "2");
        state.path_lookup.offers[0].at = Instant::now() - OFFER_LIFETIME;
        poll(&mut state, &mut Outbox::default());
        assert_eq!(state.path_lookup.offers.len(), MAX_OFFERS - 1);
        assert_eq!(state.path_lookup.offers[0].request_id, "3");
        let tab = state.tabs.active();
        assert!(pick(&mut state, "src/a.rs", None, None, view(2), "pick", tab, None).is_none());
    }
    #[test]
    fn path_lookup_unchanged_pick_opens_and_consumes_its_offer() {
        let root = Project::new();
        root.write("src/a.rs");
        root.write("test/a.rs");
        let (handle, page, _) = handle_with_recording_page();
        handle.state.borrow_mut().project_dir = root.0.clone();
        let opened = record_opens(&handle);
        ask(&handle, "a.rs");
        tick_handle_until(&handle, "choices to pick", || response(&page).is_some());
        page.sent.borrow_mut().clear();
        ask(&handle, "test/a.rs");
        tick_handle_until(&handle, "picked file", || response(&page).is_some());
        assert_eq!(response(&page).unwrap()["ok"], true);
        assert_eq!(
            *opened.borrow(),
            vec![eitri_core::scratch::open_request(&root.0.join("test/a.rs"), Some(12))]
        );
        assert!(handle.state.borrow().path_lookup.offers.is_empty());
    }
}
