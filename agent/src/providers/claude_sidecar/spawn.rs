//! Locates, builds if necessary, and spawns Verdandi's `apps/claude-sidecar` Node.js process --
//! deliberately NOT reusing `shell/src/supervisor_client.rs`'s spawn pattern wholesale. Two real,
//! verified differences from that pattern (see this plan's "Verified facts", points 5 and 6):
//!
//! 1. `apps/claude-sidecar/dist/` is gitignored in Verdandi, not committed -- a git-dependency
//!    checkout of the Rust protocol crate (Task 1) does NOT give this process a runnable sidecar.
//!    This module locates a real Verdandi checkout on disk and builds the sidecar there if needed.
//! 2. The sidecar treats stdin EOF as its own parent-death shutdown signal (its own `lifecycle.ts`
//!    says so explicitly). `Stdio::null()` -- the pattern `supervisor_client.rs` uses -- makes it
//!    exit within the same tick it starts, since reading from `/dev/null` is an instant EOF. This
//!    module spawns with `Stdio::piped()` for stdin and holds the write handle open for the whole
//!    `SpawnedSidecar`'s lifetime; dropping (or explicitly closing) that handle is the intended
//!    shutdown signal, not a process_group/signal trick.
//!
//! Unlike `supervisor` (a shared, machine-wide, detached daemon `shell` never waits on), each
//! `ClaudeSidecarProvider` owns exactly one sidecar process for its own lifetime, closer to how
//! `agent::process::AgentProcess` owns its one `claude` child -- so this module does NOT detach via
//! `process_group(0)`; the sidecar is meant to die with its owning provider, not outlive it.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How many of the sidecar's most recent stderr lines are retained for diagnostics. Bounded on
/// purpose: a long-lived sidecar can log indefinitely, and this buffer exists to explain a startup
/// failure or carry a compatibility warning, not to be a second log file.
const STDERR_TAIL_CAPACITY: usize = 40;

/// The sidecar's recent stderr, shared between its drain thread and whoever needs to explain what
/// went wrong. Before this existed, the sidecar's own startup errors reached `eprintln!` and
/// nowhere else: a real CLI-version refusal (which happens *before* the socket is bound) surfaced
/// to the caller as nothing but `TimedOut: did not bind ... within 1000ms`, with the actual cause
/// invisible to any UI.
#[derive(Default)]
struct StderrTail {
    lines: VecDeque<String>,
}

impl StderrTail {
    fn push(&mut self, line: String) {
        if self.lines.len() == STDERR_TAIL_CAPACITY {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    fn snapshot(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }
}

pub(crate) struct SpawnedSidecar {
    pub(crate) socket_path: PathBuf,
    /// Which Verdandi checkout and revision this sidecar was built and launched from. Descriptive,
    /// always present -- reaches the UI as `ProviderInfo::verdandi_checkout`, NOT as a diagnostic.
    pub(crate) build_description: String,
    /// Only things genuinely worth warning about (today: baseline drift). Usually empty.
    pub(crate) build_warnings: Vec<String>,
    stdin_keepalive: Option<ChildStdin>,
    child: Child,
    stderr_tail: Arc<Mutex<StderrTail>>,
}

impl SpawnedSidecar {
    /// The sidecar's most recent stderr lines, oldest first. Not only a failure path: the sidecar
    /// emits a real CLI-compatibility diagnostic here on a successful-but-untested start, which a
    /// UI should be able to show without scraping this process's own stderr.
    pub(crate) fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail.lock().map(|tail| tail.snapshot()).unwrap_or_default()
    }

    /// The spawned sidecar's OS pid -- the only safe handle for an orphan check (see
    /// `ClaudeSidecarProvider::sidecar_pid`).
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }
}

/// The Verdandi revision this client is developed and verified against.
///
/// Not a hard pin -- `NEOVIBE_VERDANDI_CHECKOUT` exists precisely so a Verdandi feature branch or a
/// protocol migration can be tested against an unreleased sidecar, and hard-failing on a different
/// revision would defeat that. It is a BASELINE: when the checkout is at a different revision, that
/// fact is surfaced as a startup diagnostic instead of being silent, so "which sidecar build was
/// this session actually running?" is answerable after the fact rather than guessed at.
///
/// Currently `650782f` on Verdandi's `sdk-mainline-unblock-merge-20260915`, which is also the `rev`
/// that `agent/Cargo.toml` pins `claude-runtime-protocol` to -- and those two must stay equal, because
/// the generated wire types this crate compiles against come from exactly that revision, so it is
/// the revision "verified against" can honestly refer to. (The checkout actually RUNNING may differ;
/// that is what the drift warning below reports, and what `NEOVIBE_VERDANDI_CHECKOUT` is for.)
///
/// The value earns its way here by the real suite, never by a version bump: the whole `#[ignore]`d
/// real-sidecar set is re-run against the exact pushed revision first (multi-turn with a content
/// oracle, BYPASS tool execution, post-interrupt reuse, orphan-free teardown, a real resume proven
/// by both a content oracle and a provider-session-id match, partial assistant streaming measured
/// before/after, and the replay/recovery and backpressure suites). Moved here
/// `eb70aa3 -> 2fd30fb -> c331615 -> ff05677 -> bb487d7 -> 650782f` on those terms each time; move
/// it again on the same terms, not before.
///
/// The `650782f` move (2026-09-15) is the one exception worth naming, because it was NOT earned by
/// re-running the whole billed suite here: Verdandi ran this crate's own real tests against the
/// merge on their side and reported the output, and the move was then confirmed locally by the four
/// FREE `claude_sidecar_unary_conformance` tests -- the live handshake among them -- which is what
/// proves the protocol-3 client and the merged sidecar actually agree rather than merely compile.
/// That revision sits seven commits past a real merge, `aea4ec0` (Verdandi `main` into
/// `sdk-mainline-unblock`), and its first-parent chain still contains `bb487d7`, so nothing this
/// crate pinned before became unreachable.
///
/// Keep this doc comment and the literal in step. They were not, between `6980f69` (which bumped
/// only the literal) and the 2026-09-15 cross-repository review that caught it: the prose said
/// `c331615` while the constant said `bb487d7`, two revisions apart, and nothing failed.
/// **Slated for retirement, and the replacement is already on the wire.** A git revision is a
/// development-machine concept: a packaged install will spawn a shipped sidecar ARTIFACT with no
/// checkout and no `.git` to read, so this check has nothing to compare and the description line
/// above it has nothing to describe. Agreed with Verdandi 2026-09-18: the compatibility token
/// becomes protocol major (already checked at handshake) plus `sidecar_version`, which they now
/// stamp from `package.json` at build time -- both already carried on `ProviderInfo`. Until this
/// client range-checks that version, this constant is what warns about skew, so it is kept
/// accurate rather than deleted early.
pub const EXPECTED_VERDANDI_REVISION: &str = "a2f194a";

/// Where `NEOVIBE_VERDANDI_CHECKOUT` came from, and what it points at. Carried onto `ProviderInfo`
/// so the UI can name the backend build it is talking to.
#[derive(Debug)]
pub(crate) struct VerdandiCheckout {
    pub(crate) path: PathBuf,
    /// `git rev-parse --short HEAD`, or `None` when the checkout is not a git repo or `git` is
    /// unavailable. Best-effort diagnostics only -- never a reason to refuse to start.
    pub(crate) revision: Option<String>,
    /// True when `NEOVIBE_VERDANDI_CHECKOUT` chose this path rather than the default.
    pub(crate) from_override: bool,
}

/// Locates a real Verdandi checkout.
///
/// `$NEOVIBE_VERDANDI_CHECKOUT` is a **supported development/integration override**, not a
/// temporary hack: it is how this client is pointed at a Verdandi feature branch, a protocol
/// migration, or an isolated checkout while the default one is mid-work. It takes precedence over
/// the default `$HOME/src/verdandi` and is validated the same way, so a typo'd path
/// fails with a clear message rather than silently falling back to a different sidecar than the
/// operator intended.
///
/// Either way this is a dev-machine story, not a production distribution one -- spec §13's
/// packaging profiles remain separately deferred.
/// Takes the override as an argument rather than reading it here. `resolve_sidecar_program` already
/// has to know whether an override was supplied, in order to rank it against a packaged artifact, so
/// reading it in both places made the value and the decision two separate sources of truth -- a test
/// could pass one path and this function would use another. Found by exactly that test.
/// Where a dev machine's Verdandi checkout is looked for when nothing overrides it, relative to
/// `$HOME`. Named rather than inlined because the packaging scripts carry the same default and
/// nothing but `packaging_scripts_default_to_the_same_checkout_this_code_does` makes them agree --
/// they drifted for real: three of them said `verdandi-old-checkout`, a detached checkout from while
/// Verdandi's protocol-3 merge was outstanding, after their main absorbed it on 2026-09-18.
const DEFAULT_CHECKOUT_UNDER_HOME: &str = "src/verdandi";

fn locate_verdandi_checkout(explicit: Option<&str>) -> std::io::Result<VerdandiCheckout> {
    let (path, from_override) = match explicit.map(str::trim).filter(|p| !p.is_empty()) {
        Some(path) => (PathBuf::from(path), true),
        None => {
            let home = std::env::var("HOME").map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "HOME is unset and NEOVIBE_VERDANDI_CHECKOUT was not provided",
                )
            })?;
            (PathBuf::from(home).join(DEFAULT_CHECKOUT_UNDER_HOME), false)
        }
    };

    // Validated for BOTH paths now. Previously only the default was checked, so a mistyped override
    // reached `ensure_sidecar_built` and failed somewhere much less informative.
    if !path.join("apps/claude-sidecar/package.json").exists() {
        let origin = if from_override {
            "NEOVIBE_VERDANDI_CHECKOUT points at"
        } else {
            "no Verdandi checkout found at the default"
        };
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "{origin} {path:?}, which has no apps/claude-sidecar/package.json -- \
                 set NEOVIBE_VERDANDI_CHECKOUT to a real Verdandi checkout"
            ),
        ));
    }

    let revision = git_short_revision(&path);
    Ok(VerdandiCheckout { path, revision, from_override })
}

/// Best-effort `git rev-parse --short HEAD`. Any failure (not a repo, no `git`, detached weirdness)
/// yields `None` -- this is diagnostics, and a diagnostic that can refuse to start is worse than no
/// diagnostic.
fn git_short_revision(checkout: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(checkout)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let revision = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if revision.is_empty() {
        None
    } else {
        Some(revision)
    }
}

/// One line saying which backend build this session is running, plus a separate list of things
/// genuinely worth warning about.
///
/// The split is load-bearing. The description is unconditional -- there is always a checkout, and a
/// missing git revision degrades to "unknown revision" rather than suppressing the line -- so
/// folding it into the warning list made that list never empty, and any UI that treats "has
/// diagnostics" as "something is wrong" lit up permanently for every healthy session. A warning
/// that is always on is not a warning.
fn describe_checkout(checkout: &VerdandiCheckout) -> (String, Vec<String>) {
    let source = if checkout.from_override { "NEOVIBE_VERDANDI_CHECKOUT" } else { "default path" };
    let revision = checkout.revision.as_deref().unwrap_or("unknown revision");
    let description = format!("Verdandi checkout: {} @ {revision} (via {source})", checkout.path.display());

    let mut warnings = Vec::new();
    // `starts_with` rather than equality: `git rev-parse --short` picks its own abbreviation length,
    // which grows as a repository does, so a strict comparison would start reporting false drift.
    if let Some(actual) = &checkout.revision {
        if !actual.starts_with(EXPECTED_VERDANDI_REVISION) && !EXPECTED_VERDANDI_REVISION.starts_with(actual.as_str()) {
            warnings.push(format!(
                "Verdandi baseline drift: running {actual}, this client was verified against \
                 {EXPECTED_VERDANDI_REVISION}. Not an error -- testing a Verdandi branch is exactly \
                 what NEOVIBE_VERDANDI_CHECKOUT is for -- but if behavior looks wrong, this is the \
                 first thing to check."
            ));
        }
    }
    (description, warnings)
}

/// True if the checkout's root `package.json` declares a `build` script.
///
/// Load-bearing, not a nicety: `apps/claude-sidecar`'s own `build` script does NOT build the
/// `@verdandi/claude-runtime` kernel it imports, and the sidecar resolves that package through a
/// workspace symlink to `packages/claude-runtime/dist`. Building only the app therefore either
/// fails outright (clean checkout: 8x TS2307) or -- worse -- silently succeeds against whatever
/// stale kernel `dist` happens to be present. Verdandi gained a kernel-first root fan-out on
/// 2026-09-11; a checkout predating that has no root script at all, so this probe is what decides
/// between the correct path and the legacy fallback below.
///
/// It prefers `build:claude` over `build` for a reason with a date on it -- see the comment inside.
fn root_build_script_for_claude(checkout: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(checkout.join("package.json")).ok()?;
    let manifest = serde_json::from_str::<serde_json::Value>(&contents).ok()?;
    let scripts = manifest.get("scripts")?;
    // `build:claude` first, deliberately. Verdandi's root `build` is
    // `build:terminal && build:claude`, and the terminal half is a workspace neovibe does not
    // consume and does not build against -- its `node-pty` is a native module a checkout can
    // easily be missing. Running the whole fan-out makes a failure in code this crate never loads
    // into a failure to start the agent panel at all, which is what it did on 2026-09-15:
    // `packages/terminal-runtime` failed `TS2307: Cannot find module 'node-pty'` and `build:claude`
    // never ran. Build what we consume.
    for candidate in ["build:claude", "build"] {
        if scripts.get(candidate).and_then(|b| b.as_str()).is_some() {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Existence check only, not mtime-based staleness (unlike `shell/build.rs`'s own agent-ui
/// pattern) -- this runs at every provider construction (spawn time), not once at `cargo build`
/// time, so walking every source file's mtime on each spawn is a real, avoidable cost. A developer
/// who changes Verdandi's sidecar/kernel source without rebuilding must `rm -rf dist/` themselves
/// to force a rebuild -- a documented manual step, not a silent staleness bug this module hides.
///
/// Both dist entries are checked, not just the sidecar's: a present sidecar `dist` with a missing
/// kernel `dist` is a real, reachable state (they are built by separate `tsc` invocations) and it
/// fails at `node` startup with a module-resolution error rather than anywhere useful.
fn ensure_sidecar_built(checkout: &Path) -> std::io::Result<PathBuf> {
    let sidecar_dir = checkout.join("apps/claude-sidecar");
    let dist_entry = sidecar_dir.join("dist/src/index.js");
    let kernel_dist_entry = checkout.join("packages/claude-runtime/dist/src/index.js");
    if dist_entry.exists() && kernel_dist_entry.exists() {
        return Ok(dist_entry);
    }
    if !checkout.join("node_modules").exists() {
        run_command(checkout, "npm", &["ci"])?;
    }
    if let Some(script) = root_build_script_for_claude(checkout) {
        run_command(checkout, "npm", &["run", &script])?;
    } else {
        // A Verdandi checkout predating the root fan-out. Do the ordering by hand rather than
        // building only the app, which is what silently produced stale-kernel runs.
        eprintln!(
            "agent: {checkout:?} has no root `build` script (it predates Verdandi's kernel-first \
             fan-out); building the kernel and the sidecar separately, in that order"
        );
        run_command(&checkout.join("packages/claude-runtime"), "npm", &["run", "build"])?;
        run_command(&sidecar_dir, "npm", &["run", "build"])?;
    }
    for entry in [&kernel_dist_entry, &dist_entry] {
        if !entry.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("the Verdandi build completed but {entry:?} still does not exist"),
            ));
        }
    }
    Ok(dist_entry)
}

fn run_command(dir: &Path, program: &str, args: &[&str]) -> std::io::Result<()> {
    let status = Command::new(program).args(args).current_dir(dir).status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "{program} {args:?} failed in {dir:?} with status {status}"
        )));
    }
    Ok(())
}

/// The name a PACKAGED sidecar artifact is installed under, beside this binary.
///
/// Versionless on purpose: Verdandi's release output carries its version and architecture in the
/// file name (`verdandi-claude-sidecar-0.1.0-linux-x64`), which is right for a download and wrong
/// for an installed path -- an install that encoded the version here would make every sidecar
/// upgrade a change to this crate. The compatibility statement lives in the handshake
/// (`protocol_major` plus `sidecar_version`), not in a file name.
const PACKAGED_SIDECAR_BINARY: &str = "verdandi-claude-sidecar";

/// Where the sidecar comes from, resolved once per spawn.
///
/// Two genuinely different shapes, which is why this is an enum rather than a path plus a flag: a
/// packaged artifact is one self-contained executable with no checkout, no `.git` to describe and
/// nothing to build, while the development path is `node <dist entry>` out of a checkout that may
/// need `npm` run first and whose revision is worth reporting.
#[derive(Debug)]
enum SidecarProgram {
    /// A shipped, self-contained executable -- normally a sibling of the running binary, the same
    /// convention `agent-hook`, `neovibe-supervisor` and `neovibe-tmux-shim` already follow.
    Packaged(PathBuf),
    /// A Verdandi checkout, run through `node`.
    Checkout(VerdandiCheckout),
}

/// Decides which of the two shapes to use, from values the caller supplies rather than from the
/// process environment, so the precedence is testable without mutating it.
///
/// Precedence, and each step earns its place:
/// 1. `NEOVIBE_SIDECAR_BINARY` -- an explicit artifact wins over everything, including a checkout,
///    because someone who names a binary is testing that binary.
/// 2. `NEOVIBE_VERDANDI_CHECKOUT` -- an explicit checkout beats an installed artifact for the same
///    reason in the other direction: a developer pointing at a branch wants that branch, and
///    silently preferring the shipped artifact would make that override look broken.
/// 3. A packaged artifact beside this binary -- the shape a real install has.
/// 4. The default checkout -- the shape a source tree has.
fn resolve_sidecar_program(
    explicit_binary: Option<&str>,
    explicit_checkout: Option<&str>,
    exe_dir: Option<&Path>,
) -> std::io::Result<SidecarProgram> {
    if let Some(binary) = explicit_binary.map(str::trim).filter(|b| !b.is_empty()) {
        let path = PathBuf::from(binary);
        if !path.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("NEOVIBE_SIDECAR_BINARY points at {path:?}, which is not a file"),
            ));
        }
        return Ok(SidecarProgram::Packaged(path));
    }
    if explicit_checkout.map(str::trim).is_some_and(|c| !c.is_empty()) {
        return Ok(SidecarProgram::Checkout(locate_verdandi_checkout(explicit_checkout)?));
    }
    if let Some(sibling) = exe_dir.map(|d| d.join(PACKAGED_SIDECAR_BINARY)).filter(|p| p.is_file()) {
        return Ok(SidecarProgram::Packaged(sibling));
    }
    Ok(SidecarProgram::Checkout(locate_verdandi_checkout(None)?))
}

/// True when a SHIPPED sidecar artifact is available without a checkout.
///
/// This is the question "can this installation run the sidecar backend out of the box?", and it is
/// deliberately narrower than [`resolve_sidecar_program`]: a Verdandi CHECKOUT does not count.
///
/// **Why a checkout must not count.** A developer machine has one, and treating it as availability
/// would silently switch every source build to a backend whose first start runs `npm ci` and a
/// TypeScript build -- a multi-minute stall with no UI saying why. Worse, it would make the backend
/// a property of what happens to be in `~/src`, which is not a decision anyone made.
/// A checkout stays what it has always been: an explicit override, reached through
/// `NEOVIBE_VERDANDI_CHECKOUT` or `NEOVIBE_AGENT_BACKEND=sidecar`.
///
/// So this answers for the shipped shape only, and the moment the artifact is installed beside the
/// product the default flips for real users with no further change here.
pub fn packaged_sidecar_available() -> bool {
    if std::env::var("NEOVIBE_SIDECAR_BINARY").ok().is_some_and(|v| !v.trim().is_empty()) {
        return true;
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(PACKAGED_SIDECAR_BINARY)))
        .is_some_and(|path| path.is_file())
}

/// Spawns a fresh sidecar for one `ClaudeSidecarProvider` instance. `instance_id` becomes part of
/// the socket path (`crate::socket_path::sidecar_socket`, which mirrors the legacy backend's own
/// per-conversation UUID socket and keeps both under macOS's 103-byte socket-path limit) so
/// multiple concurrent providers never collide on one path.
pub(crate) fn spawn(instance_id: &str) -> std::io::Result<SpawnedSidecar> {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    let program = resolve_sidecar_program(
        std::env::var("NEOVIBE_SIDECAR_BINARY").ok().as_deref(),
        std::env::var("NEOVIBE_VERDANDI_CHECKOUT").ok().as_deref(),
        exe_dir.as_deref(),
    )?;
    let (build_description, build_warnings, program_path, program_args) = match program {
        SidecarProgram::Packaged(path) => (
            format!("packaged sidecar artifact: {}", path.display()),
            Vec::new(),
            path,
            Vec::new(),
        ),
        SidecarProgram::Checkout(checkout) => {
            let (description, warnings) = describe_checkout(&checkout);
            // `npm` runs only on this path. A packaged artifact has nothing to build, and reaching
            // for a build step there would be the bug this branch exists to avoid.
            let dist_entry = ensure_sidecar_built(&checkout.path)?;
            (description, warnings, PathBuf::from("node"), vec![dist_entry])
        }
    };
    eprintln!("agent: {build_description}");
    for line in &build_warnings {
        eprintln!("agent: {line}");
    }
    let socket_path = crate::socket_path::sidecar_socket(&std::env::temp_dir(), instance_id)?;
    let _ = std::fs::remove_file(&socket_path); // stale leftover from a prior crash, if any

    let mut command = Command::new(&program_path);
    command
        .args(&program_args)
        .env("VERDANDI_CLAUDE_SIDECAR_SOCKET", &socket_path)
        .stdin(Stdio::piped())
        // stdout/stderr are captured, not nulled -- `supervisor`'s own follow-up review found that
        // nulling a spawned child's stderr silently discards real crash diagnostics (2026-09-09,
        // supervisor-robustness-followup plan). A Node.js sidecar crash during real CLI parity
        // testing (Task 9) is exactly the kind of thing worth seeing.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn()?;
    let stdin_keepalive = child.stdin.take();

    let stderr_tail = Arc::new(Mutex::new(StderrTail::default()));
    if let Some(stdout) = child.stdout.take() {
        spawn_log_drain_thread(stdout, "stdout", None);
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_log_drain_thread(stderr, "stderr", Some(Arc::clone(&stderr_tail)));
    }

    // 3s, not the former 1s. The old budget had no headroom over a cold Node start plus the Claude
    // Agent SDK's own module graph, and its expiry was indistinguishable from a real refusal.
    const RETRY_ATTEMPTS: u32 = 60;
    const RETRY_DELAY: Duration = Duration::from_millis(50);
    for _ in 0..RETRY_ATTEMPTS {
        if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
            return Ok(SpawnedSidecar { socket_path, build_description, build_warnings, stdin_keepalive, child, stderr_tail });
        }
        // Check for a dead child BEFORE sleeping again. The sidecar fails closed on a policy
        // violation (an incompatible Claude CLI, a socket already in use) by throwing before it
        // ever binds, so the process is simply gone -- waiting out the remaining retries would turn
        // an immediate, fully-explained failure into a multi-second silent timeout. This is exactly
        // how a real CLI-version refusal used to reach the UI as `did not bind ... within 1000ms`.
        if let Ok(Some(status)) = child.try_wait() {
            drop(stdin_keepalive);
            // The drain threads may still be flushing the final lines the child wrote on its way
            // out; those lines are the entire diagnosis, so give them a moment to land rather than
            // reporting an empty cause.
            std::thread::sleep(Duration::from_millis(100));
            return Err(std::io::Error::other(describe_startup_failure(
                &format!("claude-sidecar exited with {status} before binding {socket_path:?}"),
                &stderr_tail,
            )));
        }
        std::thread::sleep(RETRY_DELAY);
    }
    // Still running but never bound -- clean up rather than leaking a half-started process, then
    // report the failure with whatever it managed to say for itself.
    drop(stdin_keepalive);
    let _ = child.kill();
    let _ = child.wait();
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        describe_startup_failure(
            &format!(
                "claude-sidecar did not bind {socket_path:?} within {}ms",
                RETRY_ATTEMPTS as u64 * RETRY_DELAY.as_millis() as u64
            ),
            &stderr_tail,
        ),
    ))
}

/// Builds a startup-failure message that actually names the cause, by appending whatever the
/// sidecar printed to stderr before dying. Without this the caller gets a transport-shaped symptom
/// ("did not bind") for what is usually a policy-shaped cause ("refusing to start: claude CLI
/// version 2.1.269 ...") -- and the real message exists only in this process's own stderr, where no
/// UI can reach it.
fn describe_startup_failure(summary: &str, stderr_tail: &Arc<Mutex<StderrTail>>) -> String {
    let lines = stderr_tail.lock().map(|tail| tail.snapshot()).unwrap_or_default();
    if lines.is_empty() {
        return format!("{summary} (the sidecar printed nothing to stderr)");
    }
    format!("{summary}; its stderr said:\n{}", lines.join("\n"))
}

fn spawn_log_drain_thread<R: std::io::Read + Send + 'static>(
    reader: R,
    stream_name: &'static str,
    stderr_tail: Option<Arc<Mutex<StderrTail>>>,
) {
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(reader).lines().map_while(Result::ok) {
            eprintln!("claude-sidecar[{stream_name}]: {line}");
            if let Some(tail) = &stderr_tail {
                if let Ok(mut tail) = tail.lock() {
                    tail.push(line);
                }
            }
        }
    });
}

impl Drop for SpawnedSidecar {
    /// Graceful-first shutdown: close the held-open stdin (the sidecar's own documented
    /// parent-death signal, verified for real during this plan's preparation), wait briefly (a
    /// graceful exit lets the sidecar fail-close any pending permissions server-side before this
    /// process moves on), then escalate to SIGKILL if it hasn't exited -- matching design doc
    /// §12.1's shutdown escalation requirement. Deliberately simpler than
    /// `shell/src/supervisor_client.rs::connect_or_spawn`'s own background-thread reaper: that
    /// pattern exists there because a *detached* supervisor process may run for an unbounded time
    /// after being spawned, so its eventual `wait()` has no natural bound to block on. Here, by the
    /// time `drop` runs, the sidecar is either already exiting gracefully (bounded by the 3s
    /// deadline below) or about to be SIGKILLed (which returns near-instantly) -- both paths are
    /// genuinely bounded, so blocking `Drop` itself for at most ~3s is a deliberate, bounded wait,
    /// not the unbounded kind Global Constraints forbids.
    fn drop(&mut self) {
        drop(self.stdin_keepalive.take()); // closes the write end -> sidecar sees stdin EOF

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            match self.child.try_wait() {
                Ok(Some(_status)) => return,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {

    /// An explicit artifact wins over everything, including an explicit checkout.
    ///
    /// The precedence matters more than it looks: these two overrides select DIFFERENT SHAPES, not
    /// two spellings of one, so a wrong order is not a tie-break -- it silently runs `node` out of a
    /// tree when someone asked for a binary, or the reverse. Resolution takes its inputs as
    /// arguments precisely so this is checkable without mutating the process environment, which is
    /// shared by every other test in this binary.
    #[test]
    fn an_explicit_binary_beats_an_explicit_checkout() {
        let artifact = std::env::temp_dir().join(format!("nv-sidecar-{}", uuid::Uuid::new_v4()));
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();
        let resolved = resolve_sidecar_program(
            artifact.to_str(),
            Some("/definitely/not/a/checkout"),
            None,
        )
        .expect("an existing artifact resolves");
        assert!(matches!(&resolved, SidecarProgram::Packaged(p) if *p == artifact), "{resolved:?}");
        let _ = std::fs::remove_file(&artifact);
    }

    /// A named artifact that does not exist is a hard error, not a silent fall-through to a
    /// checkout. Falling through would run a DIFFERENT build than the one named and report success,
    /// which is the failure mode every override in this crate is written to avoid.
    #[test]
    fn a_named_artifact_that_is_missing_fails_rather_than_falling_back() {
        let err = resolve_sidecar_program(Some("/no/such/sidecar/binary"), None, None)
            .expect_err("a named artifact that is absent must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(err.to_string().contains("NEOVIBE_SIDECAR_BINARY"), "{err}");
    }

    /// A sibling artifact is used when nothing was named -- the shape a real install has, and the
    /// same `current_exe()`-sibling convention `agent-hook` and `neovibe-supervisor` already follow.
    #[test]
    fn a_sibling_artifact_is_found_when_nothing_is_named() {
        let dir = std::env::temp_dir().join(format!("nv-sidecar-dir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();
        let resolved = resolve_sidecar_program(None, None, Some(&dir)).expect("the sibling resolves");
        assert!(matches!(&resolved, SidecarProgram::Packaged(p) if *p == artifact), "{resolved:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An explicit checkout beats a sibling artifact. A developer pointing at a branch wants that
    /// branch; preferring the shipped artifact would make `NEOVIBE_VERDANDI_CHECKOUT` look broken
    /// on exactly the machines where both exist -- which, once neovibe ships the artifact, is every
    /// developer machine.
    #[test]
    fn an_explicit_checkout_beats_a_sibling_artifact() {
        let dir = std::env::temp_dir().join(format!("nv-sidecar-dir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(PACKAGED_SIDECAR_BINARY), b"#!/bin/sh\nexit 0\n").unwrap();
        // Points at a directory that is not a checkout, so this resolves to the Checkout ARM and
        // then fails inside it -- which is the observation: the sibling was not chosen.
        let err = resolve_sidecar_program(None, Some("/definitely/not/a/checkout"), Some(&dir))
            .expect_err("an explicit checkout that is not one must fail rather than silently using the sibling");
        assert!(
            !err.to_string().contains(PACKAGED_SIDECAR_BINARY),
            "the sibling artifact was used despite an explicit checkout: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An empty override is treated as absent. `NEOVIBE_SIDECAR_BINARY=` in a wrapper script is a
    /// way of saying "not set", and honouring it literally would fail every spawn with a path that
    /// is the empty string.
    #[test]
    fn an_empty_override_is_not_an_override() {
        let dir = std::env::temp_dir().join(format!("nv-sidecar-dir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();
        let resolved = resolve_sidecar_program(Some("   "), Some(""), Some(&dir))
            .expect("blank overrides fall through to the sibling");
        assert!(matches!(&resolved, SidecarProgram::Packaged(p) if *p == artifact), "{resolved:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
    use super::*;

    /// Real, not mocked -- spawns the actual compiled sidecar (building it first if needed) and
    /// confirms both that it binds its socket and that closing the held-open stdin handle (this
    /// module's own documented shutdown mechanism) actually makes it exit. No real API cost: no
    /// Claude CLI turn is ever sent.
    #[test]
    #[ignore]
    fn spawn_binds_the_socket_and_stdin_close_shuts_it_down_cleanly() {
        let instance_id = uuid::Uuid::new_v4().to_string();
        let mut sidecar = spawn(&instance_id).expect("spawn should succeed against a real Verdandi checkout");
        assert!(std::os::unix::net::UnixStream::connect(&sidecar.socket_path).is_ok());

        drop(sidecar.stdin_keepalive.take());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut exited = false;
        while std::time::Instant::now() < deadline {
            if matches!(sidecar.child.try_wait(), Ok(Some(_))) {
                exited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(exited, "sidecar did not exit within 5s of its stdin being closed");
        let _ = std::fs::remove_file(&sidecar.socket_path);
    }

    /// The packaging scripts hardcode the same default checkout this code computes, and nothing
    /// else keeps the two equal. They were not equal: `packaging/neovibe.launcher.sh`,
    /// `install.sh` and `try-neovibe.sh` all said `~/src/verdandi-old-checkout` -- the
    /// detached checkout used while Verdandi's protocol-3 merge was outstanding -- while this file
    /// said `~/src/verdandi`. Both directories exist on the machine that wrote them,
    /// so the disagreement was invisible: the launcher's own guard would pass against one
    /// directory and, where it did not export the override, the binary would read the other.
    ///
    /// Asserted on the literal text rather than by running the scripts, because the value has to be
    /// right on a machine where neither directory exists.
    #[test]
    fn packaging_scripts_default_to_the_same_checkout_this_code_does() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("agent/ has a parent");
        let expected = format!("${{NEOVIBE_VERDANDI_CHECKOUT:-$HOME/{DEFAULT_CHECKOUT_UNDER_HOME}}}");

        for script in ["packaging/neovibe.launcher.sh", "install.sh", "try-neovibe.sh", "publish.sh"] {
            let path = repo_root.join(script);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            let uses = text
                .lines()
                .filter(|l| l.contains("NEOVIBE_VERDANDI_CHECKOUT:-"))
                .collect::<Vec<_>>();
            assert_eq!(
                uses.len(),
                1,
                "{script} should name the default checkout exactly once, found {}:\n{}",
                uses.len(),
                uses.join("\n"),
            );
            assert!(
                uses[0].contains(&expected),
                "{script} defaults the Verdandi checkout somewhere this code does not:\n  \
                 script: {}\n  expected to contain: {expected}",
                uses[0].trim(),
            );
        }
    }
}
