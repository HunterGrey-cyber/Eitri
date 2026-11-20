//! The editor starts with snacks.nvim's scroll animation off, and the user's own config can turn it back on.
//!
//! snacks reads `vim.g.snacks_scroll` on every scroll (`vim.g.snacks_scroll ~= false` in its scroll `filter`), so
//! what has to hold is the variable's value after nvim has run the pane's `--cmd` and then the user's `init.lua`.
//! Each case starts a real headless `nvim` with the pane's own default `--cmd` (`neovide_editor::pane_default_cmd`,
//! the argument `NeovideEditorPane` builds), runs a real `init.lua` loaded with `-u` (so it runs after the `--cmd`,
//! as the user's config does), and writes the variable out to a file. No display, no plugin and no network.
//!
//! Ignored unless asked for, because it spawns a real `nvim` (>= 0.11 on `PATH`):
//!
//!     cargo test -p neovide-editor --test snacks_scroll_default -- --ignored
use std::path::{Path, PathBuf};
use std::process::Command;

/// A scratch directory under the build's own tmp dir, empty at the start of the case.
fn scratch(case: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("snacks_scroll_default")
        .join(case);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Starts nvim with the pane's default `--cmd`, then `extra_args`, then `init_lua` as the user's config (when there
/// is one); returns what the Lua expression `probe` evaluates to once startup is done.
fn after_startup(case: &str, extra_args: &[&str], init_lua: Option<&str>, probe: &str) -> String {
    let dir = scratch(case);
    let out = dir.join("answer");
    let mut command = Command::new("nvim");
    command
        .args(["--headless", "-n", "-i", "NONE", "--noplugin"])
        .args(["--cmd", &neovide_editor::pane_default_cmd()])
        .args(extra_args);
    match init_lua {
        Some(text) => {
            let init = dir.join("init.lua");
            std::fs::write(&init, text).unwrap();
            command.arg("-u").arg(init);
        }
        None => {
            command.args(["-u", "NONE"]);
        }
    }
    let status = command
        .arg("-c")
        .arg(format!(
            "lua vim.fn.writefile({{ tostring({probe}) }}, {:?})",
            out.to_str().unwrap()
        ))
        .args(["-c", "qa!"])
        .env("XDG_CONFIG_HOME", dir.join("xdg-config"))
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("XDG_STATE_HOME", dir.join("xdg-state"))
        .env("XDG_CACHE_HOME", dir.join("xdg-cache"))
        .status()
        .expect("nvim must be on PATH");
    assert!(status.success(), "nvim exited with {status} in case {case}");
    std::fs::read_to_string(&out)
        .unwrap_or_else(|error| panic!("nvim wrote no answer in case {case}: {error}"))
        .trim()
        .to_string()
}

/// `vim.g.snacks_scroll` after startup: `"false"`, `"true"` or `"nil"`.
fn snacks_scroll_after_startup(case: &str, extra_args: &[&str], init_lua: Option<&str>) -> String {
    after_startup(case, extra_args, init_lua, "vim.g.snacks_scroll")
}

#[test]
#[ignore = "spawns a real nvim"]
fn with_nothing_set_the_scroll_animation_is_off() {
    assert_eq!(snacks_scroll_after_startup("default", &[], None), "false");
}

#[test]
#[ignore = "spawns a real nvim"]
fn an_init_lua_that_does_not_mention_it_leaves_it_off() {
    let init = "vim.opt.number = true";
    assert_eq!(snacks_scroll_after_startup("unrelated-init", &[], Some(init)), "false");
}

#[test]
#[ignore = "spawns a real nvim"]
fn an_init_lua_setting_it_true_wins() {
    let init = "vim.g.snacks_scroll = true";
    assert_eq!(snacks_scroll_after_startup("init-true", &[], Some(init)), "true");
}

#[test]
#[ignore = "spawns a real nvim"]
fn a_later_cmd_from_the_host_wins_too() {
    let extra = ["--cmd", "let g:snacks_scroll = v:true"];
    assert_eq!(snacks_scroll_after_startup("host-cmd", &extra, None), "true");
}

#[test]
#[ignore = "spawns a real nvim"]
fn the_one_cmd_carries_the_cursor_default_too() {
    // Both defaults share one `--cmd`; the second must not be dropped after the first.
    let width = after_startup("both", &[], None, "vim.g.neovide_cursor_unfocused_outline_width");
    assert_eq!(width, "0");
}
