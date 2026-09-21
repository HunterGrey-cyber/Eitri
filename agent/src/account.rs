//! Which local Claude account this process spends, as a configured value rather than an accident.
//!
//! **Why this exists, measured 2026-09-21.** A resumed session opened with an empty transcript.
//! The record was real (`~/.local/state/neovibe/conversations/…/89e631b6-….json`, project
//! `/home/user/rust/learning`, title "读handoff准备开始", `updated_at` > `created_at`), and so was
//! the transcript -- but the two were in different accounts. The `shell` process had
//! `CLAUDE_CONFIG_DIR=/home/user/.claude-personal` inherited from the terminal it was launched from,
//! while the sidecar's `claude` had written the transcript into `/home/user/.claude-work`.
//! [`crate::transcript::claude_projects_dir`] read the host's own variable, so it looked in the
//! wrong one of this machine's six config directories and found nothing. `external_writer.rs` had
//! been reading a path that never existed for the same reason.
//!
//! Verdandi's `packages/claude-runtime/src/account.ts` names the disease exactly: *"on a
//! multi-account host 'which subscription does verdandi spend' was decided by whichever shell
//! happened to start the sidecar. That is an accident, not a configuration."* It fixes its own
//! half from `VERDANDI_CLAUDE_ACCOUNT`, resolving the account **once for the whole sidecar
//! process** and setting `CLAUDE_PROFILE`/`CLAUDE_CONFIG_DIR`/`CLAUDE_SECURESTORAGE_CONFIG_DIR`/
//! `ANTHROPIC_CONFIG_DIR` on every `claude` it spawns -- overriding whatever a caller inherited,
//! silently (`agent/src/providers/claude_sidecar/runtime_policy_verification.rs` records the run
//! that proved that override is real). **`account` is not on the proto**: it is a sidecar-process
//! environment variable, and neovibe is what spawns the sidecar, so nothing in Verdandi had to
//! change for this.
//!
//! What this module is, therefore, is the *other* half: neovibe deriving the same directory by the
//! same convention, from the same name, so the place it reads and the place the CLI writes cannot
//! disagree again. The derivation below mirrors `resolveAccountSpec` term for term, including the
//! `VERDANDI_CLAUDE_CONFIG_DIR` override -- because the sidecar inherits this process's
//! environment, an override set here reaches it, and a copy of the convention that ignored it
//! would reintroduce exactly the skew this exists to close.
//!
//! **Scope, stated rather than assumed.** A configured account governs two things: the
//! `VERDANDI_CLAUDE_ACCOUNT` handed to the sidecar child, and where this crate looks for the CLI's
//! transcripts. It deliberately governs **neither** of the two places where this project spawns a
//! `claude` directly -- the legacy backend (`crate::process::spawn_with_binary`) and the handoff
//! binary (`crate::handoff`) -- and that is a decision with a reason, not an omission. On the host
//! this was written for, `claude` on `PATH` is a launcher (`claude-wrapper`) that accepts only a
//! *complete, exactly matching* four-variable tuple and additionally requires an approved `tmux`
//! server for every non-test role. A GTK application started from a desktop entry has no `$TMUX`,
//! so writing the tuple onto those children would turn a working spawn into
//! `claude-wrapper: an approved tmux server is required`. They keep inheriting the environment the
//! window was launched from, and [`crate::process`] says so on stderr when the two differ, rather
//! than diverging in silence.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Verbatim from Verdandi's `ACCOUNT_NAME_PATTERN` (`/^[A-Za-z0-9][A-Za-z0-9._-]*$/`). The name is
/// interpolated into a filesystem path, so it is *restricted* to one harmless path segment rather
/// than sanitized after the fact -- a sanitizer would silently turn a typo into a different, valid
/// account, which is the failure mode this whole module exists to end.
fn name_is_path_safe(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else { return false };
    first.is_ascii_alphanumeric() && chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// One named local Claude account, already resolved to the directory the CLI keeps its config,
/// credentials and `projects/` transcripts in.
///
/// `anthropicConfigDir`, the fourth member of Verdandi's tuple, is deliberately not carried here:
/// nothing on this side reads it, the CLI creates it on demand, and the sidecar derives it for
/// itself from the same name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeAccount {
    name: String,
    config_dir: PathBuf,
}

/// Every way a configured account can be unusable. All of them are startup failures at the call
/// site, and every one names the value that was wrong -- the alternative, falling back to "whoever
/// launched this window", is the accident this module exists to end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountError {
    /// The configured value was empty or whitespace.
    EmptyName,
    /// The name is not a single path-safe segment.
    UnsafeName(String),
    /// `$HOME` is unset, so `$HOME/.claude-<name>` cannot be built.
    NoHome,
    /// `VERDANDI_CLAUDE_CONFIG_DIR` is set to a relative path. Verdandi throws on this; so does
    /// this, rather than joining it onto whatever this process's cwd happens to be.
    RelativeOverride { variable: &'static str, value: String },
    /// The account's directory does not exist (or is not a directory). Almost always a typo in the
    /// name: it passed the pattern and still points nowhere.
    MissingConfigDir { name: String, path: PathBuf },
}

impl std::fmt::Display for AccountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyName => write!(f, "the account name is empty"),
            Self::UnsafeName(name) => write!(
                f,
                "account name {name:?} is not a single path-safe segment \
                 (it must match ^[A-Za-z0-9][A-Za-z0-9._-]*$)"
            ),
            Self::NoHome => write!(
                f,
                "an account is configured but HOME is not set, so its config directory cannot be derived"
            ),
            Self::RelativeOverride { variable, value } => {
                write!(f, "{variable} must be an absolute path, got {value:?}")
            }
            Self::MissingConfigDir { name, path } => write!(
                f,
                "claude account {name:?} has no config directory at {} -- check the spelling",
                path.display()
            ),
        }
    }
}

impl std::error::Error for AccountError {}

impl ClaudeAccount {
    /// Resolves a name against this process's environment, by the host convention Verdandi's
    /// `resolveAccountSpec` implements: `$HOME/.claude-<name>`, overridden whole by
    /// `VERDANDI_CLAUDE_CONFIG_DIR` when that is set (the name is then only a label). Touches no
    /// filesystem -- see [`Self::check_config_dir`] for the half that does.
    pub fn resolve(name: &str) -> Result<Self, AccountError> {
        Self::resolve_from(name, |key| std::env::var(key).ok())
    }

    /// The pure half of [`Self::resolve`], with the environment as a parameter so the convention
    /// can be tested without mutating a process-wide variable inside a threaded test binary.
    pub fn resolve_from(name: &str, env: impl Fn(&str) -> Option<String>) -> Result<Self, AccountError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(AccountError::EmptyName);
        }
        if !name_is_path_safe(name) {
            return Err(AccountError::UnsafeName(name.to_string()));
        }

        let override_value = env("VERDANDI_CLAUDE_CONFIG_DIR")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let config_dir = match override_value {
            Some(value) => {
                if !Path::new(&value).is_absolute() {
                    return Err(AccountError::RelativeOverride {
                        variable: "VERDANDI_CLAUDE_CONFIG_DIR",
                        value,
                    });
                }
                PathBuf::from(value)
            }
            None => {
                let home = env("HOME").filter(|h| !h.is_empty()).ok_or(AccountError::NoHome)?;
                PathBuf::from(home).join(format!(".claude-{name}"))
            }
        };

        Ok(Self {
            name: name.to_string(),
            config_dir,
        })
    }

    /// The impure half: refuses an account whose directory is not there. Verdandi's
    /// `assertAccountUsable` does this at sidecar startup and also checks for stored credentials;
    /// this checks only the directory, because the credentials check belongs to the process that
    /// is about to authenticate and it has the better message. What this catches is the one case
    /// that would otherwise be silent *here*: a misspelled name resolving to a directory nobody
    /// ever wrote a transcript into, whose symptom is an empty history -- the exact symptom this
    /// change exists to fix.
    pub fn check_config_dir(&self) -> Result<(), AccountError> {
        if self.config_dir.is_dir() {
            return Ok(());
        }
        Err(AccountError::MissingConfigDir {
            name: self.name.clone(),
            path: self.config_dir.clone(),
        })
    }

    /// What goes in `VERDANDI_CLAUDE_ACCOUNT` on the sidecar child, and in `CLAUDE_PROFILE` on this
    /// host's launcher tuple.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `CLAUDE_CONFIG_DIR`: holds `.claude.json`, `.credentials.json`, `settings.json` and
    /// `projects/`.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }
}

/// Which name this process should use: `init.lua`'s `agent.account` first, and failing that
/// whatever `VERDANDI_CLAUDE_ACCOUNT` this process already inherited.
///
/// The second term is not a convenience. The sidecar is a child of this process and reads that
/// variable for itself, so an inherited value already decides which account the CLI writes to --
/// ignoring it here would leave this side reading one account while the CLI writes another, which
/// is the exact skew this module exists to close, arrived at from the other direction. An explicit
/// `agent.account` still wins, and `shell` then writes it onto the child, so both halves agree on
/// the name the window was configured with rather than the one its terminal happened to carry.
pub fn name_to_use<'a>(configured: Option<&'a str>, inherited: Option<&'a str>) -> Option<&'a str> {
    configured
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .or(inherited.map(str::trim).filter(|v| !v.is_empty()))
}

/// Both halves in order, for a host that has an optional configured value and nothing else:
/// nothing configured and nothing inherited means `None` out and no filesystem touched, so a
/// window that configures no account never pays for a feature it did not turn on. Any error here
/// is a startup failure at the call site -- `shell` prints it and exits, naming the key -- because
/// the alternative, falling back to whichever account launched the window, is the accident this
/// module exists to end.
pub fn resolve_configured(value: Option<&str>) -> Result<Option<ClaudeAccount>, AccountError> {
    let inherited = std::env::var("VERDANDI_CLAUDE_ACCOUNT").ok();
    let Some(name) = name_to_use(value, inherited.as_deref()) else {
        return Ok(None);
    };
    let account = ClaudeAccount::resolve(name)?;
    account.check_config_dir()?;
    Ok(Some(account))
}

static ACCOUNT: OnceLock<ClaudeAccount> = OnceLock::new();

/// Pins this process's account. Called once, by `shell`, immediately after `init.lua` has run --
/// the account is a property of the process, exactly as it is for the sidecar, and every later
/// reader ([`configured`]) sees one answer for the window's whole lifetime.
///
/// A second call keeps the first account and says so. It cannot happen in the product (one call
/// site, on the startup path) and silently replacing a live account mid-process would be worse
/// than the noise.
pub fn configure(account: ClaudeAccount) {
    if let Err(ignored) = ACCOUNT.set(account) {
        eprintln!(
            "[account] configure() called twice; keeping {:?} and ignoring {:?}",
            ACCOUNT.get().map(ClaudeAccount::name).unwrap_or("<none>"),
            ignored.name()
        );
    }
}

/// The pinned account, or `None` when nothing pinned one -- which is the shipped default and the
/// state every test runs in. `None` means every path in this crate behaves exactly as it did
/// before this module existed.
pub fn configured() -> Option<&'static ClaudeAccount> {
    ACCOUNT.get()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The environment as a fixture: no process-wide variable is touched, so these run alongside
    /// this crate's `CLAUDE_CONFIG_DIR`-mutating tests without the lock those need.
    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| (*v).to_string())
    }

    #[test]
    fn a_name_expands_by_the_hosts_own_convention() {
        let account = ClaudeAccount::resolve_from("work", env_of(&[("HOME", "/home/user")])).unwrap();
        assert_eq!(account.name(), "work");
        assert_eq!(account.config_dir(), Path::new("/home/user/.claude-work"));
    }

    /// The same four names this machine's launcher knows, so the convention is checked against
    /// real directories rather than one invented example.
    #[test]
    fn every_account_this_host_has_resolves_where_it_really_lives() {
        for (name, expected) in [
            ("work", "/home/user/.claude-work"),
            ("personal", "/home/user/.claude-personal"),
            ("team", "/home/user/.claude-team"),
            ("test", "/home/user/.claude-test"),
        ] {
            let account = ClaudeAccount::resolve_from(name, env_of(&[("HOME", "/home/user")])).unwrap();
            assert_eq!(account.config_dir(), Path::new(expected), "for account {name}");
        }
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_rather_than_interpolated_into_the_path() {
        let account = ClaudeAccount::resolve_from("  work\n", env_of(&[("HOME", "/home/user")])).unwrap();
        assert_eq!(account.config_dir(), Path::new("/home/user/.claude-work"));
    }

    /// Each of these would be a *different, valid* path if the name were sanitized instead of
    /// refused -- which is why it is refused.
    #[test]
    fn a_name_that_is_not_one_path_segment_is_refused_not_sanitized() {
        for bad in [
            "",
            "   ",
            "../personal",
            "a/b",
            ".hidden",
            "-lead",
            "wat?",
            "work work",
            "ca\0nary",
        ] {
            let result = ClaudeAccount::resolve_from(bad, env_of(&[("HOME", "/home/user")]));
            assert!(result.is_err(), "{bad:?} should not resolve, got {result:?}");
        }
    }

    #[test]
    fn the_override_replaces_the_whole_derivation_and_must_be_absolute() {
        let account = ClaudeAccount::resolve_from(
            "work",
            env_of(&[
                ("HOME", "/home/user"),
                ("VERDANDI_CLAUDE_CONFIG_DIR", "/srv/accounts/one"),
            ]),
        )
        .unwrap();
        assert_eq!(account.config_dir(), Path::new("/srv/accounts/one"));
        assert_eq!(
            account.name(),
            "work",
            "the name stays the label the sidecar is handed"
        );

        assert_eq!(
            ClaudeAccount::resolve_from(
                "work",
                env_of(&[("HOME", "/home/user"), ("VERDANDI_CLAUDE_CONFIG_DIR", "relative/dir")]),
            ),
            Err(AccountError::RelativeOverride {
                variable: "VERDANDI_CLAUDE_CONFIG_DIR",
                value: "relative/dir".to_string(),
            })
        );

        // An empty override is "not set", matching Verdandi's own `?.trim()` / `!== ''` guard --
        // otherwise an exported-but-empty variable would resolve the account to `/`.
        let account = ClaudeAccount::resolve_from(
            "work",
            env_of(&[("HOME", "/home/user"), ("VERDANDI_CLAUDE_CONFIG_DIR", "  ")]),
        )
        .unwrap();
        assert_eq!(account.config_dir(), Path::new("/home/user/.claude-work"));
    }

    #[test]
    fn without_home_and_without_an_override_there_is_no_directory_to_derive() {
        assert_eq!(
            ClaudeAccount::resolve_from("work", env_of(&[])),
            Err(AccountError::NoHome)
        );
    }

    #[test]
    fn a_directory_that_is_not_there_is_named_not_shrugged_at() {
        let account = ClaudeAccount::resolve_from(
            "canry",
            env_of(&[("VERDANDI_CLAUDE_CONFIG_DIR", "/nonexistent/neovibe-account-test")]),
        )
        .unwrap();
        let err = account.check_config_dir().unwrap_err();
        assert_eq!(
            err,
            AccountError::MissingConfigDir {
                name: "canry".to_string(),
                path: PathBuf::from("/nonexistent/neovibe-account-test"),
            }
        );
        // The message has to carry both, because the typo is only visible next to the path.
        let rendered = err.to_string();
        assert!(rendered.contains("canry"), "{rendered}");
        assert!(rendered.contains("/nonexistent/neovibe-account-test"), "{rendered}");
    }

    #[test]
    fn a_real_directory_passes_the_same_check() {
        let dir = std::env::temp_dir().join(format!("neovibe-account-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let account = ClaudeAccount::resolve_from(
            "work",
            env_of(&[("VERDANDI_CLAUDE_CONFIG_DIR", dir.to_str().unwrap())]),
        )
        .unwrap();
        assert_eq!(account.check_config_dir(), Ok(()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unconfigured_window_resolves_to_nothing_and_touches_no_disk() {
        assert_eq!(name_to_use(None, None), None);
    }

    /// An inherited `VERDANDI_CLAUDE_ACCOUNT` already decides the sidecar's account, so this side
    /// follows it rather than reading a different one; an explicit `agent.account` still wins.
    #[test]
    fn an_explicit_account_wins_over_an_inherited_one_and_an_inherited_one_beats_nothing() {
        assert_eq!(name_to_use(Some("work"), Some("personal")), Some("work"));
        assert_eq!(name_to_use(None, Some("personal")), Some("personal"));
        assert_eq!(name_to_use(Some("  "), Some("personal")), Some("personal"));
        assert_eq!(name_to_use(Some("work"), None), Some("work"));
        assert_eq!(name_to_use(None, Some("  ")), None);
    }

    #[test]
    fn a_misspelled_account_is_a_startup_error_carrying_the_name_and_the_path() {
        let err = resolve_configured(Some("definitely-not-an-account-on-this-host")).unwrap_err();
        assert!(
            matches!(err, AccountError::MissingConfigDir { .. }),
            "expected the directory check to catch it, got {err:?}"
        );
    }

    /// The shipped default. Nothing in the product calls `configure` unless `init.lua` asked for an
    /// account, and this is what every other test in this crate runs under.
    #[test]
    fn nothing_is_pinned_unless_something_pins_it() {
        assert_eq!(configured(), None);
    }
}
