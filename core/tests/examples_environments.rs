//! The working-environment recipes in `examples/environments/*/init.lua`, each run as an `init.lua` through the Lua
//! kernel a window starts with, the real plugin loader included, then through the checks the window makes of what
//! the file recorded: the startup keymap, the module keys, the layout trees and the open rules. A recipe that any
//! of them refuses would open a window that lists a startup problem, which is what this fails on.
//!
//! The plugin search path is narrowed to this tree's own `plugins/` (a debug build looks there after the user data
//! directory, which is pointed at an empty scratch directory, as are the state directory and `XDG_DATA_DIRS`), so a
//! copy installed on the machine running the tests is never what loads. The environment is process-wide, so the
//! recipes run in one test.
//!
//! Run: `cargo test -p eitri-core --test examples_environments`

use std::path::{Path, PathBuf};

use eitri_core::keymap::tmux::TmuxEnv;
use eitri_core::keymap::{Keymap, LuaClaims};
use eitri_core::layout::keys::ModuleKeys;
use eitri_core::layout::ModuleId;
use eitri_core::lua::kernel::Kernel;

fn recipes() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/environments");
    let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join("init.lua").is_file())
        .collect();
    found.sort();
    found
}

#[test]
fn every_recipe_runs_as_init_lua_with_no_startup_problem() {
    let scratch = std::env::temp_dir().join(format!("eitri-recipes-{}", std::process::id()));
    for sub in ["data", "data-dirs", "state", "home"] {
        std::fs::create_dir_all(scratch.join(sub)).unwrap();
    }
    std::env::set_var("XDG_DATA_HOME", scratch.join("data"));
    std::env::set_var("XDG_DATA_DIRS", scratch.join("data-dirs"));
    std::env::set_var("XDG_STATE_HOME", scratch.join("state"));
    std::env::set_var("HOME", scratch.join("home"));

    let recipes = recipes();
    let names: Vec<String> = recipes
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        ["beside-nvim", "i3", "many-agents", "markdown-writer", "tmux"],
        "a recipe was added or removed: list it here and in the guide"
    );

    for dir in &recipes {
        let name = dir.file_name().unwrap().to_string_lossy();
        let kernel = Kernel::new(dir.clone(), |_, _| Ok(())).unwrap();
        kernel.load_init_file(&dir.join("init.lua"));
        assert_eq!(kernel.startup_problems(), Vec::<String>::new(), "{name}");

        let layout = kernel.layout.borrow();
        if let Some(tree) = layout.default_tree() {
            assert!(tree.is_ok(), "{name}: {tree:?}");
        }
        for (workspace, tree) in layout.defined_workspaces() {
            assert!(tree.is_ok(), "{name}: workspace {workspace}: {tree:?}");
        }
        assert!(layout.rules().is_ok(), "{name}: {:?}", layout.rules().err());

        let plugins = kernel.plugins.borrow();
        let ids: Vec<String> = plugins
            .plugins()
            .iter()
            .map(|r| format!("lua:{}", r.plugin.manifest.id))
            .collect();
        for registration in plugins.plugins() {
            assert!(
                registration
                    .plugin
                    .dir
                    .starts_with(Path::new(env!("CARGO_MANIFEST_DIR")).join("..").canonicalize().unwrap()),
                "{name}: {} loaded from {}",
                registration.plugin.manifest.id,
                registration.plugin.dir.display()
            );
        }
        let config = kernel.config.borrow();
        let from_tmux = config.get("keymap.from_tmux").map(str::to_owned);
        drop(config);
        let ops = kernel.keymap.borrow().ops().to_vec();
        let home = scratch.join("home");
        let (keymap, _) = Keymap::for_startup(
            &ops,
            &ids,
            from_tmux.as_deref(),
            || TmuxEnv {
                home: Some(home.clone()),
                env: Default::default(),
                system_file: home.join("no-system-tmux.conf"),
            },
            &LuaClaims::default(),
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        let written: Vec<(ModuleId, Option<String>)> = plugins
            .plugins()
            .iter()
            .map(|r| (ModuleId::lua(&r.plugin.manifest.id), r.key.clone()))
            .collect();
        let defaults: Vec<(ModuleId, char)> = plugins
            .plugins()
            .iter()
            .filter_map(|r| r.default_key().map(|key| (ModuleId::lua(&r.plugin.manifest.id), key)))
            .collect();
        let (_, refused, unused) = ModuleKeys::build_with_defaults(&written, &defaults, &keymap);
        assert!(refused.is_empty(), "{name}: {refused:?}");
        assert!(
            unused.is_empty(),
            "{name}: a manifest's default key went unused: {unused:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&scratch);
}
