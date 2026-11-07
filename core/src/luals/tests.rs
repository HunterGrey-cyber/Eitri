use super::*;
use crate::onboarding::{write::write_init_with_data, Lang, WriteOutcome};
use crate::plugin::test_support::Scratch;
use std::collections::BTreeSet;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

fn env_at(home: &Path) -> impl Fn(&str) -> Option<OsString> + '_ {
    move |key| (key == "HOME").then(|| home.as_os_str().to_owned())
}

#[test]
fn meta_install_writes_only_changed_bytes_and_replaces_atomically() {
    let scratch = Scratch::new("luals-install");
    let data = scratch.0.join("data/eitri");
    let path = data.join("lua/meta/eitri.lua");
    assert_eq!(install_meta(&data).unwrap(), InstallOutcome::Written);
    assert_eq!(std::fs::read(&path).unwrap(), META.as_bytes());
    let before = std::fs::metadata(&path).unwrap();
    assert_eq!(before.permissions().mode() & 0o777, 0o644);
    for dir in [data.clone(), data.join("lua"), data.join("lua/meta")] {
        assert_eq!(std::fs::metadata(dir).unwrap().permissions().mode() & 0o777, 0o755);
    }
    assert_eq!(install_meta(&data).unwrap(), InstallOutcome::Unchanged);
    let unchanged = std::fs::metadata(&path).unwrap();
    assert_eq!(
        (before.ino(), before.modified().unwrap()),
        (unchanged.ino(), unchanged.modified().unwrap())
    );
    let mut old = std::fs::File::open(&path).unwrap();
    assert_eq!(install_bytes(&data, b"replacement").unwrap(), InstallOutcome::Written);
    assert_ne!(before.ino(), std::fs::metadata(&path).unwrap().ino());
    let mut old_bytes = Vec::new();
    std::io::Read::read_to_end(&mut old, &mut old_bytes).unwrap();
    assert_eq!(
        old_bytes,
        META.as_bytes(),
        "open readers keep the complete previous file"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
    assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    install_meta(&data).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), META.as_bytes());
}

#[test]
fn meta_install_failure_leaves_no_temporary_file() {
    let scratch = Scratch::new("luals-install-failure");
    let data = scratch.0.join("eitri");
    std::fs::create_dir_all(data.join("lua/meta/eitri.lua")).unwrap();
    assert!(install_meta(&data).is_err());
    assert_eq!(std::fs::read_dir(data.join("lua/meta")).unwrap().count(), 1);
}

#[test]
fn meta_install_writes_nothing_through_a_linked_directory() {
    let scratch = Scratch::new("luals-linked-dir");
    let data = scratch.0.join("eitri");
    let elsewhere = scratch.0.join("nvim-lua");
    std::fs::create_dir_all(data.join("lua")).unwrap();
    std::fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, data.join("lua/meta")).unwrap();
    let error = install_meta(&data).unwrap_err();
    assert!(error.to_string().contains("not a directory of its own"), "{error}");
    assert_eq!(
        std::fs::read_dir(&elsewhere).unwrap().count(),
        0,
        "nothing landed where the link points"
    );
}

#[test]
fn meta_install_makes_nothing_inside_a_linked_eitri_or_lua_directory() {
    for linked in ["", "lua"] {
        let scratch = Scratch::new("luals-linked-parent");
        let data = scratch.0.join("eitri");
        let elsewhere = scratch.0.join("nvim-lua");
        std::fs::create_dir(&elsewhere).unwrap();
        if linked.is_empty() {
            std::os::unix::fs::symlink(&elsewhere, &data).unwrap();
        } else {
            std::fs::create_dir(&data).unwrap();
            std::os::unix::fs::symlink(&elsewhere, data.join(linked)).unwrap();
        }
        let error = install_meta(&data).unwrap_err();
        assert!(
            error.to_string().contains("not a directory of its own"),
            "{linked:?}: {error}"
        );
        assert_eq!(
            std::fs::read_dir(&elsewhere).unwrap().count(),
            0,
            "{linked:?}: nothing, not even an empty meta directory, was made where the link points"
        );
    }
}

#[test]
fn meta_install_writes_nothing_in_a_directory_others_may_change() {
    let scratch = Scratch::new("luals-shared-dir");
    let data = scratch.0.join("eitri");
    std::fs::create_dir_all(data.join("lua/meta")).unwrap();
    std::fs::set_permissions(data.join("lua"), std::fs::Permissions::from_mode(0o777)).unwrap();
    let error = install_meta(&data).unwrap_err();
    assert!(error.to_string().contains("writable by others"), "{error}");
    assert_eq!(std::fs::read_dir(data.join("lua/meta")).unwrap().count(), 0);
}

#[test]
fn luarc_creates_json_and_installs_meta_only_inside_the_two_directories() {
    let scratch = Scratch::new("luals-home");
    let home = &scratch.0;
    let config = home.join(".config/eitri");
    std::fs::create_dir(home.join(".config")).unwrap();
    let data = home.join(".local/share/eitri");
    assert_eq!(
        write_luarc(&config, &data).unwrap(),
        LuarcOutcome::Created(config.join(".luarc.json"))
    );
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(config.join(".luarc.json")).unwrap()).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"workspace.library": [data.join("lua/meta")], "runtime.version": "Lua 5.4"})
    );
    assert_eq!(std::fs::read(data.join("lua/meta/eitri.lua")).unwrap(), META.as_bytes());
    fn files(dir: &Path, out: &mut BTreeSet<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files(&path, out);
            } else {
                out.insert(path);
            }
        }
    }
    let mut found = BTreeSet::new();
    files(home, &mut found);
    assert_eq!(
        found,
        BTreeSet::from([config.join(".luarc.json"), data.join("lua/meta/eitri.lua")])
    );
    assert!(!home.join(".config/nvim").exists());
}

#[test]
fn luarc_preserves_existing_json_jsonc_and_dangling_links() {
    for name in [".luarc.json", ".luarc.jsonc"] {
        for link in [false, true] {
            let scratch = Scratch::new("luals-existing");
            let config = scratch.0.join("config");
            std::fs::create_dir(&config).unwrap();
            let data = scratch.0.join("data/eitri");
            let path = config.join(name);
            if link {
                std::os::unix::fs::symlink("missing", &path).unwrap();
            } else {
                std::fs::write(&path, "user bytes").unwrap();
            }
            let before = std::fs::symlink_metadata(&path).unwrap();
            let outcome = write_luarc(&config, &data).unwrap();
            let LuarcOutcome::Existing { path: existing, line } = outcome else {
                panic!("existing workspace replaced");
            };
            assert_eq!(existing, path);
            let json: serde_json::Value = serde_json::from_str(&format!("{{{line}}}")).unwrap();
            assert_eq!(json, serde_json::json!({"workspace.library": [data.join("lua/meta")]}));
            assert_eq!(before.ino(), std::fs::symlink_metadata(&path).unwrap().ino());
            assert_eq!(std::fs::read_dir(&config).unwrap().count(), 1);
            if !link {
                assert_eq!(std::fs::read_to_string(path).unwrap(), "user bytes");
            }
        }
    }
}

#[test]
fn onboarding_adds_luarc_only_after_written_and_keeps_the_init_outcome_on_failure() {
    let scratch = Scratch::new("luals-onboarding");
    let config = scratch.0.join("config");
    let data = scratch.0.join("data/eitri");
    assert_eq!(
        write_init_with_data(&config, &data, "first", Lang::En),
        WriteOutcome::Written(config.join("init.lua"))
    );
    assert!(config.join(".luarc.json").exists());
    std::fs::remove_file(config.join(".luarc.json")).unwrap();
    assert!(matches!(
        write_init_with_data(&config, &data, "second", Lang::En),
        WriteOutcome::Failed(_)
    ));
    assert!(!config.join(".luarc.json").exists());
    let refused = scratch.0.join("refused");
    std::fs::create_dir(&refused).unwrap();
    std::fs::set_permissions(&refused, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(
        write_init_with_data(&refused, &data, "text", Lang::En),
        WriteOutcome::Failed(_)
    ));
    assert!(!refused.join(".luarc.json").exists());
    let broken_data = scratch.0.join("file");
    std::fs::write(&broken_data, "file").unwrap();
    let config2 = scratch.0.join("config2");
    assert_eq!(
        write_init_with_data(&config2, &broken_data, "text", Lang::En),
        WriteOutcome::Written(config2.join("init.lua"))
    );
    assert!(!config2.join(".luarc.json").exists());
}

#[test]
fn onboarding_never_overwrites_a_workspace() {
    let scratch = Scratch::new("luals-onboarding-existing");
    let config = scratch.0.join("config");
    std::fs::create_dir(&config).unwrap();
    std::fs::write(config.join(".luarc.json"), "user bytes").unwrap();
    assert!(matches!(
        write_init_with_data(&config, &scratch.0.join("data"), "init", Lang::En),
        WriteOutcome::Written(_)
    ));
    assert_eq!(
        std::fs::read_to_string(config.join(".luarc.json")).unwrap(),
        "user bytes"
    );
}

#[test]
fn onboarding_failed_and_not_written_outcomes_install_nothing() {
    let scratch = Scratch::new("luals-no-init");
    for outcome in [
        WriteOutcome::Failed("failed".into()),
        WriteOutcome::NotWritten("not written".into()),
    ] {
        crate::onboarding::write::install_after_written(&outcome, &scratch.0.join("config"), &scratch.0.join("data"));
        assert_eq!(std::fs::read_dir(&scratch.0).unwrap().count(), 0);
    }
}

#[test]
fn data_dir_obeys_absolute_xdg_and_home_fallback() {
    let resolve = |xdg: &str| {
        data_dir(&|key| match key {
            "XDG_DATA_HOME" => Some(xdg.into()),
            "HOME" => Some("/home/user".into()),
            _ => None,
        })
        .unwrap()
    };
    assert_eq!(resolve("/data"), PathBuf::from("/data/eitri"));
    for xdg in ["", "relative"] {
        assert_eq!(resolve(xdg), PathBuf::from("/home/user/.local/share/eitri"));
    }
    assert!(data_dir(&|_| None).is_err());
    assert!(data_dir(&|_| Some("relative".into())).is_err());
}

#[test]
fn setup_aliases_create_and_existing_prints_the_line_with_exit_zero() {
    for target in ["lua-ls", "lua_ls", "luals"] {
        let scratch = Scratch::new("luals-setup");
        std::fs::create_dir(scratch.0.join(".config")).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = [OsString::from(target)];
        assert_eq!(setup(&args, &env_at(&scratch.0), &mut out, &mut err), 0);
        assert!(err.is_empty());
        assert!(
            String::from_utf8_lossy(&out).contains(&scratch.0.join(".config/eitri/.luarc.json").display().to_string())
        );
        out.clear();
        assert_eq!(setup(&args, &env_at(&scratch.0), &mut out, &mut err), 0);
        let said = String::from_utf8(out).unwrap();
        assert!(
            said.contains("already exists") && said.contains("\"workspace.library\": ["),
            "{said}"
        );
    }
}

#[test]
fn setup_usage_is_64_errors_are_1_and_override_uses_the_window_resolver() {
    let scratch = Scratch::new("luals-args");
    for args in [vec![], vec!["other"], vec!["lua-ls", "extra"], vec!["--help"]] {
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            setup(
                &args.iter().map(OsString::from).collect::<Vec<_>>(),
                &env_at(&scratch.0),
                &mut out,
                &mut err
            ),
            64
        );
        assert!(String::from_utf8_lossy(&err).contains("usage: eitri setup lua-ls"));
    }
    let config = scratch.0.join("override");
    let env = |key: &str| match key {
        "EITRI_CONFIG_DIR" => Some(config.as_os_str().to_owned()),
        "HOME" => Some(scratch.0.as_os_str().to_owned()),
        _ => None,
    };
    assert_eq!(setup(&["lua-ls".into()], &env, &mut Vec::new(), &mut Vec::new()), 0);
    assert!(config.join(".luarc.json").exists());
    let mut err = Vec::new();
    assert_eq!(setup(&["lua-ls".into()], &|_| None, &mut Vec::new(), &mut err), 1);
    assert!(!err.is_empty());
}

#[test]
fn meta_function_names_match_the_kernel_both_ways() {
    let scratch = Scratch::new("luals-kernel");
    // The host supplies panel construction; the kernel installs its full plugin API itself.
    let kernel = crate::lua::kernel::Kernel::new(scratch.0.clone(), |lua, eitri| {
        let panel = lua.create_table()?;
        panel.set("register", lua.create_function(|_, _: mlua::Value| Ok(()))?)?;
        eitri.set("panel", panel)
    })
    .unwrap();
    kernel
        .run_checked_text(
            "collect API",
            r#"
        local paths = {}
        local function walk(t, prefix)
            for k, v in pairs(t) do
                local path = prefix .. '.' .. k
                if type(v) == 'function' then table.insert(paths, path)
                elseif type(v) == 'table' then walk(v, path) end
            end
        end
        walk(eitri, 'eitri')
        table.sort(paths)
        eitri.config.set('collected_api', table.concat(paths, '\n'))
    "#,
        )
        .unwrap();
    let actual: BTreeSet<String> = kernel
        .config
        .borrow()
        .get("collected_api")
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let declared: BTreeSet<String> = META
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .filter(|words| words[0] == "function" && words[1].starts_with("eitri."))
        .map(|words| words[1].split('(').next().unwrap().to_owned())
        .collect();
    assert_eq!(declared, actual);
}

/// The value of a named constant, with no dependence on the declaration's line wrapping.
fn constant_value<'a>(source: &'a crate::source_scan::Source, name: &str) -> &'a str {
    let code = crate::source_scan::code_only(&source.text);
    let at = crate::source_scan::word_ends(&code, "const")
        .into_iter()
        .find(|end| {
            code[*end..]
                .trim_start()
                .strip_prefix(name)
                .is_some_and(|rest| rest.trim_start().starts_with(':'))
        })
        .unwrap_or_else(|| panic!("{} has no constant {name}", source.path));
    let equals = at + code[at..].find('=').unwrap();
    let end = equals + code[equals..].find(';').unwrap();
    source.text[equals + 1..end].trim()
}

fn setting_value(
    reference: &str,
    table: &crate::source_scan::Source,
    sources: &[crate::source_scan::Source],
) -> String {
    let reference = reference
        .replace("eitri_core::", "")
        .replace("prefs::", "agent_prefs::")
        .replace("cfg::", "lua::config::");
    let (source, name) = match reference.rsplit_once("::") {
        None => (table, reference.as_str()),
        Some((module, name)) => {
            let module = module
                .strip_suffix("::ChatOnPermission")
                .unwrap_or(module)
                .replace("::", "/");
            let path = format!("core/src/{module}");
            let source = sources
                .iter()
                .find(|s| s.path == format!("{path}.rs") || s.path == format!("{path}/mod.rs"))
                .unwrap_or_else(|| panic!("no source for {reference}"));
            (source, name)
        }
    };
    serde_json::from_str(constant_value(source, name)).unwrap_or_else(|e| panic!("{reference}: {e}"))
}

/// Top-level comma-separated expressions, keeping nested function calls and lists together.
fn expressions(code: &str) -> Vec<&str> {
    let mut depth = 0;
    let mut start = 0;
    let mut parts = Vec::new();
    for (at, c) in code.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(code[start..at].trim());
                start = at + 1;
            }
            _ => {}
        }
    }
    if !code[start..].trim().is_empty() {
        parts.push(code[start..].trim());
    }
    parts
}

fn checked_setting_names(
    table: &crate::source_scan::Source,
    sources: &[crate::source_scan::Source],
) -> BTreeSet<String> {
    let (_, checks) = crate::source_scan::function_in(std::slice::from_ref(table), "checks");
    let compact: String = checks.chars().filter(|c| !c.is_whitespace()).collect();
    let list = compact.split_once("vec![").expect("the startup checks vector").1;
    let mut depth = 1;
    let end = list
        .char_indices()
        .find_map(|(at, c)| {
            match c {
                '[' => depth += 1,
                ']' => depth -= 1,
                _ => {}
            }
            (depth == 0).then_some(at)
        })
        .expect("the checks vector's end");
    let mut expected = BTreeSet::new();
    for entry in expressions(&list[..end]) {
        let fields = expressions(entry.strip_prefix('(').unwrap().strip_suffix(')').unwrap());
        assert_eq!(fields.len(), 2, "a check has a setting and parser: {entry}");
        expected.insert(setting_value(fields[0], table, sources));
    }
    assert!(
        compact.contains("forkeyinKIT_KEYS"),
        "the keyboard settings must be checked"
    );
    let kit = constant_value(table, "KIT_KEYS");
    for reference in expressions(kit.strip_prefix('[').unwrap().strip_suffix(']').unwrap()) {
        let reference: String = reference.chars().filter(|c| !c.is_whitespace()).collect();
        expected.insert(setting_value(&reference, table, sources));
    }
    let (_, known) = crate::source_scan::function_in(std::slice::from_ref(table), "known_keys");
    let known: String = known.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(known.contains("checks(agents)"));
    for pushed in known.split("keys.push(").skip(1) {
        expected.insert(setting_value(pushed.split(')').next().unwrap(), table, sources));
    }
    for tool in crate::editor_tools::tools::TOOLS {
        expected.insert(format!(
            "{}{}",
            crate::editor_tools::switches::TOOL_KEY_PREFIX,
            tool.row().name
        ));
    }
    expected
}

#[test]
fn meta_setting_alias_matches_the_startup_parser_table_both_ways() {
    // The parser lives in the panel crate; reading its table keeps core free of a dependency back to it.
    let table = crate::source_scan::read_required("panel/src/startup_config.rs");
    let sources = crate::source_scan::rust_sources(&["core/src", "panel/src"]);
    let expected = checked_setting_names(&table, &sources);
    assert!(expected.len() > 40, "the scan must read the parser table");
    let declared = meta_settings(META);
    assert_eq!(declared, expected);
    // Wrapping a declaration or a table entry on another line must not change the scan's result.
    let reflow = |source: &crate::source_scan::Source| crate::source_scan::Source {
        path: source.path.clone(),
        text: source.text.replace("const ", "const\n").replace("::", "::\n"),
    };
    assert_eq!(
        checked_setting_names(&reflow(&table), &sources.iter().map(reflow).collect::<Vec<_>>()),
        expected
    );
}

fn meta_settings(meta: &str) -> BTreeSet<String> {
    let mut words = meta.split_whitespace();
    while let Some(word) = words.next() {
        if word == "---@alias" && words.next() == Some("EitriSetting") {
            break;
        }
    }
    let mut settings = BTreeSet::new();
    while words.next() == Some("---|") {
        settings.insert(serde_json::from_str(words.next().expect("alias member")).unwrap());
    }
    settings
}

#[test]
fn meta_is_valid_lua_and_each_function_has_a_description_and_types() {
    let source = crate::source_scan::read_required("core/src/luals/eitri.meta");
    assert_eq!(source.text, META);
    assert!(META.starts_with("---@meta\n"));
    mlua::Lua::new().load(META).exec().unwrap();
    for block in META.split("\n\n").filter(|block| block.contains("\nfunction eitri.")) {
        assert!(
            block.starts_with("---") && !block.starts_with("---@"),
            "missing description: {block}"
        );
        assert!(block.contains("---@return "), "missing return type: {block}");
        let function = block.split("\nfunction ").nth(1).unwrap();
        let args = function.split_once('(').unwrap().1.split(')').next().unwrap();
        for arg in args.split(',').map(str::trim).filter(|arg| !arg.is_empty()) {
            assert!(
                block.contains(&format!("---@param {arg} ")) || block.contains(&format!("---@param {arg}? ")),
                "missing parameter type for {arg}: {block}"
            );
        }
    }
}
