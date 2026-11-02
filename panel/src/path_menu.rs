//! A window's path index and its one completion worker. The UI only queues requests and takes results.
use super::*;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

type Lister = Arc<dyn Fn(&Path) -> Vec<String> + Send + Sync>;
#[derive(Clone)]
struct Request {
    tab: TabId,
    view: ViewId,
    query: String,
    seq: u64,
    ticket: u64,
}
struct Finished {
    request: Request,
    index: Arc<Vec<String>>,
    built: Instant,
    items: Vec<String>,
}
pub(super) struct PathMenu {
    index: Option<Arc<Vec<String>>>,
    built: Option<Instant>,
    running: Option<mpsc::Receiver<Finished>>,
    waiting: Option<Request>,
    latest: BTreeMap<(TabId, ViewId), u64>,
    ticket: u64,
    lister: Lister,
}
impl Default for PathMenu {
    fn default() -> Self {
        Self {
            index: None,
            built: None,
            running: None,
            waiting: None,
            latest: BTreeMap::new(),
            ticket: 0,
            lister: Arc::new(eitri_core::path_complete::list_paths),
        }
    }
}
impl PathMenu {
    pub(super) fn ask(&mut self, tab: TabId, view: ViewId, query: String, seq: u64) {
        if let Some(previous) = self.waiting.take() {
            self.latest.remove(&(previous.tab, previous.view));
        }
        self.ticket += 1;
        self.latest.insert((tab, view), self.ticket);
        self.waiting = Some(Request {
            tab,
            view,
            query,
            seq,
            ticket: self.ticket,
        });
    }
    fn start(&mut self, root: PathBuf, wake: Option<agent::UiWake>) {
        if self.running.is_some() {
            return;
        }
        let Some(request) = self.waiting.take() else { return };
        let cached = self
            .built
            .filter(|at| at.elapsed() < Duration::from_secs(5))
            .zip(self.index.clone());
        let lister = self.lister.clone();
        let (tx, rx) = mpsc::channel();
        let fallback = request.clone();
        let fallback_tx = tx.clone();
        let fallback_wake = wake.clone();
        let spawned = std::thread::Builder::new().name("path-complete".into()).spawn(move || {
            let (built, index) = cached.unwrap_or_else(|| {
                let names = if root.as_os_str().is_empty() {
                    Vec::new()
                } else {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| lister(&root))).unwrap_or_default()
                };
                (Instant::now(), Arc::new(names))
            });
            let items = eitri_core::path_complete::rank(&index, &request.query, 50);
            let _ = tx.send(Finished {
                request,
                index,
                built,
                items,
            });
            if let Some(wake) = wake {
                wake();
            }
        });
        if spawned.is_err() {
            let _ = fallback_tx.send(Finished {
                request: fallback,
                index: Arc::new(Vec::new()),
                built: Instant::now(),
                items: Vec::new(),
            });
            if let Some(wake) = fallback_wake {
                wake();
            }
        }
        self.running = Some(rx);
    }
}

pub(super) fn poll(state: &mut AgentPanelState, out: &mut Outbox) {
    let menu = &mut state.path_menu;
    if let Some(rx) = &menu.running {
        match rx.try_recv() {
            Ok(done) => {
                menu.running = None;
                menu.index = Some(done.index);
                menu.built = Some(done.built);
                let request = done.request;
                if menu.latest.get(&(request.tab, request.view)) == Some(&request.ticket) {
                    menu.latest.remove(&(request.tab, request.view));
                    out.send(
                        request.view,
                        eitri_core::agent_bridge::serialize_path_completions_for_js(
                            request.tab,
                            request.seq,
                            &done.items,
                        ),
                    );
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => menu.running = None,
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }
    menu.start(
        state.project_dir.clone(),
        state.wake.as_ref().map(crate::panel_wake::PanelWake::waker),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;

    fn ask(handle: &AgentPanelHandle, view: ViewId, tab: TabId, query: &str, seq: u64) {
        handle.page_message(
            view,
            &serde_json::json!({
                "type": "path_complete", "request_id": format!("p{seq}"), "tab": tab.0, "query": query, "seq": seq,
            })
            .to_string(),
        );
    }
    fn answers(page: &RecordingPage) -> Vec<serde_json::Value> {
        page.sent
            .borrow()
            .iter()
            .map(|p| json_of(p))
            .filter(|v| v["kind"] == "path_completions")
            .collect()
    }
    #[test]
    fn inbound_and_tick_return_project_names_only_to_the_requesting_page() {
        let root = agent::state_dirs::test_workspace_dir("path-menu-repo");
        std::fs::create_dir_all(&root).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()
            .unwrap()
            .success());
        std::fs::write(root.join(".gitignore"), "ignored\n").unwrap();
        std::fs::write(root.join("ignored"), "").unwrap();
        std::fs::write(root.join("small.txt"), "").unwrap();
        let (handle, base, second) = handle_with_two_pages();
        let tab = handle.state.borrow().tabs.active();
        handle.state.borrow_mut().project_dir = root.clone();
        handle.set_answer_gate(Rc::new(|_| panic!("completion answers no card")));
        ask(&handle, ViewId::BASE, tab, "sm", 1);
        assert_eq!(result_for(&base.sent.borrow(), "p1"), Some(Ok(())));
        tick_handle_until(&handle, "path names", || !answers(&base).is_empty());
        assert_eq!(answers(&base)[0]["items"], serde_json::json!(["small.txt"]));
        assert!(answers(&second).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn blocking_listing_leaves_the_handler_free_and_only_the_newest_waiting_query_runs() {
        let (handle, base, _) = handle_with_two_pages();
        let tab = handle.state.borrow().tabs.active();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = std::sync::Mutex::new(release_rx);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let mut state = handle.state.borrow_mut();
            state.project_dir = PathBuf::from("unused-by-stub");
            let calls = calls.clone();
            state.path_menu.lister = Arc::new(move |_| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                started_tx.send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(60))
                    .unwrap();
                vec!["alpha".into(), "beta".into(), "gamma".into()]
            });
        }
        // This returns while the lister still holds its result behind the channel.
        ask(&handle, ViewId::BASE, tab, "a", 1);
        started_rx.recv_timeout(Duration::from_secs(60)).unwrap();
        assert_eq!(result_for(&base.sent.borrow(), "p1"), Some(Ok(())));
        assert!(answers(&base).is_empty());
        ask(&handle, ViewId::BASE, tab, "b", 2);
        ask(&handle, ViewId::BASE, tab, "g", 3);
        assert!(
            !super::super::tick::needs_polling(&handle.state.borrow()),
            "the worker wakes the tick"
        );
        release_tx.send(()).unwrap();
        tick_handle_until(&handle, "the latest query", || {
            answers(&base).iter().any(|v| v["seq"] == 3)
        });
        assert_eq!(
            answers(&base),
            vec![serde_json::json!({
                "kind": "path_completions", "tab": tab.0, "seq": 3, "items": ["gamma"],
            })]
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the fresh index is reused"
        );
    }
    #[test]
    fn no_project_root_answers_empty_without_listing() {
        let (handle, base, _) = handle_with_two_pages();
        let tab = handle.state.borrow().tabs.active();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listed = calls.clone();
        handle.state.borrow_mut().path_menu.lister = Arc::new(move |_| {
            listed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Vec::new()
        });
        ask(&handle, ViewId::BASE, tab, "", 1);
        tick_handle_until(&handle, "empty completions", || !answers(&base).is_empty());
        assert_eq!(answers(&base)[0]["items"], serde_json::json!([]));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
    #[test]
    fn an_expired_index_is_rebuilt_and_results_are_limited_to_fifty() {
        let (handle, base, _) = handle_with_two_pages();
        let tab = handle.state.borrow().tabs.active();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let mut state = handle.state.borrow_mut();
            state.project_dir = PathBuf::from("unused-by-stub");
            let calls = calls.clone();
            state.path_menu.lister = Arc::new(move |_| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (0..100).map(|n| format!("file{n}")).collect()
            });
        }
        ask(&handle, ViewId::BASE, tab, "", 1);
        tick_handle_until(&handle, "first index", || !answers(&base).is_empty());
        assert_eq!(answers(&base)[0]["items"].as_array().unwrap().len(), 50);
        handle.state.borrow_mut().path_menu.built = Some(Instant::now() - Duration::from_secs(6));
        ask(&handle, ViewId::BASE, tab, "file99", 2);
        tick_handle_until(&handle, "rebuilt index", || answers(&base).len() == 2);
        assert_eq!(answers(&base)[1]["items"], serde_json::json!(["file99"]));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
