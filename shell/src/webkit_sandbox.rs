//! Whether WebKit's sandbox can start in this process, decided once, before any `WebView` exists
//! (v1-dist sub-plan `docs/superpowers/plans/2026-09-28-v1-dist-ubuntu-userns.md`).
//!
//! WebKitGTK 6.0 runs its web and network processes inside `bwrap`, and its 6.0 API has no switch to
//! turn that off: the only escape hatch is the environment variable [`ESCAPE_HATCH`]. On a stock
//! Ubuntu 23.10+ desktop the `apparmor` package sets `kernel.apparmor_restrict_unprivileged_userns=1`,
//! so a process may create a user namespace only while an AppArmor profile that grants `userns`
//! confines it. With no such profile `bwrap` fails ("setting up uid map: Permission denied") and
//! WebKit aborts the whole of neovibe with SIGTRAP on its first `WebView`, printing nothing a user
//! would see -- the release-blocking crash the rc.1 VM pass found (`shell/MANUAL_VERIFICATION.md`,
//! "Ubuntu 24.04: the AppArmor user-namespace restriction (2026-09-28)").
//!
//! **One decision point.** [`decision`] decides, the first time it is called (from `main()`, while
//! the process is still single-threaded, before GTK starts), and every later call gets the same
//! answer: the agent panel (`agent_panel::build_agent_panel`) and every Lua panel
//! (`lua::panel::install`) ask it rather than checking anything themselves.
//!
//! - [`ESCAPE_HATCH`] set (to anything but `0`, WebKit's own rule, measured: `0` keeps the sandbox,
//!   an empty value turns it off) is [`Decision::DisabledByUser`], honoured as it always was.
//! - Otherwise, only where the restriction is on is anything run: a probe that spawns `bwrap` with a
//!   user namespace from this very process -- the same profile (or none), the same parent, the same
//!   failure WebKit's own `bwrap` will hit. Measured on the Ubuntu 24.04 VM by this module's own log
//!   line, in six launches of a release build: 6-42 ms, refused or allowed alike, the slowest being
//!   the first `bwrap` after a boot. A probe that fails there is [`Decision::Unavailable`];
//!   one that cannot be run or does not finish is not evidence of anything, and WebKit decides as it
//!   always did.
//! - [`Decision::Unavailable`] builds no `WebView` anywhere: the editor and the terminal work, and
//!   the agent panel's place shows [`unavailable_notice`] -- what happened, the one-time fix for this
//!   install's layout, and the escape hatch -- which `main()` also prints on stderr.
//!
//! **Nothing here turns the sandbox off.** The owner's choice between keeping the sandbox with a
//! profile (A) and turning it off automatically (B) is pending. (B) would be one arm here: in
//! [`decide`], where the probe failed, set [`ESCAPE_HATCH`] in this process's environment (safe only
//! because `main()` decides before any thread exists) and return a new `Decision` variant that allows
//! `WebView`s -- no other file would need to change.
//!
//! **The profile.** `packaging/apparmor/neovibe` is the one text: the `.deb` installs it as
//! `/etc/apparmor.d/neovibe` for `/usr/lib/neovibe/shell` (config, no maintainer script, so a
//! restart or one `apparmor_parser -r` loads it); for any other install [`render_profile`] writes the
//! same text with that install's own resolved path and the profile name `neovibe-user-<uid>` -- one
//! name per user, so two users' per-user installs never replace each other's profile, and a literal
//! path rather than an `@{HOME}` glob (which works, measured) so the grant covers this user's own
//! install and nobody else's. `packaging/install.sh` renders it too (`apparmor_render`), and
//! [`tests::the_installer_renders_the_same_profile_and_commands`] holds the two to the same bytes.
//!
//! **Known limit:** WebKit itself skips `bwrap` inside Flatpak, Snap and Docker. The probe runs only
//! where the AppArmor restriction is on, and does not mirror those checks, so neovibe run inside such
//! a container on an Ubuntu host would show the notice where WebKit would have run unsandboxed.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// WebKitGTK's own escape hatch. Set to anything but `0` (an empty value included), WebKit runs its
/// web and network processes with no sandbox at all (measured on WebKitGTK 2.52.6, Ubuntu 24.04).
pub(crate) const ESCAPE_HATCH: &str = "WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS";

/// `1` when the kernel lets an unprivileged process create a user namespace only under an AppArmor
/// profile granting `userns` (Ubuntu 23.10+, set by the `apparmor` package's
/// `/usr/lib/sysctl.d/10-apparmor.conf`). `packaging/install.sh` reads the same file.
pub(crate) const RESTRICTION_SYSCTL: &str = "/proc/sys/kernel/apparmor_restrict_unprivileged_userns";

/// Where the `.deb`/`.rpm` put `shell`, and where the `.deb` puts its profile.
const PACKAGED_SHELL: &str = "/usr/lib/neovibe/shell";
const APPARMOR_D: &str = "/etc/apparmor.d";
const PACKAGED_PROFILE_NAME: &str = "neovibe";

/// The one profile text (`packaging/apparmor/neovibe`), and the two lines [`render_profile`]
/// replaces in it. Each must occur exactly once (`tests::the_template_has_each_rendered_line_once`).
const PROFILE_TEMPLATE: &str = include_str!("../../packaging/apparmor/neovibe");
const TEMPLATE_ATTACHMENT: &str = "profile neovibe \"/usr/lib/neovibe/shell\" flags=(unconfined) {";
const TEMPLATE_LOCAL: &str = "include if exists <local/neovibe>";

/// The probe never holds startup up for longer than this: a `bwrap` that has not finished by then
/// is killed (by the pid this process spawned) and the probe tells nothing.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// What neovibe does about `WebView`s in this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// WebKit's sandbox can start here, or nothing here restricts it: `WebView`s are built as always.
    Available,
    /// The user set [`ESCAPE_HATCH`]: WebKit runs without its sandbox, as it always did with it set.
    DisabledByUser,
    /// The kernel refuses the sandbox WebKit would start (`reason`: what `bwrap` said). No `WebView`
    /// is built anywhere in this process.
    Unavailable { reason: String },
}

impl Decision {
    pub(crate) fn allows_webviews(&self) -> bool {
        !matches!(self, Decision::Unavailable { .. })
    }
}

/// [`RESTRICTION_SYSCTL`], read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Restriction {
    On,
    /// `0`, anything else, or no such file (a kernel without it: Fedora, Arch, Debian).
    Off,
}

pub(crate) fn restriction_from(contents: Option<&str>) -> Restriction {
    match contents.map(str::trim) {
        Some("1") => Restriction::On,
        _ => Restriction::Off,
    }
}

/// What the `bwrap` probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Probe {
    /// `bwrap` ran to completion.
    Ran { success: bool, stderr: String },
    /// `bwrap` could not be started, or did not finish in time: nothing is known either way.
    Inconclusive(String),
}

/// WebKit's own reading of [`ESCAPE_HATCH`]: set, and not `0`.
pub(crate) fn escape_hatch_set(value: Option<&OsStr>) -> bool {
    value.is_some_and(|v| v != OsStr::new("0"))
}

/// The decision, pure. `probe` runs only when its answer can change it: never with the escape
/// hatch set, never where the restriction is off.
pub(crate) fn decide(
    escape_hatch: Option<&OsStr>,
    restriction: Restriction,
    probe: impl FnOnce() -> Probe,
) -> Decision {
    if escape_hatch_set(escape_hatch) {
        return Decision::DisabledByUser;
    }
    if restriction == Restriction::Off {
        return Decision::Available;
    }
    match probe() {
        Probe::Ran { success: true, .. } => Decision::Available,
        // Option (B) would be this arm (module doc): set the escape hatch and allow WebViews.
        Probe::Ran { success: false, stderr } => Decision::Unavailable {
            reason: stderr
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("bwrap failed and said nothing")
                .to_string(),
        },
        Probe::Inconclusive(_) => Decision::Available,
    }
}

static DECISION: OnceLock<Decision> = OnceLock::new();

/// The one decision for this process. The first call decides (it reads the environment and the
/// sysctl, and may run the probe) and logs it; every later call returns the same answer.
pub(crate) fn decision() -> &'static Decision {
    DECISION.get_or_init(decide_for_this_process)
}

fn decide_for_this_process() -> Decision {
    let hatch = std::env::var_os(ESCAPE_HATCH);
    let restriction = restriction_from(std::fs::read_to_string(RESTRICTION_SYSCTL).ok().as_deref());
    let mut probed: Option<(Probe, Duration)> = None;
    let decision = decide(hatch.as_deref(), restriction, || {
        let started = Instant::now();
        let probe = run_probe();
        probed = Some((probe.clone(), started.elapsed()));
        probe
    });
    match (&decision, &probed) {
        (Decision::DisabledByUser, _) => {
            eprintln!("[webkit-sandbox] off: {ESCAPE_HATCH} is set, so WebKit runs without its sandbox")
        }
        (Decision::Available, None) => {
            eprintln!("[webkit-sandbox] no user-namespace restriction here; WebKit starts its sandbox as usual")
        }
        (Decision::Available, Some((Probe::Inconclusive(why), took))) => eprintln!(
            "[webkit-sandbox] the user-namespace restriction is on, and the probe could not tell ({why}, {} ms); \
             WebKit decides",
            took.as_millis()
        ),
        (Decision::Available, Some((_, took))) => eprintln!(
            "[webkit-sandbox] the user-namespace restriction is on, and bwrap starts under this process's profile \
             (probe {} ms)",
            took.as_millis()
        ),
        (Decision::Unavailable { reason }, probed) => eprintln!(
            "[webkit-sandbox] the user-namespace restriction is on, and bwrap cannot start here: {reason} (probe {} ms)",
            probed.as_ref().map_or(0, |(_, took)| took.as_millis())
        ),
    }
    decision
}

/// `bwrap --unshare-user --ro-bind / / true`, from this process: what WebKit's own `bwrap` needs
/// first (the uid map it could not write on the VM), and nothing it does not. `/usr/bin/bwrap`,
/// WebKit's own compiled-in path on every distribution package checked, else `bwrap` on `PATH`.
fn run_probe() -> Probe {
    let bwrap = if Path::new("/usr/bin/bwrap").exists() {
        PathBuf::from("/usr/bin/bwrap")
    } else {
        PathBuf::from("bwrap")
    };
    let spawned = Command::new(&bwrap)
        .args(["--unshare-user", "--ro-bind", "/", "/", "true"])
        // `true` is found by bwrap's own execvp, inside the sandbox: a PATH that lacks it must not
        // read as "the sandbox cannot start".
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => return Probe::Inconclusive(format!("{} could not be run: {e}", bwrap.display())),
    };
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                return Probe::Ran {
                    success: status.success(),
                    stderr,
                };
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
            Ok(None) => {
                // The child this function spawned, by its own handle: never anything by name.
                let _ = child.kill();
                let _ = child.wait();
                return Probe::Inconclusive(format!("bwrap did not finish within {} s", PROBE_TIMEOUT.as_secs()));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Probe::Inconclusive(format!("bwrap could not be waited for: {e}"));
            }
        }
    }
}

/// The one-time fix for one install's layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fix {
    /// The profile file the commands install or load: what the notice names.
    pub(crate) profile: PathBuf,
    /// The commands, in order, as a user types them.
    pub(crate) commands: Vec<String>,
    /// The rendered profile to put at `profile` first, when it is not a file a package installed.
    pub(crate) write: Option<(PathBuf, String)>,
}

/// The data directory's rule (`packaging/install.sh`'s `xdg_dir data`, `agent`'s
/// `user_sidecar_path`): an absolute `XDG_DATA_HOME`, else `$HOME/.local/share`, else nothing.
pub(crate) fn data_home(xdg_data_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    match xdg_data_home {
        Some(dir) if Path::new(dir).is_absolute() => Some(PathBuf::from(dir)),
        _ => match home {
            Some(home) if Path::new(home).is_absolute() => Some(PathBuf::from(home).join(".local/share")),
            _ => None,
        },
    }
}

/// The profile name for `exe`: the package's own for `/usr/lib/neovibe/shell`, else one per user.
pub(crate) fn profile_name(exe: &Path, uid: u32) -> String {
    if exe == Path::new(PACKAGED_SHELL) {
        PACKAGED_PROFILE_NAME.to_string()
    } else {
        format!("neovibe-user-{uid}")
    }
}

/// `packaging/apparmor/neovibe` for `exe` under `name`. The path is always quoted, and AppArmor's
/// glob characters and the quote itself are escaped with a backslash (a path holding a space and
/// `[]{}*?^@"` loaded and attached on the VM). `None` for a path this cannot write into a profile:
/// not UTF-8, or holding a control character.
pub(crate) fn render_profile(name: &str, exe: &Path) -> Option<String> {
    let path = exe.to_str()?;
    if path.chars().any(char::is_control) || !exe.is_absolute() {
        return None;
    }
    let attachment = format!("profile {name} \"{}\" flags=(unconfined) {{", apparmor_escape(path));
    let local = format!("include if exists <local/{name}>");
    Some(
        PROFILE_TEMPLATE
            .replacen(TEMPLATE_ATTACHMENT, &attachment, 1)
            .replacen(TEMPLATE_LOCAL, &local, 1),
    )
}

fn apparmor_escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if matches!(c, '[' | ']' | '\\' | '{' | '}' | '*' | '?' | '^' | '@' | '"') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `s` as one word of a POSIX shell command: as it is when it is only `[A-Za-z0-9_./-]`, else in
/// single quotes. `packaging/install.sh`'s `sh_quote` is the same rule.
pub(crate) fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-'));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// The fix for `exe` (this process's own resolved path). `packaged_profile_exists`: whether
/// `/etc/apparmor.d/neovibe` is there, which matters only for `/usr/lib/neovibe/shell`.
pub(crate) fn plan_fix(exe: &Path, packaged_profile_exists: bool, data_home: Option<&Path>, uid: u32) -> Option<Fix> {
    let name = profile_name(exe, uid);
    let target = Path::new(APPARMOR_D).join(&name);
    let load = format!("sudo apparmor_parser -r {}", shell_quote(target.to_str()?));
    if exe == Path::new(PACKAGED_SHELL) && packaged_profile_exists {
        return Some(Fix {
            profile: target,
            commands: vec![load],
            write: None,
        });
    }
    let source = data_home?.join("neovibe/apparmor").join(&name);
    let text = render_profile(&name, exe)?;
    Some(Fix {
        commands: vec![
            format!(
                "sudo install -m 0644 {} {}",
                shell_quote(source.to_str()?),
                shell_quote(target.to_str()?)
            ),
            load,
        ],
        profile: source.clone(),
        write: Some((source, text)),
    })
}

/// The text the agent panel's place shows and `main()` prints: what happened, in one sentence; the
/// fix; the escape hatch. `write_error`: the rendered profile could not be written.
pub(crate) fn notice(reason: &str, fix: Option<&Fix>, write_error: Option<&str>) -> String {
    let mut text = format!(
        "neovibe's agent panel is off: this system does not let WebKit start its sandbox (Ubuntu's AppArmor \
         restriction on unprivileged user namespaces; bwrap said \"{reason}\"). The editor and the terminal work \
         as usual.\n\n"
    );
    match fix {
        Some(fix) if fix.write.is_none() => text.push_str(
            "To fix it once, load the AppArmor profile the neovibe package installed (or restart the computer), \
             then reopen neovibe:\n\n",
        ),
        Some(_) => {
            text.push_str("To fix it once, install and load neovibe's AppArmor profile, then reopen neovibe:\n\n")
        }
        None => text.push_str(
            "neovibe cannot name an AppArmor profile for this install: one that grants `userns` to this program \
             fixes it.\n",
        ),
    }
    if let Some(fix) = fix {
        for command in &fix.commands {
            text.push_str("    ");
            text.push_str(command);
            text.push('\n');
        }
        text.push_str(&format!("\nThe profile: {}\n", fix.profile.display()));
        if let Some(error) = write_error {
            text.push_str(&format!("(neovibe could not write it: {error})\n"));
        }
    }
    text.push_str(&format!(
        "\nStarting neovibe with {ESCAPE_HATCH}=1 also works, but it removes the operating system's sandbox from \
         the process that renders the model's output.\n"
    ));
    text
}

/// [`notice`] for this process: its own resolved path, the package's profile if it is there, the
/// data directory by the installer's rule, and the real uid. A per-user profile the notice names is
/// written first if it is missing or differs (0644, under the data directory, nothing else): the
/// installer writes the same file, but one from an older installer, a restriction turned on after
/// the install, or a development build has none, and the commands must name a file that exists.
pub(crate) fn unavailable_notice(reason: &str) -> String {
    let exe = std::env::current_exe().ok();
    let data = data_home(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    let packaged_profile_exists = Path::new(APPARMOR_D).join(PACKAGED_PROFILE_NAME).is_file();
    let fix = exe.as_deref().and_then(|exe| {
        plan_fix(
            exe,
            packaged_profile_exists,
            data.as_deref(),
            agent::private_fs::current_uid(),
        )
    });
    let write_error = fix
        .as_ref()
        .and_then(|fix| fix.write.as_ref())
        .and_then(|(path, text)| write_if_changed(path, text).err());
    notice(reason, fix.as_ref(), write_error.as_deref())
}

fn write_if_changed(path: &Path, text: &str) -> Result<(), String> {
    if std::fs::read_to_string(path).is_ok_and(|current| current == text) {
        return Ok(());
    }
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no directory", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// The agent panel's place when [`Decision::Unavailable`]: `text` in a plain, selectable label (so
/// the commands can be copied), themed like the chrome by the window's own stylesheet
/// (`.pane-placeholder`). The box itself takes focus, so `Ctrl+h/j/k/l`, `prefix a` and a click land
/// here and leave again like any module (`main.rs`'s `install_module_nav`).
pub(crate) fn notice_widget(text: &str) -> gtk4::Widget {
    use gtk4::prelude::*;
    let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    container.add_css_class("pane-placeholder");
    container.set_hexpand(true);
    container.set_vexpand(true);
    container.set_focusable(true);
    let label = gtk4::Label::new(Some(text));
    label.set_wrap(true);
    label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    label.set_selectable(true);
    label.set_xalign(0.0);
    label.set_yalign(0.0);
    label.set_valign(gtk4::Align::Start);
    label.set_margin_top(24);
    label.set_margin_bottom(24);
    label.set_margin_start(24);
    label.set_margin_end(24);
    container.append(&label);
    container.upcast()
}

/// A Lua panel's place when [`Decision::Unavailable`]: it is a web page, and no `WebView` is built.
pub(crate) fn lua_panel_placeholder(title: &str) -> gtk4::Widget {
    notice_widget(&format!(
        "{title}: this panel is a web page, and WebKit cannot start here. The agent panel's place says why and \
         how to fix it."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const UID: u32 = 1000;

    fn never_probe() -> Probe {
        panic!("the probe must not run here")
    }

    #[test]
    fn the_escape_hatch_is_webkits_own_reading_of_it() {
        assert!(!escape_hatch_set(None));
        assert!(
            !escape_hatch_set(Some(OsStr::new("0"))),
            "0 keeps the sandbox (measured)"
        );
        assert!(escape_hatch_set(Some(OsStr::new("1"))));
        assert!(
            escape_hatch_set(Some(OsStr::new(""))),
            "set and empty turns it off (measured)"
        );
        assert!(escape_hatch_set(Some(OsStr::new("yes"))));
    }

    #[test]
    fn the_escape_hatch_is_honoured_without_probing() {
        for restriction in [Restriction::On, Restriction::Off] {
            assert_eq!(
                decide(Some(OsStr::new("1")), restriction, never_probe),
                Decision::DisabledByUser
            );
        }
    }

    #[test]
    fn with_no_restriction_nothing_is_probed_and_webviews_are_built() {
        assert_eq!(decide(None, Restriction::Off, never_probe), Decision::Available);
        assert_eq!(
            decide(Some(OsStr::new("0")), Restriction::Off, never_probe),
            Decision::Available
        );
    }

    #[test]
    fn under_the_restriction_the_probe_decides() {
        let ok = || Probe::Ran {
            success: true,
            stderr: String::new(),
        };
        assert_eq!(decide(None, Restriction::On, ok), Decision::Available);
        let refused = || Probe::Ran {
            success: false,
            stderr: "\nbwrap: setting up uid map: Permission denied\n".to_string(),
        };
        let decision = decide(None, Restriction::On, refused);
        assert_eq!(
            decision,
            Decision::Unavailable {
                reason: "bwrap: setting up uid map: Permission denied".to_string()
            }
        );
        assert!(!decision.allows_webviews());
        let silent = || Probe::Ran {
            success: false,
            stderr: String::new(),
        };
        assert_eq!(
            decide(None, Restriction::On, silent),
            Decision::Unavailable {
                reason: "bwrap failed and said nothing".to_string()
            }
        );
    }

    /// A probe that tells nothing never takes the panel away: WebKit decides, as before.
    #[test]
    fn an_inconclusive_probe_leaves_webviews_on() {
        let decision = decide(None, Restriction::On, || Probe::Inconclusive("no bwrap".to_string()));
        assert_eq!(decision, Decision::Available);
        assert!(decision.allows_webviews());
        assert!(Decision::DisabledByUser.allows_webviews());
    }

    #[test]
    fn only_a_one_turns_the_restriction_on() {
        assert_eq!(restriction_from(Some("1\n")), Restriction::On);
        assert_eq!(restriction_from(Some("0\n")), Restriction::Off);
        assert_eq!(restriction_from(Some("")), Restriction::Off);
        assert_eq!(restriction_from(Some("11")), Restriction::Off);
        assert_eq!(restriction_from(None), Restriction::Off);
    }

    #[test]
    fn the_template_has_each_rendered_line_once() {
        assert_eq!(PROFILE_TEMPLATE.matches(TEMPLATE_ATTACHMENT).count(), 1);
        assert_eq!(PROFILE_TEMPLATE.matches(TEMPLATE_LOCAL).count(), 1);
        assert!(PROFILE_TEMPLATE.contains("\n  userns,\n"));
        assert!(PROFILE_TEMPLATE.contains("\nabi <abi/4.0>,\ninclude <tunables/global>\n"));
    }

    /// The `.deb`'s own file is exactly what rendering the packaged path gives: one text.
    #[test]
    fn rendering_the_packaged_path_gives_the_shipped_file() {
        assert_eq!(
            render_profile("neovibe", Path::new("/usr/lib/neovibe/shell")).as_deref(),
            Some(PROFILE_TEMPLATE)
        );
    }

    #[test]
    fn a_per_user_profile_names_its_own_path_and_name() {
        let text = render_profile("neovibe-user-1000", Path::new("/home/a b/.local/lib/neovibe/shell")).unwrap();
        assert!(
            text.contains("\nprofile neovibe-user-1000 \"/home/a b/.local/lib/neovibe/shell\" flags=(unconfined) {\n")
        );
        assert!(text.contains("include if exists <local/neovibe-user-1000>\n"));
        assert!(!text.contains("<local/neovibe>"));
        assert!(!text.contains("\"/usr/lib/neovibe/shell\""));
    }

    /// The escaping that loaded and attached on the VM (a path with a space and `[]{}*?^@"`).
    #[test]
    fn apparmor_glob_characters_and_the_quote_are_escaped() {
        let text = render_profile("p", Path::new("/home/sp ace [x]{y}*?^@q\"z/shell")).unwrap();
        assert!(text.contains(r#"profile p "/home/sp ace \[x\]\{y\}\*\?\^\@q\"z/shell" flags=(unconfined) {"#));
        let text = render_profile("p", Path::new(r"/home/back\slash/shell")).unwrap();
        assert!(text.contains(r#""/home/back\\slash/shell""#));
    }

    #[test]
    fn a_path_that_cannot_go_into_a_profile_renders_nothing() {
        assert_eq!(render_profile("p", Path::new("/home/new\nline/shell")), None);
        assert_eq!(render_profile("p", Path::new("/home/tab\there/shell")), None);
        assert_eq!(render_profile("p", Path::new("relative/shell")), None);
    }

    #[test]
    fn the_data_directory_follows_the_installers_rule() {
        assert_eq!(
            data_home(Some(OsStr::new("/x/data")), Some(OsStr::new("/home/u"))),
            Some(PathBuf::from("/x/data"))
        );
        assert_eq!(
            data_home(Some(OsStr::new("relative")), Some(OsStr::new("/home/u"))),
            Some(PathBuf::from("/home/u/.local/share"))
        );
        assert_eq!(
            data_home(Some(OsStr::new("")), Some(OsStr::new("/home/u"))),
            Some(PathBuf::from("/home/u/.local/share"))
        );
        assert_eq!(data_home(None, Some(OsStr::new("relative"))), None);
        assert_eq!(data_home(None, None), None);
    }

    #[test]
    fn shell_quoting_leaves_plain_words_alone() {
        assert_eq!(
            shell_quote("/etc/apparmor.d/neovibe-user-1000"),
            "/etc/apparmor.d/neovibe-user-1000"
        );
        assert_eq!(shell_quote("/home/a b/x"), "'/home/a b/x'");
        assert_eq!(shell_quote("/home/it's/x"), r"'/home/it'\''s/x'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn the_package_with_its_profile_needs_one_command() {
        let fix = plan_fix(
            Path::new("/usr/lib/neovibe/shell"),
            true,
            Some(Path::new("/home/u/.local/share")),
            UID,
        )
        .unwrap();
        assert_eq!(fix.profile, PathBuf::from("/etc/apparmor.d/neovibe"));
        assert_eq!(fix.commands, ["sudo apparmor_parser -r /etc/apparmor.d/neovibe"]);
        assert_eq!(fix.write, None);
    }

    /// A package without the file (an older one): the same `neovibe` profile, rendered -- the same
    /// bytes the package would have installed -- and installed by hand.
    #[test]
    fn the_package_without_its_profile_gets_the_same_file_by_hand() {
        let fix = plan_fix(
            Path::new("/usr/lib/neovibe/shell"),
            false,
            Some(Path::new("/home/u/.local/share")),
            UID,
        )
        .unwrap();
        assert_eq!(
            fix.commands,
            [
                "sudo install -m 0644 /home/u/.local/share/neovibe/apparmor/neovibe /etc/apparmor.d/neovibe",
                "sudo apparmor_parser -r /etc/apparmor.d/neovibe",
            ]
        );
        assert_eq!(fix.write.unwrap().1, PROFILE_TEMPLATE);
    }

    #[test]
    fn a_per_user_install_gets_its_own_profile_and_two_commands() {
        let exe = Path::new("/home/a b/.local/lib/neovibe/shell");
        let fix = plan_fix(exe, true, Some(Path::new("/home/a b/.local/share")), UID).unwrap();
        let source = PathBuf::from("/home/a b/.local/share/neovibe/apparmor/neovibe-user-1000");
        assert_eq!(fix.profile, source);
        assert_eq!(
            fix.commands,
            [
                "sudo install -m 0644 '/home/a b/.local/share/neovibe/apparmor/neovibe-user-1000' \
                 /etc/apparmor.d/neovibe-user-1000",
                "sudo apparmor_parser -r /etc/apparmor.d/neovibe-user-1000",
            ]
        );
        assert_eq!(
            fix.write,
            Some((source, render_profile("neovibe-user-1000", exe).unwrap()))
        );
        assert_eq!(
            plan_fix(exe, true, None, UID),
            None,
            "no data directory, nothing to name"
        );
    }

    #[test]
    fn the_notice_says_what_happened_the_fix_and_the_escape_hatch() {
        let fix = plan_fix(
            Path::new("/home/u/.local/lib/neovibe/shell"),
            false,
            Some(Path::new("/home/u/.local/share")),
            UID,
        )
        .unwrap();
        let text = notice("bwrap: setting up uid map: Permission denied", Some(&fix), None);
        let first = text.split("\n\n").next().unwrap();
        assert!(!first.contains('\n'), "what happened is one paragraph: {first}");
        assert!(first.contains("bwrap: setting up uid map: Permission denied"));
        for command in &fix.commands {
            assert!(text.contains(&format!("\n    {command}\n")), "{text}");
        }
        assert!(text.contains("The profile: /home/u/.local/share/neovibe/apparmor/neovibe-user-1000\n"));
        assert!(text.contains("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1 also works"));
        assert!(
            text.contains("removes the operating system's sandbox from the process that renders the model's output")
        );
        assert!(!text.contains("could not write"));

        let packaged = plan_fix(Path::new("/usr/lib/neovibe/shell"), true, None, UID).unwrap();
        let text = notice("x", Some(&packaged), None);
        assert!(text.contains("load the AppArmor profile the neovibe package installed (or restart the computer)"));
        assert!(text.contains("\n    sudo apparmor_parser -r /etc/apparmor.d/neovibe\n"));

        let text = notice("x", Some(&fix), Some("disk full"));
        assert!(text.contains("(neovibe could not write it: disk full)"));
        let text = notice("x", None, None);
        assert!(text.contains("cannot name an AppArmor profile") && text.contains(ESCAPE_HATCH));
    }

    #[test]
    fn a_rendered_profile_is_written_only_when_it_changed() {
        let dir = std::env::temp_dir().join(format!("nv-webkit-sandbox-{}", uuid::Uuid::new_v4()));
        let path = dir.join("neovibe/apparmor/neovibe-user-1000");
        write_if_changed(&path, "one").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
        write_if_changed(&path, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The label and the installer name the same commands, and write the same profile: this runs the
    /// installer's own functions (`apparmor_plan`, `apparmor_render`, sourced from
    /// `packaging/install.sh` with `--help` so `main` does nothing) against the same inputs.
    #[test]
    fn the_installer_renders_the_same_profile_and_commands() {
        let installer = concat!(env!("CARGO_MANIFEST_DIR"), "/../packaging/install.sh");
        for (exe, data) in [
            ("/home/tester/.local/lib/neovibe/shell", "/home/tester/.local/share"),
            ("/home/a b/it's [x]/.local/lib/neovibe/shell", "/home/a b/it's [x]/data"),
            (
                r#"/home/q"uo*te\b{c}?^@/.local/lib/neovibe/shell"#,
                r#"/home/q"uo*te\b{c}?^@/.local/share"#,
            ),
            ("/usr/lib/neovibe/shell", "/home/tester/.local/share"),
        ] {
            let script = r#"
                data=$1 exe=$2
                set -- --help
                . "$0" >/dev/null
                NV_DATA=$data
                apparmor_plan "$exe" 1000
                printf '%s\n' "$AA_CMD1" "$AA_CMD2" "$AA_SRC" "$AA_NAME"
                printf '%s\n' ---
                apparmor_render "$AA_NAME" "$exe"
            "#;
            let out = Command::new("sh")
                .args(["-c", script, installer, data, exe])
                .env_remove("NEOVIBE_INSTALL_TEST")
                .output()
                .expect("sh runs");
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            let out = String::from_utf8(out.stdout).unwrap();
            let (plan, rendered) = out.split_once("---\n").unwrap();
            let lines: Vec<&str> = plan.lines().collect();
            let fix = plan_fix(Path::new(exe), false, Some(Path::new(data)), 1000).unwrap();
            let (source, text) = fix.write.clone().unwrap();
            assert_eq!(lines[..2], fix.commands[..], "{exe}");
            assert_eq!(lines[2], source.to_str().unwrap(), "{exe}");
            assert_eq!(lines[3], profile_name(Path::new(exe), 1000), "{exe}");
            assert_eq!(rendered, text, "{exe}");
        }
    }
}
