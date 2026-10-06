//! What a window removes from its own environment before any thread or child exists: the editor
//! an Eitri was started from (an nvim `:terminal`) must hand neither its RPC address nor its
//! window's sockets to the agent, its tools, the bottom shell or the embedded nvim. GTK-free, so
//! every host that starts children uses the same lists.

use std::ffi::{OsStr, OsString};

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
pub const SET_ON_THE_NVIM_CHILD: [&str; 9] = [
    "EITRI_PANE_SWITCH_SOCKET",
    "EITRI_NAV_LUA",
    "EITRI_THEME_SOCKET",
    "EITRI_THEME_LUA",
    "EITRI_KEYS_SOCKET",
    "EITRI_KEYS_LUA",
    "EITRI_EDITOR_SOCKET",
    "EITRI_EDITOR_LUA",
    "EITRI_SCRATCH_LUA",
];

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
/// ([`SET_ON_THE_NVIM_CHILD`]) and the shim's `TMUX`. The full window reads none of them, so they
/// are removed from this process. `main` calls this before any thread or child exists.
pub fn drop_inherited_editor_env() {
    let names = inherited_editor_env(&|name| std::env::var_os(name));
    scrub(&names, &|name| {
        // SAFETY: `main` calls this on its only thread, before GTK, the backend check, the nvim probe or
        // any child has started, so nothing else can be reading the environment.
        unsafe { std::env::remove_var(name) }
    });
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
        assert!(found.len() >= 9, "{found:?}");
        for name in &found {
            assert!(SET_ON_THE_NVIM_CHILD.contains(&name.as_str()), "{name} is not dropped");
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
}
