use super::*;
use crate::linked_window::driver::tests::pump_until;
use crate::linked_window::spawn::{welcome, Pair, PairHooks};
use crate::linked_window::tests::Scratch;
use crate::linked_window::LinkEnd;
use eitri_core::compositor::mode::{resolve_mode, Mode, ModeFacts, TilingChoice, Why};
use eitri_core::compositor::Wm;
use eitri_core::window_link::pair::Role;
use eitri_core::window_link::EdgeKind;
use std::cell::{Cell, RefCell};
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;
use std::sync::mpsc;

/// The project every test here pairs for. Its key, not the path, names the files.
fn root() -> PathBuf {
    PathBuf::from("/srv/relink-project")
}

/// A control directory of the test's own, short enough for a socket path, removed when the test ends.
struct RunDir {
    top: PathBuf,
    dir: PathBuf,
}

impl RunDir {
    fn new() -> RunDir {
        let short = uuid::Uuid::new_v4().simple().to_string();
        let top = std::env::temp_dir().join(format!("eitri-rl-{}", &short[..10]));
        std::fs::create_dir_all(&top).unwrap();
        RunDir {
            dir: top.join("run"),
            top,
        }
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.top);
    }
}

fn is_socket(path: &Path) -> bool {
    path.symlink_metadata().is_ok_and(|meta| meta.file_type().is_socket())
}

/// What a stand-in orphan on a thread of its own saw.
#[derive(Debug, Default)]
struct OrphanSaw {
    accepted: usize,
    linked: bool,
    later: usize,
}

/// A stand-in orphan: answers the first connection to `socket` with `answer_relink_hello`, then sends each message
/// it is given until told to stop, and counts any connection after the first.
fn orphan(
    socket: OrphanSocket,
    lock: PairLock,
    build: String,
    welcome: Msg,
    then: mpsc::Receiver<Option<Msg>>,
) -> std::thread::JoinHandle<OrphanSaw> {
    std::thread::spawn(move || {
        let mut saw = OrphanSaw::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        let conn = loop {
            if let Some(conn) = socket.accept().unwrap() {
                break conn;
            }
            assert!(Instant::now() < deadline, "nobody connected");
            std::thread::sleep(Duration::from_millis(5));
        };
        saw.accepted += 1;
        let peer = peer_of(&conn);
        let (handle, inbound) = LinkHandle::start(conn, PresenceFlag::default()).unwrap();
        let link = LinkDriver::new(handle, inbound);
        let answered = Rc::new(Cell::new(false));
        {
            let answered = answered.clone();
            let welcome = RefCell::new(Some(welcome));
            link.on("hello", move |link, msg, _| {
                let welcome = welcome.borrow_mut().take().expect("one hello");
                let _ = answer_relink_hello(link, &msg, &build, peer, &lock, welcome);
                answered.set(true);
            });
        }
        pump_until(&link, || answered.get());
        saw.linked = link.linked();
        while let Ok(Some(msg)) = then.recv_timeout(Duration::from_secs(5)) {
            link.send(msg, Vec::new()).unwrap();
        }
        while let Ok(Some(_)) = socket.accept() {
            saw.later += 1;
        }
        saw
    })
}

/// A window 2's own lock on a pair, with the layout directory it lives in.
fn held_pair(label: &str) -> (Scratch, PairLock) {
    let layout = Scratch::new(label);
    let lock = PairLock::try_take(&layout.0, &root()).unwrap().unwrap();
    (layout, lock)
}

/// [`relink_at_start`], tried again while it finds the group claimed. The claim belongs to its open file description,
/// so a process another test forks while it is up keeps it, after the window that held it has ended, until that
/// process execs; nothing else in this test's private directory takes it.
fn start_past_a_forks_copy_of_the_claim(
    dir: &Path,
    layout_dir: &Path,
    build: String,
    within: Duration,
) -> (StartRelink, Option<String>) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let started = relink_at_start(dir, layout_dir, &root(), build.clone(), within);
        if !matches!(started.0, StartRelink::Nothing) || Instant::now() >= deadline {
            return started;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// `OrphanSocket::bind`, tried again while the reclaim lock reads as held. The lock belongs to its open file
/// description, so a process another test forks while the lock is up keeps it, after the claim that took it has
/// ended, until that process execs; nothing else in this test's private directory takes it.
fn bind_past_a_forks_copy_of_the_lock(dir: &Path, project_root: &Path) -> Result<OrphanSocket, String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match OrphanSocket::bind(dir, project_root) {
            Err(why) if why.contains("held by another claim") && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1));
            }
            bound => return bound,
        }
    }
}

/// A seqpacket socket bound at `path` that never listens: a connect to it is refused, as to a leftover's.
fn bound_only(path: &Path) -> OwnedFd {
    let (addr, len) = address(path).unwrap();
    let fd = seqpacket().unwrap();
    // SAFETY: `addr` is a valid `sockaddr_un` of `len` bytes for the call.
    let bound = unsafe { libc::bind(fd.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len) };
    assert_eq!(bound, 0, "{}", io::Error::last_os_error());
    fd
}

#[test]
fn an_orphaned_window_2_binds_its_socket_with_panel_controls_posture() {
    let run = RunDir::new();
    let socket = OrphanSocket::bind(&run.dir, &root()).unwrap();
    let path = socket_path(&run.dir, &root()).unwrap();
    assert_eq!(socket.path(), path);
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with("w-") && name.ends_with(".sock") && name.len() == "w-.sock".len() + 16,
        "{name}"
    );
    let dir_mode = std::fs::metadata(&run.dir).unwrap().permissions().mode() & 0o7777;
    assert_eq!(dir_mode, 0o700, "the directory is made private");
    let sock_mode = path.symlink_metadata().unwrap().permissions().mode() & 0o7777;
    assert_eq!(sock_mode, 0o600);
    assert!(fdpass::is_cloexec(socket.listener.as_fd()).unwrap());

    // A connection from this user is accepted, close-on-exec, and is a seqpacket socket as the link is.
    let client = connect_to(&path).unwrap();
    let conn = socket.accept().unwrap().expect("the connection waits");
    assert!(fdpass::is_cloexec(conn.as_fd()).unwrap());
    assert_eq!(fdpass::socket_type(conn.as_fd()).unwrap(), libc::SOCK_SEQPACKET);
    assert!(socket.accept().unwrap().is_none(), "nothing else waits");
    drop((client, conn));

    // A listener that is up keeps its socket: a second bind is refused and removes nothing.
    let err = bind_past_a_forks_copy_of_the_lock(&run.dir, &root()).unwrap_err();
    assert!(err.contains("in use"), "{err}");
    assert!(is_socket(&path));

    // A socket nothing listens on is a leftover, and is taken over. It is made bound but never listening: a closed
    // listener would still answer while a process another test forks holds its descriptor, until that exec.
    let leftover = bound_only(&path.with_extension("old"));
    drop(socket);
    assert!(!path.exists(), "a dropped socket removes its own file");
    std::fs::rename(path.with_extension("old"), &path).unwrap();
    drop(leftover);
    let socket = bind_past_a_forks_copy_of_the_lock(&run.dir, &root()).expect("the stale socket is reclaimed");
    assert!(connect_to(&path).is_ok());
    drop(socket);

    // What is not this user's socket is never touched.
    std::fs::write(&path, b"not a socket").unwrap();
    let err = bind_past_a_forks_copy_of_the_lock(&run.dir, &root()).unwrap_err();
    assert!(err.contains("not this user's socket"), "{err}");
    assert_eq!(std::fs::read(&path).unwrap(), b"not a socket");
    std::fs::remove_file(&path).unwrap();

    // A directory others can write to is refused, never changed.
    std::fs::set_permissions(&run.dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let err = OrphanSocket::bind(&run.dir, &root()).unwrap_err();
    assert!(err.contains("not a private directory"), "{err}");
    let mode = std::fs::metadata(&run.dir).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o777);
}

/// Window 2's pair over a socketpair whose other end the test plays as window 1, linked by a real `welcome` with
/// the pair lock, its hooks those of a window: the end of the link starts the orphan's listener.
struct LinkedSecond {
    pair: Rc<Pair>,
    window1: Option<LinkHandle>,
    ends: Rc<RefCell<Vec<LinkEnd>>>,
    waited: Rc<RefCell<Vec<Result<(), String>>>>,
    linked: Rc<Cell<u32>>,
    /// The name each link made after the hooks were set was announced under.
    linked_as: Rc<RefCell<Vec<WindowName>>>,
}

fn linked_second(lock: &PairLock, run: &RunDir) -> LinkedSecond {
    let (one, two) = fdpass::seqpacket_pair().unwrap();
    linked_by_hub(Pair::second(two, &root()).unwrap(), one, lock, run)
}

/// As [`linked_second`], for the module window `name`.
fn linked_module(name: &WindowName, lock: &PairLock, run: &RunDir) -> LinkedSecond {
    let (one, two) = fdpass::seqpacket_pair().unwrap();
    linked_by_hub(Pair::module(two, name.clone(), &root()).unwrap(), one, lock, run)
}

fn linked_by_hub(pair: Rc<Pair>, one: OwnedFd, lock: &PairLock, run: &RunDir) -> LinkedSecond {
    let (window1, window1_in) = LinkHandle::start(one, PresenceFlag::default()).unwrap();
    match window1_in.recv_timeout(Duration::from_secs(5)).unwrap() {
        Inbound::Msg {
            msg: Msg::Hello { .. }, ..
        } => {}
        other => panic!("window 2 said {other:?} first"),
    }
    window1
        .send(welcome(BTreeMap::new(), Vec::new()), vec![lock.share().unwrap()])
        .unwrap();
    let link = pair.link().unwrap();
    assert!(pump_until(&link, || link.linked()));
    let ends: Rc<RefCell<Vec<LinkEnd>>> = Rc::default();
    let waited: Rc<RefCell<Vec<Result<(), String>>>> = Rc::default();
    let linked: Rc<Cell<u32>> = Rc::default();
    let ended: crate::linked_window::spawn::OnEnded = {
        let (ends, waited) = (ends.clone(), waited.clone());
        let pair = Rc::downgrade(&pair);
        let dir = run.dir.clone();
        Rc::new(move |_, _, _, end| {
            ends.borrow_mut().push(end.clone());
            let pair = pair.upgrade().unwrap();
            if pair.waits_for_relink_after(end) {
                let held: HeldNow = Rc::new(|| ["agent", "terminal#2", "lua:notes"].map(String::from).into());
                waited.borrow_mut().push(pair.wait_for_relink(&dir, held));
            }
        })
    };
    let linked_as: Rc<RefCell<Vec<WindowName>>> = Rc::default();
    let on_linked: crate::linked_window::spawn::OnLinked = {
        let (linked, linked_as) = (linked.clone(), linked_as.clone());
        Rc::new(move |name, _, _| {
            linked.set(linked.get() + 1);
            linked_as.borrow_mut().push(name.clone());
        })
    };
    pair.set_hooks(PairHooks {
        linked: on_linked,
        post: Rc::new(|_| {}),
        ended,
    });
    LinkedSecond {
        pair,
        window1: Some(window1),
        ends,
        waited,
        linked,
        linked_as,
    }
}

#[test]
fn a_bye_and_an_end_of_file_both_start_the_orphan_listener() {
    for bye in [true, false] {
        let (layout, lock) = held_pair("orphan-ends");
        let run = RunDir::new();
        let mut second = linked_second(&lock, &run);
        let window1 = second.window1.take().unwrap();
        if bye {
            window1.close("window 1 closed");
        } else {
            drop(window1);
        }
        let link = second.pair.link().unwrap();
        assert!(pump_until(&link, || link.ended().is_some()));
        let want = if bye {
            LinkEnd::Bye {
                why: "window 1 closed".into(),
            }
        } else {
            LinkEnd::Eof
        };
        assert_eq!(*second.ends.borrow(), vec![want]);
        assert_eq!(*second.waited.borrow(), vec![Ok(())]);
        let path = socket_path(&run.dir, &root()).unwrap();
        assert!(is_socket(&path), "the orphan listens once its link ended (bye: {bye})");
        drop(lock);

        // A window 1 starting now links to it: the lock comes with the welcome, the link is window 2's own from
        // then on, its linked hook runs, and the socket goes.
        let (dir, layout_dir) = (run.dir.clone(), layout.0.clone());
        let new_window = std::thread::spawn(move || {
            relink(
                &dir,
                &layout_dir,
                &root(),
                crate::version::fingerprint(),
                Duration::from_secs(5),
            )
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !new_window.is_finished() {
            assert!(Instant::now() < deadline, "the re-link did not finish");
            second.pair.accept_relink();
            if let Some(candidate) = second.pair.relink_candidate() {
                candidate.pump();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let relinked = new_window.join().unwrap().expect("the new window 1 is linked");
        assert_eq!(
            relinked.holders,
            BTreeMap::from([
                ("lua:notes".to_string(), "2".to_string()),
                ("terminal#2".to_string(), "2".to_string()),
            ]),
            "the welcome names what only one window can hold"
        );
        let link = second.pair.link().unwrap();
        assert!(link.linked() && link.is_up());
        assert_eq!(link.partner().map(|p| p.pid), Some(std::process::id()));
        assert_eq!(second.linked.get(), 1, "the window hears of its new link once");
        assert!(!path.exists(), "the socket goes once the link is made");
        assert!(
            PairLock::try_take(&layout.0, &root()).unwrap().is_none(),
            "the pair stays held"
        );
    }
}

#[test]
fn a_relinked_window_1_takes_the_orphans_pages_as_away() {
    use crate::linked_window::pair_files::{apply_main_restore, relink_restore};
    use crate::linked_window::tests::{id, window};
    use eitri_core::layout::workspace::Workspaces;
    use eitri_core::layout::OpenRules;
    use eitri_core::window_link::pair::{PairFile, ReadPair};

    // The orphan rebuilt window 1's plugin page and Lua panel when window 1 ended, and holds a terminal of its own.
    let holders = BTreeMap::from([
        ("lua:markdown".to_string(), "2".to_string()),
        ("lua:notes".to_string(), "2".to_string()),
        ("terminal#3".to_string(), "2".to_string()),
        // Every window has its own chat; and what is not a module, or names another window, is left out.
        ("agent".to_string(), "2".to_string()),
        ("terminal#2".to_string(), "1".to_string()),
        ("no such\u{1b}[2J module".to_string(), "2".to_string()),
    ]);
    let file = PairFile {
        window2_open: true,
        holders: BTreeMap::from([("terminal#3".to_string(), "2".to_string())]),
        ..Default::default()
    };
    let plan = relink_restore(&ReadPair::Read(file), &holders);
    assert_eq!(
        plan.away,
        [id("lua:markdown"), id("lua:notes"), id("terminal#3")].into()
    );
    assert!(!plan.restore_second, "window 2 is up; nothing starts it again");
    assert!(!plan.take_back, "nothing of window 2 comes here");
    assert!(plan.start.window2_open);
    assert_eq!(
        plan.start.holders.keys().cloned().collect::<Vec<_>>(),
        vec!["lua:markdown", "lua:notes", "terminal#3"]
    );
    assert!(plan.write_now, "the file now names what the orphan holds");
    assert!(
        plan.notes.iter().any(|note| note.contains("\\u{1b}")),
        "a holder that is not a module is noted with its control characters escaped: {:?}",
        plan.notes
    );

    let mut layout = window(&["lua:markdown", "lua:notes", "terminal#2"]);
    let mut workspaces = Workspaces::single();
    let notes = apply_main_restore(&plan, &mut layout, &mut workspaces, &OpenRules::default(), &[], &|_| {
        true
    });
    for page in ["lua:markdown", "lua:notes"] {
        assert!(layout.is_away(&id(page)), "{page} is held away, not built: {notes:?}");
    }
    assert!(
        !layout.is_away(&id("terminal#2")),
        "window 1's own terminal stays its own"
    );
    assert!(
        !layout.contains(&id("terminal#3")),
        "a module window 1 never had is not placed here"
    );

    // A list longer than any window sends is cut at its bound.
    let flood: BTreeMap<String, String> = (2..2000).map(|n| (format!("lua:page{n}"), "2".to_string())).collect();
    let plan = relink_restore(&ReadPair::Missing, &flood);
    assert_eq!(plan.away.len(), MAX_WELCOME_HOLDERS);
}

#[test]
fn a_new_window_1_finds_the_held_pair_lock_and_relinks_once() {
    let (layout, lock) = held_pair("relink-once");
    let run = RunDir::new();
    let socket = OrphanSocket::bind(&run.dir, &root()).unwrap();
    let identity_of = |fd: &PairLock| {
        let meta = std::fs::File::from(fd.share().unwrap()).metadata().unwrap();
        (meta.dev(), meta.ino())
    };
    let held_identity = identity_of(&lock);
    let (tell, then) = mpsc::channel();
    let held = ["agent", "terminal", "terminal#2", "lua:markdown"]
        .map(String::from)
        .into();
    let saw = orphan(
        socket,
        lock,
        crate::version::fingerprint(),
        relink_welcome(&held, Vec::new()),
        then,
    );

    assert!(
        PairLock::try_take(&layout.0, &root()).unwrap().is_none(),
        "the orphan holds the pair"
    );
    let relinked = relink(
        &run.dir,
        &layout.0,
        &root(),
        crate::version::fingerprint(),
        RELINK_WITHIN,
    )
    .expect("the new window 1 links");
    assert_eq!(identity_of(&relinked.lock), held_identity, "the lock is the pair's own");
    assert_eq!(relinked.partner.pid, std::process::id());
    assert_eq!(
        relinked.holders.keys().cloned().collect::<Vec<_>>(),
        vec!["lua:markdown", "terminal#2"]
    );
    tell.send(None).unwrap();
    let saw = saw.join().unwrap();
    assert_eq!((saw.accepted, saw.linked, saw.later), (1, true, 0), "{saw:?}");
    // The orphan's own copy is gone with its thread: the new window 1's copy keeps the pair held.
    assert!(PairLock::try_take(&layout.0, &root()).unwrap().is_none());
    drop(relinked);
    // A process another test forks holds a copy of every descriptor until its exec closes them, so the release is
    // waited for, briefly, rather than read once.
    let deadline = Instant::now() + Duration::from_secs(2);
    while PairLock::try_take(&layout.0, &root()).unwrap().is_none() {
        assert!(Instant::now() < deadline, "the last copy released");
        std::thread::sleep(Duration::from_millis(5));
    }

    // With nobody waiting, there is nothing to link to, at once.
    let started = Instant::now();
    let err = relink(&run.dir, &layout.0, &root(), "b".into(), RELINK_WITHIN)
        .err()
        .unwrap();
    assert!(err.contains("no window 2 waits"), "{err}");
    assert_eq!(
        refusal_note(&err),
        None,
        "a window with no orphan to link to says nothing on screen"
    );
    assert!(started.elapsed() < RELINK_WITHIN);
}

#[test]
fn a_relink_from_another_build_is_refused() {
    let (layout, lock) = held_pair("relink-build");
    let run = RunDir::new();
    let socket = OrphanSocket::bind(&run.dir, &root()).unwrap();
    let (tell, then) = mpsc::channel();
    let saw = orphan(
        socket,
        lock,
        "0.4.0+old".into(),
        relink_welcome(&BTreeSet::new(), Vec::new()),
        then,
    );
    let err = relink(&run.dir, &layout.0, &root(), "0.5.0+new".into(), RELINK_WITHIN)
        .err()
        .expect("another build is refused");
    assert!(err.contains("another build"), "{err}");
    assert!(
        err.contains("close window 2, then run eitri again"),
        "the refusal says how to get the pair back: {err}"
    );
    let note = refusal_note(&err).expect("a refused re-link is shown in the new window");
    assert!(
        note.contains("close window 2, then run eitri again") && note.contains("runs unpaired"),
        "{note}"
    );
    tell.send(None).unwrap();
    let saw = saw.join().unwrap();
    assert!(!saw.linked);
    // Waited for as above: a process another test forks holds every descriptor until its exec.
    let deadline = Instant::now() + Duration::from_secs(2);
    while PairLock::try_take(&layout.0, &root()).unwrap().is_none() {
        assert!(Instant::now() < deadline, "no lock went to the refused window");
        std::thread::sleep(Duration::from_millis(5));
    }

    let hello = |build: &str, window: &str, pid: u32| Msg::Hello {
        build: build.into(),
        window: window.into(),
        pid,
        start: 7,
        presence_ready: false,
        running: Vec::new(),
        summary: Summary::default(),
    };
    let peer = Some(Partner { pid: 42, start: 7 });
    assert_eq!(
        check_relink_hello(&hello("b", "1", 42), "b", peer),
        Ok(Partner { pid: 42, start: 7 })
    );
    assert!(
        check_relink_hello(&hello("b", "2", 42), "b", peer).is_err(),
        "it must be window 1"
    );
    assert!(
        check_relink_hello(&hello("b", "1", 43), "b", peer).is_err(),
        "said by the process that connected"
    );
    assert!(check_relink_hello(&hello("b", "1", 42), "b", None).is_err());
    let long = "x".repeat(10_000);
    let err = check_relink_hello(&hello(&long, "1", 42), "b", peer).unwrap_err();
    assert!(err.len() < 400, "the other window's text is cut: {} bytes", err.len());
}

#[test]
fn a_turn_the_orphan_started_just_before_the_relink_is_seen_by_the_new_window_1() {
    let (layout, lock) = held_pair("relink-turn");
    let run = RunDir::new();
    let socket = OrphanSocket::bind(&run.dir, &root()).unwrap();
    let turn = TurnEdge {
        session: "session-started-just-now".into(),
        edge: EdgeKind::Started,
        at_ns: 123,
    };
    let (tell, then) = mpsc::channel();
    let saw = orphan(
        socket,
        lock,
        crate::version::fingerprint(),
        relink_welcome(&BTreeSet::new(), vec![turn.clone()]),
        then,
    );
    let relinked = relink(
        &run.dir,
        &layout.0,
        &root(),
        crate::version::fingerprint(),
        RELINK_WITHIN,
    )
    .unwrap();
    assert_eq!(relinked.running, vec![turn.clone()]);
    let pair = Pair::relinked(relinked, &root(), false, false);
    let link = pair.link().unwrap();
    assert!(link.linked());
    assert_eq!(
        link.take_partner_running(),
        vec![turn],
        "the turn reaches the new window's turn review"
    );
    let counts = link.presence_flag();
    assert!(
        counts.load(std::sync::atomic::Ordering::SeqCst),
        "the orphan counts as another window until it confirms its presence"
    );
    tell.send(Some(Msg::Presence { held: true })).unwrap();
    assert!(pump_until(&link, || !counts.load(std::sync::atomic::Ordering::SeqCst)));

    // The link was made before the window had hooks: they hear of it once they are set.
    let linked: Rc<Cell<u32>> = Rc::default();
    let on_linked: crate::linked_window::spawn::OnLinked = {
        let linked = linked.clone();
        Rc::new(move |_, _, _| linked.set(linked.get() + 1))
    };
    pair.set_hooks(PairHooks {
        linked: on_linked,
        post: Rc::new(|_| {}),
        ended: Rc::new(|_, _, _, _| {}),
    });
    assert_eq!(linked.get(), 1);
    assert!(pair.holds_pair(), "the re-linked window is the pair's window 1");
    assert!(pair.relink_holders().is_some());
    tell.send(None).unwrap();
    saw.join().unwrap();
}

#[test]
fn start_editor_detaches_eitri_for_the_root() {
    let scratch = Scratch::new("start-editor");
    let project = scratch.1.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let report = scratch.1.join("report");
    let exe = scratch.1.join("eitri-stand-in");
    std::fs::write(
        &exe,
        format!(
            // The standard streams are read before the report's own redirection takes fd 1 for the group.
            "#!/bin/sh\nstreams=$(readlink /proc/$$/fd/0 /proc/$$/fd/1 /proc/$$/fd/2)\n{{ echo \"$*\"; echo $$; \
             cut -d' ' -f6 /proc/$$/stat; echo \"$streams\"; pwd; }} > '{0}.tmp'\nmv '{0}.tmp' '{0}'\n",
            report.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();

    let started = start_editor(
        &exe,
        &project,
        EditorFlags {
            legacy: false,
            clean: true,
        },
    )
    .unwrap();
    let pid = started.pid();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !report.exists() {
        assert!(Instant::now() < deadline, "the stand-in never ran");
        std::thread::sleep(Duration::from_millis(10));
    }
    let lines: Vec<String> = std::fs::read_to_string(&report)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    let project_text = project.display().to_string();
    assert_eq!(
        lines[0],
        format!("--clean {project_text}"),
        "started for the project root, with window 2's flags"
    );
    assert_eq!(lines[1], pid.to_string());
    assert_eq!(lines[2], pid.to_string(), "in a session of its own");
    assert_eq!(&lines[3..6], ["/dev/null", "/dev/null", "/dev/null"]);
    assert_eq!(lines[6], project_text);
    // Reaped by this window, never left a zombie, and never waited for by the caller.
    let gone = |pid: u32| {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|stat| {
                stat.rsplit_once(')')
                    .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
            })
            .map(|zombie| !zombie)
            .is_err()
    };
    while !gone(pid) || started.running() {
        assert!(Instant::now() < deadline, "the editor window's process was not reaped");
        std::thread::sleep(Duration::from_millis(10));
    }

    let err = start_editor(&exe, Path::new("relative"), EditorFlags::default()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    for (legacy, clean, want) in [
        (false, false, vec!["/p"]),
        (true, false, vec!["--legacy", "/p"]),
        (true, true, vec!["--legacy", "--clean", "/p"]),
    ] {
        assert_eq!(
            editor_args(Path::new("/p"), EditorFlags { legacy, clean }),
            want.iter().map(std::ffi::OsString::from).collect::<Vec<_>>()
        );
    }
}

#[test]
fn window_2_starts_one_editor_window_until_that_one_links_or_ends() {
    use crate::gtk_host::LinkedWindows;
    use crate::linked_window::spawn::Starts;
    let scratch = Scratch::new("one-editor");
    let project = scratch.1.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let report = scratch.1.join("report");
    let exe = scratch.1.join("eitri-stand-in");
    std::fs::write(
        &exe,
        format!(
            "#!/bin/sh\necho \"$$ $*\" > '{0}.tmp'\nmv '{0}.tmp' '{0}'\nexec sleep 30\n",
            report.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
    // A window 2 whose window 1 is gone, never linked.
    let (one, two) = fdpass::seqpacket_pair().unwrap();
    let pair = Pair::second(two, &project).unwrap();
    drop(one);
    let link = pair.link().unwrap();
    assert!(pump_until(&link, || link.ended().is_some()));
    pair.set_launch_flags(true, false);
    pair.set_starts(Starts {
        exe,
        ..Starts::default()
    });
    let win = eitri_core::wm::WinId::MAIN;
    let started_pid = || {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(&report) {
                let _ = std::fs::remove_file(&report);
                let (pid, args) = text.trim().split_once(' ').unwrap();
                return (pid.parse::<i32>().unwrap(), args.to_string());
            }
            assert!(Instant::now() < deadline, "the stand-in never ran");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let kill = |pid: i32| {
        // SAFETY: a signal to the stand-in this test started, by its pid.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    };

    LinkedWindows::start_editor(&*pair, win).unwrap();
    let (first, args) = started_pid();
    assert_eq!(
        args,
        format!("--legacy {}", project.display()),
        "with window 2's own flags"
    );
    let err = LinkedWindows::start_editor(&*pair, win).unwrap_err();
    assert!(err.contains("has not linked yet"), "{err}");
    kill(first);
    // Once that one has ended, another may be started.
    let deadline = Instant::now() + Duration::from_secs(10);
    let second = loop {
        match LinkedWindows::start_editor(&*pair, win) {
            Ok(()) => break started_pid().0,
            Err(err) => {
                assert!(err.contains("has not linked yet"), "{err}");
                assert!(
                    Instant::now() < deadline,
                    "the ended editor window still counts as starting"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    kill(second);
}

#[test]
fn a_second_hello_on_a_relinked_link_leaves_the_pair_file_to_the_new_window_1() {
    use crate::linked_window::pair_files::PairWriter;
    let (layout, lock) = held_pair("relink-hello-twice");
    let run = RunDir::new();
    let mut second = linked_second(&lock, &run);
    second
        .pair
        .set_pair_writer(PairWriter::new(&layout.0, &root(), Default::default(), false));
    drop(second.window1.take().unwrap());
    let link = second.pair.link().unwrap();
    assert!(pump_until(&link, || link.ended().is_some()));
    // Orphaned, window 2 writes the pair file, as the window's own end of a link has it do.
    second.pair.with_pair_writer(PairWriter::take_over);
    drop(lock);
    let (dir, layout_dir) = (run.dir.clone(), layout.0.clone());
    let new_window = std::thread::spawn(move || {
        relink(
            &dir,
            &layout_dir,
            &root(),
            crate::version::fingerprint(),
            Duration::from_secs(5),
        )
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !new_window.is_finished() {
        assert!(Instant::now() < deadline, "the re-link did not finish");
        second.pair.accept_relink();
        if let Some(candidate) = second.pair.relink_candidate() {
            candidate.pump();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let relinked = new_window.join().unwrap().expect("the new window 1 is linked");
    let writing = || {
        let mut writing = None;
        second.pair.with_pair_writer(|writer| writing = Some(writer.writing()));
        writing.unwrap()
    };
    assert!(!writing(), "the new window 1 writes the pair file from its welcome on");

    let me = std::process::id();
    let start = agent::child::start_time(me).unwrap();
    relinked
        .handle
        .send(hello_as_window_1(crate::version::fingerprint(), me, start), Vec::new())
        .unwrap();
    let link = second.pair.link().unwrap();
    let until = Instant::now() + Duration::from_millis(300);
    while Instant::now() < until {
        link.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(link.linked(), "the link stays up");
    assert!(!writing(), "another hello does not give the pair file back to window 2");
}

#[test]
fn a_busy_pair_file_leaves_window_two_orphaned_and_answers_bye_instead_of_welcome() {
    use crate::linked_window::pair_files::PairWriter;
    let (layout, lock) = held_pair("relink-busy-file");
    let run = RunDir::new();
    let mut second = linked_second(&lock, &run);
    // Keep the orphan's listener out of this test; only the hello decision and file ownership are exercised.
    second.pair.set_hooks(PairHooks {
        linked: Rc::new(|_, _, _| {}),
        post: Rc::new(|_| {}),
        ended: Rc::new(|_, _, _, _| {}),
    });
    drop(second.window1.take());
    let old_link = second.pair.link().unwrap();
    assert!(pump_until(&old_link, || old_link.ended().is_some()));
    // The handover has disabled writing and reported a write still in progress.
    second
        .pair
        .set_pair_writer(PairWriter::new(&layout.0, &root(), Default::default(), false));
    let (candidate, _window_one, inbound) = crate::linked_window::driver::tests::pair();
    let pid = std::process::id();
    let start = agent::child::start_time(pid).unwrap();
    let hello = hello_as_window_1(crate::version::fingerprint(), pid, start);
    second
        .pair
        .answer_relink_after_hand_over(&candidate, &hello, Some(Partner { pid, start }), false);
    let mut bye = false;
    loop {
        match inbound.recv_timeout(Duration::from_secs(5)).unwrap() {
            Inbound::Msg {
                msg: Msg::Bye { why }, ..
            } => {
                assert!(why.contains("the pair file is still being written"), "{why}");
                bye = true;
            }
            Inbound::Closed => break,
            Inbound::Msg {
                msg: Msg::Welcome { .. },
                ..
            } => panic!("a busy writer cannot welcome a new owner"),
            _ => {}
        }
    }
    assert!(bye);
    assert!(!candidate.linked());
    assert!(second.pair.is_second());
    assert!(
        second.pair.link().unwrap().same(&old_link),
        "the orphan kept its ended link"
    );
    second.pair.with_pair_writer(|writer| assert!(writer.writing()));
}

fn c(n: u8) -> WindowName {
    WindowName::Chat(n)
}

fn markdown() -> WindowName {
    WindowName::Page("markdown".into())
}

/// A stand-in module orphan on its own socket, of this build unless told otherwise, and what stops it.
fn module_orphan(
    run: &RunDir,
    name: &WindowName,
    lock: PairLock,
    build: String,
) -> (mpsc::Sender<Option<Msg>>, std::thread::JoinHandle<OrphanSaw>) {
    let socket = OrphanSocket::bind_as(&run.dir, &root(), name).unwrap();
    let (tell, then) = mpsc::channel();
    let welcome = relink_welcome_as(name, &BTreeSet::new(), Vec::new());
    (tell, orphan(socket, lock, build, welcome, then))
}

/// A second copy of `lock`, as each window of a group holds its own.
fn copy_of(lock: &PairLock) -> PairLock {
    PairLock::adopt(lock.share().unwrap()).unwrap()
}

fn relink_all_now(run: &RunDir, layout: &Scratch, within: Duration) -> Result<Vec<Relinked>, String> {
    relink_all(&run.dir, &layout.0, &root(), crate::version::fingerprint(), within)
}

#[test]
fn window_2s_orphan_socket_name_is_unchanged() {
    let dir = Path::new("/run/user/1000/eitri");
    let key = eitri_core::state_paths::project_key(&root());
    let second = socket_path_for(dir, &root(), &WindowName::Second).unwrap();
    assert_eq!(second, dir.join(format!("w-{key}.sock")));
    assert_eq!(second, socket_path(dir, &root()).unwrap());
    assert_eq!(
        lock_path_for(dir, &root(), &WindowName::Second),
        dir.join(format!("w-{key}.lock"))
    );
    assert_eq!(
        socket_path_for(dir, &root(), &c(3)).unwrap(),
        dir.join(format!("w-{key}-c3.sock"))
    );
    assert_eq!(lock_path_for(dir, &root(), &c(3)), dir.join(format!("w-{key}-c3.lock")));
    let page = markdown().file_form();
    assert_eq!(
        socket_path_for(dir, &root(), &markdown()).unwrap(),
        dir.join(format!("w-{key}-{page}.sock"))
    );

    // `bind` is window 2's socket, and a module window's own name binds beside it.
    let run = RunDir::new();
    let bound = OrphanSocket::bind(&run.dir, &root()).unwrap();
    assert_eq!(bound.path(), socket_path(&run.dir, &root()).unwrap());
    let chat = OrphanSocket::bind_as(&run.dir, &root(), &c(1)).unwrap();
    assert_eq!(chat.path(), socket_path_for(&run.dir, &root(), &c(1)).unwrap());
}

#[test]
fn the_longest_module_orphan_socket_fits_the_path_cap() {
    // The longest control directory a socket path is held to: the macOS temp dir, 49 bytes, under the fallback a
    // missing `XDG_RUNTIME_DIR` gives, with a ten-digit uid.
    let tmp = Path::new("/var/folders/33/0tqfpnyn4z3c049gljzppdv00000gn/T/");
    assert_eq!(tmp.as_os_str().len(), 49);
    let dir = tmp.join(format!("eitri-{}", u32::MAX));
    let page = WindowName::Page("a-plugin-with-a-rather-long-identifier-indeed".into());
    let path = socket_path_for(&dir, &root(), &page).expect("the page's socket fits");
    assert!(
        path.as_os_str().len() <= agent::socket_path::MAX_SOCKET_PATH_BYTES,
        "{}",
        path.display()
    );
    let form = page.file_form();
    assert_eq!(form.len(), "p-xxxxxxxx".len());
    assert!(path.to_str().unwrap().ends_with(&format!("-{form}.sock")));
}

#[test]
fn two_orphans_both_relink_to_a_new_window_1_and_keep_their_modules() {
    let (layout, lock) = held_pair("relink-all");
    let run = RunDir::new();
    let (tell_chat, chat) = module_orphan(&run, &c(1), copy_of(&lock), crate::version::fingerprint());
    let (tell_page, page) = module_orphan(&run, &markdown(), lock, crate::version::fingerprint());

    let relinked = relink_all_now(&run, &layout, RELINK_WITHIN).expect("both link");
    let named: BTreeMap<WindowName, BTreeMap<String, String>> = relinked
        .iter()
        .map(|one| (one.name.clone(), one.holders.clone()))
        .collect();
    assert_eq!(
        named,
        BTreeMap::from([
            (c(1), BTreeMap::from([("agent".to_string(), "c1".to_string())])),
            (
                markdown(),
                BTreeMap::from([("lua:markdown".to_string(), "p:markdown".to_string())])
            ),
        ])
    );
    assert!(relinked.iter().all(|one| one.partner.pid == std::process::id()));
    for (tell, saw) in [(tell_chat, chat), (tell_page, page)] {
        tell.send(None).unwrap();
        let saw = saw.join().unwrap();
        assert_eq!((saw.accepted, saw.linked, saw.later), (1, true, 0), "{saw:?}");
    }
}

#[test]
fn an_orphan_that_misses_the_bound_is_left_and_the_rest_relink() {
    let (layout, lock) = held_pair("relink-miss");
    let run = RunDir::new();
    // It listens and never answers.
    let silent = OrphanSocket::bind_as(&run.dir, &root(), &c(2)).unwrap();
    let (tell, chat) = module_orphan(&run, &c(1), lock, crate::version::fingerprint());

    let started = Instant::now();
    let relinked = relink_all_now(&run, &layout, RELINK_WITHIN).expect("the one that answered links");
    assert!(
        started.elapsed() < RELINK_WITHIN + Duration::from_secs(5),
        "the call stops at its bound: {:?}",
        started.elapsed()
    );
    let names: Vec<WindowName> = relinked.iter().map(|one| one.name.clone()).collect();
    assert_eq!(names, vec![c(1)]);
    assert!(is_socket(silent.path()), "the silent orphan is left listening");
    tell.send(None).unwrap();
    assert!(chat.join().unwrap().linked);
}

#[test]
fn a_window_1_that_relinks_two_module_orphans_is_main_and_holds_the_lock() {
    let (layout, lock) = held_pair("relink-group");
    let run = RunDir::new();
    let lock_file = layout.0.join(pair_lock_name(&root()));
    let (tell_chat, chat) = module_orphan(&run, &c(1), copy_of(&lock), crate::version::fingerprint());
    let (tell_page, page) = module_orphan(&run, &markdown(), lock, crate::version::fingerprint());

    let relinked = relink_all_now(&run, &layout, RELINK_WITHIN).expect("both link");
    let pair = Pair::relinked_group(relinked, &root(), false, false);

    assert_eq!(pair.pair_role(), Role::Main);
    assert!(pair.link().is_none(), "no window 2 here");
    assert!(pair.holds_pair());
    assert!(
        !pair.keeps_pair_file(),
        "a group leaves the pair's files as `eitri` mode wrote them"
    );
    assert!(pair.relink_holders().is_some(), "nothing of window 2 is started again");
    assert_eq!(pair.module_names(), vec![c(1), markdown()]);
    let peers = (pair.module_peer(&c(1)).unwrap(), pair.module_peer(&markdown()).unwrap());
    assert_ne!(peers.0, peers.1, "each window has a peer of its own");
    assert!(pair.module_process(&c(1)).is_some());
    assert_eq!(pair.page_peer("markdown"), peers.1, "a page window holds its own page");
    assert_eq!(pair.page_peer("snotra"), crate::linked_window::spawn::SECOND_PEER);

    // Announced to the hooks once each, as a link made before the window was built.
    let announced: Rc<RefCell<Vec<WindowName>>> = Rc::default();
    let on_linked: crate::linked_window::spawn::OnLinked = {
        let announced = announced.clone();
        Rc::new(move |name, _, link| {
            assert!(link.linked());
            announced.borrow_mut().push(name.clone());
        })
    };
    pair.set_hooks(PairHooks {
        linked: on_linked,
        post: Rc::new(|_| {}),
        ended: Rc::new(|_, _, _, _| {}),
    });
    assert_eq!(*announced.borrow(), vec![c(1), markdown()]);

    // The stand-ins' copies go with them; the one this window kept is the project's lock, and the only copy left.
    for (tell, saw) in [(tell_chat, chat), (tell_page, page)] {
        tell.send(None).unwrap();
        assert!(saw.join().unwrap().linked);
    }
    let meta = std::fs::metadata(&lock_file).unwrap();
    let copies = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| std::fs::metadata(entry.ok()?.path()).ok())
        .filter(|fd| (fd.dev(), fd.ino()) == (meta.dev(), meta.ino()))
        .count();
    assert_eq!(copies, 1, "one lock descriptor is kept and the others are closed");
    assert!(PairLock::try_take(&layout.0, &root()).unwrap().is_none());

    let facts = ModeFacts {
        choice: TilingChoice::Eitri,
        mac: false,
        wayland: true,
        wm: Wm::Sway,
        live: None,
        own_window: false,
        window2_orphan: false,
        module_orphans: !pair.module_names().is_empty(),
        unpaired: pair.is_unpaired(),
        alone: false,
    };
    assert_eq!(resolve_mode(&facts), (Mode::Compositor, Why::ModuleOrphans));
}

#[test]
fn a_group_of_no_orphans_runs_unpaired() {
    let pair = Pair::relinked_group(Vec::new(), &root(), false, false);
    assert!(pair.is_unpaired());
    assert!(pair.module_names().is_empty());
}

#[test]
fn a_welcome_under_another_windows_name_or_lock_is_refused() {
    let (layout, lock) = held_pair("relink-wrong");
    let run = RunDir::new();

    // The socket of `c2`, answered by a window calling itself `c1`, then `2`.
    for said in [
        relink_welcome_as(&c(1), &BTreeSet::new(), Vec::new()),
        relink_welcome(&BTreeSet::new(), Vec::new()),
    ] {
        let socket = OrphanSocket::bind_as(&run.dir, &root(), &c(2)).unwrap();
        let (tell, then) = mpsc::channel();
        let saw = orphan(socket, copy_of(&lock), crate::version::fingerprint(), said, then);
        let err = relink_all_now(&run, &layout, RELINK_WITHIN).err().expect("refused");
        assert!(err.contains("welcome came from window"), "{err}");
        tell.send(None).unwrap();
        saw.join().unwrap();
    }

    // Another project's lock.
    let (_other, foreign) = held_pair("relink-foreign");
    let (tell, saw) = module_orphan(&run, &c(2), foreign, crate::version::fingerprint());
    let err = relink_all_now(&run, &layout, RELINK_WITHIN).err().expect("refused");
    assert!(err.contains("not this project's pair lock"), "{err}");
    tell.send(None).unwrap();
    saw.join().unwrap();
}

#[test]
fn a_module_window_that_loses_its_hub_without_bye_waits_on_its_own_name() {
    let (layout, lock) = held_pair("module-orphan");
    let run = RunDir::new();
    let mut module = linked_module(&markdown(), &lock, &run);
    drop(module.window1.take());
    let link = module.pair.link().unwrap();
    assert!(pump_until(&link, || link.ended().is_some()));
    assert_eq!(*module.waited.borrow(), vec![Ok(())]);
    let path = socket_path_for(&run.dir, &root(), &markdown()).unwrap();
    assert!(is_socket(&path), "the orphan listens under its own name");
    assert!(
        !is_socket(&socket_path(&run.dir, &root()).unwrap()),
        "window 2's name stays free"
    );
    drop(lock);

    let (dir, layout_dir) = (run.dir.clone(), layout.0.clone());
    let new_window = std::thread::spawn(move || {
        relink_all(
            &dir,
            &layout_dir,
            &root(),
            crate::version::fingerprint(),
            Duration::from_secs(5),
        )
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !new_window.is_finished() {
        assert!(Instant::now() < deadline, "the re-link did not finish");
        module.pair.accept_relink();
        if let Some(candidate) = module.pair.relink_candidate() {
            candidate.pump();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let relinked = new_window.join().unwrap().expect("the new window 1 is linked");
    assert_eq!(relinked.len(), 1);
    assert_eq!(relinked[0].name, markdown());
    assert_eq!(
        relinked[0].holders,
        BTreeMap::from([("lua:markdown".to_string(), "p:markdown".to_string())]),
        "the welcome names the window's one module"
    );
    let link = module.pair.link().unwrap();
    assert!(link.linked() && link.is_up());
    assert_eq!(*module.linked_as.borrow(), vec![markdown()], "still the same window");
    assert!(!path.exists(), "the socket goes once the link is made");

    // It can be orphaned again, and waits again under the same name.
    drop(relinked);
    assert!(pump_until(&link, || link.ended().is_some()));
    assert_eq!(*module.waited.borrow(), vec![Ok(()), Ok(())]);
    assert!(is_socket(&path));
}

/// Window 1 dies, the chat window's tabs change while nobody can save them, and a new window 1 relinks: the file holds
/// the latest set, and the tabs in between are gone with no harm.
#[test]
fn an_orphan_chat_windows_tabs_survive_window_1s_restart() {
    use crate::linked_window::group_tabs::GroupTabs;
    use eitri_core::saved_tabs::{load, Loaded, SavedTab, SavedTabs};
    let tabs = |sessions: &[&str]| {
        let saved = |session: &&str| SavedTab {
            agent: eitri_core::agent_backend::AgentId::Claude,
            conversation_id: "0123456789abcdef".into(),
            provider_session_id: session.to_string(),
            name: None,
            mode: eitri_core::agent_bridge::SessionModeChoice::Auto,
            view: None,
            window: None,
            turn_running: false,
            draft: None,
        };
        SavedTabs::from_parts(sessions.iter().map(saved).collect(), 0, Vec::new(), None)
    };
    let (layout, lock) = held_pair("orphan-tabs");
    let state = Scratch::new("orphan-tabs-state");
    let run = RunDir::new();
    let mut module = linked_module(&c(2), &lock, &run);
    let timers: Rc<RefCell<Vec<Box<dyn FnOnce()>>>> = Rc::default();
    module.pair.set_starts(crate::linked_window::spawn::Starts {
        after: {
            let timers = timers.clone();
            Rc::new(move |_, run| timers.borrow_mut().push(run))
        },
        ..Default::default()
    });
    drop(module.window1.take());
    let link = module.pair.link().unwrap();
    assert!(pump_until(&link, || link.ended().is_some()));

    module.pair.tabs_changed(tabs(&["s-early"]));
    module.pair.tabs_changed(tabs(&["s-1", "s-2"]));
    for timer in timers.borrow_mut().drain(..) {
        timer();
    }
    drop(lock);

    let (dir, layout_dir) = (run.dir.clone(), layout.0.clone());
    let new_window = std::thread::spawn(move || {
        relink_all(
            &dir,
            &layout_dir,
            &root(),
            crate::version::fingerprint(),
            Duration::from_secs(5),
        )
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !new_window.is_finished() {
        assert!(Instant::now() < deadline, "the re-link did not finish");
        module.pair.accept_relink();
        if let Some(candidate) = module.pair.relink_candidate() {
            candidate.pump();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let relinked = new_window.join().unwrap().expect("the new window 1 is linked");
    let hub = Pair::relinked_group(relinked, &root(), false, false);
    hub.keep_group_tabs(GroupTabs::read(Some(&state.0), &root(), false));
    hub.set_hooks(PairHooks {
        linked: Rc::new(|_, _, _| {}),
        post: Rc::new(|_| {}),
        ended: Rc::new(|_, _, _, _| {}),
    });

    // As the window's `linked` hook does.
    module.pair.say_tabs();
    let hub_link = hub.module_link(&c(2)).unwrap();
    let on_disk = || match load(&state.0, &root()) {
        Loaded::Saved(saved) => saved.tabs.into_iter().map(|t| t.provider_session_id).collect(),
        Loaded::Missing => Vec::new(),
        Loaded::Unusable(why) => panic!("{why}"),
    };
    assert!(pump_until(&hub_link, || on_disk() == ["s-1".to_string(), "s-2".to_string()]));
}

#[test]
fn a_module_window_whose_hub_said_goodbye_does_not_wait() {
    let (_layout, lock) = held_pair("module-bye");
    let run = RunDir::new();
    let mut module = linked_module(&c(2), &lock, &run);
    module.window1.take().unwrap().close("window 1 closed");
    let link = module.pair.link().unwrap();
    assert!(pump_until(&link, || link.ended().is_some()));
    assert!(module.waited.borrow().is_empty());
    assert!(!is_socket(&socket_path_for(&run.dir, &root(), &c(2)).unwrap()));
}

#[test]
fn a_module_orphan_of_another_build_says_which_window_to_close() {
    let (layout, lock) = held_pair("module-build");
    let run = RunDir::new();
    let (tell, saw) = module_orphan(&run, &c(1), lock, "0.4.0+old".into());
    let (found, note) = relink_at_start(&run.dir, &layout.0, &root(), "0.5.0+new".into(), RELINK_WITHIN);
    assert!(matches!(found, StartRelink::Nothing));
    let note = note.expect("the refusal is shown in the new window");
    assert!(note.contains("close the window c1, then run eitri again"), "{note}");
    assert!(note.contains("runs unpaired"), "{note}");
    tell.send(None).unwrap();
    assert!(!saw.join().unwrap().linked);
}

#[test]
fn window_1_tries_window_2_first_and_then_every_module_window() {
    let (layout, lock) = held_pair("relink-start");
    let run = RunDir::new();
    let build = crate::version::fingerprint();

    // With nobody waiting there is nothing to link and nothing to say.
    let (found, note) = relink_at_start(&run.dir, &layout.0, &root(), build.clone(), RELINK_WITHIN);
    assert!(matches!(found, StartRelink::Nothing));
    assert_eq!(note, None);

    // A window 2 orphan is linked; a module orphan beside it is not asked.
    let beside = OrphanSocket::bind_as(&run.dir, &root(), &c(1)).unwrap();
    let (tell, then) = mpsc::channel();
    let second = orphan(
        OrphanSocket::bind(&run.dir, &root()).unwrap(),
        copy_of(&lock),
        build.clone(),
        relink_welcome(&BTreeSet::new(), Vec::new()),
        then,
    );
    let (found, note) = relink_at_start(&run.dir, &layout.0, &root(), build.clone(), RELINK_WITHIN);
    assert!(matches!(&found, StartRelink::Window2(one) if one.name == WindowName::Second));
    assert_eq!(note, None);
    assert!(
        beside.accept().unwrap().is_none(),
        "window 2 linked, so nothing else is asked"
    );
    tell.send(None).unwrap();
    second.join().unwrap();
    drop(found);
    drop(beside);

    // A window 2 that refuses ends it: the note is its refusal, and the module orphan is not asked either.
    let beside = OrphanSocket::bind_as(&run.dir, &root(), &c(1)).unwrap();
    let (tell, then) = mpsc::channel();
    let second = orphan(
        OrphanSocket::bind(&run.dir, &root()).unwrap(),
        copy_of(&lock),
        "0.4.0+old".into(),
        relink_welcome(&BTreeSet::new(), Vec::new()),
        then,
    );
    let (found, note) = relink_at_start(&run.dir, &layout.0, &root(), "0.5.0+new".into(), RELINK_WITHIN);
    assert!(matches!(found, StartRelink::Nothing));
    assert!(note.expect("shown").contains("close window 2"));
    assert!(beside.accept().unwrap().is_none());
    tell.send(None).unwrap();
    second.join().unwrap();
    drop(beside);

    // No window 2: the module orphans are linked.
    let (tell, chat) = module_orphan(&run, &c(1), lock, build.clone());
    let (found, note) = start_past_a_forks_copy_of_the_claim(&run.dir, &layout.0, build, RELINK_WITHIN);
    assert!(matches!(&found, StartRelink::Modules(group, _) if group.len() == 1));
    assert_eq!(note, None);
    tell.send(None).unwrap();
    assert!(chat.join().unwrap().linked);
}

#[test]
fn a_start_that_finds_the_group_claimed_leaves_every_orphan_waiting() {
    let (layout, lock) = held_pair("relink-claimed");
    let run = RunDir::new();
    let build = crate::version::fingerprint();
    let claim = GroupClaim::try_take(&layout.0, &root())
        .unwrap()
        .expect("nobody holds it");

    // It would answer anyone who connected.
    let (tell, chat) = module_orphan(&run, &c(1), lock, build.clone());
    let (found, note) = relink_at_start(&run.dir, &layout.0, &root(), build.clone(), RELINK_WITHIN);
    assert!(
        matches!(found, StartRelink::Nothing),
        "two windows would lead one group"
    );
    assert_eq!(note, None);

    // The claim ends with the window that held it, and the orphan is the next start's.
    drop(claim);
    let (found, _) = start_past_a_forks_copy_of_the_claim(&run.dir, &layout.0, build, RELINK_WITHIN);
    assert!(matches!(&found, StartRelink::Modules(group, _) if group.len() == 1));
    tell.send(None).unwrap();
    assert!(chat.join().unwrap().linked);
}

#[test]
fn a_window_that_took_part_of_a_group_keeps_the_rest_from_a_later_start() {
    let (layout, lock) = held_pair("relink-part");
    let run = RunDir::new();
    let build = crate::version::fingerprint();
    // Past the bound of the first start: it listens and does not answer.
    let late = OrphanSocket::bind_as(&run.dir, &root(), &c(2)).unwrap();
    let (tell, chat) = module_orphan(&run, &c(1), copy_of(&lock), build.clone());
    let within = Duration::from_millis(300);
    let (first, _) = relink_at_start(&run.dir, &layout.0, &root(), build.clone(), within);
    assert!(matches!(&first, StartRelink::Modules(group, _) if group.len() == 1));
    assert!(late.accept().unwrap().is_some(), "the first start asked it");

    // The first window lives on, and a start now would take the late orphan from it.
    let (second, note) = relink_at_start(&run.dir, &layout.0, &root(), build, within);
    assert!(matches!(second, StartRelink::Nothing));
    assert_eq!(note, None);
    assert!(late.accept().unwrap().is_none(), "the later start connected to nothing");

    drop(lock);
    tell.send(None).unwrap();
    assert!(chat.join().unwrap().linked);
}

#[test]
fn two_starts_at_once_never_both_lead_the_group() {
    let (layout, lock) = held_pair("relink-race");
    let run = RunDir::new();
    let build = crate::version::fingerprint();
    let (tell_one, one) = module_orphan(&run, &c(1), copy_of(&lock), build.clone());
    let (tell_two, two) = module_orphan(&run, &c(2), lock, build.clone());

    let gate = std::sync::Barrier::new(2);
    // Each start's result is kept to the end: the claim goes with it, as it goes with a window.
    let starts = std::thread::scope(|scope| {
        let start = || {
            scope.spawn(|| {
                gate.wait();
                relink_at_start(&run.dir, &layout.0, &root(), build.clone(), RELINK_WITHIN).0
            })
        };
        [start(), start()].map(|start| start.join().unwrap())
    });
    let linked = starts.each_ref().map(|found| match found {
        StartRelink::Modules(group, _) => group.len(),
        _ => 0,
    });
    assert!(
        linked == [2, 0] || linked == [0, 2],
        "one start has every orphan: {linked:?}"
    );
    for (tell, saw) in [(tell_one, one), (tell_two, two)] {
        tell.send(None).unwrap();
        assert!(saw.join().unwrap().linked);
    }
}

#[test]
fn a_late_orphan_is_found_by_the_start_after_its_window_1_ended() {
    let (layout, lock) = held_pair("relink-late");
    let run = RunDir::new();
    let build = crate::version::fingerprint();
    // Orphaned, and busy through the first start: nothing here steps it until that start has given up on it.
    let mut late = linked_module(&c(2), &lock, &run);
    drop(late.window1.take());
    let link = late.pair.link().unwrap();
    assert!(pump_until(&link, || link.ended().is_some()));
    let (tell, chat) = module_orphan(&run, &c(1), lock, build.clone());
    let within = Duration::from_millis(300);
    let (first, _) = relink_at_start(&run.dir, &layout.0, &root(), build.clone(), within);
    assert!(matches!(&first, StartRelink::Modules(group, _) if group.len() == 1));

    // It reads the connection that gave up on it, and goes on waiting.
    late.pair.accept_relink();
    let gone = late.pair.relink_candidate().expect("the first start's connection");
    assert!(pump_until(&gone, || gone.ended().is_some()));
    let path = socket_path_for(&run.dir, &root(), &c(2)).unwrap();
    assert!(is_socket(&path), "it still listens under its name");

    // While that window lives the orphan is not another start's.
    let (second, _) = relink_at_start(&run.dir, &layout.0, &root(), build.clone(), within);
    assert!(matches!(second, StartRelink::Nothing));

    // That window ends, here with no word to it, and the next start takes the orphan up.
    drop(first);
    tell.send(None).unwrap();
    assert!(chat.join().unwrap().linked);
    let (dir, layout_dir) = (run.dir.clone(), layout.0.clone());
    let next = std::thread::spawn(move || {
        start_past_a_forks_copy_of_the_claim(&dir, &layout_dir, build, Duration::from_secs(5)).0
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !next.is_finished() {
        assert!(Instant::now() < deadline, "the re-link did not finish");
        late.pair.accept_relink();
        if let Some(candidate) = late.pair.relink_candidate() {
            candidate.pump();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let StartRelink::Modules(group, _claim) = next.join().unwrap() else {
        panic!("the next start found nothing");
    };
    let names: Vec<WindowName> = group.iter().map(|one| one.name.clone()).collect();
    assert_eq!(names, vec![c(2)]);
    let link = late.pair.link().unwrap();
    assert!(link.linked() && link.is_up());
}

#[test]
fn a_window_says_its_own_name_as_it_closes() {
    let (_layout, lock) = held_pair("close-words");
    let run = RunDir::new();
    assert_eq!(
        linked_module(&c(3), &lock, &run).pair.closed_words(),
        "window c3 closed"
    );
    assert_eq!(linked_second(&lock, &run).pair.closed_words(), "window 2 closed");
}
