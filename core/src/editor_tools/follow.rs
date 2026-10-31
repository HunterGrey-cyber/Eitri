//! Optional host-driven views of completed edits, through the same queue and chunk as `open_file`.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use agent::setting_sources::ProjectTrust;
use agent::PathJudgement;

use super::paths;
use super::stamp::EDITOR_BUDGET;
use super::step::{Step, StepEvent, ToolCx, ToolResult, ToolRun};
use super::way::EditorCall;
// Transcript attribution is pure and shared with review; following reads no review state for it.
pub(crate) use crate::turn_review::attribution::edited_by;
use crate::turn_review::FirstChangeJob;

pub const FOLLOW_EVERY: Duration = Duration::from_millis(500);

pub struct FollowTarget {
    pub rel: PathBuf,
    pub line: Option<u32>,
}

#[derive(Default)]
struct TabFollow {
    last: Option<Instant>,
    wanted: Option<PathBuf>,
    job: Option<Receiver<Option<FollowTarget>>>,
    out: Rc<Cell<bool>>,
    called_off: Rc<Cell<bool>>,
}

/// A tab keeps only its latest wanted file while a worker or an editor run is outstanding.
#[derive(Default)]
pub struct Follower {
    tabs: BTreeMap<u64, TabFollow>,
}

impl Follower {
    pub fn edited(&mut self, tab: u64, path: PathBuf) {
        self.tabs.entry(tab).or_default().wanted = Some(path);
    }

    /// Starts due work without waiting. Each tab submits at most one run per interval and keeps just
    /// its latest wanted path while that run is queued or in flight.
    pub fn poll(
        &mut self,
        root: &Path,
        home: Option<&Path>,
        job: &dyn Fn(u64) -> Option<FirstChangeJob>,
        now: Instant,
    ) -> Vec<(u64, FollowRun)> {
        let mut runs = Vec::new();
        for (&tab, state) in &mut self.tabs {
            if state.out.get() {
                continue;
            }
            if let Some(rx) = &state.job {
                match rx.try_recv() {
                    Err(TryRecvError::Empty) => continue,
                    result => {
                        state.job = None;
                        // An edit that arrived during the worker supersedes its answer too.
                        if state.wanted.is_none() {
                            if let Ok(Some(target)) = result {
                                let mut run = FollowRun::new(root, target);
                                run.out = state.out.clone();
                                run.called_off = state.called_off.clone();
                                state.out.set(true);
                                state.last = Some(now);
                                runs.push((tab, run));
                                continue;
                            }
                        }
                    }
                }
            }
            if state
                .last
                .is_some_and(|last| now.saturating_duration_since(last) < FOLLOW_EVERY)
            {
                continue;
            }
            let Some(path) = state.wanted.take() else { continue };
            let root = root.to_path_buf();
            let home = home.map(Path::to_path_buf);
            let review = job(tab);
            let (tx, rx) = mpsc::channel();
            if std::thread::Builder::new()
                .name("editor-follow".into())
                .spawn(move || {
                    let target = (|| {
                        if crate::agent_bridge::path_holds_control(&path.to_string_lossy()) {
                            return None;
                        }
                        let PathJudgement::Inside(abs) = agent::judge_project_path(&root, home.as_deref(), &path)
                        else {
                            return None;
                        };
                        paths::file_check(&abs).ok()?;
                        let rel = abs.strip_prefix(&root).ok()?.to_path_buf();
                        let line = review.and_then(|job| job.run(&rel));
                        Some(FollowTarget { rel, line })
                    })();
                    let _ = tx.send(target);
                })
                .is_ok()
            {
                state.job = Some(rx);
            }
        }
        runs
    }

    /// Drops what the tab wanted, and a run already handed over: one queued behind a long tool has not told the
    /// editor anything yet, and it must not once following is off.
    pub fn forget(&mut self, tab: u64) {
        if let Some(state) = self.tabs.remove(&tab) {
            state.called_off.set(true);
        }
    }

    pub fn is_idle(&self) -> bool {
        self.tabs
            .values()
            .all(|state| state.wanted.is_none() && state.job.is_none() && !state.out.get())
    }
}

/// One host run, answered nowhere. Dropping it releases the tab even if the broker withdraws its line.
pub struct FollowRun {
    root: PathBuf,
    target: FollowTarget,
    out: Rc<Cell<bool>>,
    called_off: Rc<Cell<bool>>,
}

impl FollowRun {
    pub fn new(root: &Path, target: FollowTarget) -> FollowRun {
        FollowRun {
            root: root.to_path_buf(),
            target,
            out: Rc::new(Cell::new(true)),
            called_off: Rc::new(Cell::new(false)),
        }
    }
}

impl Drop for FollowRun {
    fn drop(&mut self) {
        self.out.set(false);
    }
}

impl ToolRun for FollowRun {
    fn step(&mut self, event: StepEvent, cx: &ToolCx<'_>) -> Step {
        match event {
            StepEvent::Start if self.called_off.get() => Step::Done(ToolResult {
                text: String::new(),
                is_error: false,
            }),
            StepEvent::Start => Step::Call {
                call: EditorCall::OpenFile {
                    path: self.root.join(&self.target.rel),
                    line: self.target.line,
                    end_line: None,
                    load: cx.trust == ProjectTrust::Trusted,
                },
                budget: EDITOR_BUDGET,
            },
            _ => Step::Done(ToolResult {
                text: String::new(),
                is_error: false,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_tools::memory::Memory;
    use crate::editor_tools::step::{AgentCall, EditorFailure, EditorReach, EditorSight};
    use crate::editor_tools::testing::drive;
    use crate::test_scratch_dir::ScratchDir;

    fn fixture(label: &str) -> ScratchDir {
        let dir = ScratchDir::new("eitri-follow", label);
        for n in 0..12 {
            std::fs::write(dir.join(format!("{n}.rs")), "one\ntwo\n").unwrap();
        }
        dir
    }

    fn ready(f: &mut Follower, root: &Path, now: Instant) -> FollowRun {
        let limit = Instant::now() + Duration::from_secs(5);
        loop {
            let mut runs = f.poll(root, None, &|_| None, now);
            assert!(runs.len() <= 1);
            if let Some((tab, run)) = runs.pop() {
                assert_eq!(tab, 1);
                return run;
            }
            assert!(Instant::now() < limit, "the path worker finishes");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn rejected(f: &mut Follower, root: &Path, now: Instant) {
        let limit = Instant::now() + Duration::from_secs(5);
        while !f.is_idle() {
            assert!(f.poll(root, None, &|_| None, now).is_empty());
            assert!(Instant::now() < limit, "the path worker finishes");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn at_most_one_follow_in_500_ms_and_the_latest_wins() {
        let dir = fixture("rate");
        let mut f = Follower::default();
        let now = Instant::now();
        f.edited(1, "0.rs".into());
        drop(ready(&mut f, &dir, now));
        f.edited(1, "1.rs".into());
        assert!(f
            .poll(&dir, None, &|_| None, now + FOLLOW_EVERY - Duration::from_millis(1))
            .is_empty());
        f.edited(1, "2.rs".into());
        let run = ready(&mut f, &dir, now + FOLLOW_EVERY);
        assert_eq!(run.target.rel, Path::new("2.rs"));
    }

    #[test]
    fn a_blocked_editor_and_ten_edits_give_one_run_then_the_latest() {
        let dir = fixture("blocked");
        let mut f = Follower::default();
        let now = Instant::now();
        f.edited(1, "0.rs".into());
        let run = ready(&mut f, &dir, now);
        for n in 1..=10 {
            f.edited(1, format!("{n}.rs").into());
            assert!(f.poll(&dir, None, &|_| None, now + FOLLOW_EVERY * n).is_empty());
        }
        drop(run);
        assert_eq!(
            ready(&mut f, &dir, now + FOLLOW_EVERY * 11).target.rel,
            Path::new("10.rs")
        );
    }

    #[test]
    fn a_dropped_run_frees_the_tab() {
        let dir = fixture("dropped");
        let mut f = Follower::default();
        let now = Instant::now();
        f.edited(1, "0.rs".into());
        let run = ready(&mut f, &dir, now);
        assert!(!f.is_idle());
        drop(run);
        assert!(f.is_idle());
        f.edited(1, "1.rs".into());
        assert_eq!(ready(&mut f, &dir, now + FOLLOW_EVERY).target.rel, Path::new("1.rs"));
    }

    #[test]
    fn a_quiet_tab_runs_at_once() {
        let dir = fixture("quiet");
        let mut f = Follower::default();
        f.edited(1, "0.rs".into());
        let run = ready(&mut f, &dir, Instant::now());
        assert_eq!(run.target.rel, Path::new("0.rs"));
        f.edited(2, "1.rs".into());
        let limit = Instant::now() + Duration::from_secs(5);
        loop {
            let runs = f.poll(&dir, None, &|_| None, Instant::now());
            if !runs.is_empty() {
                assert_eq!(runs[0].0, 2);
                break;
            }
            assert!(Instant::now() < limit);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_path_outside_runs_nothing() {
        let dir = fixture("outside");
        let mut f = Follower::default();
        f.edited(1, dir.parent().unwrap().join("outside.rs"));
        rejected(&mut f, &dir, Instant::now());
    }

    #[test]
    fn a_link_leaving_the_root_runs_nothing() {
        let dir = fixture("link");
        let outside = fixture("link-outside");
        std::os::unix::fs::symlink(outside.join("0.rs"), dir.join("link.rs")).unwrap();
        let mut f = Follower::default();
        f.edited(1, "link.rs".into());
        rejected(&mut f, &dir, Instant::now());
    }

    #[test]
    fn a_fifo_runs_nothing() {
        use std::os::unix::ffi::OsStrExt;
        let dir = fixture("fifo");
        let path = std::ffi::CString::new(dir.join("fifo").as_os_str().as_bytes()).unwrap();
        // SAFETY: the C string stays alive for the call and names this test's scratch file.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let mut f = Follower::default();
        f.edited(1, "fifo".into());
        rejected(&mut f, &dir, Instant::now());
    }

    #[test]
    fn no_review_shows_the_top() {
        let dir = fixture("no-review");
        let mut f = Follower::default();
        f.edited(1, "0.rs".into());
        assert_eq!(ready(&mut f, &dir, Instant::now()).target.line, None);
    }

    fn cx<'a>(root: &'a Path, memory: &'a Memory, trust: ProjectTrust) -> ToolCx<'a> {
        ToolCx {
            root,
            home: None,
            memory,
            trust,
            mode: agent::PermissionMode::Bypass,
            reach: EditorReach::Own,
            sight: EditorSight::OnScreen,
            context: None,
            tab: 1,
            session: "s1",
            call: "follow",
            agent_call: AgentCall::Host,
        }
    }

    fn check_load(trust: ProjectTrust, expected: bool, answer: Result<rmpv::Value, EditorFailure>) {
        let dir = fixture("load");
        let run = FollowRun::new(
            &dir,
            FollowTarget {
                rel: "0.rs".into(),
                line: Some(2),
            },
        );
        let memory = Memory::default();
        let mut calls = 0;
        let steps = drive(Box::new(run), &cx(&dir, &memory, trust), |call| {
            let EditorCall::OpenFile {
                path,
                line,
                end_line,
                load,
            } = call
            else {
                panic!("opens a file")
            };
            assert_eq!(path, &dir.join("0.rs"));
            assert_eq!((*line, *end_line, *load), (Some(2), None, expected));
            calls += 1;
            answer.clone()
        });
        assert_eq!(calls, 1);
        assert!(matches!(steps.as_slice(), [Step::Call { budget, .. }, Step::Done(_)] if *budget == EDITOR_BUDGET));
    }

    #[test]
    fn an_n_session_never_loads() {
        check_load(
            ProjectTrust::Untrusted,
            false,
            Ok(rmpv::Value::Map(vec![("needs_load".into(), true.into())])),
        );
    }

    #[test]
    fn a_y_session_loads_with_no_card() {
        check_load(
            ProjectTrust::Trusted,
            true,
            Ok(rmpv::Value::Map(vec![("shown".into(), true.into())])),
        );
        check_load(
            ProjectTrust::Trusted,
            true,
            Ok(rmpv::Value::Map(vec![("swap".into(), true.into())])),
        );
    }

    #[test]
    fn a_follow_past_its_stamp_changes_nothing() {
        check_load(ProjectTrust::Trusted, true, Err(EditorFailure::NotAnswered));
    }

    #[test]
    fn a_run_waiting_in_the_queue_when_following_goes_off_tells_the_editor_nothing() {
        let dir = fixture("called-off");
        let mut f = Follower::default();
        f.edited(1, "0.rs".into());
        let now = Instant::now();
        assert!(f.poll(&dir, None, &|_| None, now).is_empty());
        let run = ready(&mut f, &dir, now);
        f.forget(1);

        let memory = Memory::default();
        let steps = drive(Box::new(run), &cx(&dir, &memory, ProjectTrust::Trusted), |call| {
            panic!("the editor was asked: {call:?}")
        });
        assert!(matches!(steps.as_slice(), [Step::Done(result)] if !result.is_error));
    }

    #[test]
    fn a_later_run_of_the_tab_is_not_the_one_called_off() {
        let dir = fixture("on-again");
        let mut f = Follower::default();
        f.edited(1, "0.rs".into());
        let now = Instant::now();
        assert!(f.poll(&dir, None, &|_| None, now).is_empty());
        drop(ready(&mut f, &dir, now));
        f.forget(1);

        f.edited(1, "1.rs".into());
        let later = now + FOLLOW_EVERY;
        assert!(f.poll(&dir, None, &|_| None, later).is_empty());
        let run = ready(&mut f, &dir, later);
        let memory = Memory::default();
        let mut asked = 0;
        drive(Box::new(run), &cx(&dir, &memory, ProjectTrust::Trusted), |_| {
            asked += 1;
            Ok(rmpv::Value::Nil)
        });
        assert_eq!(asked, 1);
    }

    #[test]
    fn a_session_nobody_was_asked_about_never_loads() {
        check_load(
            ProjectTrust::NothingFound,
            false,
            Ok(rmpv::Value::Map(vec![("needs_load".into(), true.into())])),
        );
    }

    #[test]
    fn a_forgotten_tab_runs_nothing() {
        let dir = fixture("forgotten");
        let mut f = Follower::default();
        f.edited(1, "0.rs".into());
        assert!(f.poll(&dir, None, &|_| None, Instant::now()).is_empty());
        f.forget(1);
        assert!(f.is_idle());
        assert!(f.poll(&dir, None, &|_| None, Instant::now() + FOLLOW_EVERY).is_empty());
    }
}
