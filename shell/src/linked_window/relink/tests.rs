use super::*;
use crate::linked_window::driver::tests::pump_until;
use crate::linked_window::spawn::{welcome, Pair, PairHooks};
use crate::linked_window::tests::Scratch;
use crate::linked_window::LinkEnd;
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
}

fn linked_second(lock: &PairLock, run: &RunDir) -> LinkedSecond {
    let (one, two) = fdpass::seqpacket_pair().unwrap();
    let (window1, window1_in) = LinkHandle::start(one, PresenceFlag::default()).unwrap();
    let pair = Pair::second(two, &root()).unwrap();
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
        Rc::new(move |_, end| {
            ends.borrow_mut().push(end.clone());
            let held: HeldNow = Rc::new(|| ["agent", "terminal#2", "lua:notes"].map(String::from).into());
            let result = pair.upgrade().unwrap().wait_for_relink(&dir, held);
            waited.borrow_mut().push(result);
        })
    };
    let on_linked: crate::linked_window::spawn::OnLinked = {
        let linked = linked.clone();
        Rc::new(move |_| linked.set(linked.get() + 1))
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
        Rc::new(move |_| linked.set(linked.get() + 1))
    };
    pair.set_hooks(PairHooks {
        linked: on_linked,
        post: Rc::new(|_| {}),
        ended: Rc::new(|_, _| {}),
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
        linked: Rc::new(|_| {}),
        post: Rc::new(|_| {}),
        ended: Rc::new(|_, _| {}),
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
