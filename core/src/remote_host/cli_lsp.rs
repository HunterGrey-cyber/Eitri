//! `eitri host lsp --root <root> [--grace <seconds>] -- <cmd> [args...]`: the language server `cmd` for a root inside
//! a remote project, run on the project's server, spoken to over this process's stdio as a language server speaks to
//! its editor. Neovim starts it as the server's command (`eitri.nvim` rewrites `cmd` for a remote root); the bridge in
//! `remote_lsp` keeps the editor's client alive across the link's loss.
//!
//! Stdout is the protocol's: from the start it is a copy of stderr for everything but the bridge's own writer, so
//! nothing else (ssh, a library) can print into the stream. Exit codes: 0 after the editor's `exit`, 1 when the
//! server cannot be had or the bridge cannot go on (with a line on stderr), 64 for a command line it does not take,
//! `128 + n` after the signal `n` (SIGTERM or SIGHUP) stopped it.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent::remote_answers::{checked_id, AnswerBound, ITEM_OVERHEAD_BYTES};
use agent::remote_client::terminal_transport::wire;
use agent::remote_client::REMOTE_RECONNECT_BACKOFF;
use sha2::{Digest, Sha256};

use super::cli::Context;
use super::cli_exec::StdoutOnStderr;
use crate::onboarding::display::display_text;
use crate::remote_lsp::bridge::Disk;
use crate::remote_lsp::link::{Dial, DialError, Redial};
use crate::remote_lsp::session::{self, Io, Options};
use crate::remote_lsp::uris::{Mirror, Rewriter};
use crate::remote_project::terminals::local_time;
use crate::remote_project::RemoteConnection;
use crate::remote_sync::ignores::Matcher;
use crate::remote_sync::{actor, recreate, terminal_text};

pub const USAGE: &str = "usage: eitri host lsp --root <root> [--grace <seconds>] -- <cmd> [args...]";

/// How long the server is kept after the bridge goes away when `--grace` is not given.
pub const DEFAULT_GRACE_SECONDS: u64 = 1_800;

/// How long the first open is retried while the helper cannot be reached, before the editor's `initialize` is
/// answered with why.
const OPEN_DEADLINE: Duration = Duration::from_secs(120);

/// The largest file whose hash a save carries.
const HASH_FILE_MAX: u64 = 64 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
struct Parsed {
    root: String,
    grace_seconds: u64,
    argv: Vec<String>,
}

/// `--root <root> [--grace <seconds>] -- <cmd> [args...]`; `--` is required, so a server's own options are never read
/// as these.
fn parse(args: &[String]) -> Option<Parsed> {
    let (mut root, mut grace) = (None, DEFAULT_GRACE_SECONDS);
    let mut args = args.iter();
    loop {
        match args.next()?.as_str() {
            "--root" => root = Some(args.next()?.clone()),
            "--grace" => grace = args.next()?.parse().ok()?,
            "--" => break,
            _ => return None,
        }
    }
    let argv: Vec<String> = args.cloned().collect();
    let root = root.filter(|root| Path::new(root).is_absolute())?;
    (!argv.is_empty()).then_some(Parsed {
        root,
        grace_seconds: grace,
        argv,
    })
}

/// The laptop's files as the bridge reads them: regular files only (a link is never followed), up to a size.
pub struct LaptopDisk {
    mirror: Mirror,
    hash_max: u64,
}

impl LaptopDisk {
    /// Files under `root` not matched by `patterns` (sync's recorded ignores), hashed up to `hash_max` bytes.
    pub fn new(root: PathBuf, patterns: &[String], hash_max: u64) -> Self {
        Self {
            mirror: Mirror::new(root, Matcher::new(patterns)),
            hash_max,
        }
    }
}

impl Disk for LaptopDisk {
    /// Read in pieces of 64 KiB, so a large file never sits in memory whole.
    fn sha256(&self, path: &Path) -> std::io::Result<String> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let meta = file.metadata()?;
        let too_large = || std::io::Error::other("not a regular file within the size limit");
        if !meta.is_file() || meta.len() > self.hash_max {
            return Err(too_large());
        }
        let mut hasher = Sha256::new();
        let (mut reader, mut buf, mut total) = (file.take(self.hash_max + 1), vec![0u8; 64 * 1024], 0u64);
        loop {
            let read = reader.read(&mut buf)?;
            if read == 0 {
                break;
            }
            total += read as u64;
            if total > self.hash_max {
                return Err(too_large());
            }
            hasher.update(&buf[..read]);
        }
        Ok(format!("{:x}", hasher.finalize()))
    }

    fn carried(&self, path: &Path) -> bool {
        self.mirror.carried(path)
    }

    fn rewriter(&self, host: &str) -> Rewriter {
        Rewriter::of_mirror(host, self.mirror.clone())
    }
}

/// `eitri host lsp <args>` with this process's stdio.
pub fn run_lsp(args: &[String], cx: &mut Context<'_>) -> i32 {
    let Some(parsed) = parse(args) else {
        eprintln!("{USAGE}");
        return 64;
    };
    // First: the handlers are in place, and the pipe's descriptors close-on-exec, before anything starts a process.
    let stop_signals = match session::stop_signal_pipe() {
        Ok(read_end) => read_end,
        Err(error) => {
            eprintln!("eitri host lsp: could not watch for stop signals: {error}");
            return 1;
        }
    };
    let Some(real_stdout) = stdout_off_the_stream() else {
        eprintln!("eitri host lsp: stdout could not be moved off the protocol's stream; nothing was started");
        return 1;
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("eitri host lsp: the current directory cannot be read: {error}");
            return 1;
        }
    };
    let (project, rel) = match super::cli_sync::project_for(cx, &cwd, Some(&parsed.root)) {
        Ok(found) => found,
        Err(why) => {
            eprintln!("eitri host lsp: {}", terminal_text(&why));
            return 1;
        }
    };
    let root = if rel.is_empty() {
        project.root.clone()
    } else {
        project.root.join(&rel)
    };
    let patterns = cx
        .store
        .state_home()
        .and_then(|state| recreate::read_created_patterns(&actor::project_dir(&state, &project)))
        .unwrap_or_default();
    let disk = LaptopDisk::new(project.root.clone(), &patterns, HASH_FILE_MAX);
    let mut options = Options {
        host: project.host.clone(),
        root,
        argv: parsed.argv,
        grace_ms: parsed.grace_seconds.saturating_mul(1000),
        backoff: REMOTE_RECONNECT_BACKOFF.to_vec(),
        open_deadline: OPEN_DEADLINE,
        nvim_queue_max: session::NVIM_QUEUE_MAX,
        stop_signals: Some(stop_signals),
        flush: sync_flush(cx, &project),
        quiet_exit: session::QUIET_EXIT,
    };
    let redial: Redial = match cx.reach.exec_connection(&project) {
        Ok(connection) => Box::new(move || dial(&connection)),
        Err(why) => {
            // Nothing to retry through: the editor's `initialize` is answered with why, at once.
            options.open_deadline = Duration::ZERO;
            Box::new(move || Err(DialError::Unreachable(why.clone())))
        }
    };
    let stdout = match real_stdout.try_clone() {
        Ok(fd) => std::fs::File::from(fd),
        Err(error) => {
            eprintln!("eitri host lsp: stdout cannot be kept: {error}");
            return 1;
        }
    };
    let io = Io {
        stdin: Box::new(std::io::stdin()),
        stdout: Box::new(stdout),
        stderr: Box::new(std::io::stderr()),
    };
    serve(options, Box::new(disk), redial, io, Some(real_stdout))
}

/// How a save asks the project's sync to carry the laptop's changes now: the tree session flushed within
/// `remote_dev.flush_cap`, its name read from the project's record at each flush (the session is re-created under
/// another name when the excludes change). `None`, with a line, when sync cannot be asked at all; the saves then wait
/// for sync to notice them by itself.
fn sync_flush(cx: &mut Context<'_>, project: &crate::remote_project::RemoteProject) -> Option<session::Flush> {
    let engine = match cx.reach.sync_engine() {
        Ok(engine) => engine,
        Err(error) => {
            eprintln!(
                "eitri host lsp: saves wait for sync to notice them: {}",
                terminal_text(&error.to_string())
            );
            return None;
        }
    };
    let cap = cx.reach.sync_settings().flush_cap;
    let store = cx.store.clone();
    let (root, host) = (project.root.clone(), project.host.clone());
    Some(Box::new(move || {
        let current = store
            .find(&root)
            .map_err(|error| error.to_string())?
            .filter(|current| current.host == host)
            .ok_or_else(|| format!("{} is no longer a recorded project on {host}", root.display()))?;
        super::cli_sync::flush_tree(engine.as_ref(), &current, cap).map(drop)
    }))
}

/// Makes descriptor 1 a copy of stderr for the life of the process and gives the real stdout (close-on-exec), for the
/// protocol's writer alone: nothing else (ssh, a library, a server run locally before it is handed stdout back) can
/// print into the stream. `None` when the descriptors cannot be moved.
pub fn stdout_off_the_stream() -> Option<OwnedFd> {
    let _ = std::io::stdout().flush();
    StdoutOnStderr::new().map(StdoutOnStderr::keep_stdout)
}

/// A helper connection of its own for this bridge, once Hello says the helper runs language servers.
pub fn dial(connection: &RemoteConnection) -> Result<Dial, DialError> {
    let unreachable = |why: String| DialError::Unreachable(why);
    let client = connection
        .lsp_client()
        .map_err(|error| unreachable(error.to_string()))?;
    let capabilities = client
        .hello_capabilities()
        .map_err(|status| unreachable(status.message().to_owned()))?;
    if !capabilities.iter().any(|capability| capability == wire::CAP_LSP) {
        return Err(DialError::NoLsp);
    }
    let (channel, handle) = client
        .lsp_channel()
        .map_err(|status| unreachable(status.message().to_owned()))?;
    Ok(Dial {
        channel,
        handle,
        owner: Box::new(client),
    })
}

/// Connects, then runs the bridge; a helper without language servers runs `argv` here instead, before anything of the
/// editor's stdin is read. `real_stdout` is what stdout was before it became a copy of stderr, put back for that run.
pub fn serve(
    options: Options,
    disk: Box<dyn Disk + Send>,
    mut redial: Redial,
    io: Io,
    real_stdout: Option<OwnedFd>,
) -> i32 {
    let first = match redial() {
        Ok(dial) => Some(dial),
        Err(DialError::NoLsp) => {
            // A stop asked for while the connection was made is not lost at the `exec` (the pipe closes there).
            if let Some(signal) = options.stop_signals.as_ref().and_then(session::pending_signal) {
                return 128 + signal;
            }
            return run_locally(&options.argv, &options.host, real_stdout.as_ref());
        }
        // The session tries again, and says why when it stays so.
        Err(DialError::Unreachable(_)) => None,
    };
    session::run(options, io, first, redial, disk)
}

/// Replaces this process with the server on this machine, after a line saying so. Only comes back when that fails.
fn run_locally(argv: &[String], host: &str, real_stdout: Option<&OwnedFd>) -> i32 {
    let name = terminal_text(&argv[0]);
    eprintln!(
        "eitri host lsp: the Eitri server on {} is older than language servers on the server; {name} runs on this \
         machine until the project is reopened",
        terminal_text(host)
    );
    if let Some(stdout) = real_stdout {
        // SAFETY: dup2 onto descriptor 1 from a descriptor this process holds; 1 lands without close-on-exec.
        if unsafe { libc::dup2(stdout.as_raw_fd(), 1) } < 0 {
            eprintln!("eitri host lsp: stdout could not be put back; {name} was not started");
            return 1;
        }
    }
    let error = std::process::Command::new(&argv[0]).args(&argv[1..]).exec();
    eprintln!(
        "eitri host lsp: {name} could not be started: {}",
        terminal_text(&error.to_string())
    );
    1
}

/// What `eitri host list` keeps of one installation's `ListServers` answer: at most [`wire::LSP_LIST_MAX`] servers,
/// each charged its encoded size against two paths' worth of bytes.
const LANGUAGE_SERVERS: AnswerBound = AnswerBound::new(
    "the server's language-server list",
    wire::LSP_LIST_MAX,
    wire::LSP_LIST_MAX * (2 * wire::LSP_PATH_MAX + 512 + ITEM_OVERHEAD_BYTES),
);

/// One line per language server the helper runs (id, root, the command's file name, pid, attached or detached since
/// when), every string from the server escaped. The answer is refused whole when it passes its bound, a path is longer
/// than [`wire::LSP_PATH_MAX`] or an id is not one: never cut short and shown as all of it.
pub(super) fn language_server_lines(answer: &wire::ListServersResponse) -> Result<Vec<String>, String> {
    LANGUAGE_SERVERS.check(&answer.servers)?;
    let mut lines = Vec::with_capacity(answer.servers.len() + 1);
    for server in &answer.servers {
        checked_id("a language server's id", &server.server_id)?;
        if server.root.len() > wire::LSP_PATH_MAX || server.command.len() > wire::LSP_PATH_MAX {
            return Err(format!(
                "the server's language-server list names a path longer than {} bytes; refused",
                wire::LSP_PATH_MAX
            ));
        }
        let command = Path::new(&server.command)
            .file_name()
            .map_or_else(|| server.command.clone(), |name| name.to_string_lossy().into_owned());
        let state = if server.attached {
            "attached".to_owned()
        } else if server.detached_since_unix_ms == 0 {
            "detached".to_owned()
        } else {
            format!("detached since {}", local_time(server.detached_since_unix_ms))
        };
        lines.push(format!(
            "  language server {}  {}  {}  pid {}  {state}",
            display_text(&server.server_id),
            display_text(&server.root),
            display_text(&command),
            server.pid
        ));
    }
    if answer.truncated {
        lines.push("  language server list truncated".to_owned());
    }
    Ok(lines)
}

/// The ids of the servers a `ListServers` answer names, for `stop` to stop them one by one: refused whole under the
/// bounds [`language_server_lines`] holds the answer to, so no id past them is sent back to the helper.
pub(super) fn server_ids(answer: &wire::ListServersResponse) -> Result<Vec<String>, String> {
    LANGUAGE_SERVERS.check(&answer.servers)?;
    answer
        .servers
        .iter()
        .map(|server| checked_id("a language server's id", &server.server_id).map(|()| server.server_id.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_server_lines_name_each_server_escaped_and_refuse_an_answer_past_its_bounds() {
        let server = |id: &str, root: &str, command: &str, attached: bool| wire::LspServerInfo {
            server_id: id.into(),
            root: root.into(),
            command: command.into(),
            pid: 4242,
            attached,
            detached_since_unix_ms: if attached { 0 } else { 1_700_000_000_000 },
            queued_bytes: 0,
        };
        let answer = wire::ListServersResponse {
            servers: vec![
                server("0123abcd", "/srv/project", "/usr/bin/rust-analyzer", true),
                server("4567ef01", "/srv/project\u{1b}[31m", "/opt/x/lua\u{202e}ls", false),
            ],
            truncated: true,
        };
        let lines = language_server_lines(&answer).unwrap();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(
            lines[0],
            "  language server 0123abcd  /srv/project  rust-analyzer  pid 4242  attached"
        );
        assert!(
            lines[1].contains("4567ef01") && lines[1].contains("detached since "),
            "{lines:?}"
        );
        assert_eq!(lines[2], "  language server list truncated");
        assert!(lines
            .iter()
            .all(|line| !line.contains('\u{1b}') && !line.contains('\u{202e}')));

        let too_many = wire::ListServersResponse {
            servers: vec![server("0123abcd", "/srv/project", "/bin/x", true); wire::LSP_LIST_MAX + 1],
            truncated: false,
        };
        assert!(language_server_lines(&too_many).unwrap_err().contains("refused"));
        let long = "/".repeat(wire::LSP_PATH_MAX + 1);
        for bad in [
            server("0123abcd", &long, "/bin/x", true),
            server("0123abcd", "/srv/project", &long, true),
            server("not an id", "/srv/project", "/bin/x", true),
        ] {
            let answer = wire::ListServersResponse {
                servers: vec![bad],
                truncated: false,
            };
            assert!(language_server_lines(&answer).is_err());
        }
        assert_eq!(
            language_server_lines(&wire::ListServersResponse::default()).unwrap(),
            Vec::<String>::new()
        );
    }

    fn args(line: &[&str]) -> Vec<String> {
        line.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn the_command_line_needs_a_root_its_separator_and_a_command() {
        let parsed = parse(&args(&[
            "--root",
            "/srv/project",
            "--grace",
            "90",
            "--",
            "rust-analyzer",
            "--",
            "-x",
        ]))
        .unwrap();
        assert_eq!(parsed.root, "/srv/project");
        assert_eq!(parsed.grace_seconds, 90);
        assert_eq!(parsed.argv, ["rust-analyzer", "--", "-x"]);
        assert_eq!(
            parse(&args(&["--root", "/r", "--", "ls"])).unwrap().grace_seconds,
            DEFAULT_GRACE_SECONDS
        );
        for line in [
            &[][..],
            &["--", "ls"][..],
            &["--root", "/r"][..],
            &["--root", "/r", "--"][..],
            &["--root", "relative", "--", "ls"][..],
            &["--root", "/r", "--grace", "soon", "--", "ls"][..],
            &["--root", "/r", "ls"][..],
        ] {
            assert!(parse(&args(line)).is_none(), "{line:?}");
        }
    }

    #[test]
    fn the_laptop_disk_hashes_a_file_in_pieces_and_refuses_one_past_its_limit() {
        let dir = std::env::temp_dir().join(format!("eitri-lsp-disk-{}", crate::remote_lsp::link::random_id()));
        agent::private_fs::create_private_dir_all(&dir, &dir).unwrap();
        let body: Vec<u8> = (0..200_000u32).map(|n| (n % 251) as u8).collect();
        std::fs::write(dir.join("big.rs"), &body).unwrap();
        let disk = LaptopDisk::new(dir.clone(), &[], 300_000);
        assert_eq!(
            disk.sha256(&dir.join("big.rs")).unwrap(),
            format!("{:x}", Sha256::digest(&body))
        );
        assert!(LaptopDisk::new(dir.clone(), &[], 100_000)
            .sha256(&dir.join("big.rs"))
            .is_err());
        std::os::unix::fs::symlink(dir.join("big.rs"), dir.join("link.rs")).unwrap();
        assert!(disk.sha256(&dir.join("link.rs")).is_err(), "a link is never followed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
