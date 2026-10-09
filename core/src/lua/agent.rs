//! `eitri.agent.register({ name, command, project_files, auto_mode })`: an agent spoken to over ACP.
//!
//! The registration is configuration the user wrote, so every refusal is a startup failure naming the
//! key (`AgentRegistry::refused`, checked by `Kernel::run_and_check_init_file` after `init.lua` ran, so a
//! `pcall` around the call changes nothing). Nothing is started here: a tab of a registered agent starts
//! only after the user trusted the project for it.

use mlua::{Lua, Table, Value};
use std::cell::RefCell;
use std::rc::Rc;

/// The longest name accepted. The name ends up in a state file's name (`<16 hex>.json.acp-<name>`),
/// which a file system caps at 255 bytes.
pub const MAX_NAME_LEN: usize = 64;

/// One registered agent, as `init.lua` described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRegistration {
    /// `^[A-Za-z0-9][A-Za-z0-9._-]*$`, never `claude` or `codex` in any letter case.
    pub name: String,
    /// The program and its arguments; the first word is an absolute path or a bare command name looked
    /// up on `PATH`, never a path relative to the project the agent runs in.
    pub command: Vec<String>,
    /// The files and directories (ending in `/`) the agent reads from a project: its trust profile.
    /// Never empty, each one name.
    pub project_files: Vec<String>,
    /// The mode id set at the start of every session, if any.
    pub auto_mode: Option<String>,
}

/// `pub`: `Kernel` holds one behind an `Rc<RefCell<_>>` field and calls `install`.
#[derive(Default)]
pub struct AgentRegistry {
    agents: Vec<AgentRegistration>,
    /// The first refusal, kept however `init.lua` handled the Lua error.
    refused: Option<String>,
}

impl AgentRegistry {
    /// The registered agents, in the order `init.lua` registered them.
    pub fn agents(&self) -> &[AgentRegistration] {
        &self.agents
    }

    /// The agent registered under `name`, if any.
    pub fn get(&self, name: &str) -> Option<&AgentRegistration> {
        self.agents.iter().find(|a| a.name == name)
    }

    /// The message of the first registration refused, or `None`.
    pub fn refused(&self) -> Option<&str> {
        self.refused.as_deref()
    }
}

/// Whether `name` may name a registered agent: the pattern, the length, and not a built-in agent's name
/// in any letter case (`Claude` beside `claude` would read as the same agent in every label).
pub fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && name.len() <= MAX_NAME_LEN
        && !name.eq_ignore_ascii_case("claude")
        && !name.eq_ignore_ascii_case("codex")
}

const PREFIX: &str = "eitri.agent.register";

/// Every key a registration may carry; any other is refused, so a misspelt `auto_mode` is not dropped.
const KEYS: &[&str] = &["name", "command", "project_files", "auto_mode"];

/// Checks one registration table. `Err` names the key and is the startup-failure text.
pub fn parse_registration(spec: &Table) -> Result<AgentRegistration, String> {
    for pair in spec.clone().pairs::<Value, Value>() {
        let (key, _) = pair.map_err(|e| format!("{PREFIX}: the table cannot be read ({e})"))?;
        let known = match &key {
            Value::String(text) => text.to_str().ok().filter(|k| KEYS.contains(&&**k)).is_some(),
            _ => false,
        };
        if !known {
            return Err(format!(
                "{PREFIX}: unknown key {} (the keys are {})",
                describe(&key),
                KEYS.join(", ")
            ));
        }
    }

    let name = match spec.raw_get::<Value>("name") {
        Ok(Value::String(text)) => text.to_str().map(|t| t.to_string()).ok(),
        _ => None,
    }
    .ok_or_else(|| format!("{PREFIX}: name must be a string"))?;
    if !valid_name(&name) {
        return Err(format!(
            "{PREFIX}: name {name:?} must match ^[A-Za-z0-9][A-Za-z0-9._-]*$, be at most {MAX_NAME_LEN} \
             characters and not be claude or codex"
        ));
    }

    let command = string_list(spec, "command", &name)?;
    let Some(first) = command.first() else {
        return Err(format!("{PREFIX}({name}): command must name a program"));
    };
    if first.is_empty() || (!first.starts_with('/') && first.contains('/')) {
        return Err(format!(
            "{PREFIX}({name}): command's first word {first:?} must be an absolute path or a command name on PATH \
             (a relative path would name a file in the project the agent runs in)"
        ));
    }
    if let Some(bad) = command.iter().find(|word| word.contains('\0')) {
        return Err(format!("{PREFIX}({name}): command word {bad:?} holds a NUL"));
    }

    let project_files = string_list(spec, "project_files", &name)?;
    if project_files.is_empty() {
        return Err(format!(
            "{PREFIX}({name}): project_files must list the files the agent reads from a project"
        ));
    }
    for file in &project_files {
        let bare = file.strip_suffix('/').unwrap_or(file);
        if bare.is_empty() || bare == "." || bare == ".." || bare.contains('/') || bare.chars().any(char::is_control) {
            return Err(format!(
                "{PREFIX}({name}): project_files entry {file:?} must be one file name, or one directory name \
                 ending in /"
            ));
        }
    }

    let auto_mode = match spec.raw_get::<Value>("auto_mode") {
        Ok(Value::Nil) => None,
        Ok(Value::String(text)) => match text.to_str() {
            Ok(mode) if !mode.trim().is_empty() && !mode.chars().any(char::is_control) => Some(mode.to_string()),
            _ => return Err(format!("{PREFIX}({name}): auto_mode must be a mode id")),
        },
        _ => return Err(format!("{PREFIX}({name}): auto_mode must be a string")),
    };

    Ok(AgentRegistration {
        name,
        command,
        project_files,
        auto_mode,
    })
}

/// A key's value as a list of strings: a table whose keys are exactly 1..n, each value a UTF-8 string.
fn string_list(spec: &Table, key: &str, name: &str) -> Result<Vec<String>, String> {
    let refused = || format!("{PREFIX}({name}): {key} must be a list of strings");
    let Ok(Value::Table(list)) = spec.raw_get::<Value>(key) else {
        return Err(refused());
    };
    let len = list.raw_len();
    let mut count = 0usize;
    for pair in list.clone().pairs::<Value, Value>() {
        let (index, _) = pair.map_err(|_| refused())?;
        match index {
            Value::Integer(i) if i >= 1 && (i as usize) <= len => count += 1,
            _ => return Err(refused()),
        }
    }
    if count != len {
        return Err(refused());
    }
    (1..=len)
        .map(|i| match list.raw_get::<Value>(i) {
            Ok(Value::String(text)) => text.to_str().map(|t| t.to_string()).map_err(|_| refused()),
            _ => Err(refused()),
        })
        .collect()
}

fn describe(key: &Value) -> String {
    match key {
        Value::String(text) => format!("{:?}", text.to_string_lossy()),
        other => format!("of type {}", other.type_name()),
    }
}

/// `pub`: `Kernel::new` calls this.
pub fn install(lua: &Lua, eitri: &Table, registry: Rc<RefCell<AgentRegistry>>) -> mlua::Result<()> {
    let register = lua.create_function(move |_, spec: Value| {
        let parsed = match &spec {
            Value::Table(table) => parse_registration(table),
            other => Err(format!("{PREFIX}: takes a table, not a {}", other.type_name())),
        };
        let parsed = parsed.and_then(|agent| {
            if registry.borrow().get(&agent.name).is_some() {
                Err(format!("{PREFIX}: {} is registered twice", agent.name))
            } else {
                Ok(agent)
            }
        });
        match parsed {
            Ok(agent) => {
                registry.borrow_mut().agents.push(agent);
                Ok(())
            }
            Err(message) => {
                registry.borrow_mut().refused.get_or_insert_with(|| message.clone());
                Err(mlua::Error::RuntimeError(message))
            }
        }
    })?;
    let agent = lua.create_table()?;
    agent.set("register", register)?;
    eitri.set("agent", agent)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> (Lua, Rc<RefCell<AgentRegistry>>) {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(AgentRegistry::default()));
        install(&lua, &eitri, registry.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();
        (lua, registry)
    }

    /// The error a registration raises (the same text the registry keeps for the startup failure).
    fn refusal(source: &str) -> String {
        let (lua, registry) = engine();
        let err = lua.load(source).exec().expect_err(source).to_string();
        let kept = registry
            .borrow()
            .refused()
            .map(str::to_owned)
            .expect("the refusal is kept");
        assert!(err.contains(&kept), "{err} / {kept}");
        assert!(registry.borrow().agents().is_empty());
        kept
    }

    #[test]
    fn a_registration_is_kept_with_every_key() {
        let (lua, registry) = engine();
        lua.load(
            r#"eitri.agent.register({ name = "gemini", command = { "gemini", "--experimental-acp" },
                 project_files = { ".gemini/", "GEMINI.md" }, auto_mode = "auto_edit" })
               eitri.agent.register({ name = "my.agent_2-x", command = { "/opt/a" }, project_files = { "A.md" } })"#,
        )
        .exec()
        .unwrap();
        let registry = registry.borrow();
        assert_eq!(registry.refused(), None);
        assert_eq!(
            registry.get("gemini"),
            Some(&AgentRegistration {
                name: "gemini".into(),
                command: vec!["gemini".into(), "--experimental-acp".into()],
                project_files: vec![".gemini/".into(), "GEMINI.md".into()],
                auto_mode: Some("auto_edit".into()),
            })
        );
        assert_eq!(registry.get("my.agent_2-x").unwrap().auto_mode, None);
        assert_eq!(registry.agents().len(), 2);
    }

    #[test]
    fn register_refuses_bad_names_reserved_names_and_missing_project_files() {
        let with_name = |name: &str| {
            format!(r#"eitri.agent.register({{ name = {name}, command = {{ "a" }}, project_files = {{ "A.md" }} }})"#)
        };
        for bad in [
            r#""""#,
            r#""-x""#,
            r#"".x""#,
            r#""a b""#,
            r#""a/b""#,
            r#""a:b""#,
            r#""é""#,
            "42",
            "nil",
            &format!("{:?}", "a".repeat(MAX_NAME_LEN + 1)),
        ] {
            let err = refusal(&with_name(bad));
            assert!(err.contains("name"), "{bad}: {err}");
        }
        for reserved in ["claude", "codex", "Claude", "CODEX"] {
            let err = refusal(&with_name(&format!("{reserved:?}")));
            assert!(err.contains("not be claude or codex"), "{reserved}: {err}");
        }
        assert!(valid_name(&"a".repeat(MAX_NAME_LEN)));

        for (files, why) in [
            ("", "project_files must be a list"),
            ("project_files = {},", "project_files must list"),
            ("project_files = \"A.md\",", "project_files must be a list"),
            ("project_files = { \"a/b\" },", "project_files entry"),
            ("project_files = { \"..\" },", "project_files entry"),
            ("project_files = { \"/\" },", "project_files entry"),
            ("project_files = { \"/etc/x\" },", "project_files entry"),
            ("project_files = { \"a\\nb\" },", "project_files entry"),
            ("project_files = { \"A.md\", 3 },", "project_files must be a list"),
            ("project_files = { [2] = \"A.md\" },", "project_files must be a list"),
            (
                "project_files = { \"A.md\", x = \"B\" },",
                "project_files must be a list",
            ),
        ] {
            let err = refusal(&format!(
                r#"eitri.agent.register({{ name = "gem", command = {{ "a" }}, {files} }})"#
            ));
            assert!(err.contains(why), "{files}: {err}");
            assert!(err.contains("gem"), "{files}: {err}");
        }

        for (command, why) in [
            ("", "command must be a list"),
            ("command = \"gem\",", "command must be a list"),
            ("command = {},", "command must name a program"),
            ("command = { \"bin/gem\" },", "command's first word"),
            ("command = { \"./gem\" },", "command's first word"),
            ("command = { \"\" },", "command's first word"),
        ] {
            let err = refusal(&format!(
                r#"eitri.agent.register({{ name = "gem", {command} project_files = {{ "A.md" }} }})"#
            ));
            assert!(err.contains(why), "{command}: {err}");
        }

        for (extra, why) in [
            ("auto_mode = 3,", "auto_mode must be a string"),
            ("auto_mode = \"\",", "auto_mode must be a mode id"),
            ("automode = \"x\",", "unknown key \"automode\""),
        ] {
            let err = refusal(&format!(
                r#"eitri.agent.register({{ name = "gem", command = {{ "a" }}, project_files = {{ "A.md" }}, {extra} }})"#
            ));
            assert!(err.contains(why), "{extra}: {err}");
        }
        assert!(refusal(r#"eitri.agent.register("gem")"#).contains("takes a table"));
    }

    /// A second registration of a name is refused, and the first is kept as it was.
    #[test]
    fn a_name_registered_twice_is_refused() {
        let (lua, registry) = engine();
        let one = r#"eitri.agent.register({ name = "gem", command = { "a" }, project_files = { "A.md" } })"#;
        lua.load(one).exec().unwrap();
        assert!(lua.load(one).exec().is_err());
        let registry = registry.borrow();
        assert_eq!(registry.agents().len(), 1);
        assert!(registry.refused().unwrap().contains("registered twice"));
    }

    /// A `pcall` around a refused registration does not hide it from the startup check.
    #[test]
    fn a_refusal_under_pcall_still_fails_the_start() {
        let dir = std::env::temp_dir().join(format!("nv-agent-register-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let init = dir.join("init.lua");
        std::fs::write(
            &init,
            "pcall(eitri.agent.register, { name = \"claude\", command = { \"a\" }, project_files = { \"A\" } })\n\
             eitri.config.set(\"after\", \"reached\")\n",
        )
        .unwrap();
        let kernel = super::super::kernel::Kernel::new(dir.clone(), super::super::kernel::refuse_panels).unwrap();
        let outcome = kernel.run_and_check_init_file(&init);
        let after = kernel.config.borrow().get("after").map(str::to_owned);
        let _ = std::fs::remove_dir_all(&dir);
        let err = outcome.unwrap_err();
        assert!(err.starts_with("eitri.agent.register: name \"claude\""), "{err}");
        assert_eq!(after.as_deref(), Some("reached"));
    }
}
