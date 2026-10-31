//! The `init.lua` keys that turn the tools off. A session reads them when it starts and keeps its tool list.

use std::collections::BTreeMap;

use super::catalog::ToolRow;
use super::tools;
use crate::config_switch::parse_switch;

pub const TOOLS_KEY: &str = "agent.editor.tools";
pub const TOOL_KEY_PREFIX: &str = "agent.editor.tool.";

#[derive(Debug, Clone)]
pub struct Switches {
    all: bool,
    /// What a key said, by tool name; a tool with no key takes its row's default.
    tools: BTreeMap<String, bool>,
}

impl Switches {
    pub fn on_by_default() -> Switches {
        Switches {
            all: true,
            tools: BTreeMap::new(),
        }
    }

    /// `all`: `agent.editor.tools` as stored; `tools`: every key under the prefix. An unknown name or a bad value is
    /// `Err` naming the key.
    pub fn parse(all: Option<&str>, tools: &[(String, String)]) -> Result<Switches, String> {
        let mut switches = Switches {
            all: parse_switch(TOOLS_KEY, all, true)?,
            tools: BTreeMap::new(),
        };
        for (key, value) in tools {
            let name = key.strip_prefix(TOOL_KEY_PREFIX).unwrap_or(key);
            let Some(tool) = tools::tool_named(name) else {
                return Err(format!(
                    "eitri.config.set(\"{key}\", ...): no editor tool has that name"
                ));
            };
            let on = parse_switch(key, Some(value), tool.row().default_on)?;
            switches.tools.insert(name.to_owned(), on);
        }
        Ok(switches)
    }

    pub fn tool_on(&self, row: &ToolRow) -> bool {
        self.all && self.tools.get(row.name).copied().unwrap_or(row.default_on)
    }
}

#[cfg(test)]
mod tests {
    use super::super::catalog::{client_tool_set, declared};
    use super::*;
    use agent::setting_sources::ProjectTrust;

    const OFF_BY_DEFAULT: ToolRow = ToolRow {
        name: "off_by_default",
        description: "",
        schema: "{}",
        always_load: false,
        default_on: false,
        in_untrusted: false,
    };

    fn key(name: &str, value: &str) -> (String, String) {
        (format!("{TOOL_KEY_PREFIX}{name}"), value.to_owned())
    }

    #[test]
    fn everything_is_on_by_default_but_a_tool_off_by_default() {
        let parsed = Switches::parse(None, &[]).unwrap();
        for switches in [Switches::on_by_default(), parsed] {
            for tool in tools::TOOLS {
                assert_eq!(
                    switches.tool_on(tool.row()),
                    tool.row().default_on,
                    "{}",
                    tool.row().name
                );
            }
            assert!(!switches.tool_on(&OFF_BY_DEFAULT));
        }
        let mut turned_on = Switches::on_by_default();
        turned_on.tools.insert(OFF_BY_DEFAULT.name.to_owned(), true);
        assert!(turned_on.tool_on(&OFF_BY_DEFAULT));
    }

    #[test]
    fn the_key_off_declares_no_server() {
        for off in ["false", "off"] {
            let switches = Switches::parse(Some(off), &[key("open_file", "on")]).unwrap();
            assert!(declared(&switches, ProjectTrust::Trusted).is_empty());
            assert!(client_tool_set(&switches, ProjectTrust::Trusted).is_none());
        }
        for on in ["true", "on"] {
            let switches = Switches::parse(Some(on), &[]).unwrap();
            assert!(client_tool_set(&switches, ProjectTrust::Trusted).is_some());
        }
    }

    #[test]
    fn one_tool_off_is_not_declared() {
        let switches = Switches::parse(None, &[key("read_buffer", "off")]).unwrap();
        let names: Vec<&str> = declared(&switches, ProjectTrust::Trusted)
            .iter()
            .map(|row| row.name)
            .collect();
        assert!(!names.contains(&"read_buffer"), "{names:?}");
        assert_eq!(names[..3], ["open_file", "editor_state", "diagnostics"]);
    }

    #[test]
    fn an_unknown_tool_name_is_an_error_naming_the_key() {
        let error = Switches::parse(None, &[key("nvim_nope", "off")]).err().unwrap();
        assert!(error.contains("agent.editor.tool.nvim_nope"), "{error}");
    }

    #[test]
    fn a_bad_value_is_an_error_naming_the_key() {
        let error = Switches::parse(Some("maybe"), &[]).err().unwrap();
        assert!(error.contains("agent.editor.tools"), "{error}");
        let error = Switches::parse(None, &[key("open_file", "maybe")]).err().unwrap();
        assert!(error.contains("agent.editor.tool.open_file"), "{error}");
    }
}
