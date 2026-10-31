//! What each tool declares, and which tools a session is given. Everything in a [`ToolRow`] is fixed when the
//! session starts: a tool that is not declared is one the model never sees.

use agent::setting_sources::ProjectTrust;
use agent::{ClientToolDecl, ClientToolSet};

use super::switches::Switches;
use super::tools;
use super::{CALL_TIMEOUT_MS, SERVER_NAME};

#[derive(Debug)]
pub struct ToolRow {
    /// What the model sees after the server name.
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema text, an object, at most 16 KiB.
    pub schema: &'static str,
    /// In the model's tool list from the first request, for a tool a model needs without knowing to look.
    pub always_load: bool,
    /// False for a tool that stays off until its key turns it on.
    pub default_on: bool,
    /// Declared in a session held to a no at the trust question ([`ProjectTrust::refused`]).
    pub in_untrusted: bool,
}

/// The rows a session started with `trust` is given, in the catalogue's order.
pub fn declared(switches: &Switches, trust: ProjectTrust) -> Vec<&'static ToolRow> {
    given(tools::TOOLS.iter().map(|tool| tool.row()), switches, trust)
}

/// [`declared`] over rows a test made: the catalogue of a build may not hold the kind of row it needs.
#[cfg(any(test, feature = "test-support"))]
pub fn declared_among(rows: &[&'static ToolRow], switches: &Switches, trust: ProjectTrust) -> Vec<&'static ToolRow> {
    given(rows.iter().copied(), switches, trust)
}

fn given(
    rows: impl Iterator<Item = &'static ToolRow>,
    switches: &Switches,
    trust: ProjectTrust,
) -> Vec<&'static ToolRow> {
    rows.filter(|row| switches.tool_on(row) && (!trust.refused() || row.in_untrusted))
        .collect()
}

/// `None` when no tool is declared: the field is then left out.
pub fn client_tool_set(switches: &Switches, trust: ProjectTrust) -> Option<ClientToolSet> {
    let rows = declared(switches, trust);
    if rows.is_empty() {
        return None;
    }
    Some(ClientToolSet {
        server_name: SERVER_NAME.to_owned(),
        timeout_ms: CALL_TIMEOUT_MS,
        tools: rows
            .into_iter()
            .map(|row| ClientToolDecl {
                name: row.name.to_owned(),
                description: row.description.to_owned(),
                input_schema_json: row.schema.to_owned(),
                always_load: row.always_load,
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::CARD_LIMIT;
    use super::*;

    const SCHEMA_MAX: usize = 16 * 1024;
    const NAME_MAX: usize = 64;

    #[test]
    fn every_schema_is_an_object_under_16_kib() {
        for tool in tools::TOOLS {
            let row = tool.row();
            assert!(row.schema.len() <= SCHEMA_MAX, "{}", row.name);
            let schema: serde_json::Value = serde_json::from_str(row.schema).expect(row.name);
            assert_eq!(schema["type"], "object", "{}", row.name);
            for keyword in ["oneOf", "anyOf", "allOf"] {
                assert!(
                    schema.get(keyword).is_none(),
                    "{} has a root {keyword}, which the model API refuses",
                    row.name
                );
            }
        }
    }

    #[test]
    fn a_name_fits_the_transport() {
        for tool in tools::TOOLS {
            let name = tool.row().name;
            assert!(name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'));
            assert!(format!("mcp__{SERVER_NAME}__{name}").len() <= NAME_MAX, "{name}");
        }
    }

    #[test]
    fn phase_a_tools_load_up_front() {
        let on = Switches::on_by_default();
        let up_front: Vec<&str> = declared(&on, ProjectTrust::Trusted)
            .iter()
            .filter(|row| row.always_load)
            .map(|row| row.name)
            .collect();
        assert_eq!(up_front, ["open_file", "editor_state", "read_buffer", "diagnostics"]);
    }

    #[test]
    fn an_empty_declaration_is_none() {
        let off = Switches::parse(Some("off"), &[]).unwrap();
        assert!(client_tool_set(&off, ProjectTrust::Trusted).is_none());
        let each_off: Vec<(String, String)> = tools::TOOLS
            .iter()
            .map(|tool| (format!("agent.editor.tool.{}", tool.row().name), "off".to_owned()))
            .collect();
        let off = Switches::parse(None, &each_off).unwrap();
        assert!(client_tool_set(&off, ProjectTrust::Trusted).is_none());
    }

    #[test]
    fn a_set_carries_the_rows_and_outlasts_a_card() {
        let set = client_tool_set(&Switches::on_by_default(), ProjectTrust::Trusted).unwrap();
        assert_eq!(set.server_name, "eitri");
        let on_by_default = tools::TOOLS.iter().filter(|tool| tool.row().default_on);
        assert_eq!(set.tools.len(), on_by_default.count());
        assert_eq!(set.tools[0].name, tools::TOOLS[0].row().name);
        assert_eq!(set.tools[0].input_schema_json, tools::TOOLS[0].row().schema);
        assert!(u128::from(set.timeout_ms) > CARD_LIMIT.as_millis());
    }

    #[test]
    fn an_untrusted_start_leaves_out_what_a_row_says() {
        static ROWS: [ToolRow; 2] = [
            ToolRow {
                name: "everywhere",
                description: "",
                schema: "{}",
                always_load: false,
                default_on: true,
                in_untrusted: true,
            },
            ToolRow {
                name: "trusted_only",
                description: "",
                schema: "{}",
                always_load: false,
                default_on: true,
                in_untrusted: false,
            },
        ];
        let on = Switches::on_by_default();
        let names = |trust| {
            given(ROWS.iter(), &on, trust)
                .iter()
                .map(|row| row.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(ProjectTrust::Trusted), ["everywhere", "trusted_only"]);
        assert_eq!(names(ProjectTrust::Untrusted), ["everywhere"]);
        assert_eq!(
            names(ProjectTrust::NothingFound),
            ["everywhere", "trusted_only"],
            "a project nobody was asked about is no refusal"
        );
    }
}
