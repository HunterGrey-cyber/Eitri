//! The Lua kernel's `require` sandbox against the process's working directory. Each test changes
//! the cwd to a planted project directory, which is process-wide, so these tests live in a binary
//! of their own: in `eitri-core`'s unit-test binary they would race every test that reads the cwd
//! (`split`, `project_root`, `nvim_bin`). Inside this binary `CWD_TEST_LOCK` serializes them.
//!
//! The kernel is driven through its public surface only: an `init.lua` runs `require` under
//! `pcall` and records what it saw in the config store.
//!
//! Run: `cargo test -p eitri-core --test lua_require_sandbox`

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use eitri_core::lua::kernel::{refuse_panels, Kernel};

/// Cargo runs a binary's tests on several threads and `std::env::set_current_dir` is
/// process-wide, so a second test could observe (or fight over) a cwd this one is still using.
static CWD_TEST_LOCK: Mutex<()> = Mutex::new(());

fn unique_dir(what: &str) -> PathBuf {
    std::env::temp_dir().join(format!("nv-lua-test-{what}-{}", uuid::Uuid::new_v4()))
}

/// Runs `init_lua` as `<config_dir>/init.lua` with the process cwd set to `cwd`, restores the
/// cwd even when the closure panics, and returns the kernel's answer for each of `keys`.
fn run_init_in(cwd: &Path, config_dir: &Path, init_lua: &str, keys: &[&str]) -> Vec<Option<String>> {
    let _cwd_guard = CWD_TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let original_cwd = std::env::current_dir().expect("cwd");
    std::fs::create_dir_all(config_dir).expect("create the config dir");
    let init = config_dir.join("init.lua");
    std::fs::write(&init, init_lua).expect("write init.lua");

    std::env::set_current_dir(cwd).expect("chdir into the fake project");
    // Caught rather than propagated, so the cwd is restored whatever the kernel does.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let kernel = Kernel::new(config_dir.to_path_buf(), refuse_panels).expect("construct Kernel");
        kernel.run_and_check_init_file(&init).expect("init.lua is accepted");
        let config = kernel.config.borrow();
        keys.iter()
            .map(|k| config.get(k).map(str::to_owned))
            .collect::<Vec<_>>()
    }));
    std::env::set_current_dir(&original_cwd).expect("restore the real cwd");
    outcome.unwrap()
}

/// Before the sandbox, `Lua::new()`'s default `package.path` included `./?.lua;./?/init.lua`, so
/// `require` searched the process's current working directory -- which for a window is
/// normally the project the user opened, never the owner's own config directory. A module
/// planted in an opened project could therefore shadow, or simply run as, one of the owner's
/// own `init.lua` modules. This reproduces that failure (a module resolving from the cwd) and
/// checks the fix (the same name resolving from `<config_dir>/lua/` instead, and `package.cpath`
/// empty).
#[test]
fn require_resolves_only_against_the_config_directory_never_the_launch_cwd() {
    let project_dir = unique_dir("project");
    std::fs::create_dir_all(&project_dir).expect("create fake project dir");
    std::fs::write(project_dir.join("planted.lua"), "return 'planted-from-project'")
        .expect("plant a module in the fake project");

    let config_dir = unique_dir("config");
    std::fs::create_dir_all(config_dir.join("lua")).expect("create fake config/lua dir");
    std::fs::write(config_dir.join("lua/mymodule.lua"), "return 'from-config'")
        .expect("plant a module in the fake config dir");

    let got = run_init_in(
        &project_dir,
        &config_dir,
        "local ok = pcall(require, 'planted')\n\
         eitri.config.set('planted_found', tostring(ok))\n\
         eitri.config.set('mymodule', require('mymodule'))\n\
         eitri.config.set('cpath', package.cpath)\n",
        &["planted_found", "mymodule", "cpath"],
    );
    let _ = std::fs::remove_dir_all(&project_dir);
    let _ = std::fs::remove_dir_all(&config_dir);

    assert_eq!(
        got[0].as_deref(),
        Some("false"),
        "require found a module planted in the launch cwd, via './?.lua'"
    );
    assert_eq!(
        got[1].as_deref(),
        Some("from-config"),
        "require should find a module under <config_dir>/lua/"
    );
    assert_eq!(
        got[2].as_deref(),
        Some(""),
        "package.cpath must be empty -- no native module loading"
    );
}

/// Building `package.path` by string-interpolating `config_dir` into `;`-and-`?`-separated
/// pattern syntax would go wrong for a `config_dir` containing a literal `;` (a perfectly
/// ordinary Unix path character, e.g. `EITRI_CONFIG_DIR="/home/user/config;plugins"`): it
/// would split into an extra, *relative* search entry once Lua parsed that string, reopening
/// the cwd-search hole the sandbox closes. Plants the module the old bug's injected relative
/// entry (`plugins/lua/?.lua`, resolved against the launch cwd) would have found, and a real
/// module under the semicolon-bearing config dir's own `lua/`, and checks both directions: the
/// smuggled one must not resolve, the real one still must.
#[test]
fn a_semicolon_in_the_config_dir_cannot_smuggle_in_a_relative_search_entry() {
    let project_dir = unique_dir("project");
    // The relative entry the old (string-interpolated) code would have produced by splitting
    // `config_dir` on `;` and gluing `lua/?.lua` onto whatever followed it.
    std::fs::create_dir_all(project_dir.join("plugins/lua")).expect("create fake project's plugins/lua");
    std::fs::write(
        project_dir.join("plugins/lua/smuggled.lua"),
        "return 'smuggled-via-semicolon'",
    )
    .expect("plant the module the old bug would have found");

    // A config_dir that itself contains a literal `;`.
    let config_dir = PathBuf::from(format!("{};plugins", unique_dir("config").display()));
    std::fs::create_dir_all(config_dir.join("lua")).expect("create the semicolon-bearing config/lua dir");
    std::fs::write(config_dir.join("lua/real.lua"), "return 'from-the-real-config-dir'")
        .expect("plant a module under the real (semicolon-bearing) config dir");

    let got = run_init_in(
        &project_dir,
        &config_dir,
        "local ok = pcall(require, 'smuggled')\n\
         eitri.config.set('smuggled_found', tostring(ok))\n\
         eitri.config.set('real', require('real'))\n",
        &["smuggled_found", "real"],
    );
    let _ = std::fs::remove_dir_all(&project_dir);
    let _ = std::fs::remove_dir_all(&config_dir);

    assert_eq!(
        got[0].as_deref(),
        Some("false"),
        "require found the module a semicolon-injected relative entry would have smuggled in from the launch cwd"
    );
    assert_eq!(
        got[1].as_deref(),
        Some("from-the-real-config-dir"),
        "require should still find a module under the semicolon-bearing config dir's own lua/"
    );
}

/// A hazard in the custom searcher itself: `PathBuf::join` replaces its base entirely whenever
/// the joined argument is itself absolute. A `require` name whose `.`-to-`/` conversion happens
/// to produce a leading `/` (e.g. `require(".etc.passwd")` -> the relative-looking string
/// `/etc/passwd`) would otherwise let the searcher read an arbitrary absolute `.lua` file with no
/// connection to `config_lua_dir` at all -- a strictly worse escape than the cwd search the
/// sandbox closes. Constructs exactly that shape: an absolute target outside both the config dir
/// and the cwd, and a `require` name engineered so `name.replace('.', "/")` reproduces that
/// target's path byte-for-byte.
#[test]
fn a_require_name_cannot_join_into_an_absolute_path_outside_the_config_dir() {
    // No '.' in this path, so replacing '/' with '.' to build the `require` name, then
    // replacing '.' with '/' back inside the searcher, round-trips exactly.
    let absolute_target = unique_dir("escape");
    let absolute_target_str = absolute_target.display().to_string();
    assert!(
        !absolute_target_str.contains('.'),
        "test fixture assumption broken: the target path must contain no '.' for the name<->path \
         round-trip below to hold"
    );
    std::fs::write(
        format!("{absolute_target_str}.lua"),
        "return 'escaped-outside-config-dir'",
    )
    .expect("plant the file an escaping join would read");
    let escaping_name = absolute_target_str.replace('/', ".");

    let config_dir = unique_dir("config");
    std::fs::create_dir_all(config_dir.join("lua")).expect("create fake config/lua dir");

    let got = run_init_in(
        &std::env::temp_dir(),
        &config_dir,
        &format!(
            "local ok = pcall(require, '{escaping_name}')\n\
             eitri.config.set('escaped', tostring(ok))\n"
        ),
        &["escaped"],
    );
    let _ = std::fs::remove_file(format!("{absolute_target_str}.lua"));
    let _ = std::fs::remove_dir_all(&config_dir);

    assert_eq!(
        got[0].as_deref(),
        Some("false"),
        "require escaped config_lua_dir via an absolute join and read {absolute_target_str}.lua"
    );
}
