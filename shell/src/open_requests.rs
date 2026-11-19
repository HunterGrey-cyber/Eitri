//! The shell's two ends of the open socket (`eitri_core::open_socket`).
//!
//! **The launch.** `eitri FILE` asks before any GTK whether a running window will take the file, and exits if one
//! does ([`hand_over`]). A terminal's shell tells it which window to ask first ([`shell_env`]). `--raise` asks the same
//! way whether the window of the root will come forward ([`raise`]).
//!
//! **The window.** A window that owns its editor holds the socket for as long as it lives ([`OpenRequests`]). A
//! request reaches the GTK thread as one byte on a wake socket, becomes a `HostEvent::OpenFile` or
//! `HostEvent::RaiseWindow` with an id of its own, and waits here until the controller's `HostOp::OpenAnswer` of that
//! id says what the client is told. What became of an open itself comes later: the call goes to nvim and its answer
//! is looked for from a bounded timer ([`watch_open`]), because at a blocking prompt nvim answers nothing until the
//! prompt ends.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

use eitri_core::agent_backend::BackendKind;
use eitri_core::companion::env::OPEN_SOCKET_ENV;
use eitri_core::nvim_rpc::Pending;
use eitri_core::open_socket::{deliver, socket_path, Answer, Answering, Delivered, OpenServer, Raised, Request};
use eitri_core::project_root::Launch;
use eitri_core::remote_project::WindowOrigin;
use eitri_core::wm::{EditorEvent, HostEvent, HostSink, WinId};
use gtk4::glib;

/// How often an open's answer is looked for.
const LOOK_EVERY: Duration = Duration::from_millis(50);
/// How many looks an open gets: five seconds' worth.
const LOOKS: u32 = 100;
/// Why an open has no answer, for the line that says the file is not open yet.
const NO_ANSWER: &str = "nvim did not answer; it may be waiting at a prompt";

/// Where the sockets of windows of `origin` are: a remote project's window and a window of its copy share a root
/// and are still two projects, so each answers only for its own.
fn control_dir(origin: &WindowOrigin) -> PathBuf {
    eitri_core::remote_project::control_dir(origin)
}

/// The socket of the terminal this process was started in. For `main` to read before it scrubs the variable.
pub(crate) fn hint_from_env() -> Option<PathBuf> {
    std::env::var_os(OPEN_SOCKET_ENV)
        .filter(|socket| !socket.is_empty())
        .map(PathBuf::from)
}

/// The launcher's activation token, for the window that comes forward. GTK's library constructor copies both
/// variables and takes them out of the environment as the binary loads, before `main` runs, and keeps its copy
/// private. So they are read from the environment the process was started with, which the kernel keeps unchanged in
/// `/proc/self/environ`; the live environment is read only where that file cannot be.
pub(crate) fn token_from_env() -> Option<String> {
    match std::fs::read("/proc/self/environ") {
        Ok(block) => activation_token(&|name| initial_var(&block, name)),
        Err(_) => activation_token(&|name| std::env::var(name).ok()),
    }
}

/// The value of `name` in an environment block as `/proc/<pid>/environ` holds it: the first entry of that exact name,
/// and only as text.
fn initial_var(block: &[u8], name: &str) -> Option<String> {
    block
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(name.as_bytes())?.strip_prefix(b"="))
        .and_then(|value| String::from_utf8(value.to_vec()).ok())
}

fn activation_token(get: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"]
        .into_iter()
        .find_map(|name| get(name).filter(|token| !token.is_empty()))
}

/// Gives the launch's files to a running window of the same project and origin. `Some` is how this process ends, and
/// nothing of a window of its own has been started; `None` is a launch that goes on to its own window. A terminal's
/// hint names the window it was started in, whatever its origin: that window is the one asked first.
pub(crate) fn hand_over(
    launch: &Launch,
    origin: &WindowOrigin,
    clean: bool,
    backend: BackendKind,
    hint: Option<&Path>,
    token: Option<&str>,
) -> Option<glib::ExitCode> {
    hand_over_in(&control_dir(origin), launch, clean, backend, hint, token)
}

fn hand_over_in(
    dir: &Path,
    launch: &Launch,
    clean: bool,
    backend: BackendKind,
    hint: Option<&Path>,
    token: Option<&str>,
) -> Option<glib::ExitCode> {
    // `--clean` and the legacy backend ask for a window unlike the one running, and `--new-window` says so outright.
    if launch.open.is_empty() || launch.new_window || clean || backend == BackendKind::Legacy {
        return None;
    }
    match deliver(&launch.open, &launch.root, &launch.wider_roots, dir, hint, token) {
        Delivered::NotHere => None,
        Delivered::Opened { root } => {
            println!("{}", opened_line(root.as_deref()));
            Some(glib::ExitCode::SUCCESS)
        }
        // The window has the rest, so a new window would hold those files twice.
        Delivered::Partly { failed } => {
            for (path, why) in failed {
                eprintln!("eitri: {}: {why}", path.display());
            }
            Some(glib::ExitCode::FAILURE)
        }
    }
}

/// Asks the running window of the launch's root to come forward. `Some` is how this process ends, and nothing of a
/// window of its own has been started; `None` is a launch that goes on to be that window.
pub(crate) fn raise(
    launch: &Launch,
    origin: &WindowOrigin,
    clean: bool,
    backend: BackendKind,
    token: Option<&str>,
) -> Option<glib::ExitCode> {
    raise_in(&control_dir(origin), launch, clean, backend, token)
}

fn raise_in(
    dir: &Path,
    launch: &Launch,
    clean: bool,
    backend: BackendKind,
    token: Option<&str>,
) -> Option<glib::ExitCode> {
    // `--clean` and the legacy backend ask for a window unlike the one running, as for a file.
    if !launch.asks_for_a_raise() || clean || backend == BackendKind::Legacy {
        return None;
    }
    // The window moving is the answer, so nothing is printed.
    (eitri_core::open_socket::raise(&launch.root, dir, token) == Raised::Done).then_some(glib::ExitCode::SUCCESS)
}

/// What a launch says once a window took its files. It names a root only when that root's own socket was the one
/// asked: the window behind a terminal's socket can be any window, and a client is not told the root of the window
/// that answers.
fn opened_line(root: Option<&Path>) -> String {
    match root {
        Some(root) => format!("eitri: opened in the running window of {}", root.display()),
        None => "eitri: opened in the running window".to_owned(),
    }
}

/// What a terminal's shell is told: the socket this window holds, or the root's when it holds none, as window 2 does,
/// whose editor is window 1's. A window whose root's socket has no usable name tells it nothing.
pub(crate) fn shell_env(held: Option<&OpenRequests>, root: &Path, origin: &WindowOrigin) -> Vec<(OsString, OsString)> {
    shell_env_in(held.map(OpenRequests::socket), &control_dir(origin), root)
}

fn shell_env_in(held: Option<&Path>, dir: &Path, root: &Path) -> Vec<(OsString, OsString)> {
    let socket = match held {
        Some(socket) => Some(socket.to_owned()),
        None => socket_path(dir, root).ok(),
    };
    socket
        .map(|socket| (OsString::from(OPEN_SOCKET_ENV), socket.into_os_string()))
        .into_iter()
        .collect()
}

/// The window's end of the open socket: the socket itself and the requests the controller has not answered yet.
pub(crate) struct OpenRequests {
    server: OpenServer,
    sink: Weak<dyn HostSink>,
    /// Keyed by the id of the event each was posted as. Dropping one answers `busy`.
    waiting: RefCell<HashMap<u64, Answering>>,
    last_id: Rc<Cell<u64>>,
    /// A replacement host key gets its own address; existing terminal hints keep reaching this same window.
    aliases: RefCell<Vec<Rc<OpenRequests>>>,
    /// Removed before the server closes: the watch reads the server's descriptor.
    watch: Cell<Option<glib::SourceId>>,
}

impl OpenRequests {
    /// Claims the socket of `root`, which must be canonical. A failed claim is one log line: the window runs without
    /// reuse.
    pub(crate) fn claim(root: &Path, origin: &WindowOrigin, sink: Weak<dyn HostSink>) -> Option<Rc<OpenRequests>> {
        Self::claim_server(root, origin).map(|server| OpenRequests::new(server, sink))
    }

    /// Claims an origin's address on a worker when a connected window discovers a replacement host key.
    pub(crate) fn claim_server(root: &Path, origin: &WindowOrigin) -> Option<OpenServer> {
        let dir = control_dir(origin);
        // A remote origin's directory is made inside the shared one, which may not exist yet.
        if let Some(shared) = dir.parent().filter(|_| origin.key().is_some()) {
            if let Err(why) = eitri_core::panel_control::ensure_dir(shared) {
                eprintln!("[open] {why}; this window runs without reuse");
                return None;
            }
        }
        match OpenServer::claim(&dir, root) {
            Ok(server) => Some(server),
            Err(why) => {
                eprintln!("[open] {why}; this window runs without reuse");
                None
            }
        }
    }

    fn new(server: OpenServer, sink: Weak<dyn HostSink>) -> Rc<OpenRequests> {
        Rc::new(OpenRequests {
            server,
            sink,
            waiting: RefCell::default(),
            last_id: Rc::new(Cell::new(0)),
            aliases: RefCell::default(),
            watch: Cell::new(None),
        })
    }

    pub(crate) fn add_origin(&self, server: OpenServer) {
        self.keep_origin(server).watch();
    }

    fn keep_origin(&self, server: OpenServer) -> Rc<OpenRequests> {
        let alias = Rc::new(OpenRequests {
            server,
            sink: self.sink.clone(),
            waiting: RefCell::default(),
            last_id: self.last_id.clone(),
            aliases: RefCell::default(),
            watch: Cell::new(None),
        });
        self.aliases.borrow_mut().push(alias.clone());
        alias
    }

    pub(crate) fn socket(&self) -> &Path {
        self.server.socket()
    }

    /// Starts taking requests. One that came in before this has waited on the wake socket, and reads as a wake at once.
    pub(crate) fn watch(self: &Rc<Self>) {
        let requests = Rc::downgrade(self);
        let watch = neovide_editor::fd_watch::add_local(
            self.server.wake_fd(),
            glib::IOCondition::IN,
            glib::ffi::G_PRIORITY_DEFAULT,
            move |_| {
                if let Some(requests) = requests.upgrade() {
                    requests.on_wake();
                }
                glib::ControlFlow::Continue
            },
        );
        match watch {
            Ok(source) => self.watch.set(Some(source)),
            // A socket nobody reads would only make every client wait out its answer.
            Err(neovide_editor::fd_watch::NotOwner) => {
                eprintln!("[open] the socket could not be watched; this window runs without reuse");
                self.close();
            }
        }
    }

    /// Everything that waits, posted in order. Reads the wake bytes first, so a window whose driver has gone stops
    /// being woken, and what it held answers `busy`.
    fn on_wake(&self) {
        let incoming = self.server.drain();
        let Some(sink) = self.sink.upgrade() else {
            return;
        };
        for request in incoming {
            // Its client may have been told `busy`, or left, and may have a new window by now.
            let Some((request, answering)) = request.take() else {
                continue;
            };
            let id = self.last_id.get() + 1;
            self.last_id.set(id);
            // Before the post: the batch it starts can answer before `post` returns.
            self.waiting.borrow_mut().insert(id, answering);
            sink.post(match request {
                Request::Open(open) => HostEvent::OpenFile {
                    win: WinId::MAIN,
                    id,
                    path: open.path,
                    line: open.line,
                    col: open.col,
                    token: open.token,
                },
                Request::Raise { token } => HostEvent::RaiseWindow {
                    win: WinId::MAIN,
                    id,
                    token,
                },
            });
        }
    }

    /// What the controller decided about request `id`. One that is no longer here went with the socket's close, and
    /// its client has its answer.
    pub(crate) fn answer(&self, id: u64, accepted: bool) {
        let Some(answering) = self.waiting.borrow_mut().remove(&id) else {
            for alias in self.aliases.borrow().iter() {
                alias.answer(id, accepted);
            }
            return;
        };
        answering.answer(if accepted { Answer::Accepted } else { Answer::Busy });
    }

    /// Takes the socket away and answers `busy` to whatever is still unanswered. Idempotent.
    pub(crate) fn close(&self) {
        for alias in self.aliases.borrow_mut().drain(..) {
            alias.close();
        }
        if let Some(source) = self.watch.take() {
            source.remove();
        }
        self.server.close();
        self.waiting.borrow_mut().clear();
    }
}

impl Drop for OpenRequests {
    fn drop(&mut self) {
        self.close();
    }
}

/// Follows an open sent to nvim. An error, or no answer in time, becomes `EditorEvent::OpenFailed`; nvim may still run
/// the call, so that event never means the file will not open.
pub(crate) fn watch_open(pending: Pending, win: WinId, sink: Weak<dyn HostSink>) {
    let mut looks_left = LOOKS;
    glib::timeout_add_local(LOOK_EVERY, move || {
        let Some(outcome) = look(&pending, &mut looks_left) else {
            return glib::ControlFlow::Continue;
        };
        if let (Err(why), Some(sink)) = (outcome, sink.upgrade()) {
            sink.post(HostEvent::Editor(EditorEvent::OpenFailed { win, why }));
        }
        glib::ControlFlow::Break
    });
}

/// One look at an open's answer: `None` while one may still come.
fn look(pending: &Pending, looks_left: &mut u32) -> Option<Result<(), String>> {
    if let Some(reply) = pending.try_take() {
        return Some(reply.map(drop).map_err(|error| error.to_string()));
    }
    *looks_left = looks_left.saturating_sub(1);
    (*looks_left == 0).then(|| Err(NO_ANSWER.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::nvim_rpc::RpcError;
    use eitri_core::project_root::OpenTarget;
    use eitri_core::wm::conformance::RecordingSink;
    use eitri_core::wm::{CloseBy, CloseVerdict};
    use std::io::{BufRead, BufReader, Write};
    use std::os::fd::RawFd;
    use std::os::unix::net::UnixStream;
    use std::thread::JoinHandle;
    use std::time::Instant;

    /// How long any one step may take on a loaded machine; a test that passes never waits it out.
    const PATIENCE: Duration = Duration::from_secs(20);

    /// A control directory and a canonical root, with a short name: a socket path has a length limit.
    struct Scratch {
        top: PathBuf,
        dir: PathBuf,
        root: PathBuf,
    }

    impl Scratch {
        fn new() -> Scratch {
            let name = uuid::Uuid::new_v4().simple().to_string();
            let top = std::env::temp_dir().join(format!("eo-{}", &name[..10]));
            std::fs::create_dir(&top).unwrap();
            let top = top.canonicalize().unwrap();
            let root = top.join("root");
            std::fs::create_dir(&root).unwrap();
            Scratch {
                dir: top.join("run"),
                root,
                top,
            }
        }

        fn file(&self, name: &str) -> PathBuf {
            let path = self.root.join(name);
            std::fs::write(&path, "text\n").unwrap();
            path
        }

        fn requests(&self, sink: &Rc<RecordingSink>) -> Rc<OpenRequests> {
            let erased: Rc<dyn HostSink> = sink.clone();
            OpenRequests::new(
                OpenServer::claim(&self.dir, &self.root).unwrap(),
                Rc::downgrade(&erased),
            )
        }

        fn launch(&self, open: &[&Path]) -> Launch {
            Launch {
                ssh: None,
                local: false,
                root: self.root.clone(),
                wider_roots: Vec::new(),
                open: open
                    .iter()
                    .map(|path| OpenTarget {
                        path: path.to_path_buf(),
                        line: None,
                        col: None,
                    })
                    .collect(),
                new_window: false,
                raise: false,
                notes: Vec::new(),
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.top);
        }
    }

    /// One request on a new connection; the reply is what the thread's handle returns.
    fn ask(sock: &Path, path: &Path) -> JoinHandle<String> {
        let (sock, path) = (sock.to_owned(), path.to_owned());
        std::thread::spawn(move || {
            let stream = UnixStream::connect(sock).unwrap();
            stream.set_read_timeout(Some(PATIENCE)).unwrap();
            let mut reader = BufReader::new(&stream);
            let mut ready = String::new();
            let _ = reader.read_line(&mut ready);
            let line = serde_json::json!({ "v": 1, "op": "open", "path": path });
            (&stream).write_all(format!("{line}\n").as_bytes()).unwrap();
            let mut reply = String::new();
            let _ = reader.read_line(&mut reply);
            reply.trim_end().to_owned()
        })
    }

    /// How many wake bytes wait, read without taking them.
    fn wakes_waiting(fd: RawFd) -> usize {
        let mut bytes = [0u8; 8];
        // SAFETY: the buffer is valid for its length, and `MSG_PEEK` leaves the descriptor as it was.
        let read = unsafe {
            libc::recv(
                fd,
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        read.max(0) as usize
    }

    fn wait_until(what: &str, done: impl Fn() -> bool) {
        let give_up = Instant::now() + PATIENCE;
        while !done() {
            assert!(Instant::now() < give_up, "never happened: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn opens(sink: &RecordingSink) -> Vec<(u64, PathBuf)> {
        sink.events()
            .into_iter()
            .filter_map(|(_, event)| match event {
                HostEvent::OpenFile { id, path, .. } => Some((id, path)),
                _ => None,
            })
            .collect()
    }

    /// What the GTK side does for one request: the wake, then the post.
    fn receive_one(requests: &OpenRequests, sink: &RecordingSink) -> u64 {
        let before = opens(sink).len();
        wait_until("a wake", || wakes_waiting(requests.server.wake_fd()) > 0);
        requests.on_wake();
        let posted = opens(sink);
        assert_eq!(posted.len(), before + 1, "{posted:?}");
        posted[before].0
    }

    #[test]
    fn one_wake_drains_everything_and_a_request_that_cannot_be_taken_posts_nothing() {
        let scratch = Scratch::new();
        let sink = Rc::new(RecordingSink::default());
        let requests = scratch.requests(&sink);
        let (given_up, wanted) = (scratch.file("a"), scratch.file("b"));
        // Nothing drains, so the thread gives up on this one and goes on to the next.
        assert_eq!(ask(requests.socket(), &given_up).join().unwrap(), "busy");
        let client = ask(requests.socket(), &wanted);
        wait_until("both wakes", || wakes_waiting(requests.server.wake_fd()) == 2);
        requests.on_wake();
        assert_eq!(wakes_waiting(requests.server.wake_fd()), 0, "one wake reads every byte");
        let posted = opens(&sink);
        assert_eq!(posted.len(), 1, "{posted:?}");
        assert_eq!(posted[0].1, wanted);
        requests.answer(posted[0].0, true);
        assert_eq!(client.join().unwrap(), "ok");
    }

    #[test]
    fn a_requests_line_column_and_token_reach_the_event_for_the_main_window() {
        let scratch = Scratch::new();
        let sink = Rc::new(RecordingSink::default());
        let requests = scratch.requests(&sink);
        let file = scratch.file("a");
        let client = {
            let (sock, line) = (
                requests.socket().to_owned(),
                serde_json::json!({ "v": 1, "op": "open", "path": file, "line": 4, "col": 2, "token": "t" }),
            );
            std::thread::spawn(move || {
                let stream = UnixStream::connect(sock).unwrap();
                let mut reader = BufReader::new(&stream);
                let mut ready = String::new();
                let _ = reader.read_line(&mut ready);
                (&stream).write_all(format!("{line}\n").as_bytes()).unwrap();
                let mut reply = String::new();
                let _ = reader.read_line(&mut reply);
                reply
            })
        };
        let id = receive_one(&requests, &sink);
        assert_eq!(
            sink.events().last().map(|(_, event)| event.clone()),
            Some(HostEvent::OpenFile {
                win: WinId::MAIN,
                id,
                path: file,
                line: Some(4),
                col: Some(2),
                token: Some("t".to_owned()),
            })
        );
        requests.answer(id, true);
        assert_eq!(client.join().unwrap(), "ok\n");
    }

    #[test]
    fn ids_are_unique_and_an_answered_id_is_forgotten() {
        let scratch = Scratch::new();
        let sink = Rc::new(RecordingSink::default());
        let requests = scratch.requests(&sink);
        let first = ask(requests.socket(), &scratch.file("a"));
        let one = receive_one(&requests, &sink);
        requests.answer(one, true);
        assert_eq!(first.join().unwrap(), "ok");
        assert!(requests.waiting.borrow().is_empty(), "an answered id is forgotten");

        let second = ask(requests.socket(), &scratch.file("b"));
        let two = receive_one(&requests, &sink);
        assert_ne!(one, two);
        // An id already answered cannot touch the request that waits now.
        requests.answer(one, false);
        assert_eq!(requests.waiting.borrow().len(), 1);
        requests.answer(two, false);
        assert_eq!(
            second.join().unwrap(),
            "busy",
            "`false` sends the client to a new window"
        );
    }

    /// The controller runs inside `post` when no batch is active, so its answer comes before `post` returns.
    #[test]
    fn an_answer_given_during_the_post_finds_its_request() {
        #[derive(Default)]
        struct AnswersAtOnce(RefCell<Weak<OpenRequests>>);
        impl HostSink for AnswersAtOnce {
            fn post(&self, event: HostEvent) {
                let HostEvent::OpenFile { id, .. } = event else {
                    return;
                };
                let requests = self.0.borrow().upgrade();
                requests.expect("the requests are alive").answer(id, true);
            }
            fn post_deferred(&self, _: HostEvent) {}
            fn close_requested(&self, _: WinId, _: CloseBy) -> CloseVerdict {
                unreachable!("nothing closes here")
            }
        }
        let scratch = Scratch::new();
        let sink = Rc::new(AnswersAtOnce::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        let requests = OpenRequests::new(
            OpenServer::claim(&scratch.dir, &scratch.root).unwrap(),
            Rc::downgrade(&erased),
        );
        *sink.0.borrow_mut() = Rc::downgrade(&requests);
        let client = ask(requests.socket(), &scratch.file("a"));
        wait_until("a wake", || wakes_waiting(requests.server.wake_fd()) > 0);
        requests.on_wake();
        assert_eq!(client.join().unwrap(), "ok");
        assert!(requests.waiting.borrow().is_empty());
    }

    #[test]
    fn an_unanswered_id_at_teardown_answers_busy_and_the_socket_goes() {
        let scratch = Scratch::new();
        let sink = Rc::new(RecordingSink::default());
        let requests = scratch.requests(&sink);
        let socket = requests.socket().to_owned();
        let client = ask(&socket, &scratch.file("a"));
        let id = receive_one(&requests, &sink);
        requests.close();
        assert_eq!(client.join().unwrap(), "busy");
        assert!(!socket.exists(), "the socket closes with the window");
        requests.answer(id, true);
        requests.close();
    }

    #[test]
    fn with_the_driver_gone_a_request_answers_busy_and_stops_waking() {
        let scratch = Scratch::new();
        let sink = Rc::new(RecordingSink::default());
        let requests = scratch.requests(&sink);
        drop(sink);
        let client = ask(requests.socket(), &scratch.file("a"));
        wait_until("a wake", || wakes_waiting(requests.server.wake_fd()) > 0);
        requests.on_wake();
        assert_eq!(client.join().unwrap(), "busy");
        assert_eq!(wakes_waiting(requests.server.wake_fd()), 0);
    }

    #[test]
    fn a_shell_is_told_the_held_socket_or_else_the_roots() {
        let scratch = Scratch::new();
        let held = Path::new("/run/user/1/eitri/open-0123456789abcdef-42.sock");
        assert_eq!(
            shell_env_in(Some(held), &scratch.dir, &scratch.root),
            [(OsString::from("EITRI_OPEN_SOCKET"), held.as_os_str().to_owned())]
        );
        assert_eq!(
            shell_env_in(None, &scratch.dir, &scratch.root),
            [(
                OsString::from("EITRI_OPEN_SOCKET"),
                socket_path(&scratch.dir, &scratch.root).unwrap().into_os_string()
            )]
        );
        let too_long = scratch.top.join("x".repeat(200));
        assert!(
            shell_env_in(None, &too_long, &scratch.root).is_empty(),
            "a path no socket can have is not handed on"
        );
    }

    /// Set in the copy of this test binary the test below starts; unset, the test starts that copy.
    const TOKEN_CHILD: &str = "EITRI_TEST_LAUNCH_TOKEN_CHILD";

    #[test]
    fn the_launch_token_is_read_although_gtk_takes_it_out_of_the_environment_as_the_binary_loads() {
        if std::env::var_os(TOKEN_CHILD).is_some() {
            // This binary links GTK, whose library constructor has run by now, as in a window's process.
            assert_eq!(
                std::env::var_os("XDG_ACTIVATION_TOKEN"),
                None,
                "GTK took the variable as the binary loaded"
            );
            assert_eq!(token_from_env().as_deref(), Some("launch-token"));
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "open_requests::tests::the_launch_token_is_read_although_gtk_takes_it_out_of_the_environment_as_the_binary_loads",
                "--test-threads=1",
            ])
            .env(TOKEN_CHILD, "1")
            .env("XDG_ACTIVATION_TOKEN", "launch-token")
            .env_remove("DESKTOP_STARTUP_ID")
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(
            status.success(),
            "the started copy could not read the token it was started with"
        );
    }

    #[test]
    fn the_initial_environment_is_read_by_exact_name_first_entry_first() {
        let block = b"XDG_ACTIVATION_TOKEN_X=no\0XDG_ACTIVATION_TOKEN=yes\0XDG_ACTIVATION_TOKEN=later\0EMPTY=\0";
        assert_eq!(initial_var(block, "XDG_ACTIVATION_TOKEN").as_deref(), Some("yes"));
        assert_eq!(initial_var(block, "EMPTY").as_deref(), Some(""));
        assert_eq!(initial_var(block, "DESKTOP_STARTUP_ID"), None);
        assert_eq!(initial_var(b"BAD=\xff\0", "BAD"), None, "a token is text");
    }

    #[test]
    fn the_activation_token_is_xdgs_else_the_startup_ids_and_never_empty() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        assert_eq!(
            activation_token(&env(&[("XDG_ACTIVATION_TOKEN", "x"), ("DESKTOP_STARTUP_ID", "d")])).as_deref(),
            Some("x")
        );
        assert_eq!(
            activation_token(&env(&[("XDG_ACTIVATION_TOKEN", ""), ("DESKTOP_STARTUP_ID", "d")])).as_deref(),
            Some("d")
        );
        assert_eq!(activation_token(&env(&[("DESKTOP_STARTUP_ID", "")])), None);
        assert_eq!(activation_token(&env(&[])), None);
    }

    /// Runs `hand_over_in` off the thread that plays the window, which answers each request in turn.
    fn launched(
        scratch: &Scratch,
        launch: Launch,
        requests: &OpenRequests,
        sink: &RecordingSink,
        answers: &[bool],
    ) -> Option<glib::ExitCode> {
        let dir = scratch.dir.clone();
        let client =
            std::thread::spawn(move || hand_over_in(&dir, &launch, false, BackendKind::Sidecar, None, Some("token")));
        for accepted in answers {
            let id = receive_one(requests, sink);
            requests.answer(id, *accepted);
        }
        client.join().unwrap()
    }

    #[test]
    fn a_launch_a_window_took_ends_with_success() {
        let scratch = Scratch::new();
        let sink = Rc::new(RecordingSink::default());
        let requests = scratch.requests(&sink);
        let file = scratch.file("a");
        let launch = scratch.launch(&[&file]);
        assert_eq!(
            launched(&scratch, launch, &requests, &sink, &[true]),
            Some(glib::ExitCode::SUCCESS)
        );
        assert_eq!(opens(&sink)[0].1, file);
    }

    /// The last target goes first, so the one that decides is the last on the command line; once a window has it, a
    /// new window would hold the files twice.
    #[test]
    fn a_later_targets_refusal_ends_with_failure_and_no_new_window() {
        let scratch = Scratch::new();
        let sink = Rc::new(RecordingSink::default());
        let requests = scratch.requests(&sink);
        let (shown, last) = (scratch.file("a"), scratch.file("b"));
        let launch = scratch.launch(&[&shown, &last]);
        assert_eq!(
            launched(&scratch, launch, &requests, &sink, &[true, false]),
            Some(glib::ExitCode::FAILURE)
        );
        let order: Vec<PathBuf> = opens(&sink).into_iter().map(|(_, path)| path).collect();
        assert_eq!(order, [last, shown], "last to first, so the first ends up shown");
    }

    #[test]
    fn the_opened_line_names_a_root_only_when_its_own_socket_was_the_one_asked() {
        assert_eq!(
            opened_line(Some(Path::new("/w/repo"))),
            "eitri: opened in the running window of /w/repo"
        );
        assert_eq!(
            opened_line(None),
            "eitri: opened in the running window",
            "the terminal's window can be any window"
        );
    }

    /// `cd repo/crate; eitri src/x.rs` with a window on `repo`: the launch goes no further.
    #[test]
    fn a_launch_a_window_on_a_wider_root_took_ends_with_success() {
        let scratch = Scratch::new();
        let sink = Rc::new(RecordingSink::default());
        let requests = scratch.requests(&sink);
        let narrow = scratch.root.join("crate");
        std::fs::create_dir(&narrow).unwrap();
        let file = narrow.join("x.rs");
        std::fs::write(&file, "text").unwrap();
        let launch = Launch {
            wider_roots: vec![scratch.root.clone()],
            root: narrow,
            ..scratch.launch(&[&file])
        };
        assert_eq!(
            launched(&scratch, launch, &requests, &sink, &[true]),
            Some(glib::ExitCode::SUCCESS)
        );
        assert_eq!(opens(&sink)[0].1, file);
    }

    #[test]
    fn a_launch_the_window_turned_down_goes_on_to_its_own_window() {
        let scratch = Scratch::new();
        let sink = Rc::new(RecordingSink::default());
        let requests = scratch.requests(&sink);
        let file = scratch.file("a");
        let launch = scratch.launch(&[&file]);
        assert_eq!(launched(&scratch, launch, &requests, &sink, &[false]), None);
    }

    #[test]
    fn with_no_window_a_launch_goes_on_to_its_own() {
        let scratch = Scratch::new();
        let file = scratch.file("a");
        let launch = scratch.launch(&[&file]);
        let handed = hand_over_in(&scratch.dir, &launch, false, BackendKind::Sidecar, None, None);
        assert_eq!(handed, None);
    }

    #[test]
    fn only_a_plain_launch_with_files_asks_a_window() {
        let scratch = Scratch::new();
        let sink = Rc::new(RecordingSink::default());
        let requests = scratch.requests(&sink);
        let file = scratch.file("a");
        let plain = scratch.launch(&[&file]);
        let cases = [
            (scratch.launch(&[]), false, BackendKind::Sidecar),
            (
                Launch {
                    new_window: true,
                    ..plain.clone()
                },
                false,
                BackendKind::Sidecar,
            ),
            (plain.clone(), true, BackendKind::Sidecar),
            (plain, false, BackendKind::Legacy),
        ];
        for (launch, clean, backend) in cases {
            assert_eq!(hand_over_in(&scratch.dir, &launch, clean, backend, None, None), None);
        }
        assert_eq!(wakes_waiting(requests.server.wake_fd()), 0, "none of them asked");
    }

    fn raises(sink: &RecordingSink) -> Vec<(u64, Option<String>)> {
        sink.events()
            .into_iter()
            .filter_map(|(_, event)| match event {
                HostEvent::RaiseWindow { id, token, .. } => Some((id, token)),
                _ => None,
            })
            .collect()
    }

    /// `raise_in` off the thread that plays the window, which waits for the request and answers it.
    fn raised(
        scratch: &Scratch,
        requests: &OpenRequests,
        sink: &RecordingSink,
        accepted: bool,
    ) -> Option<glib::ExitCode> {
        let dir = scratch.dir.clone();
        let launch = Launch {
            raise: true,
            ..scratch.launch(&[])
        };
        let client = std::thread::spawn(move || raise_in(&dir, &launch, false, BackendKind::Sidecar, Some("token")));
        wait_until("a wake", || wakes_waiting(requests.server.wake_fd()) > 0);
        requests.on_wake();
        let (id, token) = raises(sink).pop().expect("the raise was posted");
        assert_eq!(token.as_deref(), Some("token"));
        requests.answer(id, accepted);
        client.join().unwrap()
    }

    #[test]
    fn a_raise_the_window_took_ends_with_success() {
        let scratch = Scratch::new();
        let sink = Rc::new(RecordingSink::default());
        let requests = scratch.requests(&sink);
        assert_eq!(raised(&scratch, &requests, &sink, true), Some(glib::ExitCode::SUCCESS));
        assert!(opens(&sink).is_empty(), "a raise opens nothing");
        assert!(requests.waiting.borrow().is_empty(), "an answered id is forgotten");
    }

    #[test]
    fn a_raise_the_window_turned_down_goes_on_to_its_own_window() {
        let scratch = Scratch::new();
        let sink = Rc::new(RecordingSink::default());
        let requests = scratch.requests(&sink);
        assert_eq!(raised(&scratch, &requests, &sink, false), None);
    }

    #[test]
    fn with_no_window_a_raise_goes_on_to_its_own() {
        let scratch = Scratch::new();
        let launch = Launch {
            raise: true,
            ..scratch.launch(&[])
        };
        assert_eq!(raise_in(&scratch.dir, &launch, false, BackendKind::Sidecar, None), None);
    }

    #[test]
    fn only_a_plain_raise_with_no_file_asks_a_window() {
        let scratch = Scratch::new();
        let sink = Rc::new(RecordingSink::default());
        let requests = scratch.requests(&sink);
        let file = scratch.file("a");
        let plain = Launch {
            raise: true,
            ..scratch.launch(&[])
        };
        let cases = [
            (scratch.launch(&[]), false, BackendKind::Sidecar),
            (
                Launch {
                    raise: true,
                    ..scratch.launch(&[&file])
                },
                false,
                BackendKind::Sidecar,
            ),
            (
                Launch {
                    new_window: true,
                    ..plain.clone()
                },
                false,
                BackendKind::Sidecar,
            ),
            (plain.clone(), true, BackendKind::Sidecar),
            (plain, false, BackendKind::Legacy),
        ];
        for (launch, clean, backend) in cases {
            assert_eq!(raise_in(&scratch.dir, &launch, clean, backend, None), None);
        }
        assert_eq!(wakes_waiting(requests.server.wake_fd()), 0, "none of them asked");
    }

    #[test]
    fn the_looks_cover_five_seconds() {
        assert_eq!(LOOK_EVERY * LOOKS, Duration::from_secs(5));
    }

    #[test]
    fn an_answer_ends_the_looking_and_only_an_error_is_a_failure() {
        let (answer, pending) = Pending::pair();
        answer.send(Ok(rmpv::Value::Nil));
        assert_eq!(look(&pending, &mut 3), Some(Ok(())));

        let (answer, pending) = Pending::pair();
        answer.send(Err(RpcError::Nvim("E5108: no such path".to_owned())));
        assert_eq!(look(&pending, &mut 3), Some(Err("E5108: no such path".to_owned())));

        let refused = Pending::failed(RpcError::Unavailable("the editor is closed".to_owned()));
        assert_eq!(
            look(&refused, &mut 3),
            Some(Err("no editor to ask: the editor is closed".to_owned()))
        );
    }

    #[test]
    fn no_answer_runs_out_after_the_last_look() {
        let (_answer, pending) = Pending::pair();
        let mut left = 2;
        assert_eq!(look(&pending, &mut left), None);
        assert_eq!(look(&pending, &mut left), Some(Err(NO_ANSWER.to_owned())));
    }
    #[test]
    fn a_replacement_origin_and_existing_terminal_hint_reach_the_same_window() {
        let scratch = Scratch::new();
        let sink = Rc::new(RecordingSink::default());
        let requests = scratch.requests(&sink);
        let new_dir = scratch.top.join("new-key");
        let new_server = OpenServer::claim(&new_dir, &scratch.root).unwrap();
        let new_address = new_server.socket().to_owned();
        let alias = requests.keep_origin(new_server);
        let old_file = scratch.file("old-terminal");
        let new_file = scratch.file("outside-launch");
        let old_client = ask(requests.socket(), &old_file);
        let old_id = receive_one(&requests, &sink);
        let new_client = ask(&new_address, &new_file);
        let new_id = receive_one(&alias, &sink);
        assert_ne!(old_id, new_id);
        requests.answer(old_id, true);
        requests.answer(new_id, true);
        assert_eq!(old_client.join().unwrap(), "ok");
        assert_eq!(new_client.join().unwrap(), "ok");
        assert_eq!(
            opens(&sink).iter().map(|(_, path)| path).collect::<Vec<_>>(),
            [&old_file, &new_file]
        );
        requests.close();
        assert!(!new_address.exists());
    }
}
