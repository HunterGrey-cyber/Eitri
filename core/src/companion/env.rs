//! What a window removes from its own environment before any thread or child exists: the editor
//! an Eitri was started from (an nvim `:terminal`) must hand neither its RPC address nor its
//! window's sockets to the agent, its tools, the bottom shell or the embedded nvim. GTK-free, so
//! every host that starts children uses the same lists.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;

/// What a panel started by nvim's `jobstart` inherits from that nvim. Left in this process's
/// environment, every sidecar and every agent tool command would inherit `$NVIM` and could drive the
/// user's editor over RPC, which companion mode must not add. `eitri panel` reads `$NVIM` (the default
/// address) and then removes all five, with everything the full window removes, before any thread
/// exists.
pub const INHERITED_FROM_EDITOR: [&str; 5] = ["NVIM", "NVIM_LISTEN_ADDRESS", "MYVIMRC", "VIMRUNTIME", "VIM"];

/// The two variables that carry an nvim's RPC address to the programs it starts. The full window
/// removes these (see [`drop_inherited_editor_env`]) but not `VIMRUNTIME`, `VIM` or `MYVIMRC`, which
/// may be the user's own exports, meant for the embedded nvim too.
pub const EDITOR_RPC_ADDRESS: [&str; 2] = ["NVIM", "NVIM_LISTEN_ADDRESS"];

/// What a window sets on its own embedded nvim child alone: the paths of that window's pane-switch,
/// theme, keys and editor-context sockets and of the Lua each loads. No window reads them from its own
/// environment; it builds them per child. So when they are present in this process, they were
/// inherited from another window's nvim (this one was started from its `:terminal`), and every child
/// of this window -- the sidecar and each agent tool, the bottom shell -- could write fake pane
/// switches, key reports or editor context into that other window's sockets, and this window's own
/// nvim would run the other window's Lua wherever this one has none of its own to set.
pub const SET_ON_THE_NVIM_CHILD: [&str; 11] = [
    "EITRI_PANE_SWITCH_SOCKET",
    "EITRI_NAV_LUA",
    "EITRI_EDITOR_WATCH_LUA",
    "EITRI_REOPEN_LUA",
    "EITRI_THEME_SOCKET",
    "EITRI_THEME_LUA",
    "EITRI_KEYS_SOCKET",
    "EITRI_KEYS_LUA",
    "EITRI_EDITOR_SOCKET",
    "EITRI_EDITOR_LUA",
    "EITRI_SCRATCH_LUA",
];

/// The socket a terminal's shell gives an `eitri FILE` run inside it, so the file opens in the window
/// that holds the terminal. A window reads it from its environment only to hand a file over, and only
/// before it starts anything.
pub const OPEN_SOCKET_ENV: &str = "EITRI_OPEN_SOCKET";

/// What a window sets on its bottom terminals' shells alone. Left in this process, every child of an
/// Eitri started from such a terminal -- the sidecar and each agent tool, its own shells -- could open
/// files in the window that holds that terminal.
pub const SET_ON_THE_TERMINAL_CHILD: [&str; 1] = [OPEN_SOCKET_ENV];

/// The prefix of every variable the pinned Neovide fork's command line reads (`NEOVIDE_SERVER`, `NEOVIDE_FORK`,
/// `NEOVIDE_CHDIR`, `NEOVIDE_NO_MULTIGRID`, `NEOVIDE_VSYNC`, ...; about thirty, each also a flag) and its
/// configuration path (`NEOVIDE_CONFIG`). They belong to a person's Neovide, not to Eitri's embedded editor.
const NEOVIDE_ENV_PREFIX: &[u8] = b"NEOVIDE_";

/// The `NEOVIDE_`-prefixed variables that are Eitri's own debug switches, read by `neovide-editor`'s keyboard
/// path (`NEOVIDE_EDITOR_LOG_IME`, `NEOVIDE_EDITOR_LOG_KEYS`): the one place the prefix means Eitri. They stay.
pub const NEOVIDE_ENV_OF_EITRI: [&str; 2] = ["NEOVIDE_EDITOR_LOG_IME", "NEOVIDE_EDITOR_LOG_KEYS"];

/// Whether `name` is a variable meant for a person's Neovide that this process must not obey: it starts with
/// `NEOVIDE_` (byte-wise, so a name that is not UTF-8 counts) and is not one of [`NEOVIDE_ENV_OF_EITRI`].
///
/// The embedded editor builds its settings through the fork's own command-line parser, which reads these
/// from the process environment whatever arguments it is given. Left in place, `NEOVIDE_SERVER` makes every
/// editor attach to that server instead of starting its own nvim (and close it, or the process it names,
/// when the window ends), and `NEOVIDE_FORK`, `NEOVIDE_CHDIR`, `NEOVIDE_NO_MULTIGRID` or `NEOVIDE_VSYNC`
/// change the editor in ways nothing here expects. Eitri's own settings are `eitri.config`, so none of them
/// has a meaning for it.
pub fn is_neovide_env(name: &OsStr) -> bool {
    name.as_bytes().starts_with(NEOVIDE_ENV_PREFIX) && !NEOVIDE_ENV_OF_EITRI.iter().any(|own| name == *own)
}

/// Every name in `names` (a process's environment) for which [`is_neovide_env`] holds, each once.
pub fn neovide_env_names(names: impl Iterator<Item = OsString>) -> Vec<OsString> {
    let mut found: Vec<OsString> = Vec::new();
    for name in names {
        if is_neovide_env(&name) && !found.contains(&name) {
            found.push(name);
        }
    }
    found
}

/// The `TMUX`/`TMUX_PANE` pair a window's nvim child gets for the pane-switch shim, recognised by
/// `TMUX`'s socket field (tmux's own `<socket>,<pid>,<session>` shape, the socket possibly holding a
/// comma) naming the inherited pane-switch socket. A real tmux's `TMUX` names its own socket, never
/// that one, so it is kept.
fn is_the_shims_tmux(tmux: Option<&OsStr>, pane_switch_socket: Option<&OsStr>) -> bool {
    let (Some(tmux), Some(socket)) = (tmux.and_then(OsStr::to_str), pane_switch_socket) else {
        return false;
    };
    !socket.is_empty()
        && tmux
            .rsplitn(3, ',')
            .nth(2)
            .is_some_and(|field| OsStr::new(field) == socket)
}

/// Every inherited editor variable [`drop_inherited_editor_env`] removes, given how this process's
/// environment reads (`get`). Decided before anything is removed: the shim's `TMUX` is recognised by
/// the pane-switch socket that is itself on the list.
pub fn inherited_editor_env(get: &dyn Fn(&str) -> Option<OsString>) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = EDITOR_RPC_ADDRESS
        .iter()
        .chain(&SET_ON_THE_NVIM_CHILD)
        .chain(&SET_ON_THE_TERMINAL_CHILD)
        .copied()
        .collect();
    if is_the_shims_tmux(get("TMUX").as_deref(), get("EITRI_PANE_SWITCH_SOCKET").as_deref()) {
        names.extend(["TMUX", "TMUX_PANE"]);
    }
    names
}

/// Calls `remove` once for every name in `names`. Separate from the `remove_var` it is given, so a
/// test can see every name reach it.
pub fn scrub(names: &[&str], remove: &dyn Fn(&str)) {
    for name in names {
        remove(name);
    }
}

/// What `eitri split` removes from its own environment, given how that environment reads (`get`):
/// always the RPC address and the other window's sockets ([`inherited_editor_env`]), and
/// `MYVIMRC`, `VIMRUNTIME` and `VIM` only when `$NVIM` is set, which is how a split started from an
/// nvim's `:terminal` shows (the same test `eitri split`'s entry point in `shell` reads first). Without it those three are the
/// user's own exports, which the Neovide nvim started next needs (a `VIMRUNTIME` for a source or
/// Nix build), and they pass through.
pub fn split_scrub_names(get: &dyn Fn(&str) -> Option<OsString>) -> Vec<&'static str> {
    let mut names = inherited_editor_env(get);
    if get("NVIM").is_some() {
        for name in ["MYVIMRC", "VIMRUNTIME", "VIM"] {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// For the full window: an Eitri started from an nvim `:terminal` inherits that editor's RPC address,
/// and every child -- the sidecar and each agent tool under it, the bottom terminal's shell, the
/// embedded nvim -- would inherit it in turn and could drive the outer editor over RPC. When that
/// nvim is another window's, it also carries that window's socket variables
/// ([`SET_ON_THE_NVIM_CHILD`]) and the shim's `TMUX`; from a bottom terminal, it inherits
/// [`OPEN_SOCKET_ENV`]. Whatever Neovide variables a person's shell exports ([`is_neovide_env`]) are
/// removed too, since the fork's parser would obey them. Nothing after this call reads any of them, so they
/// are removed from this process. `main` calls this before any thread or child exists.
pub fn drop_inherited_editor_env() {
    let names = inherited_editor_env(&|name| std::env::var_os(name));
    let neovide = neovide_env_names(std::env::vars_os().map(|(name, _)| name));
    let remove = |name: &OsStr| {
        // SAFETY: `main` calls this on its only thread, before GTK, the backend check, the nvim probe or
        // any child has started, so nothing else can be reading the environment.
        unsafe { std::env::remove_var(name) }
    };
    scrub(&names, &|name| remove(OsStr::new(name)));
    for name in &neovide {
        remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn editor_env_list_names_nvims_rpc_address() {
        assert!(INHERITED_FROM_EDITOR.contains(&"NVIM"));
        assert!(INHERITED_FROM_EDITOR.contains(&"NVIM_LISTEN_ADDRESS"));
    }

    #[test]
    fn editor_rpc_address_is_nvims_and_inside_the_companion_list() {
        assert!(EDITOR_RPC_ADDRESS.contains(&"NVIM"));
        assert!(EDITOR_RPC_ADDRESS.contains(&"NVIM_LISTEN_ADDRESS"));
        for name in EDITOR_RPC_ADDRESS {
            assert!(INHERITED_FROM_EDITOR.contains(&name), "{name}");
        }
    }

    /// Every socket or Lua path a window hands its nvim child is one an inner window must drop. Read
    /// off `eitri-core`'s sources, every string literal naming an `EITRI_..._SOCKET` or
    /// `EITRI_..._LUA` variable, so a new feed cannot be added without joining the list.
    #[test]
    fn every_nvim_child_socket_variable_is_dropped() {
        let mut found = std::collections::BTreeSet::new();
        for source in crate::source_scan::rust_sources(&["core/src"]) {
            let text = &source.text;
            for (at, _) in text.match_indices(concat!("\"", "EITRI_")) {
                let rest = &text[at + 1..];
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_uppercase() || *c == '_')
                    .collect();
                if rest[name.len()..].starts_with('"') && (name.ends_with("_SOCKET") || name.ends_with("_LUA")) {
                    found.insert(name);
                }
            }
        }
        assert!(found.len() >= 11, "{found:?}");
        assert_eq!(SET_ON_THE_NVIM_CHILD.len(), 11);
        assert!(SET_ON_THE_NVIM_CHILD.contains(&"EITRI_REOPEN_LUA"));
        for name in &found {
            assert!(
                SET_ON_THE_NVIM_CHILD
                    .iter()
                    .chain(&SET_ON_THE_TERMINAL_CHILD)
                    .any(|listed| listed == name),
                "{name} is not dropped"
            );
        }
    }

    #[test]
    fn an_inner_window_drops_the_outer_windows_nvim_variables() {
        let socket = "/tmp/eitri-ps-1,2/ps";
        let outer = |name: &str| -> Option<OsString> {
            match name {
                "TMUX" => Some(format!("{socket},4242,0").into()),
                "EITRI_PANE_SWITCH_SOCKET" => Some(socket.into()),
                _ => None,
            }
        };
        let names = inherited_editor_env(&outer);
        for name in EDITOR_RPC_ADDRESS.iter().chain(&SET_ON_THE_NVIM_CHILD) {
            assert!(names.contains(name), "{name}");
        }
        assert!(names.contains(&"TMUX") && names.contains(&"TMUX_PANE"), "{names:?}");
        for kept in ["VIMRUNTIME", "VIM", "MYVIMRC", "PATH"] {
            assert!(!names.contains(&kept), "{kept}");
        }
    }

    /// An Eitri started in a terminal of another window, and that Eitri's agent and tools, must not
    /// be able to open files in the window that started the terminal.
    #[test]
    fn a_window_drops_the_open_socket_of_the_terminal_it_was_started_from() {
        let names = inherited_editor_env(&|_| None);
        assert!(names.contains(&"EITRI_OPEN_SOCKET"), "{names:?}");
        assert!(split_scrub_names(&|_| None).contains(&"EITRI_OPEN_SOCKET"));
    }

    /// A real tmux's `TMUX` stays: only the shim's names the pane-switch socket.
    #[test]
    fn a_real_tmux_is_kept() {
        let real = |name: &str| -> Option<OsString> {
            match name {
                "TMUX" => Some("/tmp/tmux-1000/default,1234,0".into()),
                "EITRI_PANE_SWITCH_SOCKET" => Some("/tmp/eitri-ps-1/ps".into()),
                _ => None,
            }
        };
        assert!(!inherited_editor_env(&real).contains(&"TMUX"));
        let only_tmux =
            |name: &str| -> Option<OsString> { (name == "TMUX").then(|| "/tmp/tmux-1000/default,1234,0".into()) };
        assert!(!inherited_editor_env(&only_tmux).contains(&"TMUX"));
        assert!(!is_the_shims_tmux(Some(OsStr::new("ps")), Some(OsStr::new("ps"))));
        assert!(!is_the_shims_tmux(Some(OsStr::new(",1,0")), Some(OsStr::new(""))));
    }

    #[test]
    fn a_split_keeps_the_users_own_runtime_variables_unless_it_started_inside_an_nvim() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        // From a plain shell: the RPC address names are always on the list, the three are not.
        let plain = split_scrub_names(&env(&[("VIMRUNTIME", "/src/nvim/runtime"), ("MYVIMRC", "/x/init.lua")]));
        for name in EDITOR_RPC_ADDRESS {
            assert!(plain.contains(&name), "{name}");
        }
        for name in ["MYVIMRC", "VIMRUNTIME", "VIM"] {
            assert!(!plain.contains(&name), "{name} is the user's own here");
        }
        // From an nvim's `:terminal` (`$NVIM` set): those three describe that nvim.
        let inside = split_scrub_names(&env(&[
            ("NVIM", "/run/nvim"),
            ("VIMRUNTIME", "/usr/share/nvim/runtime"),
        ]));
        for name in INHERITED_FROM_EDITOR {
            assert!(inside.contains(&name), "{name}");
        }
        // Another window's sockets go either way.
        assert!(plain.contains(&"EITRI_PANE_SWITCH_SOCKET"));
        assert_eq!(inside.iter().filter(|name| **name == "NVIM").count(), 1);
    }

    #[test]
    fn editor_env_is_scrubbed_before_any_thread() {
        for names in [&INHERITED_FROM_EDITOR[..], &EDITOR_RPC_ADDRESS[..]] {
            let seen = RefCell::new(Vec::new());
            scrub(names, &|name| seen.borrow_mut().push(name.to_string()));
            assert_eq!(seen.into_inner(), names.to_vec());
        }
    }

    #[test]
    fn every_variable_the_forks_command_line_reads_is_a_neovide_variable_to_drop() {
        // The names the pinned fork's parser reads (`cmd_line.rs` at the pinned rev), as `env = "..."`.
        for name in [
            "NEOVIDE_SERVER",
            "NEOVIDE_FORK",
            "NEOVIDE_CHDIR",
            "NEOVIDE_NO_MULTIGRID",
            "NEOVIDE_VSYNC",
            "NEOVIDE_SRGB",
            "NEOVIDE_FRAME",
            "NEOVIDE_WSL",
            "NEOVIDE_IDLE",
            "NEOVIDE_GRID",
            "NEOVIDE_SIZE",
            "NEOVIDE_MAXIMIZED",
            "NEOVIDE_TABS",
            "NEOVIDE_OPENGL",
            "NEOVIDE_APP_ID",
            "NEOVIDE_ICON",
            "NEOVIDE_SYSTEM_QUIT_HOTKEY",
            "NEOVIDE_CONFIG",
            "NEOVIDE_FORKED_FROM_TTY",
            "NEOVIDE_BACKTRACES",
        ] {
            assert!(is_neovide_env(OsStr::new(name)), "{name}");
        }
    }

    #[test]
    fn only_the_neovide_prefix_counts_and_eitris_own_debug_switches_stay() {
        for name in [
            "NEOVIDEX",
            "NEOVIDE",
            "XNEOVIDE_SERVER",
            "neovide_server",
            "NEOVIM_BIN",
            "EITRI_NEOVIDE",
            "HOME",
        ] {
            assert!(!is_neovide_env(OsStr::new(name)), "{name}");
        }
        for name in NEOVIDE_ENV_OF_EITRI {
            assert!(!is_neovide_env(OsStr::new(name)), "{name}");
        }
        // Only those two names: a longer one with the same stem is Neovide's namespace again.
        assert!(is_neovide_env(OsStr::new("NEOVIDE_EDITOR_LOG_KEYS_X")));
        assert!(is_neovide_env(OsStr::new("NEOVIDE_EDITOR")));
    }

    #[test]
    fn a_name_that_is_not_utf8_is_dropped_and_each_name_is_listed_once() {
        use std::os::unix::ffi::OsStringExt;
        let odd = OsString::from_vec(b"NEOVIDE_\xff".to_vec());
        let found = neovide_env_names(
            [
                odd.clone(),
                OsString::from("NEOVIDE_SERVER"),
                OsString::from("PATH"),
                OsString::from("NEOVIDE_SERVER"),
                OsString::from("NEOVIDE_EDITOR_LOG_IME"),
            ]
            .into_iter(),
        );
        assert_eq!(found, [odd, OsString::from("NEOVIDE_SERVER")]);
    }
}
