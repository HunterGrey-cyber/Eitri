//! Locates, builds if necessary, and spawns Verdandi's `apps/claude-sidecar` Node.js process --
//! deliberately NOT reusing `panel/src/supervisor_client.rs`'s spawn pattern wholesale. Two real,
//! verified differences from that pattern (see this plan's "Verified facts", points 5 and 6):
//!
//! 1. `apps/claude-sidecar/dist/` is gitignored in Verdandi, not committed -- a git-dependency
//!    checkout of the Rust protocol crate does NOT give this process a runnable sidecar.
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
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long `SpawnedSidecar::drop` waits after closing stdin before sending SIGKILL.
/// The sidecar first reaps its CLI: 250ms grace, then SIGTERM, then SIGKILL after 3s,
/// then a final 1s wait, for about 4.3s total. Killing the sidecar during that cleanup
/// can orphan the CLI. The 6s grace leaves more than 1.5s of margin.
pub const SIDECAR_EXIT_GRACE: Duration = Duration::from_secs(6);

/// How long a started sidecar is given to bind its socket before the start is called failed.
///
/// Before it binds, the sidecar loads its own single-executable Node binary (about 105 MB), the
/// Claude Agent SDK's module graph, and runs the host CLI's `--version` to check it, synchronously
/// (through a launcher script on some machines). With a cold page cache after boot that took over
/// three seconds, while the same sidecar warm binds in well under half a second: a 3 s budget made
/// the first start of a session fail and the retry work. Fifteen seconds is five times the
/// budget that proved too short, and only a slow start ever reaches it. A sidecar that refuses to
/// start is not waited out: the loop notices a dead child at once, so this only bounds one that is
/// alive and slow. The wait runs on the connect's own worker thread, never the GTK thread.
const SIDECAR_BIND_BUDGET: Duration = Duration::from_secs(15);
/// How often the socket is tried while the sidecar starts.
const SIDECAR_BIND_POLL: Duration = Duration::from_millis(50);

/// The timeout's summary line; it names the budget actually waited so the message cannot drift from it.
fn bind_timeout_summary(socket_path: &std::path::Path, budget: Duration) -> String {
    format!(
        "claude-sidecar did not bind {socket_path:?} within {}ms",
        budget.as_millis()
    )
}

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

/// Baseline sidecar revision for development and conformance testing.
/// Revision mismatches produce a startup diagnostic so the running build can be
/// identified. Checkout overrides allow testing unreleased sidecars and protocol
/// migrations without rejecting a different revision at startup.
///
/// Keep this value aligned with the protocol dependency revision in `agent/Cargo.toml`,
/// since that revision supplies the generated wire types compiled into this client.
///
/// Update this value after the ignored real-sidecar conformance tests pass against
/// the candidate revision. They cover multiple turns with content checks, BYPASS
/// tool execution, reuse after interruption, teardown without orphan processes,
/// resume verified by content and provider session ID, partial assistant streaming,
/// replay, recovery, and backpressure.
/// The unary conformance tests include a live handshake to check protocol agreement.
///
/// A packaged sidecar may have no checkout or `.git` directory to compare.
/// Protocol major is checked at handshake. It and the build-time `sidecar_version`
/// are carried on `ProviderInfo`. Keep this diagnostic until the client checks
/// the supported sidecar version range.
///
/// The baseline protocol includes `SetPermissionMode`, `PermissionModeChanged`,
/// and `TextDelta.message_id`.
///
/// For gated sessions using CLI `default`, the sidecar explicitly passes
/// `permissionMode: 'default'` to prevent local `.claude/settings.json` from widening permissions.
///
/// The protocol also includes `ClaudeHostPolicy.provider_permission_prompts`,
/// `PermissionRequested.origin` and its `provider_*` fields, `PermissionOrigin`,
/// `MatchedAskRule`, and the `provider_permission_prompts` capability.
///
/// Protocol 3.14 supports CLI auto mode under the gate through
/// `ClaudeHostPolicy.cli_permission_mode`, `ResolvePermissionRequest.defer`,
/// `PermissionOutcome.DEFERRED`, and the `PermissionDenied` event.
/// Its capabilities include `cli_auto_mode`, `permission_defer`, and
/// `permission_denied_events`. This client does not send or read
/// `await_account_identity`, `ToolAllowList.init_check`, the handshake fields
/// `egress_probe` and `structured_output_tools`, or `SessionReady.effective_*`.
/// Keep this value as the seven-character prefix of the full dependency revision:
/// the per-user sidecar directory and release checks use that prefix.
pub const EXPECTED_VERDANDI_REVISION: &str = "8e0f7e1";

/// Where `EITRI_VERDANDI_CHECKOUT` came from, and what it points at. Carried onto `ProviderInfo`
/// so the UI can name the backend build it is talking to.
#[derive(Debug)]
pub(crate) struct VerdandiCheckout {
    pub(crate) path: PathBuf,
    /// `git rev-parse --short HEAD`, or `None` when the checkout is not a git repo or `git` is
    /// unavailable. Best-effort diagnostics only -- never a reason to refuse to start.
    pub(crate) revision: Option<String>,
    /// True when `EITRI_VERDANDI_CHECKOUT` chose this path rather than the default.
    pub(crate) from_override: bool,
    /// A release artifact this checkout has ALREADY built for this machine, if any.
    ///
    /// When present it is run directly and `node <dist entry>` is never reached -- so a source tree
    /// and an installed copy execute the same executable rather than two shapes of the same code.
    /// Before this, every sidecar measurement taken on a development machine was of `node` out of a
    /// `dist/` tree while the shipped product ran a Node SEA binary, and nothing said so.
    pub(crate) prebuilt: Option<PathBuf>,
}

/// Where a checkout's own release build leaves its artifacts, relative to the checkout root.
/// Verdandi's `apps/claude-sidecar/scripts/buildBinary.mjs` writes
/// `dist-bin/verdandi-claude-sidecar-<version>-<platform>-<arch>`.
const CHECKOUT_ARTIFACT_DIR: &str = "apps/claude-sidecar/dist-bin";

/// The `<platform>-<arch>` suffix an artifact built for THIS machine carries, in Node's own
/// spelling (`process.platform`-`process.arch`), which is what that build script names files with.
///
/// Load-bearing rather than cosmetic. `dist-bin/` is one directory holding whatever was built last,
/// and a Verdandi checkout can be synced between machines -- this environment already syncs
/// `~/src` to a Mac. Running a `linux-x64` artifact on an arm64 Mac fails with an exec
/// error that names neither the platform nor this decision. `None` on a target this mapping does
/// not know, which simply leaves that machine on the `node <dist>` path rather than guessing.
fn node_platform_suffix() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("macos", "aarch64") => Some("darwin-arm64"),
        _ => None,
    }
}

/// A release artifact already built inside `checkout`, for this machine, if there is one.
///
/// Newest by mtime when several versions are present, which is the same rule `publish.sh` stages
/// with. Purely a lookup: it never builds anything, which is the whole point -- an artifact that is
/// already there costs nothing, and "run npm first" is exactly what must not happen behind a
/// backend that was chosen for the user.
fn prebuilt_artifact_in(checkout: &Path) -> Option<PathBuf> {
    let suffix = node_platform_suffix()?;
    let prefix = format!("{PACKAGED_SIDECAR_BINARY}-");
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(checkout.join(CHECKOUT_ARTIFACT_DIR)).ok()?.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if !name.starts_with(&prefix) || !name.ends_with(suffix) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else { continue };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if newest.as_ref().is_none_or(|(best, _)| modified > *best) {
            newest = Some((modified, entry.path()));
        }
    }
    newest.map(|(_, path)| path)
}

/// Where a dev machine's Verdandi checkout is looked for when nothing overrides it, relative to
/// `$HOME`. Named rather than inlined because the packaging scripts carry the same default and
/// nothing but `packaging_scripts_default_to_the_same_checkout_this_code_does` makes them agree --
/// they drifted for real: three of them said `verdandi-old-checkout`, a detached checkout from while
/// Verdandi's protocol-3 merge was outstanding, after their main absorbed it on 2026-09-18.
const DEFAULT_CHECKOUT_UNDER_HOME: &str = "src/verdandi";

/// Locates and validates a sidecar checkout for development or integration testing.
/// An explicit override takes precedence over the default checkout under `$HOME`.
/// An invalid override fails with an error instead of silently selecting a different sidecar.
/// The override and `home` are passed in rather than read from the environment here.
/// `resolve_sidecar_program` already needs the override to rank the checkout against a packaged
/// artifact, so sharing it keeps selection and lookup in sync. Passing `home` also lets tests
/// exercise the default path without accidentally using a real checkout on the test machine.
fn locate_verdandi_checkout(explicit: Option<&str>, home: Option<&OsStr>) -> std::io::Result<VerdandiCheckout> {
    let (path, from_override) = match explicit.map(str::trim).filter(|p| !p.is_empty()) {
        Some(path) => (PathBuf::from(path), true),
        None => {
            let home = home.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "HOME is unset and EITRI_VERDANDI_CHECKOUT was not provided",
                )
            })?;
            (PathBuf::from(home).join(DEFAULT_CHECKOUT_UNDER_HOME), false)
        }
    };

    // Validated for BOTH paths now. Previously only the default was checked, so a mistyped override
    // reached `ensure_sidecar_built` and failed somewhere much less informative.
    if !path.join("apps/claude-sidecar/package.json").exists() {
        let origin = if from_override {
            "EITRI_VERDANDI_CHECKOUT points at"
        } else {
            "no Verdandi checkout found at the default"
        };
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "{origin} {path:?}, which has no apps/claude-sidecar/package.json -- \
                 set EITRI_VERDANDI_CHECKOUT to a real Verdandi checkout"
            ),
        ));
    }

    let revision = git_short_revision(&path);
    let prebuilt = prebuilt_artifact_in(&path);
    Ok(VerdandiCheckout {
        path,
        revision,
        from_override,
        prebuilt,
    })
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
    let source = if checkout.from_override {
        "EITRI_VERDANDI_CHECKOUT"
    } else {
        "default path"
    };
    let revision = checkout.revision.as_deref().unwrap_or("unknown revision");
    let shape = match &checkout.prebuilt {
        Some(artifact) => format!(", running its prebuilt artifact {}", artifact.display()),
        None => String::new(),
    };
    let description = format!(
        "Verdandi checkout: {} @ {revision} (via {source}){shape}",
        checkout.path.display()
    );

    let mut warnings = Vec::new();
    // `starts_with` rather than equality: `git rev-parse --short` picks its own abbreviation length,
    // which grows as a repository does, so a strict comparison would start reporting false drift.
    if let Some(actual) = &checkout.revision {
        if !actual.starts_with(EXPECTED_VERDANDI_REVISION) && !EXPECTED_VERDANDI_REVISION.starts_with(actual.as_str()) {
            warnings.push(format!(
                "Verdandi baseline drift: running {actual}, this client was verified against \
                 {EXPECTED_VERDANDI_REVISION}. Not an error -- testing a Verdandi branch is exactly \
                 what EITRI_VERDANDI_CHECKOUT is for -- but if behavior looks wrong, this is the \
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
    // `build:terminal && build:claude`, and the terminal half is a workspace Eitri does not
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

/// Builds the user-facing message when the sidecar cannot be found, including when the binary
/// override names a path that is not a file. Reuse this text for the panel's failure message.
/// `agent-ui/web/src/problems.ts::classify` matches the exact opening sentence and treats the
/// remainder as the remedy. Changing that sentence requires updating the classifier's marker
/// to preserve the panel's headline. `problems.test.ts` checks it directly from the Rust source.
pub fn sidecar_missing_message(searched: &[PathBuf]) -> String {
    let mut message = String::from("The agent sidecar is not installed. Looked for it at:\n");
    for path in searched {
        message.push_str(&format!("  - {}\n", path.display()));
    }
    message.push_str(&format!(
        "Install the eitri package that ships it ({PACKAGED_SIDECAR_BINARY}), or point EITRI_SIDECAR_BINARY at a real one."
    ));
    message
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
    /// convention `agent-hook`, `eitri-supervisor` and `eitri-tmux-shim` already follow.
    Packaged(PathBuf),
    /// A Verdandi checkout, run through `node`.
    Checkout(VerdandiCheckout),
}

/// Resolves the sidecar program from caller-supplied values so precedence can be tested
/// without changing the process environment.
///
/// `EITRI_SIDECAR_BINARY`, an explicit binary, wins first. `EITRI_VERDANDI_CHECKOUT`, an explicit
/// checkout, comes next and is the only source that may trigger `ensure_sidecar_built`;
/// selecting a checkout explicitly permits building that checkout.
/// Next comes a packaged executable beside this binary. Its optional revision file is checked
/// against the pinned revision, and a mismatch produces a warning, as checkout drift does.
/// Next comes the per-user build at `user_sidecar_path`, where `eitri setup` installs it.
/// The path includes the pinned revision, so a build for another revision cannot be selected.
/// Finally, the default checkout is usable only if it already contains a prebuilt artifact.
/// The sidecar is the default backend, so building an implicitly found checkout would run a build
/// at session start without an explicit request. A checkout without an artifact is skipped.
/// Every returned `Checkout` with `prebuilt: None` therefore has `from_override: true`, which
/// lets `invocation_for` rely on that flag when building.
///
/// If nothing is found, the error includes `NO_SIDECAR_HINT` and the exact path `eitri setup`
/// would use, supplied by `no_sidecar_message`.
fn resolve_sidecar_program(
    explicit_binary: Option<&str>,
    explicit_checkout: Option<&str>,
    exe_dir: Option<&Path>,
    xdg_data_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> std::io::Result<SidecarProgram> {
    if let Some(binary) = explicit_binary.map(str::trim).filter(|b| !b.is_empty()) {
        let path = PathBuf::from(binary);
        if !path.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                sidecar_missing_message(std::slice::from_ref(&path)),
            ));
        }
        return Ok(SidecarProgram::Packaged(path));
    }
    if explicit_checkout.map(str::trim).is_some_and(|c| !c.is_empty()) {
        return Ok(SidecarProgram::Checkout(locate_verdandi_checkout(
            explicit_checkout,
            home,
        )?));
    }
    if let Some(sibling) = exe_dir.map(|d| d.join(PACKAGED_SIDECAR_BINARY)).filter(|p| p.is_file()) {
        return Ok(SidecarProgram::Packaged(sibling));
    }
    if let Some(user_path) = user_sidecar_path(xdg_data_home, home, EXPECTED_VERDANDI_REVISION).filter(|p| p.is_file())
    {
        return Ok(SidecarProgram::Packaged(user_path));
    }
    // Step 5: the default checkout counts only when it already holds a prebuilt artifact. A
    // missing or invalid checkout (`Err`, e.g. no `HOME`, or no `apps/claude-sidecar/package.json`)
    // is swallowed here rather than propagated -- it means exactly the same thing as a present
    // checkout with nothing built: there is nothing to run without building, and building here is
    // exactly what must not happen.
    if let Ok(checkout) = locate_verdandi_checkout(None, home) {
        if checkout.prebuilt.is_some() {
            return Ok(SidecarProgram::Checkout(checkout));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        no_sidecar_message(xdg_data_home, home),
    ))
}

/// Reports sidecar availability for the backend's startup diagnostic.
/// This result selects the message, not the backend; legacy is never a fallback.
/// Availability is ranked like `resolve_sidecar_program` so the diagnostic describes what
/// startup will actually use, including an explicit checkout that still needs a build.
/// `SidecarAvailability::Runnable` means there is an executable with nothing to build.
/// A checkout qualifies when `prebuilt_artifact_in` finds an artifact for this machine;
/// a per-user build at `user_sidecar_path` qualifies for the same reason.
/// Using the prebuilt artifact also lets source builds run the same executable as an installed
/// copy, rather than measuring a separate `node` process running a `dist/` tree.
pub fn sidecar_availability() -> SidecarAvailability {
    // Read `HOME` once and pass it to `checkout_path_for` and `sidecar_availability_in`.
    // Using `var_os` preserves non-UTF-8 paths; a separate `var` call would reject them and
    // could make the two lookups disagree.
    let home = std::env::var_os("HOME");
    let explicit_checkout = std::env::var("EITRI_VERDANDI_CHECKOUT").ok();
    let checkout_is_explicit = explicit_checkout
        .as_deref()
        .map(str::trim)
        .is_some_and(|c| !c.is_empty());
    sidecar_availability_in(
        std::env::var("EITRI_SIDECAR_BINARY").ok().as_deref(),
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .as_deref(),
        checkout_path_for(explicit_checkout.as_deref(), home.as_deref()).as_deref(),
        checkout_is_explicit,
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        home.as_deref(),
    )
}

/// [`sidecar_availability`]'s answer. Decides nothing (see that function's correction): every value
/// selects the sidecar backend, and each only picks the line `BackendKind::choose` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarAvailability {
    /// Something runs with nothing to build: a named `EITRI_SIDECAR_BINARY` that is a file, the
    /// packaged sibling, the per-user build, or a checkout holding a prebuilt artifact.
    Runnable,
    /// `EITRI_VERDANDI_CHECKOUT` names a checkout with nothing prebuilt for this machine, so the
    /// first session start builds it there (`npm`, discovery step 2) -- naming a checkout is the act
    /// that authorizes that build. Only an explicit checkout can be this; the default one never
    /// builds (step 5).
    BuildsNamedCheckout,
    /// Nothing to run: a session start fails with [`NO_SIDECAR_HINT`] (or, for a named binary that
    /// is not a file, `sidecar_missing_message`).
    Missing,
}

/// [`sidecar_availability`] with its inputs passed in, so the matrix is testable without mutating
/// the process environment -- the same reason `BackendKind::choose` takes the answer as an
/// argument rather than calling this.
///
/// **Ranked exactly like `resolve_sidecar_program`**: the first step that applies
/// decides, so the startup line and the spawn cannot describe different sidecars. This used to be an
/// OR over the same inputs, which answered "available" for an explicit checkout with nothing built
/// beside an installed artifact while the spawn built that checkout; that was harmless while the
/// answer only chose the default backend, and wrong once it chose the words. `checkout` is
/// `checkout_path_for`'s answer and `checkout_is_explicit` says whether it came from
/// `EITRI_VERDANDI_CHECKOUT` (step 2) or the default location (step 5).
fn sidecar_availability_in(
    named_binary: Option<&str>,
    exe_dir: Option<&Path>,
    checkout: Option<&Path>,
    checkout_is_explicit: bool,
    xdg_data_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> SidecarAvailability {
    let prebuilt = |path: &Path| prebuilt_artifact_in(path).is_some();
    // Step 1: a named binary wins, and `resolve_sidecar_program` refuses one that is not a file.
    if let Some(binary) = named_binary.map(str::trim).filter(|b| !b.is_empty()) {
        return if Path::new(binary).is_file() {
            SidecarAvailability::Runnable
        } else {
            SidecarAvailability::Missing
        };
    }
    // Step 2: a named checkout wins over everything below it, built or not.
    if checkout_is_explicit {
        return match checkout {
            Some(path) if prebuilt(path) => SidecarAvailability::Runnable,
            _ => SidecarAvailability::BuildsNamedCheckout,
        };
    }
    // Steps 3-5: the sibling, the per-user build, a default checkout holding a prebuilt artifact.
    let sibling = exe_dir.is_some_and(|dir| dir.join(PACKAGED_SIDECAR_BINARY).is_file());
    let user_build =
        user_sidecar_path(xdg_data_home, home, EXPECTED_VERDANDI_REVISION).is_some_and(|path| path.is_file());
    if sibling || user_build || checkout.is_some_and(prebuilt) {
        SidecarAvailability::Runnable
    } else {
        SidecarAvailability::Missing
    }
}

/// Returns the checkout path, preferring the override to the default under `$HOME`.
/// Leaves validation to checkout resolution: this path only feeds an artifact-existence probe,
/// so a missing checkout simply yields no artifact.
/// Takes `home` from `sidecar_availability`, which reads it once for `sidecar_availability_in`.
/// Sharing that value keeps the lookups consistent for non-UTF-8 paths, which `std::env::var`
/// rejects but `var_os` preserves.
fn checkout_path_for(explicit: Option<&str>, home: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(path) = explicit.map(str::trim).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    home.map(|home| PathBuf::from(home).join(DEFAULT_CHECKOUT_UNDER_HOME))
}

/// Returns the path to a per-user sidecar build, keyed by the exact revision so a build for
/// another revision cannot be selected. The filename is `PACKAGED_SIDECAR_BINARY`.
/// Uses `XDG_DATA_HOME` when it is absolute; otherwise uses `<home>/.local/share`.
/// Unset, empty and relative `XDG_DATA_HOME` values all use that fallback.
/// Returns `None` if neither value provides an absolute base, avoiding resolution against the
/// process's current directory.
/// This mirrors `core::layout::persist::state_subdir` and `state_dir`. The helper cannot be
/// shared here because `core` depends on `agent`, so this crate cannot depend on `core`.
/// `state_dirs::conversations_dir` differs: it accepts a relative `$HOME` and refuses only
/// when `$HOME` is unset.
pub fn user_sidecar_path(xdg_data_home: Option<&OsStr>, home: Option<&OsStr>, rev: &str) -> Option<PathBuf> {
    let base = match xdg_data_home {
        Some(data_home) if Path::new(data_home).is_absolute() => PathBuf::from(data_home),
        _ => match home {
            Some(home) if Path::new(home).is_absolute() => PathBuf::from(home).join(".local/share"),
            _ => return None,
        },
    };
    Some(base.join("eitri/sidecar").join(rev).join(PACKAGED_SIDECAR_BINARY))
}

/// Shared hint for backend selection and resolution when no runnable sidecar is found.
/// It starts with the same sentence as `sidecar_missing_message` so the UI can recognise the
/// failure and show the remaining text as a remedy. Only the opening sentence is shared:
/// an explicitly missing binary needs a different remedy from exhausted discovery.
/// The constant omits the user path, which depends on runtime environment variables, and
/// the expected revision, since `concat!` cannot interpolate a constant identifier.
/// `no_sidecar_message` appends those details at runtime without duplicating the revision literal.
pub const NO_SIDECAR_HINT: &str = "The agent sidecar is not installed. Run \"eitri setup\" -- it builds one for you.";

/// The full message for `resolve_sidecar_program`'s last resort (step 5 finding no prebuilt
/// checkout, after steps 1-4 already found nothing): `NO_SIDECAR_HINT` plus the pinned revision and
/// the exact path `eitri setup` would write to -- the same path `user_sidecar_path` (step 4) just
/// looked at and did not find. Falls back to naming what is missing, rather than silently omitting
/// the path, when neither `$XDG_DATA_HOME` nor `$HOME` gives that function anywhere to answer.
fn no_sidecar_message(xdg_data_home: Option<&OsStr>, home: Option<&OsStr>) -> String {
    match user_sidecar_path(xdg_data_home, home, EXPECTED_VERDANDI_REVISION) {
        Some(path) => format!(
            "{NO_SIDECAR_HINT} (verdandi {EXPECTED_VERDANDI_REVISION}, into {})",
            path.display()
        ),
        None => format!(
            "{NO_SIDECAR_HINT} (verdandi {EXPECTED_VERDANDI_REVISION}, but cannot say where -- neither \
             $XDG_DATA_HOME nor $HOME is a usable absolute path)"
        ),
    }
}

/// Everything the sidecar child's process image is, in one place a test can read back.
///
/// `VERDANDI_CLAUDE_ACCOUNT` is the whole of the account handoff (2026-09-21). Verdandi's
/// `packages/claude-runtime/src/account.ts` resolves it **once for the sidecar process** and then
/// sets the full `CLAUDE_PROFILE`/`CLAUDE_CONFIG_DIR`/`CLAUDE_SECURESTORAGE_CONFIG_DIR`/
/// `ANTHROPIC_CONFIG_DIR` tuple on every `claude` it spawns -- overriding whatever this process
/// inherited. So Eitri hands over the *name* and lets the sidecar derive the rest: writing the
/// tuple here as well would be a second copy of a convention that already lives on one side, and
/// the copy that drifted would win silently on some hosts and lose on others. There is no proto
/// field for this and none is needed -- it is a property of the process Eitri itself starts.
///
/// With no account configured this sets exactly what it always set, and the sidecar's own account
/// support does not engage at all (`VERDANDI_CLAUDE_ACCOUNT` unset is its shipped default).
fn sidecar_command(
    program_path: &Path,
    program_args: &[PathBuf],
    socket_path: &Path,
    account: Option<&crate::account::ClaudeAccount>,
) -> Command {
    let mut command = Command::new(program_path);
    command
        .args(program_args)
        .env("VERDANDI_CLAUDE_SIDECAR_SOCKET", socket_path)
        .stdin(Stdio::piped())
        // stdout/stderr are captured, not nulled -- `supervisor`'s own follow-up review found that
        // nulling a spawned child's stderr silently discards real crash diagnostics (2026-09-09,
        // supervisor-robustness-followup plan). A Node.js sidecar crash during real CLI parity
        // testing is exactly the kind of thing worth seeing.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The sidecar merges this variable's MCP servers into every session it serves. Eitri states
        // which servers a session gets and states none today, so a value left in the user's shell
        // must not turn into servers (and tools) in an Eitri session.
        .env_remove(EXTRA_MCP_SERVERS_VAR);
    if let Some(account) = account {
        command.env("VERDANDI_CLAUDE_ACCOUNT", account.name());
    }
    command
}

/// The sidecar's process-level list of extra MCP servers (JSON); never inherited from the shell.
const EXTRA_MCP_SERVERS_VAR: &str = "VERDANDI_CLAUDE_EXTRA_MCP_SERVERS";

/// True when `path` is exactly discovery step 3's candidate -- `current_exe()`'s sibling -- rather
/// than an explicitly named `EITRI_SIDECAR_BINARY` (step 1) or the per-user rev-keyed path (step
/// 4). Compared by path instead of adding a third `SidecarProgram::Packaged` shape to every match
/// arm and existing test in this file, for a distinction only `sibling_rev_skew_warning`'s call site
/// needs.
fn is_exe_sibling_artifact(path: &Path, exe_dir: Option<&Path>) -> bool {
    exe_dir.map(|dir| dir.join(PACKAGED_SIDECAR_BINARY)).as_deref() == Some(path)
}

/// Warns when the sibling artifact's optional `<PACKAGED_SIDECAR_BINARY>.rev` file names
/// a different revision, indicating revision drift. A missing or empty file, or a prefix match in either direction,
/// gives no warning. Accepting both prefix directions handles differing revision abbreviation
/// lengths, using the same comparison as `describe_checkout`.
fn sibling_rev_skew_warning(artifact_path: &Path) -> Vec<String> {
    let Some(dir) = artifact_path.parent() else {
        return Vec::new();
    };
    let Ok(actual) = std::fs::read_to_string(dir.join(format!("{PACKAGED_SIDECAR_BINARY}.rev"))) else {
        return Vec::new();
    };
    let actual = actual.trim();
    if actual.is_empty()
        || actual.starts_with(EXPECTED_VERDANDI_REVISION)
        || EXPECTED_VERDANDI_REVISION.starts_with(actual)
    {
        return Vec::new();
    }
    vec![format!(
        "Verdandi baseline drift: the sidecar beside this binary was built for {actual}, this client \
         was verified against {EXPECTED_VERDANDI_REVISION}. Not an error -- but if behavior looks \
         wrong, this is the first thing to check."
    )]
}

/// Turns a resolved `SidecarProgram` into its description, warnings, executable and arguments.
/// An explicit checkout with `prebuilt: None` invokes the injected `build` closure;
/// prebuilt checkouts and packaged artifacts do not. The closure lets tests check both paths
/// without running `npm` or `node`, or changing process environment variables.
fn invocation_for(
    program: SidecarProgram,
    exe_dir: Option<&Path>,
    build: impl FnOnce(&Path) -> std::io::Result<PathBuf>,
) -> std::io::Result<(String, Vec<String>, PathBuf, Vec<PathBuf>)> {
    Ok(match program {
        SidecarProgram::Packaged(path) => {
            // Only a discovery-step-3 sibling of THIS binary carries an optional `.rev` file to
            // check -- an explicitly named `EITRI_SIDECAR_BINARY` was chosen on
            // purpose, and the per-user path (step 4) is already keyed by rev in its own directory
            // name, so neither needs this warning.
            let warnings = if is_exe_sibling_artifact(&path, exe_dir) {
                sibling_rev_skew_warning(&path)
            } else {
                Vec::new()
            };
            (
                format!("packaged sidecar artifact: {}", path.display()),
                warnings,
                path,
                Vec::new(),
            )
        }
        SidecarProgram::Checkout(checkout) => {
            let (description, warnings) = describe_checkout(&checkout);
            match checkout.prebuilt {
                // The same executable the package ships, found already built in the checkout. Taken
                // over `node <dist>` deliberately: a developer machine then runs the shape that
                // ships, and nothing here has to start `npm`.
                Some(artifact) => (description, warnings, artifact, Vec::new()),
                // `build` runs only on this path. A packaged artifact has nothing to build, and
                // reaching for a build step there would be the bug this branch exists to avoid.
                None => {
                    let dist_entry = build(&checkout.path)?;
                    (description, warnings, PathBuf::from("node"), vec![dist_entry])
                }
            }
        }
    })
}

/// Spawns a fresh sidecar for one `ClaudeSidecarProvider` instance. `instance_id` becomes part of
/// the socket path (`crate::socket_path::sidecar_socket`, which mirrors the legacy backend's own
/// per-conversation UUID socket and keeps both under macOS's 103-byte socket-path limit) so
/// multiple concurrent providers never collide on one path.
pub(crate) fn spawn(instance_id: &str) -> std::io::Result<SpawnedSidecar> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let xdg_data_home = std::env::var_os("XDG_DATA_HOME");
    let home = std::env::var_os("HOME");
    let program = resolve_sidecar_program(
        std::env::var("EITRI_SIDECAR_BINARY").ok().as_deref(),
        std::env::var("EITRI_VERDANDI_CHECKOUT").ok().as_deref(),
        exe_dir.as_deref(),
        xdg_data_home.as_deref(),
        home.as_deref(),
    )?;
    let (build_description, build_warnings, program_path, program_args) =
        invocation_for(program, exe_dir.as_deref(), ensure_sidecar_built)?;
    eprintln!("agent: {build_description}");
    for line in &build_warnings {
        eprintln!("agent: {line}");
    }
    let socket_path = crate::socket_path::sidecar_socket(&std::env::temp_dir(), instance_id)?;
    let _ = std::fs::remove_file(&socket_path); // stale leftover from a prior crash, if any

    let account = crate::account::configured();
    if let Some(account) = account {
        eprintln!(
            "agent: sidecar pinned to claude account '{}' ({})",
            account.name(),
            account.config_dir().display()
        );
    }
    let mut command = sidecar_command(&program_path, &program_args, &socket_path, account);

    let mut child = command.spawn()?;
    let stdin_keepalive = child.stdin.take();

    let stderr_tail = Arc::new(Mutex::new(StderrTail::default()));
    if let Some(stdout) = child.stdout.take() {
        spawn_log_drain_thread(stdout, "stdout", None);
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_log_drain_thread(stderr, "stderr", Some(Arc::clone(&stderr_tail)));
    }

    let started_waiting = std::time::Instant::now();
    loop {
        if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
            return Ok(SpawnedSidecar {
                socket_path,
                build_description,
                build_warnings,
                stdin_keepalive,
                child,
                stderr_tail,
            });
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
        if started_waiting.elapsed() >= SIDECAR_BIND_BUDGET {
            break;
        }
        std::thread::sleep(SIDECAR_BIND_POLL);
    }
    // Still running but never bound -- clean up rather than leaking a half-started process, then
    // report the failure with whatever it managed to say for itself.
    drop(stdin_keepalive);
    let _ = child.kill();
    let _ = child.wait();
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        describe_startup_failure(&bind_timeout_summary(&socket_path, SIDECAR_BIND_BUDGET), &stderr_tail),
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

/// Polls `child` for up to `grace`, then calls `kill()` and `wait()` if it has not exited.
/// Keeping this separate from `SpawnedSidecar::drop` lets tests exercise the shutdown sequence
/// with a real child process without constructing a whole sidecar.
pub(crate) fn stop_child(child: &mut Child, grace: Duration) -> std::io::Result<ExitStatus> {
    let deadline = std::time::Instant::now() + grace;
    loop {
        match child.try_wait()? {
            Some(status) => return Ok(status),
            None if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            None => break,
        }
    }
    child.kill()?;
    child.wait()
}

impl Drop for SpawnedSidecar {
    /// Closes the held-open stdin to signal shutdown, then waits up to `SIDECAR_EXIT_GRACE`
    /// for the sidecar to reap its CLI and cancel pending permissions before exiting.
    /// If the sidecar is still running after the grace period, kills it and waits for it to exit.
    fn drop(&mut self) {
        drop(self.stdin_keepalive.take()); // closes the write end -> sidecar sees stdin EOF
        let _ = stop_child(&mut self.child, SIDECAR_EXIT_GRACE);
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
        let resolved = resolve_sidecar_program(artifact.to_str(), Some("/definitely/not/a/checkout"), None, None, None)
            .expect("an existing artifact resolves");
        assert!(
            matches!(&resolved, SidecarProgram::Packaged(p) if *p == artifact),
            "{resolved:?}"
        );
        let _ = std::fs::remove_file(&artifact);
    }

    /// A named artifact that does not exist is a hard error, not a silent fall-through to a
    /// checkout. Falling through would run a DIFFERENT build than the one named and report success,
    /// which is the failure mode every override in this crate is written to avoid.
    #[test]
    fn a_named_artifact_that_is_missing_fails_rather_than_falling_back() {
        let err = resolve_sidecar_program(Some("/no/such/sidecar/binary"), None, None, None, None)
            .expect_err("a named artifact that is absent must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(err.to_string().contains("EITRI_SIDECAR_BINARY"), "{err}");
        // Keep the shared `sidecar_missing_message` opening sentence and the searched path in the
        // error, so edits to the error path or message cannot silently remove either.
        assert!(err.to_string().contains("The agent sidecar is not installed."), "{err}");
        assert!(err.to_string().contains("/no/such/sidecar/binary"), "{err}");
    }

    /// The missing-sidecar message names every searched path and the package that supplies it.
    /// The UI splits the text after the shared opening sentence into the displayed remedy.
    #[test]
    fn sidecar_missing_message_names_every_searched_path_and_the_package() {
        let searched = [PathBuf::from("/a/b/sidecar"), PathBuf::from("/c/d/sidecar")];
        let message = sidecar_missing_message(&searched);
        assert!(message.starts_with("The agent sidecar is not installed."), "{message}");
        assert!(message.contains("/a/b/sidecar"), "{message}");
        assert!(message.contains("/c/d/sidecar"), "{message}");
        assert!(message.contains(PACKAGED_SIDECAR_BINARY), "{message}");
        assert!(message.contains("EITRI_SIDECAR_BINARY"), "{message}");
    }

    /// A sibling artifact is used when nothing was named -- the shape a real install has, and the
    /// same `current_exe()`-sibling convention `agent-hook` and `eitri-supervisor` already follow.
    #[test]
    fn a_sibling_artifact_is_found_when_nothing_is_named() {
        let dir = std::env::temp_dir().join(format!("nv-sidecar-dir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();
        let resolved = resolve_sidecar_program(None, None, Some(&dir), None, None).expect("the sibling resolves");
        assert!(
            matches!(&resolved, SidecarProgram::Packaged(p) if *p == artifact),
            "{resolved:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An explicit checkout beats a sibling artifact. A developer pointing at a branch wants that
    /// branch; preferring the shipped artifact would make `EITRI_VERDANDI_CHECKOUT` look broken
    /// on exactly the machines where both exist -- which, once Eitri ships the artifact, is every
    /// developer machine.
    #[test]
    fn an_explicit_checkout_beats_a_sibling_artifact() {
        let dir = std::env::temp_dir().join(format!("nv-sidecar-dir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(PACKAGED_SIDECAR_BINARY), b"#!/bin/sh\nexit 0\n").unwrap();
        // Points at a directory that is not a checkout, so this resolves to the Checkout ARM and
        // then fails inside it -- which is the observation: the sibling was not chosen.
        let err = resolve_sidecar_program(None, Some("/definitely/not/a/checkout"), Some(&dir), None, None)
            .expect_err("an explicit checkout that is not one must fail rather than silently using the sibling");
        assert!(
            !err.to_string().contains(PACKAGED_SIDECAR_BINARY),
            "the sibling artifact was used despite an explicit checkout: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An empty override is treated as absent. `EITRI_SIDECAR_BINARY=` in a wrapper script is a
    /// way of saying "not set", and honouring it literally would fail every spawn with a path that
    /// is the empty string.
    #[test]
    fn an_empty_override_is_not_an_override() {
        let dir = std::env::temp_dir().join(format!("nv-sidecar-dir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();
        let resolved = resolve_sidecar_program(Some("   "), Some(""), Some(&dir), None, None)
            .expect("blank overrides fall through to the sibling");
        assert!(
            matches!(&resolved, SidecarProgram::Packaged(p) if *p == artifact),
            "{resolved:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_bind_budget_outlasts_a_cold_start_and_the_timeout_names_it() {
        // A cold start measured over 3 s; the budget must clear that with room, and stay a bound.
        assert!(SIDECAR_BIND_BUDGET >= Duration::from_secs(10));
        assert!(SIDECAR_BIND_BUDGET <= Duration::from_secs(30));
        assert!(SIDECAR_BIND_POLL <= Duration::from_millis(100));
        let message = bind_timeout_summary(std::path::Path::new("/run/x/endpoint"), SIDECAR_BIND_BUDGET);
        assert!(message.contains("within 15000ms"), "{message}");
        assert!(message.contains("did not bind"), "{message}");
    }

    /// The sidecar reaps its CLIs before exiting: 250 ms, SIGTERM, then SIGKILL after 3 s and
    /// another 1 s before giving up. Killing it sooner can orphan the CLI it is shutting down.
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn the_grace_covers_the_sidecars_own_reap() {
        assert!(super::SIDECAR_EXIT_GRACE >= Duration::from_millis(250 + 3000 + 1000 + 500));
    }

    #[test]
    fn a_child_that_exits_inside_the_grace_is_not_killed() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "read _; sleep 0.3; exit 7"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        drop(child.stdin.take());
        let status = super::stop_child(&mut child, Duration::from_secs(2)).unwrap();
        assert_eq!(status.code(), Some(7), "exited on its own: {status:?}");
    }

    #[test]
    fn a_child_still_running_after_the_grace_is_killed() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = std::process::Command::new("sh")
            .args(["-c", "trap '' TERM; read _; sleep 30"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        drop(child.stdin.take());
        let started = std::time::Instant::now();
        let status = super::stop_child(&mut child, Duration::from_millis(200)).unwrap();
        assert_eq!(status.signal(), Some(9));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    use super::*;

    /// Spawns the compiled sidecar, checks that its socket accepts connections, and confirms
    /// that closing the held-open stdin makes it exit. No CLI turn is sent.
    /// Requires a prebuilt artifact, a per-user build, or an explicit checkout override that may
    /// build on demand. An unbuilt default checkout fails with `NO_SIDECAR_HINT`.
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

    /// Checks that every default checkout named by the scripts matches the path computed here.
    /// Inspecting literal script text catches disagreements even when neither path exists locally.
    /// The launcher and root installer are excluded because they no longer supply checkout defaults.
    #[test]
    fn packaging_scripts_default_to_the_same_checkout_this_code_does() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("agent/ has a parent");
        let expected = format!("${{EITRI_VERDANDI_CHECKOUT:-$HOME/{DEFAULT_CHECKOUT_UNDER_HOME}}}");

        for script in [
            "try-eitri.sh",
            // publish.sh (the private deploy script) does not ship publicly.
        ] {
            let path = repo_root.join(script);
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            let uses = text
                .lines()
                .filter(|l| l.contains("EITRI_VERDANDI_CHECKOUT:-"))
                .collect::<Vec<_>>();
            // Require at least one default and check every occurrence. A script may need the default
            // in more than one place; checking only `uses[0]` would miss a conflicting value elsewhere.
            assert!(!uses.is_empty(), "{script} never names the default Verdandi checkout");
            for line in &uses {
                assert!(
                    line.contains(&expected),
                    "{script} defaults the Verdandi checkout somewhere this code does not:\n  \
                     script: {}\n  expected to contain: {expected}",
                    line.trim(),
                );
            }
        }
    }

    /// The other half of the change above: `eitri.launcher.sh` does not merely stop AGREEING
    /// with this default -- it stops naming `EITRI_VERDANDI_CHECKOUT` as a default for anything
    /// at all. A stray `EITRI_VERDANDI_CHECKOUT:-...` line re-added there (e.g. by a bad merge
    /// with an older revision of this file) would silently reintroduce the exact drift the test
    /// above exists to catch, one script at a time -- while itself passing, since a script this
    /// test no longer scans can drift freely.
    #[test]
    fn the_launcher_no_longer_defaults_the_verdandi_checkout() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("agent/ has a parent");
        let path = repo_root.join("packaging/eitri.launcher.sh");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        assert!(
            !text.contains("EITRI_VERDANDI_CHECKOUT:-"),
            "packaging/eitri.launcher.sh still defaults EITRI_VERDANDI_CHECKOUT to a path"
        );
    }

    /// The root `install.sh`'s own half of the same change: it is a thin
    /// wrapper over `packaging/install.sh --from-source --checkout`, passing `--verdandi-checkout`
    /// only `if` the variable is set -- never defaulted to a guessed path (a stranger's checkout has
    /// no `~/src/verdandi`) the way the old script's own default silently disagreed with
    /// this file. `${EITRI_VERDANDI_CHECKOUT:-}` (an empty-string default, just to test whether it
    /// is set) is not the thing being ruled out here and is expected to appear.
    #[test]
    fn the_root_install_sh_no_longer_defaults_the_verdandi_checkout() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("agent/ has a parent");
        let path = repo_root.join("install.sh");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        assert!(
            !text.contains(&format!("EITRI_VERDANDI_CHECKOUT:-$HOME/{DEFAULT_CHECKOUT_UNDER_HOME}"))
                && !text.contains("EITRI_VERDANDI_CHECKOUT:-$HOME"),
            "the root install.sh still defaults EITRI_VERDANDI_CHECKOUT to a guessed path"
        );
        assert!(
            text.contains("--from-source") && text.contains("--checkout"),
            "the root install.sh no longer looks like packaging/install.sh --from-source --checkout's wrapper"
        );
    }

    /// Builds a directory that `locate_verdandi_checkout` accepts, optionally holding artifacts.
    /// Returns the checkout root; the caller removes it.
    fn fake_checkout(artifacts: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("nv-checkout-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("apps/claude-sidecar")).unwrap();
        std::fs::write(root.join("apps/claude-sidecar/package.json"), b"{}").unwrap();
        if !artifacts.is_empty() {
            std::fs::create_dir_all(root.join(CHECKOUT_ARTIFACT_DIR)).unwrap();
        }
        for name in artifacts {
            std::fs::write(root.join(CHECKOUT_ARTIFACT_DIR).join(name), b"#!/bin/sh\nexit 0\n").unwrap();
        }
        root
    }

    /// A fake `$HOME` whose `src/verdandi` is a checkout `locate_verdandi_checkout`
    /// accepts (mirroring `fake_checkout`, but nested at the exact path step 5's default-checkout
    /// lookup uses), optionally holding artifacts. Returns the home root; the caller removes it.
    ///
    /// Exists because this development machine's REAL `$HOME/src/verdandi` really does
    /// hold a prebuilt sidecar artifact -- so a test of "what happens with no user override and the
    /// real default" that read `std::env::var("HOME")` directly would pass against the wrong
    /// checkout on this machine and differently on a fresh one. `locate_verdandi_checkout` and
    /// `resolve_sidecar_program` take `home` as a parameter for exactly this reason.
    fn fake_home_with_default_checkout(artifacts: &[&str]) -> PathBuf {
        let home = std::env::temp_dir().join(format!("nv-home-{}", uuid::Uuid::new_v4()));
        let checkout = home.join(DEFAULT_CHECKOUT_UNDER_HOME);
        std::fs::create_dir_all(checkout.join("apps/claude-sidecar")).unwrap();
        std::fs::write(checkout.join("apps/claude-sidecar/package.json"), b"{}").unwrap();
        if !artifacts.is_empty() {
            std::fs::create_dir_all(checkout.join(CHECKOUT_ARTIFACT_DIR)).unwrap();
        }
        for name in artifacts {
            std::fs::write(checkout.join(CHECKOUT_ARTIFACT_DIR).join(name), b"#!/bin/sh\nexit 0\n").unwrap();
        }
        home
    }

    /// A platform suffix this machine is definitely not, so a test can plant an artifact that must
    /// be ignored. Derived from the real mapping rather than hardcoded, so it stays wrong-on-purpose
    /// on every target instead of only on x86_64 Linux.
    fn foreign_platform_suffix() -> &'static str {
        match node_platform_suffix() {
            Some("darwin-arm64") => "linux-x64",
            _ => "darwin-arm64",
        }
    }

    /// The correction this predicate carries in its own doc, as a test: a checkout that has ALREADY
    /// built an artifact for this machine makes the sidecar startable, because running it involves
    /// no build. Before 2026-09-18 this answered `false` and every source build silently stayed on
    /// the legacy backend -- which is the backend with no partial streaming, so the product's most
    /// visible complaint was reachable by the default on the machine its author uses.
    #[test]
    fn a_checkout_holding_a_built_artifact_counts_as_available() {
        let suffix = node_platform_suffix().expect("this test target has a known Node platform name");
        let root = fake_checkout(&[&format!("{PACKAGED_SIDECAR_BINARY}-0.1.0-{suffix}")]);
        assert_eq!(
            sidecar_availability_in(None, None, Some(&root), false, None, None),
            SidecarAvailability::Runnable
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// And the half of the original guarantee that was actually about something: a checkout with
    /// nothing built still does not count. Counting it would put the first start of a source build
    /// into `npm ci` and a TypeScript build, with no UI saying why, behind a backend nobody chose.
    #[test]
    fn a_checkout_with_nothing_built_is_not_availability() {
        let root = fake_checkout(&[]);
        assert_eq!(
            sidecar_availability_in(None, None, Some(&root), false, None, None),
            SidecarAvailability::Missing
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// An artifact built for a different machine is not an artifact. `dist-bin/` holds whatever was
    /// built last and this environment syncs `~/src` to a Mac, so a `linux-x64` binary
    /// really can be sitting in an arm64 machine's checkout. Picking it would fail at exec with a
    /// message naming neither the platform nor this decision.
    #[test]
    fn an_artifact_built_for_another_platform_is_neither_picked_nor_counted() {
        let root = fake_checkout(&[&format!(
            "{PACKAGED_SIDECAR_BINARY}-0.1.0-{}",
            foreign_platform_suffix()
        )]);
        assert_eq!(prebuilt_artifact_in(&root), None);
        assert_eq!(
            sidecar_availability_in(None, None, Some(&root), false, None, None),
            SidecarAvailability::Missing
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Several versions in one `dist-bin/` resolve to the newest, the same rule `publish.sh` stages
    /// with -- so a rebuild takes effect without anyone cleaning the directory first.
    #[test]
    fn the_newest_artifact_wins_when_a_checkout_holds_several() {
        let suffix = node_platform_suffix().expect("this test target has a known Node platform name");
        let older = format!("{PACKAGED_SIDECAR_BINARY}-0.1.0-{suffix}");
        let newer = format!("{PACKAGED_SIDECAR_BINARY}-0.2.0-{suffix}");
        let root = fake_checkout(&[&older, &newer]);
        // Written in one loop above, so mtimes can tie at this filesystem's resolution. Make the
        // ordering real rather than assuming creation order survived.
        let newer_path = root.join(CHECKOUT_ARTIFACT_DIR).join(&newer);
        filetime_bump(&newer_path);
        assert_eq!(prebuilt_artifact_in(&root), Some(newer_path));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Rewrites `path` so its mtime is strictly later than its siblings', without depending on the
    /// clock: `std::fs::write` stamps the current time, and a second write after a real sleep is the
    /// only portable way to guarantee a difference on a coarse-resolution filesystem.
    fn filetime_bump(path: &Path) {
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
    }

    /// The whole reason for the widening: a checkout with a built artifact runs THAT, not `node` out
    /// of a `dist/` tree, so a developer machine executes the same binary the package ships.
    #[test]
    fn a_checkout_with_a_built_artifact_resolves_to_running_it() {
        let suffix = node_platform_suffix().expect("this test target has a known Node platform name");
        let artifact_name = format!("{PACKAGED_SIDECAR_BINARY}-0.1.0-{suffix}");
        let root = fake_checkout(&[&artifact_name]);
        let resolved = resolve_sidecar_program(None, root.to_str(), None, None, None).expect("the checkout resolves");
        let SidecarProgram::Checkout(checkout) = resolved else {
            panic!("an explicit checkout must resolve to the checkout shape: {resolved:?}");
        };
        assert_eq!(
            checkout.prebuilt,
            Some(root.join(CHECKOUT_ARTIFACT_DIR).join(&artifact_name))
        );
        // And it says so, because "which of the two shapes ran" is not otherwise visible.
        let (description, _) = describe_checkout(&checkout);
        assert!(description.contains("prebuilt artifact"), "{description}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A checkout with nothing built keeps the `node <dist>` shape, and says nothing about an
    /// artifact -- the negative control for the assertion above.
    #[test]
    fn a_checkout_with_nothing_built_still_describes_the_node_shape() {
        let root = fake_checkout(&[]);
        let resolved = resolve_sidecar_program(None, root.to_str(), None, None, None).expect("the checkout resolves");
        let SidecarProgram::Checkout(checkout) = resolved else {
            panic!("{resolved:?}")
        };
        assert_eq!(checkout.prebuilt, None);
        let (description, _) = describe_checkout(&checkout);
        assert!(!description.contains("prebuilt"), "{description}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Reads the account back off the real `Command` the real spawn path builds -- the env map, not
    /// a stand-in for it -- so deleting the `VERDANDI_CLAUDE_ACCOUNT` line fails here.
    #[test]
    fn a_configured_account_reaches_the_sidecar_child_as_one_named_variable() {
        let account =
            crate::account::ClaudeAccount::resolve_from("work", |k| (k == "HOME").then(|| "/home/user".to_string()))
                .unwrap();
        // Built through `socket_path`, not spelled out, so this test cannot become the one place
        // in the crate that invents a socket path (`socket_path`'s own scanner enforces that).
        let socket = crate::socket_path::sidecar_socket(Path::new("/tmp"), "account-test").unwrap();
        let command = sidecar_command(
            Path::new("/usr/lib/eitri/verdandi-claude-sidecar"),
            &[],
            &socket,
            Some(&account),
        );
        let env: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.map(|v| v.to_string_lossy().to_string()),
                )
            })
            .collect();
        assert!(
            env.contains(&("VERDANDI_CLAUDE_ACCOUNT".to_string(), Some("work".to_string()))),
            "{env:?}"
        );
        assert!(
            env.contains(&(
                "VERDANDI_CLAUDE_SIDECAR_SOCKET".to_string(),
                Some(socket.to_string_lossy().to_string())
            )),
            "{env:?}"
        );
        // The tuple stays Verdandi's to derive; a second copy here is what would drift.
        for derived in [
            "CLAUDE_PROFILE",
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_SECURESTORAGE_CONFIG_DIR",
            "ANTHROPIC_CONFIG_DIR",
        ] {
            assert!(
                !env.iter().any(|(k, _)| k == derived),
                "{derived} should not be set here"
            );
        }
    }

    /// `get_envs` reports a removal as `(key, None)`; an inherited value is gone only if that entry
    /// exists, and it must survive an account being set too.
    #[test]
    fn the_extra_mcp_servers_variable_is_removed_from_the_sidecar_child() {
        let socket = crate::socket_path::sidecar_socket(Path::new("/tmp"), "account-test").unwrap();
        let account =
            crate::account::ClaudeAccount::resolve_from("work", |k| (k == "HOME").then(|| "/home/user".to_string()))
                .unwrap();
        for account in [None, Some(&account)] {
            let command = sidecar_command(
                Path::new("/usr/lib/eitri/verdandi-claude-sidecar"),
                &[],
                &socket,
                account,
            );
            let removed: Vec<_> = command
                .get_envs()
                .filter(|(k, _)| k.to_string_lossy() == "VERDANDI_CLAUDE_EXTRA_MCP_SERVERS")
                .collect();
            assert_eq!(removed.len(), 1, "{removed:?}");
            assert!(removed[0].1.is_none(), "it must be removed, not set: {removed:?}");
        }
    }

    /// The shipped default: with nothing configured the child's environment is what it always was
    /// (plus the one removal above), and the sidecar's own account support never engages.
    #[test]
    fn with_no_account_the_child_sets_the_socket_and_only_removes_the_extra_servers() {
        let socket = crate::socket_path::sidecar_socket(Path::new("/tmp"), "account-test").unwrap();
        let command = sidecar_command(Path::new("/usr/lib/eitri/verdandi-claude-sidecar"), &[], &socket, None);
        let keys: Vec<String> = command
            .get_envs()
            .map(|(k, _)| k.to_string_lossy().to_string())
            .collect();
        let mut keys = keys;
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "VERDANDI_CLAUDE_EXTRA_MCP_SERVERS".to_string(),
                "VERDANDI_CLAUDE_SIDECAR_SOCKET".to_string()
            ]
        );
    }

    // The per-user sidecar is keyed by the pinned revision.

    /// An absolute `$XDG_DATA_HOME` takes precedence over the home-directory fallback.
    #[test]
    fn user_sidecar_path_prefers_an_absolute_xdg_data_home() {
        let path = user_sidecar_path(Some(OsStr::new("/data")), Some(OsStr::new("/home/x")), "abc1234")
            .expect("an absolute XDG_DATA_HOME resolves");
        assert_eq!(
            path,
            Path::new("/data/eitri/sidecar/abc1234").join(PACKAGED_SIDECAR_BINARY)
        );
    }

    /// And falls back to `<home>/.local/share` when `$XDG_DATA_HOME` is unset, empty or relative --
    /// the same three cases `core::layout::persist::state_subdir` handles for `XDG_STATE_HOME`.
    #[test]
    fn user_sidecar_path_falls_back_to_home_local_share() {
        for unusable in [None, Some(OsStr::new("")), Some(OsStr::new("relative/dir"))] {
            let path = user_sidecar_path(unusable, Some(OsStr::new("/home/x")), "abc1234")
                .unwrap_or_else(|| panic!("home should still resolve for {unusable:?}"));
            assert_eq!(
                path,
                Path::new("/home/x/.local/share/eitri/sidecar/abc1234").join(PACKAGED_SIDECAR_BINARY),
                "{unusable:?}"
            );
        }
    }

    /// Neither variable gives an absolute directory to build on: `None`, never a path resolved
    /// against this process's own cwd.
    #[test]
    fn user_sidecar_path_is_none_when_neither_variable_is_usable() {
        assert_eq!(user_sidecar_path(None, None, "abc1234"), None);
        assert_eq!(
            user_sidecar_path(
                Some(OsStr::new("relative")),
                Some(OsStr::new("also/relative")),
                "abc1234"
            ),
            None
        );
        assert_eq!(
            user_sidecar_path(Some(OsStr::new("")), None, "abc1234"),
            None,
            "an empty XDG_DATA_HOME with no HOME must not resolve"
        );
    }

    /// Paths are keyed by revision, so different builds live in different directories.
    /// A client cannot mistake a build for another revision for its own.
    #[test]
    fn user_sidecar_path_is_keyed_by_rev() {
        let a = user_sidecar_path(Some(OsStr::new("/data")), None, "aaaaaaa").unwrap();
        let b = user_sidecar_path(Some(OsStr::new("/data")), None, "bbbbbbb").unwrap();
        assert_ne!(a, b);
        assert!(a.to_string_lossy().contains("aaaaaaa"), "{a:?}");
        assert!(b.to_string_lossy().contains("bbbbbbb"), "{b:?}");
    }

    /// Step 4: a per-user build at the exact rev-keyed path this client expects is found when
    /// nothing earlier in the precedence (steps 1-3) matches.
    #[test]
    fn resolve_sidecar_program_finds_the_user_path_when_nothing_else_matches() {
        let xdg_data_home = std::env::temp_dir().join(format!("nv-xdg-{}", uuid::Uuid::new_v4()));
        let artifact = user_sidecar_path(Some(xdg_data_home.as_os_str()), None, EXPECTED_VERDANDI_REVISION)
            .expect("an absolute XDG_DATA_HOME resolves");
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();

        let resolved = resolve_sidecar_program(None, None, None, Some(xdg_data_home.as_os_str()), None)
            .expect("the user path resolves");
        assert!(
            matches!(&resolved, SidecarProgram::Packaged(p) if *p == artifact),
            "{resolved:?}"
        );
        let _ = std::fs::remove_dir_all(&xdg_data_home);
    }

    /// A user-built sidecar for a DIFFERENT revision is never found -- the whole reason step 4 is
    /// keyed by `EXPECTED_VERDANDI_REVISION`: a build from a previous release must be invisible,
    /// never "almost right", so a stale sidecar cannot silently keep running against a newer client.
    /// Isolated from step 5 by pointing `home` at an empty directory with no default checkout.
    #[test]
    fn a_user_sidecar_built_for_another_rev_is_never_found() {
        let xdg_data_home = std::env::temp_dir().join(format!("nv-xdg-other-rev-{}", uuid::Uuid::new_v4()));
        let other_rev_artifact = user_sidecar_path(Some(xdg_data_home.as_os_str()), None, "deadbee")
            .expect("a fake other-rev path resolves");
        std::fs::create_dir_all(other_rev_artifact.parent().unwrap()).unwrap();
        std::fs::write(&other_rev_artifact, b"#!/bin/sh\nexit 0\n").unwrap();

        let home = std::env::temp_dir().join(format!("nv-home-empty-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();

        let err = resolve_sidecar_program(
            None,
            None,
            None,
            Some(xdg_data_home.as_os_str()),
            Some(home.as_os_str()),
        )
        .expect_err("a build for a different revision must be invisible");
        assert!(err.to_string().contains(NO_SIDECAR_HINT), "{err}");
        assert!(
            !err.to_string().contains("deadbee"),
            "the wrong-rev artifact must never be named as the answer: {err}"
        );

        let _ = std::fs::remove_dir_all(&xdg_data_home);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The default checkout is usable only when it already contains a prebuilt artifact.
    /// An unbuilt default checkout returns `NO_SIDECAR_HINT` instead of triggering a build.
    /// `resolve_sidecar_program` performs no builds, so this check does not depend on timing.
    #[test]
    fn the_default_checkout_never_builds_when_it_is_present_but_unbuilt() {
        let home = fake_home_with_default_checkout(&[]);
        let err = resolve_sidecar_program(None, None, None, None, Some(home.as_os_str()))
            .expect_err("an unbuilt default checkout must not resolve to something runnable");
        assert!(err.to_string().contains(NO_SIDECAR_HINT), "{err}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// When both a sibling artifact and a per-user build for the expected revision exist,
    /// the sibling wins. Providing both candidates makes the test check their relative precedence.
    #[test]
    fn a_sibling_artifact_beats_a_user_build_for_the_same_revision() {
        let dir = std::env::temp_dir().join(format!("nv-sidecar-dir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let sibling = dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&sibling, b"#!/bin/sh\nexit 0\n").unwrap();
        let xdg_data_home = std::env::temp_dir().join(format!("nv-xdg-sibling-{}", uuid::Uuid::new_v4()));
        let user_build = user_sidecar_path(Some(xdg_data_home.as_os_str()), None, EXPECTED_VERDANDI_REVISION)
            .expect("an absolute XDG_DATA_HOME resolves");
        std::fs::create_dir_all(user_build.parent().unwrap()).unwrap();
        std::fs::write(&user_build, b"#!/bin/sh\nexit 0\n").unwrap();

        let resolved = resolve_sidecar_program(None, None, Some(&dir), Some(xdg_data_home.as_os_str()), None)
            .expect("both candidates are runnable");
        assert!(
            matches!(&resolved, SidecarProgram::Packaged(p) if *p == sibling),
            "step 3 (the sibling) must outrank step 4 (the user build): {resolved:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&xdg_data_home);
    }

    /// Steps 4 and 5 ranked against each other: a per-user build for this exact
    /// revision beats a default checkout that holds a prebuilt artifact. The user build is keyed to
    /// `EXPECTED_VERDANDI_REVISION`; the default checkout holds whatever revision was last built
    /// there, and at best earns a drift warning. Before this test no resolver test ever gave the
    /// default checkout an artifact, so moving step 4 below step 5 failed nothing. The user build
    /// sits under the fake home's own `.local/share` (no `XDG_DATA_HOME`), so the `$HOME` fallback
    /// is exercised through the resolver too.
    #[test]
    fn a_user_build_beats_a_default_checkout_with_a_prebuilt_artifact() {
        let suffix = node_platform_suffix().expect("this test target has a known Node platform name");
        let home = fake_home_with_default_checkout(&[&format!("{PACKAGED_SIDECAR_BINARY}-0.1.0-{suffix}")]);
        let user_build = user_sidecar_path(None, Some(home.as_os_str()), EXPECTED_VERDANDI_REVISION)
            .expect("an absolute HOME resolves");
        std::fs::create_dir_all(user_build.parent().unwrap()).unwrap();
        std::fs::write(&user_build, b"#!/bin/sh\nexit 0\n").unwrap();

        let resolved = resolve_sidecar_program(None, None, None, None, Some(home.as_os_str()))
            .expect("both candidates are runnable");
        assert!(
            matches!(&resolved, SidecarProgram::Packaged(p) if *p == user_build),
            "step 4 (the user build) must outrank step 5 (the default checkout): {resolved:?}"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    /// A default checkout with a prebuilt artifact resolves to that artifact without building.
    /// Availability and resolution must agree: otherwise startup reports a runnable sidecar
    /// but spawning fails for the same checkout.
    #[test]
    fn a_default_checkout_with_a_prebuilt_artifact_runs_it_without_building() {
        let suffix = node_platform_suffix().expect("this test target has a known Node platform name");
        let home = fake_home_with_default_checkout(&[&format!("{PACKAGED_SIDECAR_BINARY}-0.1.0-{suffix}")]);
        let checkout_root = home.join(DEFAULT_CHECKOUT_UNDER_HOME);
        let expected_artifact =
            prebuilt_artifact_in(&checkout_root).expect("this fake default checkout has a prebuilt artifact");

        // Availability and resolution must agree, or the startup line and the spawn disagree.
        assert_eq!(
            sidecar_availability_in(
                None,
                None,
                checkout_path_for(None, Some(home.as_os_str())).as_deref(),
                false,
                None,
                Some(home.as_os_str()),
            ),
            SidecarAvailability::Runnable
        );

        let resolved = resolve_sidecar_program(None, None, None, None, Some(home.as_os_str()))
            .expect("a prebuilt default checkout resolves");
        let SidecarProgram::Checkout(checkout) = &resolved else {
            panic!("a prebuilt default checkout must resolve to the checkout shape: {resolved:?}");
        };
        assert!(!checkout.from_override, "found by step 5, not named by the operator");
        assert_eq!(checkout.path, checkout_root);
        assert_eq!(checkout.prebuilt.as_ref(), Some(&expected_artifact));

        let (description, _, program_path, args) =
            invocation_for(resolved, None, |_| panic!("the default checkout must never build"))
                .expect("a prebuilt checkout never fails to resolve its program");
        assert_eq!(program_path, expected_artifact);
        assert!(args.is_empty());
        assert!(description.contains("default path"), "{description}");
        assert!(description.contains("prebuilt artifact"), "{description}");

        let _ = std::fs::remove_dir_all(&home);
    }

    /// The other half of the guarantee above: the test just
    /// above pins that `resolve_sidecar_program` never *returns* a buildable `Checkout` for the
    /// default-checkout case, but nothing had pinned the wiring on the other side -- that a
    /// `Checkout{prebuilt: None}` `spawn()` DOES receive (only possible from the explicit
    /// `EITRI_VERDANDI_CHECKOUT` branch, step 2) actually reaches the build step. `invocation_for`
    /// exists so that wiring is this directly testable, with a fake `build` closure standing in for
    /// `ensure_sidecar_built` -- no real `npm`/`node`, no process-env mutation.
    #[test]
    fn invocation_for_builds_an_explicit_checkout_that_has_nothing_prebuilt() {
        let root = fake_checkout(&[]);
        let resolved = resolve_sidecar_program(None, root.to_str(), None, None, None).expect("the checkout resolves");
        let build_calls = std::cell::Cell::new(0);
        let (_, _, program_path, args) = invocation_for(resolved, None, |checkout_path| {
            build_calls.set(build_calls.get() + 1);
            assert_eq!(checkout_path, root.as_path());
            Ok(checkout_path.join("apps/claude-sidecar/dist/src/index.js"))
        })
        .expect("the fake build succeeds");
        assert_eq!(
            build_calls.get(),
            1,
            "an unbuilt explicit checkout must build exactly once"
        );
        assert_eq!(program_path, PathBuf::from("node"));
        assert_eq!(args, vec![root.join("apps/claude-sidecar/dist/src/index.js")]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The two shapes that must NEVER build, proven by a `build` closure that panics if called at
    /// all -- stronger than asserting a call count afterward, since a panic inside `invocation_for`
    /// fails this test even if a later line is never reached.
    #[test]
    fn invocation_for_never_builds_a_checkout_that_already_has_a_prebuilt_artifact() {
        let suffix = node_platform_suffix().expect("this test target has a known Node platform name");
        let root = fake_checkout(&[&format!("{PACKAGED_SIDECAR_BINARY}-0.1.0-{suffix}")]);
        let resolved = resolve_sidecar_program(None, root.to_str(), None, None, None).expect("the checkout resolves");
        let expected_artifact = prebuilt_artifact_in(&root).expect("this fake checkout has a prebuilt artifact");
        let (_, _, program_path, args) = invocation_for(resolved, None, |_| {
            panic!("a checkout with a prebuilt artifact must never build")
        })
        .expect("resolving a prebuilt checkout never fails");
        assert_eq!(program_path, expected_artifact);
        assert!(args.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn invocation_for_never_builds_a_packaged_artifact() {
        let path = PathBuf::from("/fake/verdandi-claude-sidecar");
        let (_, _, program_path, args) = invocation_for(SidecarProgram::Packaged(path.clone()), None, |_| {
            panic!("a packaged artifact has nothing to build")
        })
        .expect("resolving a packaged artifact never fails");
        assert_eq!(program_path, path);
        assert!(args.is_empty());
    }

    /// And when there is no default checkout at all (a fresh machine, or `$HOME` pointing nowhere
    /// useful), resolution ends the same way -- not with `locate_verdandi_checkout`'s own "no
    /// checkout found" wording, which would name a path nobody asked about -- and the message names
    /// exactly the path an `eitri setup` run would use.
    #[test]
    fn nothing_found_at_all_gives_the_shared_hint_and_the_exact_user_path() {
        let xdg_data_home = std::env::temp_dir().join(format!("nv-xdg-nothing-{}", uuid::Uuid::new_v4()));
        let home = std::env::temp_dir().join(format!("nv-home-nothing-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();

        let err = resolve_sidecar_program(
            None,
            None,
            None,
            Some(xdg_data_home.as_os_str()),
            Some(home.as_os_str()),
        )
        .expect_err("nothing at all must still fail, never silently succeed");
        assert!(err.to_string().contains(NO_SIDECAR_HINT), "{err}");
        let expected_path = user_sidecar_path(
            Some(xdg_data_home.as_os_str()),
            Some(home.as_os_str()),
            EXPECTED_VERDANDI_REVISION,
        )
        .expect("this test's own home is absolute");
        assert!(err.to_string().contains(&expected_path.display().to_string()), "{err}");

        let _ = std::fs::remove_dir_all(&home);
    }

    /// `sidecar_availability_in` must count the per-user build, just as resolution does.
    /// Otherwise `BackendKind::choose` reports no installed sidecar even though the session
    /// can start successfully from that path.
    #[test]
    fn sidecar_availability_counts_the_user_path() {
        let xdg_data_home = std::env::temp_dir().join(format!("nv-xdg-avail-{}", uuid::Uuid::new_v4()));
        let artifact = user_sidecar_path(Some(xdg_data_home.as_os_str()), None, EXPECTED_VERDANDI_REVISION)
            .expect("an absolute XDG_DATA_HOME resolves");
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();

        assert_eq!(
            sidecar_availability_in(None, None, None, false, Some(xdg_data_home.as_os_str()), None),
            SidecarAvailability::Runnable
        );
        let _ = std::fs::remove_dir_all(&xdg_data_home);
    }

    /// And a build for another revision does not count as availability either -- the same
    /// invisibility `resolve_sidecar_program` enforces (above), checked at the other function that
    /// must agree with it or the startup line and the actual spawn could disagree about whether
    /// anything is really there.
    #[test]
    fn sidecar_availability_does_not_count_another_revs_user_path() {
        let xdg_data_home = std::env::temp_dir().join(format!("nv-xdg-avail-other-{}", uuid::Uuid::new_v4()));
        let other_rev_artifact = user_sidecar_path(Some(xdg_data_home.as_os_str()), None, "deadbee")
            .expect("a fake other-rev path resolves");
        std::fs::create_dir_all(other_rev_artifact.parent().unwrap()).unwrap();
        std::fs::write(&other_rev_artifact, b"#!/bin/sh\nexit 0\n").unwrap();

        assert_eq!(
            sidecar_availability_in(None, None, None, false, Some(xdg_data_home.as_os_str()), None),
            SidecarAvailability::Missing
        );
        let _ = std::fs::remove_dir_all(&xdg_data_home);
    }

    /// A named `EITRI_VERDANDI_CHECKOUT` with nothing prebuilt is the one case the spawn builds
    /// (step 2), so availability says so rather than "not installed" -- the line a developer running
    /// `try-eitri.sh` against an unbuilt checkout used to see above an `npm` build. And it ranks
    /// like `resolve_sidecar_program`: an installed sibling or a per-user build below it does not
    /// turn the answer into "runnable", because the spawn still builds the named checkout.
    #[test]
    fn a_named_checkout_with_nothing_prebuilt_builds_even_beside_an_installed_artifact() {
        let root = fake_checkout(&[]);
        assert_eq!(
            sidecar_availability_in(None, None, Some(&root), true, None, None),
            SidecarAvailability::BuildsNamedCheckout
        );

        let exe_dir = std::env::temp_dir().join(format!("nv-avail-sibling-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::write(exe_dir.join(PACKAGED_SIDECAR_BINARY), b"#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(
            sidecar_availability_in(None, Some(&exe_dir), Some(&root), true, None, None),
            SidecarAvailability::BuildsNamedCheckout,
            "step 2 outranks step 3, as in resolve_sidecar_program"
        );
        // The same sibling with no named checkout is runnable (step 3).
        assert_eq!(
            sidecar_availability_in(None, Some(&exe_dir), Some(&root), false, None, None),
            SidecarAvailability::Runnable
        );

        let suffix = node_platform_suffix().expect("this test target has a known Node platform name");
        let built = fake_checkout(&[&format!("{PACKAGED_SIDECAR_BINARY}-0.1.0-{suffix}")]);
        assert_eq!(
            sidecar_availability_in(None, None, Some(&built), true, None, None),
            SidecarAvailability::Runnable,
            "a named checkout holding a prebuilt artifact runs it without building"
        );

        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&built).unwrap();
        let _ = std::fs::remove_dir_all(&exe_dir);
    }

    /// A named `EITRI_SIDECAR_BINARY` decides alone (step 1), and one that is not a file is
    /// missing -- `resolve_sidecar_program` refuses it with `sidecar_missing_message` -- even when a
    /// sibling artifact sits right there.
    #[test]
    fn a_named_binary_decides_alone_and_a_missing_one_is_missing() {
        let exe_dir = std::env::temp_dir().join(format!("nv-avail-named-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&exe_dir).unwrap();
        let sibling = exe_dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&sibling, b"#!/bin/sh\nexit 0\n").unwrap();
        let missing = exe_dir.join("not-there");

        assert_eq!(
            sidecar_availability_in(sibling.to_str(), None, None, false, None, None),
            SidecarAvailability::Runnable
        );
        assert_eq!(
            sidecar_availability_in(missing.to_str(), Some(&exe_dir), None, false, None, None),
            SidecarAvailability::Missing,
            "step 1 names a binary that is not a file; the sibling below it is never reached"
        );
        assert!(resolve_sidecar_program(missing.to_str(), None, Some(&exe_dir), None, None).is_err());
        let _ = std::fs::remove_dir_all(&exe_dir);
    }

    /// `is_exe_sibling_artifact` matches only the exact step-3 candidate -- not an arbitrary path,
    /// and not the sibling shape with no `exe_dir` known at all.
    #[test]
    fn is_exe_sibling_artifact_matches_only_the_sibling_candidate() {
        let dir = Path::new("/some/exe/dir");
        let sibling = dir.join(PACKAGED_SIDECAR_BINARY);
        assert!(is_exe_sibling_artifact(&sibling, Some(dir)));
        assert!(!is_exe_sibling_artifact(
            Path::new("/elsewhere/verdandi-claude-sidecar"),
            Some(dir)
        ));
        assert!(!is_exe_sibling_artifact(&sibling, None));
    }

    /// A sibling artifact without a `.rev` file gives no warning, allowing older installations.
    #[test]
    fn sibling_rev_skew_warning_is_empty_when_the_rev_file_is_absent() {
        let dir = std::env::temp_dir().join(format!("nv-sibling-rev-absent-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();
        assert!(sibling_rev_skew_warning(&artifact).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `.rev` file matching this client's expectation gives no warning either -- the common case
    /// for a real, current install.
    #[test]
    fn sibling_rev_skew_warning_is_empty_when_the_rev_file_matches() {
        let dir = std::env::temp_dir().join(format!("nv-sibling-rev-match-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(
            dir.join(format!("{PACKAGED_SIDECAR_BINARY}.rev")),
            EXPECTED_VERDANDI_REVISION,
        )
        .unwrap();
        assert!(sibling_rev_skew_warning(&artifact).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `.rev` value with no prefix match in either direction yields exactly one warning
    /// that names both the actual and expected revisions.
    #[test]
    fn sibling_rev_skew_warning_names_both_revs_on_a_mismatch() {
        let dir = std::env::temp_dir().join(format!("nv-sibling-rev-mismatch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(dir.join(format!("{PACKAGED_SIDECAR_BINARY}.rev")), "deadbee").unwrap();
        let warnings = sibling_rev_skew_warning(&artifact);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("deadbee") && warnings[0].contains(EXPECTED_VERDANDI_REVISION),
            "{warnings:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A full 40-character revision beginning with the expected abbreviated revision gives
    /// no warning. This checks the longer-file-value direction of the prefix comparison.
    #[test]
    fn sibling_rev_skew_warning_is_empty_when_the_rev_file_is_a_longer_sha_starting_with_the_expected_prefix() {
        let dir = std::env::temp_dir().join(format!("nv-sibling-rev-long-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();
        let full_sha = format!("{EXPECTED_VERDANDI_REVISION}1234567890abcdef1234567890abcdef1");
        assert_eq!(full_sha.len(), 40, "{full_sha}");
        std::fs::write(dir.join(format!("{PACKAGED_SIDECAR_BINARY}.rev")), &full_sha).unwrap();
        assert!(sibling_rev_skew_warning(&artifact).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `.rev` value shorter than the expected revision also gives no warning when it is
    /// a prefix of that revision.
    #[test]
    fn sibling_rev_skew_warning_is_empty_when_the_rev_file_is_a_shorter_prefix_of_the_expected_revision() {
        let dir = std::env::temp_dir().join(format!("nv-sibling-rev-short-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();
        let short_prefix = &EXPECTED_VERDANDI_REVISION[..EXPECTED_VERDANDI_REVISION.len() - 1];
        std::fs::write(dir.join(format!("{PACKAGED_SIDECAR_BINARY}.rev")), short_prefix).unwrap();
        assert!(sibling_rev_skew_warning(&artifact).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pins the cross-plan seam: `NO_SIDECAR_HINT` must start
    /// with `sidecar_missing_message`'s own opening sentence, or
    /// `agent-ui/web/src/problems.ts::classifySidecarMissing` stops recognising the step-5
    /// fallthrough and a fresh public-release user sees unrecognised raw text with no headline or
    /// remedy. `problems.test.ts` pins the same literal from this file's source independently; this
    /// test is the Rust-side half of that agreement.
    #[test]
    fn no_sidecar_hint_starts_with_the_panels_sidecar_missing_headline() {
        const SIDECAR_MISSING_HEADLINE: &str = "The agent sidecar is not installed.";
        assert!(
            NO_SIDECAR_HINT.starts_with(SIDECAR_MISSING_HEADLINE),
            "{NO_SIDECAR_HINT:?} must start with {SIDECAR_MISSING_HEADLINE:?}"
        );
    }
}
