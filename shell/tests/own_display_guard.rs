//! Holds `support/own_x_server.rs` to the one property it exists for: a client of the display it
//! hands out can reach that `Xvfb` or nothing -- never another X server, the desktop's included
//! (2026-09-29, fix round 1 of "GUI tests' own display").
//!
//! **The environment it recreates** is the one incident 2 happened in: an agent shell with a network
//! namespace of its own and the host's `/tmp`. There the desktop's abstract `@/tmp/.X11-unix/X0` is
//! invisible, so an `Xvfb -displayfd` takes `:0`, while the desktop's *file* `/tmp/.X11-unix/X0` is
//! right there. `libxcb` and `x11rb` try the abstract socket first and fall back to that file on
//! `ENOENT`/`ECONNREFUSED`; so a client that cannot see this namespace's abstract socket (WebKit's
//! sandboxed processes unshare the network), or one that connects after the server has gone (it
//! died, or `-terminate` ended it when its last client left), reached the desktop's XWayland under
//! the very name the helper exported. A display-name check cannot tell the two `:0`s apart.
//!
//! **It never touches a real display, nor the real `/tmp`.** It does not trust the namespace it was
//! started in (fix round 2, 2026-09-29): an absent `/tmp/.X11-unix` proves nothing about whether
//! `/tmp` is shared -- a Wayland-only host has none -- so a run that trusted it created, bound and
//! removed `/tmp/.X11-unix/X0` and `/tmp/.X500-lock` in the host's own `/tmp`. So it re-executes
//! itself inside a namespace it builds: `bwrap --dev-bind / / --tmpfs /tmp --tmpfs /run/user/<uid>
//! --unshare-net --die-with-parent`, with a marker directory named after a random token that only
//! that fresh `/tmp` holds, and the token in the environment. Everything below runs only where both
//! agree and `/tmp` holds the marker and nothing else, the network namespace lists no X socket, and
//! `/tmp/.X11-unix` does not exist; the one-connection child it spawns (step 4) checks the token and
//! marker too, so an inherited `OWN_DISPLAY_GUARD_CONNECT` alone connects to nothing. Run it as any
//! GUI test is run (inside the mandated wrapper):
//!
//! ```sh
//! cargo test -p shell --test own_display_guard -- --ignored
//! ```
//!
//! Then it plants a stand-in for the desktop -- a listener on the file `/tmp/.X11-unix/X0` that counts
//! every connection -- and an inherited desktop cookie (`XAUTHORITY`), calls [`own_x_server::isolate`],
//! and connects to the display it was given exactly as `libxcb` does, from this namespace and from a
//! fresh one, while the server runs, after a client has come and gone, and after the server is
//! stopped. Any connection the stand-in counts is a failure. It also checks that the helper's
//! connection audit ([`own_x_server::foreign_x_connections`]) names a connection to the stand-in.
//! Last, it kills a second server behind the helper's back and checks that `stop` then signals
//! nothing: the pid of a reaped child may already be someone else's. Needs `Xvfb` and `bwrap`.
#[path = "support/own_x_server.rs"]
mod own_x_server;

use std::io::{Read, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Set on this binary's own child (run in a fresh network namespace): the display number to connect
/// to, the `libxcb` way; the child prints what it reached.
const CHILD_ENV: &str = "OWN_DISPLAY_GUARD_CONNECT";
/// Set by this binary on the namespace it re-executes itself in: a random token, which names the one
/// directory that namespace's fresh `/tmp` starts with.
const TOKEN_ENV: &str = "OWN_DISPLAY_GUARD_TOKEN";
const STAND_IN: &str = "/tmp/.X11-unix/X0";

enum Reached {
    Abstract(UnixStream),
    /// A socket file answered (the connection is dropped: reaching it at all is the failure).
    File,
}

/// `libxcb`'s `_xcb_open` for a local display `:n` on Linux (and `x11rb`'s `DefaultStream`): the
/// abstract socket, then -- on `ENOENT` or `ECONNREFUSED` only -- the file.
fn connect_like_libxcb(n: u32) -> Result<Reached, String> {
    let name = format!("/tmp/.X11-unix/X{n}");
    let addr = SocketAddr::from_abstract_name(name.as_bytes()).expect("abstract name");
    match UnixStream::connect_addr(&addr) {
        Ok(stream) => return Ok(Reached::Abstract(stream)),
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ECONNREFUSED)) => {}
        Err(e) => return Err(format!("abstract socket: {e}")),
    }
    UnixStream::connect(&name)
        .map(|_| Reached::File)
        .map_err(|e| format!("no abstract socket, and the file {name}: {e}"))
}

/// An X11 connection setup with no authorization; the server's first reply byte (1 = success).
fn x_handshake(stream: &mut UnixStream) -> std::io::Result<u8> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(&[0x6c, 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0])?;
    let mut reply = [0u8; 8];
    stream.read_exact(&mut reply)?;
    Ok(reply[0])
}

fn peer_pid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for writes of the sizes given.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (rc == 0 && cred.pid > 0).then_some(cred.pid as u32)
}

fn marker(token: &str) -> String {
    format!("/tmp/.own-display-guard-{token}")
}

/// The token this process's namespace was built with, if its `/tmp` holds the marker that only the
/// namespace built by [`reexec_in_own_namespace`] can hold. An inherited token alone is not enough:
/// the marker is created by `bwrap --dir` on a fresh tmpfs, never in any other `/tmp`.
fn own_namespace_token() -> Option<String> {
    let token = std::env::var(TOKEN_ENV).ok()?;
    let well_formed = token.len() == 32 && token.bytes().all(|b| b.is_ascii_hexdigit());
    (well_formed && Path::new(&marker(&token)).is_dir()).then_some(token)
}

/// Runs this binary again inside a namespace of its own -- a fresh tmpfs `/tmp` holding only the
/// marker, a fresh `/run/user/<uid>`, a fresh network namespace, no inherited display -- and exits
/// with its status.
fn reexec_in_own_namespace() -> ! {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .unwrap_or_else(|e| refuse(&format!("/dev/urandom: {e}")));
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let mut command = std::process::Command::new("bwrap");
    command
        .args(["--dev-bind", "/", "/", "--tmpfs", "/tmp", "--dir"])
        .arg(marker(&token));
    // SAFETY: `getuid` has no preconditions and cannot fail.
    let run_user = format!("/run/user/{}", unsafe { libc::getuid() });
    if Path::new(&run_user).is_dir() {
        command.args(["--tmpfs", &run_user]);
    }
    command.args(["--unshare-net", "--die-with-parent"]);
    for var in ["DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "XAUTHORITY", CHILD_ENV] {
        command.args(["--unsetenv", var]);
    }
    command
        .args(["--setenv", TOKEN_ENV, &token, "--"])
        .arg(std::env::current_exe().unwrap_or_else(|e| refuse(&format!("current_exe: {e}"))))
        .arg("--ignored");
    let status = command
        .status()
        .unwrap_or_else(|e| refuse(&format!("`bwrap` could not be started ({e})")));
    std::process::exit(status.code().unwrap_or(1));
}

fn child_connect(n: &str) {
    // Only step 4's child, in a namespace this binary built: never from an inherited variable.
    if own_namespace_token().is_none() {
        refuse(&format!("{CHILD_ENV} is set outside this test's own namespace"));
    }
    let n: u32 = n.parse().expect("display number");
    if n < own_x_server::FIRST_DISPLAY {
        refuse(&format!("{CHILD_ENV}={n} is below :{}", own_x_server::FIRST_DISPLAY));
    }
    match connect_like_libxcb(n) {
        Ok(Reached::Abstract(_)) => println!("REACHED abstract"),
        Ok(Reached::File) => println!("REACHED file"),
        Err(e) => println!("REACHED none ({e})"),
    }
}

fn refuse(why: &str) -> ! {
    eprintln!("own_display_guard: refusing: {why} (see this file's header)");
    std::process::exit(2);
}

fn main() {
    if let Ok(n) = std::env::var(CHILD_ENV) {
        child_connect(&n);
        return;
    }
    if !std::env::args().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!(
            "own_display_guard: ignored (starts an Xvfb in a namespace it builds; needs bwrap and Xvfb); run with `-- --ignored`"
        );
        return;
    }
    let Some(token) = own_namespace_token() else {
        if std::env::var_os(TOKEN_ENV).is_some() {
            refuse(&format!("{TOKEN_ENV} is set, but this /tmp holds no marker for it"));
        }
        reexec_in_own_namespace();
    };
    // Nothing real can be reached or touched here, or this test does nothing at all.
    let tmp: Vec<String> = std::fs::read_dir("/tmp")
        .unwrap_or_else(|e| refuse(&format!("/tmp: {e}")))
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    if tmp != [marker(&token).trim_start_matches("/tmp/").to_string()] {
        refuse(&format!("/tmp holds {tmp:?}, not only this namespace's marker"));
    }
    let unix = std::fs::read_to_string("/proc/net/unix").unwrap_or_else(|e| refuse(&format!("/proc/net/unix: {e}")));
    if unix.contains("X11-unix") {
        refuse("this network namespace has X sockets");
    }
    if Path::new("/tmp/.X11-unix").symlink_metadata().is_ok() {
        refuse("/tmp/.X11-unix exists");
    }

    // The desktop, as such a shell sees it: its socket file, and its cookie in the environment.
    std::fs::create_dir("/tmp/.X11-unix").expect("create /tmp/.X11-unix");
    std::fs::set_permissions("/tmp/.X11-unix", std::fs::Permissions::from_mode(0o1777)).expect("chmod");
    let listener = UnixListener::bind(STAND_IN).expect("bind the stand-in");
    let stand_in_inode = || {
        std::fs::symlink_metadata(STAND_IN)
            .map(|m| std::os::unix::fs::MetadataExt::ino(&m))
            .ok()
    };
    let planted_inode = stand_in_inode();
    let reached = Arc::new(AtomicUsize::new(0));
    {
        let reached = reached.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                reached.fetch_add(1, Ordering::SeqCst);
                drop(conn);
            }
        });
    }
    let cookie =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("own_display_guard-cookie-{}", std::process::id()));
    std::fs::write(&cookie, b"the desktop's cookie").expect("write the stand-in cookie");
    std::env::set_var("XAUTHORITY", &cookie);
    // A stand-in the fallback reaches is counted before any check reads the count.
    let settle = || std::thread::sleep(Duration::from_millis(200));
    // Three numbers the helper would otherwise take first, each claimed a different way it must
    // honour: a lock file, a file at the socket path, an abstract socket in this namespace.
    let first = own_x_server::FIRST_DISPLAY;
    let claimed_lock = format!("/tmp/.X{first}-lock");
    std::fs::write(&claimed_lock, b"      4242\n").expect("plant a lock file");
    let claimed_file = format!("/tmp/.X11-unix/X{}", first + 1);
    std::fs::write(&claimed_file, b"").expect("plant a file at a socket path");
    let claimed_abstract = format!("/tmp/.X11-unix/X{}", first + 2);
    let _abstract_listener =
        UnixListener::bind_addr(&SocketAddr::from_abstract_name(claimed_abstract.as_bytes()).expect("abstract name"))
            .expect("bind an abstract socket");

    let mut server = own_x_server::isolate("own_display_guard", "640x480x24");
    let mut failures = Vec::new();
    let display = server.display().to_string();
    let n: u32 = display.trim_start_matches(':').parse().expect("display number");
    println!(
        "own_display_guard: the helper's Xvfb took {display} (pid {:?})",
        server.pid()
    );

    // 1. The number: none of the three claimed ones, no socket file and no lock file of any other
    // server's under it, and nothing new in /tmp.
    if (first..first + 3).contains(&n) || n < first {
        failures.push(format!("took {display}, which was claimed (or below :{first})"));
    }
    let file = format!("/tmp/.X11-unix/X{n}");
    if Path::new(&file).symlink_metadata().is_ok() {
        failures.push(format!("took {display}, whose socket file {file} is another server's"));
    }
    let lock = format!("/tmp/.X{n}-lock");
    if Path::new(&lock).symlink_metadata().is_ok() {
        failures.push(format!("took {display}, which {lock} says is another server's"));
    }
    let entries: Vec<String> = std::fs::read_dir("/tmp/.X11-unix")
        .map(|dir| {
            dir.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    let mut entries = entries;
    entries.sort();
    if entries != ["X0".to_string(), format!("X{}", first + 1)] {
        failures.push(format!(
            "/tmp/.X11-unix now holds {entries:?}, not only what was planted"
        ));
    }
    let locks: Vec<String> = std::fs::read_dir("/tmp")
        .map(|dir| {
            dir.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with("-lock"))
                .collect()
        })
        .unwrap_or_default();
    if locks != [format!(".X{first}-lock")] {
        failures.push(format!("/tmp's lock files are now {locks:?}, not only the planted one"));
    }

    // 2. The cookie: none of the desktop's is ever offered.
    match std::env::var_os("XAUTHORITY") {
        Some(path) if Path::new(&path) == cookie => {
            failures.push("XAUTHORITY still names the inherited (desktop's) cookie".into())
        }
        Some(path) if std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) => failures.push(format!(
            "XAUTHORITY names {}, which is not empty",
            Path::new(&path).display()
        )),
        _ => {}
    }

    // 3. From this namespace, while it runs: the helper's server, by its own pid.
    match connect_like_libxcb(n) {
        Ok(Reached::Abstract(mut stream)) => {
            if peer_pid(&stream) != server.pid() {
                failures.push(format!(
                    "{display}'s abstract socket is served by pid {:?}, not the helper's {:?}",
                    peer_pid(&stream),
                    server.pid()
                ));
            }
            match x_handshake(&mut stream) {
                Ok(1) => {}
                other => failures.push(format!("the helper's server refused a connection: {other:?}")),
            }
        }
        Ok(Reached::File) => failures.push(format!("{display} from this namespace reached a socket FILE")),
        Err(e) => failures.push(format!("{display} from this namespace, while the server runs: {e}")),
    }
    settle();

    // 4. From a fresh network namespace (as WebKit's sandboxed processes are), while it runs.
    let before = reached.load(Ordering::SeqCst);
    let child = std::process::Command::new("bwrap")
        .args(["--dev-bind", "/", "/", "--unshare-net", "--"])
        .arg(std::env::current_exe().expect("current_exe"))
        .env(CHILD_ENV, n.to_string())
        .output()
        .expect("run bwrap");
    let said = String::from_utf8_lossy(&child.stdout).trim().to_string();
    println!("own_display_guard: from a fresh network namespace: {said}");
    settle();
    if !said.starts_with("REACHED none") || reached.load(Ordering::SeqCst) != before {
        failures.push(format!(
            "{display} from another network namespace reached something other than nothing: {said:?}, \
             stand-in connections {} -> {}",
            before,
            reached.load(Ordering::SeqCst)
        ));
    }

    // 5. After its only client has come and gone: still running, still the only thing there.
    std::thread::sleep(Duration::from_millis(1500));
    if !server.is_running() {
        failures.push(format!(
            "{display}'s server ended once its last client left (-terminate)"
        ));
    }
    let before = reached.load(Ordering::SeqCst);
    match connect_like_libxcb(n) {
        Ok(Reached::Abstract(stream)) if peer_pid(&stream) == server.pid() && server.pid().is_some() => {}
        Ok(Reached::Abstract(stream)) => failures.push(format!(
            "{display} later: an abstract socket served by pid {:?}",
            peer_pid(&stream)
        )),
        Ok(Reached::File) => failures.push(format!("{display} later reached a socket FILE")),
        Err(e) => failures.push(format!("{display} later: {e}")),
    }
    settle();
    if reached.load(Ordering::SeqCst) != before {
        failures.push("the stand-in was reached after a client had come and gone".into());
    }

    // 6. The audit `init_gtk` refuses on: a connection to the stand-in (made on purpose here, as GDK's
    // would be had it reached the desktop's file) is named; one to this server is not.
    match (connect_like_libxcb(n), UnixStream::connect(STAND_IN)) {
        (Ok(Reached::Abstract(_own)), Ok(_desktop)) => {
            let (mine, foreign) = server.x_connections();
            if mine == 0 || foreign.len() != 1 || !foreign[0].contains(STAND_IN) {
                failures.push(format!(
                    "the connection audit saw {mine} own and {foreign:?} foreign, not 1+ own and the stand-in"
                ));
            }
            drop(_desktop);
            if !server.foreign_x_connections().is_empty() {
                failures.push("the connection audit still names a foreign connection after it closed".into());
            }
        }
        _ => failures.push("could not set up the connection audit's two connections".into()),
    }
    settle();

    // 7. After the server has stopped (it died, or was stopped): nothing at all.
    let _ = server.stop();
    let before = reached.load(Ordering::SeqCst);
    match connect_like_libxcb(n) {
        Ok(Reached::Abstract(_)) => failures.push(format!("{display} after the stop: an abstract socket answered")),
        Ok(Reached::File) => failures.push(format!("{display} after the stop reached a socket FILE")),
        Err(_) => {}
    }
    settle();
    if reached.load(Ordering::SeqCst) != before {
        failures.push("the stand-in was reached after the server stopped".into());
    }

    // 8. A server that died and was reaped behind `stop`'s back: `stop` signals nothing, because the
    // pid may already be an unrelated process's.
    let mut second = own_x_server::isolate("own_display_guard-second", "320x240x24");
    match second.pid() {
        Some(pid) => {
            // SAFETY: `kill` has no memory-safety preconditions; `pid` is this process's unreaped child.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while second.is_running() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if second.is_running() {
                failures.push(format!("a second server (pid {pid}) survived SIGKILL"));
            } else if second.stop() {
                failures.push(format!(
                    "stop() signalled pid {pid} after is_running() had reaped it -- that pid may be anyone's now"
                ));
            }
        }
        None => failures.push("a second server has no pid".into()),
    }

    // Incident 2 itself: the desktop's socket file is never unlinked or replaced.
    if stand_in_inode() != planted_inode {
        failures.push(format!(
            "the stand-in's socket file changed: {planted_inode:?} -> {:?}",
            stand_in_inode()
        ));
    }
    let _ = std::fs::remove_file(&cookie);
    let _ = std::fs::remove_file(&claimed_lock);
    let _ = std::fs::remove_file(&claimed_file);
    let _ = std::fs::remove_file(STAND_IN);
    let _ = std::fs::remove_dir("/tmp/.X11-unix");
    println!(
        "own_display_guard: the stand-in counted {} connection(s); 1 is the audit's own, made on purpose",
        reached.load(Ordering::SeqCst)
    );
    if failures.is_empty() {
        println!("own_display_guard: ok");
        server.exit(0);
    }
    for failure in &failures {
        eprintln!("own_display_guard: FAIL: {failure}");
    }
    server.exit(1);
}
