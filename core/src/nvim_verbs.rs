//! `:Eitri`'s words, as the embedded nvim sends them on the pane-switch socket (`W <word>…`,
//! [`crate::pane_switch::PaneMessage::Verb`]), turned into the requests `eitri.layout` queues.
//!
//! Every word is parsed by the parser `eitri.layout` uses for the same thing --
//! [`crate::keymap::action::parse`] for an action and its options, [`ModuleId::parse`] for a module,
//! [`Workspaces::valid_name`] for a workspace -- and an action `eitri.layout.run` will not run is
//! refused by the same [`run_refusal`], so `:Eitri` and Lua accept the same things and refuse the
//! rest with the same words.
//!
//! Any descendant of the embedded nvim can write the socket (it holds the path in its environment),
//! so what a line can ask for is bounded here: a [`NvimVerb`] is a layout request, the key sheet or the
//! first-start screen or a named plugin load, and what it carries is a parsed action, a module id, a workspace name
//! or a plugin id accepted by the manifest parser -- never text that
//! is typed somewhere, sent to the agent or run as a command. No toolkit and no I/O here; [`install`] only asks the
//! editor to define `:Eitri`.

use crate::keymap::action::{self, Action, OptValue};
use crate::layout::{Axis, ModuleId, ModuleKind, Workspaces};
use crate::lua::layout::{run_refusal, LayoutRequest};
use crate::pane_switch::{is_verb_word, VERB_MAX_WORDS};

pub mod install;
pub mod leader;

/// What an `:Eitri` line asks the window for.
#[derive(Debug, Clone, PartialEq)]
pub enum NvimVerb {
    /// A request `eitri.layout` could have queued.
    Request(LayoutRequest),
    /// The key sheet.
    Keys,
    /// The first-start screen, opened again (`window.setup`).
    Setup,
}

/// The first words `:Eitri` reads as its own verbs; any other first word is an action name.
pub const VERBS: [&str; 9] = [
    "split",
    "focus",
    "show",
    "hide",
    "open",
    "workspace",
    "keys",
    "setup",
    "plugin",
];

/// The request `words` make, or why not as one line starting `:Eitri `. `lua_panels` are the ids of
/// the Lua panels `init.lua` registered, which `module.<id>` may name (as for `eitri.layout.run`).
///
/// - `split right|below <module>`: [`LayoutRequest::Split`], as `eitri.layout.split(module, …)`.
/// - `focus|show|hide <module>`: [`LayoutRequest::Focus`], `Show`, `Hide`.
/// - `open terminal|agent`: [`LayoutRequest::Open`], placed by the open rules.
/// - `workspace <name>`: [`LayoutRequest::Switch`].
/// - `keys`: [`NvimVerb::Keys`].
/// - `setup`: [`NvimVerb::Setup`], the first-start screen again.
/// - `plugin load <name>`: [`LayoutRequest::Run`], loading one plugin page for this run.
/// - `<action> [key=value …]`: [`LayoutRequest::Run`]; a value that is a whole number is a number,
///   `true` and `false` are booleans, anything else a string, as Lua would have passed them.
pub fn parse_words(words: &[String], lua_panels: &[String]) -> Result<NvimVerb, String> {
    if words.is_empty() || words.len() > VERB_MAX_WORDS || !words.iter().all(|word| is_verb_word(word)) {
        return Err(format!(
            ":Eitri takes 1 to {VERB_MAX_WORDS} words, each 1 to 64 of the characters A-Z a-z 0-9 . _ : = < > # -"
        ));
    }
    let first = words[0].as_str();
    let rest: Vec<&str> = words[1..].iter().map(String::as_str).collect();
    let request = match (first, rest.as_slice()) {
        ("split", [direction, module]) => {
            let axis = match *direction {
                "right" => Axis::Row,
                "below" => Axis::Column,
                other => return Err(format!(":Eitri split: {other:?} is not 'right' or 'below'")),
            };
            LayoutRequest::Split(module_arg("split", module)?, axis)
        }
        ("split", _) => {
            return Err(usage(
                "split",
                "right|below and a module, as :Eitri split right terminal",
            ))
        }
        ("focus", [module]) => LayoutRequest::Focus(module_arg("focus", module)?),
        ("show", [module]) => LayoutRequest::Show(module_arg("show", module)?),
        ("hide", [module]) => LayoutRequest::Hide(module_arg("hide", module)?),
        ("focus" | "show" | "hide", _) => return Err(usage(first, "one module, as :Eitri focus terminal")),
        ("open", [kind]) => {
            let kind = match *kind {
                "terminal" => ModuleKind::Terminal,
                "agent" => ModuleKind::Agent,
                other => {
                    return Err(format!(
                        ":Eitri open: kind must be \"terminal\" or \"agent\", not {other:?}"
                    ))
                }
            };
            LayoutRequest::Open(kind, None)
        }
        ("open", _) => return Err(usage("open", "terminal or agent")),
        ("workspace", [name]) => {
            Workspaces::valid_name(name).map_err(|err| format!(":Eitri workspace: {err}"))?;
            LayoutRequest::Switch(name.to_string())
        }
        ("workspace", _) => return Err(usage("workspace", "one workspace name")),
        ("keys", []) => return Ok(NvimVerb::Keys),
        ("keys", _) => return Err(":Eitri keys takes no more words".to_string()),
        ("setup", []) => return Ok(NvimVerb::Setup),
        ("setup", _) => return Err(usage("setup", "no words")),
        ("plugin", ["load", name]) if crate::plugin::manifest::id_ok(name) => {
            LayoutRequest::Run(run_action("plugin.load", &[&format!("name={name}")], lua_panels)?)
        }
        ("plugin", _) => return Err("usage: :Eitri plugin load <name>".into()),
        (name, options) => LayoutRequest::Run(run_action(name, options, lua_panels)?),
    };
    Ok(NvimVerb::Request(request))
}

/// What the snippet is told about `:Eitri`'s words.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandWords {
    /// What a first word completes to.
    pub first: Vec<String>,
    /// The first words the host has a reading for, runnable or not: `tab.new` goes to the host, whose refusal says
    /// why, while `zoomm` stops in nvim. A Lua panel's `module.<id>` is not here; the snippet leaves those to the host.
    pub known: Vec<String>,
    /// What a module word completes to.
    pub modules: Vec<String>,
}

/// `:Eitri`'s words. `first` is the verbs, then each of `keymap_actions` that `eitri.layout.run` would run (in their
/// order, once each). `modules` is the ids of `modules`, then every numbered instance [`ModuleId::parse`] takes:
/// the list is fixed at install, and a window opens those later. A word that cannot travel in a `W` line (a Lua panel
/// id with a space, say) is left out, since `:Eitri` refuses to send it.
pub fn completion_words(keymap_actions: &[String], modules: &[ModuleId]) -> CommandWords {
    let lua_panels: Vec<String> = modules
        .iter()
        .filter(|id| id.kind() == ModuleKind::LuaWebview)
        .filter_map(|id| id.as_str().strip_prefix("lua:").map(str::to_string))
        .collect();
    let mut first: Vec<String> = VERBS.iter().map(|verb| verb.to_string()).collect();
    for name in keymap_actions {
        let runs = action::parse(name, &action::required_options(name), &lua_panels)
            .is_ok_and(|parsed| run_refusal(name, &parsed.action).is_none());
        if runs && !first.contains(name) {
            first.push(name.clone());
        }
    }
    let numbered = ["terminal", "agent"]
        .into_iter()
        .flat_map(|kind| (2..).map_while(move |n| ModuleId::parse(&format!("{kind}#{n}")).ok()));
    let mut ids: Vec<String> = Vec::new();
    for id in modules.iter().cloned().chain(numbered) {
        let id = id.as_str().to_string();
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    first.retain(|word| is_verb_word(word));
    ids.retain(|word| is_verb_word(word));
    CommandWords {
        first,
        known: VERBS.iter().chain(action::NAMES).map(|name| name.to_string()).collect(),
        modules: ids,
    }
}

fn usage(verb: &str, takes: &str) -> String {
    format!(":Eitri {verb} takes {takes}")
}

fn module_arg(verb: &str, id: &str) -> Result<ModuleId, String> {
    ModuleId::parse(id).map_err(|err| format!(":Eitri {verb}: {err}"))
}

/// The action `name` with `options` (`key=value` words), refused as `eitri.layout.run` refuses it.
fn run_action(name: &str, options: &[&str], lua_panels: &[String]) -> Result<Action, String> {
    let mut opts: Vec<(String, OptValue)> = Vec::new();
    for word in options {
        let Some((key, value)) = word.split_once('=') else {
            return Err(format!(
                ":Eitri {name}: {word:?} is not an option; an option is written key=value"
            ));
        };
        if opts.iter().any(|(given, _)| given == key) {
            return Err(format!(":Eitri {name}: option {key:?} is given twice"));
        }
        opts.push((key.to_string(), option_value(key, value)));
    }
    if opts.iter().any(|(key, _)| key == "repeatable") {
        return Err(format!(":Eitri {name}: repeatable is a key binding's option"));
    }
    let action = action::parse(name, &opts, lua_panels)
        .map_err(|err| format!(":Eitri {err}"))?
        .action;
    if let Some(why) = run_refusal(name, &action) {
        return Err(format!(":Eitri {why}"));
    }
    Ok(action)
}

/// Option `key`'s value as Lua would have passed it: the text itself for an option that takes text, so a workspace
/// named `123` stays a name, otherwise a whole number, a boolean, or a string the parser refuses in its own words.
fn option_value(key: &str, text: &str) -> OptValue {
    if action::is_text_option(key) {
        return OptValue::Str(text.to_string());
    }
    let digits = text.strip_prefix('-').unwrap_or(text);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(number) = text.parse::<i64>() {
            return OptValue::Int(number);
        }
    }
    match text {
        "true" => OptValue::Bool(true),
        "false" => OptValue::Bool(false),
        _ => OptValue::Str(text.to_string()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn plugin_completion_offers_load_and_no_page_names() {
        let lua = mlua::Lua::new();
        lua.load(
            r#"
            local commands = {}
            vim = {
                v = { vim_did_enter = 1 }, g = { eitri = 1 },
                api = {
                    nvim_get_commands = function() return commands end,
                    nvim_buf_get_commands = function() return {} end,
                    nvim_create_user_command = function(name, callback, opts)
                        commands[name] = { callback = callback, definition = opts.desc }
                        completion = opts.complete
                    end,
                },
            }
        "#,
        )
        .exec()
        .unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("socket", "/unused").unwrap();
        opts.set("words", lua.create_sequence_from(["plugin"]).unwrap())
            .unwrap();
        opts.set("names", lua.create_sequence_from(["plugin"]).unwrap())
            .unwrap();
        opts.set("modules", lua.create_sequence_from(["editor"]).unwrap())
            .unwrap();
        lua.load(include_str!("nvim_verbs/eitri_commands.lua"))
            .call::<mlua::Value>(opts)
            .unwrap();
        let complete: mlua::Function = lua.globals().get("completion").unwrap();
        let words = |lead: &str, line: &str| complete.call::<Vec<String>>((lead, line, line.len())).unwrap();
        assert_eq!(words("", "Eitri plugin "), vec!["load"]);
        assert_eq!(words("lo", "Eitri plugin lo"), vec!["load"]);
        assert!(words("", "Eitri plugin load ").is_empty());
        assert!(words("ma", "Eitri plugin load ma").is_empty());
    }
    #[test]
    fn plugin_load_sugar_and_action_ask_for_the_same_page() {
        let sugar = request("plugin load agents-top");
        assert_eq!(sugar, request("plugin.load name=agents-top"));
        let LayoutRequest::Run(action) = sugar else {
            panic!("plugin load must run an action")
        };
        assert_eq!(action.name(), "plugin.load");
        assert_eq!(
            action.options(),
            vec![("name".into(), OptValue::Str("agents-top".into()))]
        );
    }

    #[test]
    fn plugin_load_refuses_bad_words_with_usage() {
        for line in [
            "plugin",
            "plugin load",
            "plugin load markdown extra",
            "plugin reload markdown",
            "plugin load Bad",
            "plugin load lua:markdown",
            "plugin load foo_bar",
        ] {
            assert!(refusal(line).contains(":Eitri plugin load <name>"), "{line}");
        }
    }

    #[test]
    fn plugin_is_a_known_completed_verb() {
        let words = completion_words(&[], &[]);
        assert!(words.first.contains(&"plugin".into()));
        assert!(words.known.contains(&"plugin".into()));
    }
    use super::*;
    use crate::keymap::action::{required_options, NAMES};
    use crate::layout::Direction;

    fn parse(line: &str) -> Result<NvimVerb, String> {
        let words: Vec<String> = line.split(' ').map(str::to_string).collect();
        parse_words(&words, &["notes".to_string()])
    }

    fn request(line: &str) -> LayoutRequest {
        match parse(line) {
            Ok(NvimVerb::Request(request)) => request,
            other => panic!("{line}: {other:?}"),
        }
    }

    fn refusal(line: &str) -> String {
        match parse(line) {
            Err(why) => why,
            Ok(verb) => panic!("{line} was accepted as {verb:?}"),
        }
    }

    fn id(text: &str) -> ModuleId {
        ModuleId::parse(text).unwrap()
    }

    /// `name` and its required options, as the words `:Eitri` sends for them.
    fn words_for(name: &str) -> Vec<String> {
        let mut words = vec![name.to_string()];
        for (key, value) in required_options(name) {
            words.push(match value {
                OptValue::Str(s) => format!("{key}={s}"),
                OptValue::Int(n) => format!("{key}={n}"),
                OptValue::Bool(b) => format!("{key}={b}"),
                OptValue::Other(kind) => unreachable!("no required option is {kind}"),
            });
        }
        words
    }

    #[test]
    fn split_places_a_module_right_or_below() {
        assert_eq!(
            request("split right agent"),
            LayoutRequest::Split(ModuleId::agent(), Axis::Row)
        );
        assert_eq!(
            request("split below terminal#2"),
            LayoutRequest::Split(id("terminal#2"), Axis::Column)
        );
        assert_eq!(
            request("split right lua:notes"),
            LayoutRequest::Split(ModuleId::lua("notes"), Axis::Row)
        );
        assert_eq!(
            refusal("split left agent"),
            ":Eitri split: \"left\" is not 'right' or 'below'"
        );
        assert!(refusal("split right").starts_with(":Eitri split takes "));
        assert!(refusal("split right agent terminal").starts_with(":Eitri split takes "));
    }

    #[test]
    fn focus_show_and_hide_take_one_module() {
        assert_eq!(request("focus terminal#2"), LayoutRequest::Focus(id("terminal#2")));
        assert_eq!(request("show agent#9"), LayoutRequest::Show(id("agent#9")));
        assert_eq!(request("hide editor"), LayoutRequest::Hide(ModuleId::editor()));
        for verb in ["focus", "show", "hide"] {
            assert!(refusal(verb).starts_with(&format!(":Eitri {verb} takes ")));
            assert!(refusal(&format!("{verb} agent terminal")).starts_with(&format!(":Eitri {verb} takes ")));
        }
    }

    #[test]
    fn a_module_id_is_refused_with_module_id_parses_own_words() {
        for (line, verb, text) in [
            ("focus terminal#1", "focus", "terminal#1"),
            ("show c2", "show", "c2"),
            ("split right agent#10", "split", "agent#10"),
            ("hide editor#2", "hide", "editor#2"),
            ("focus lua:", "focus", "lua:"),
        ] {
            let why = ModuleId::parse(text).unwrap_err().to_string();
            assert_eq!(refusal(line), format!(":Eitri {verb}: {why}"));
        }
        assert_eq!(
            refusal("focus terminal#1"),
            ":Eitri focus: 'terminal#1': instance 1 is 'terminal'"
        );
    }

    #[test]
    fn open_makes_a_terminal_or_a_chat_view_placed_by_the_rules() {
        assert_eq!(
            request("open terminal"),
            LayoutRequest::Open(ModuleKind::Terminal, None)
        );
        assert_eq!(request("open agent"), LayoutRequest::Open(ModuleKind::Agent, None));
        assert_eq!(
            refusal("open editor"),
            ":Eitri open: kind must be \"terminal\" or \"agent\", not \"editor\""
        );
        assert!(refusal("open").starts_with(":Eitri open takes "));
    }

    #[test]
    fn workspace_switches_to_a_name_the_workspace_rules_accept() {
        assert_eq!(request("workspace study"), LayoutRequest::Switch("study".into()));
        let long = "w".repeat(40);
        let why = Workspaces::valid_name(&long).unwrap_err();
        assert_eq!(
            refusal(&format!("workspace {long}")),
            format!(":Eitri workspace: {why}")
        );
        assert!(refusal("workspace").starts_with(":Eitri workspace takes "));
        assert!(refusal("workspace my study").starts_with(":Eitri workspace takes "));
    }

    #[test]
    fn keys_opens_the_key_sheet_and_takes_nothing_more() {
        assert_eq!(parse("keys"), Ok(NvimVerb::Keys));
        assert_eq!(refusal("keys now"), ":Eitri keys takes no more words");
    }

    #[test]
    fn eitri_setup_parses_to_nvim_verb_setup() {
        assert_eq!(parse("setup"), Ok(NvimVerb::Setup));
        assert_eq!(refusal("setup x"), ":Eitri setup takes no words");
        assert_eq!(
            refusal("window.setup"),
            ":Eitri window.setup is not a layout action",
            "the action runs from the prefix and the command palette, not as a layout request"
        );
    }

    #[test]
    fn an_action_name_runs_with_its_options_typed_as_lua_would_pass_them() {
        assert_eq!(request("zoom"), LayoutRequest::Run(Action::Zoom));
        assert_eq!(
            request("resize.left cells=5"),
            LayoutRequest::Run(Action::Resize {
                dir: Direction::Left,
                cells: 5
            })
        );
        assert_eq!(
            request("move.to-window module=terminal#2 follow=true"),
            LayoutRequest::Run(Action::MoveToWindow {
                module: Some(id("terminal#2")),
                follow: true
            })
        );
        assert_eq!(
            request("instance.new kind=agent"),
            LayoutRequest::Run(Action::InstanceNew {
                kind: ModuleKind::Agent
            })
        );
        assert_eq!(
            request("module.notes"),
            LayoutRequest::Run(Action::Module(ModuleId::lua("notes")))
        );
        assert_eq!(request("tab.break"), LayoutRequest::Run(Action::TabBreak));
        assert_eq!(
            request("layout.even-horizontal"),
            LayoutRequest::Run(Action::Even(Axis::Row))
        );
        assert_eq!(request("float.toggle"), LayoutRequest::Run(Action::FloatToggle));
    }

    #[test]
    fn option_values_are_numbers_booleans_or_strings() {
        assert_eq!(option_value("cells", "5"), OptValue::Int(5));
        assert_eq!(option_value("cells", "-3"), OptValue::Int(-3));
        assert_eq!(option_value("n", "007"), OptValue::Int(7));
        assert_eq!(option_value("up", "true"), OptValue::Bool(true));
        assert_eq!(option_value("follow", "false"), OptValue::Bool(false));
        assert_eq!(option_value("cells", "pane"), OptValue::Str("pane".into()));
        assert_eq!(option_value("cells", "-"), OptValue::Str("-".into()));
        assert_eq!(option_value("cells", "5x"), OptValue::Str("5x".into()));
        assert_eq!(option_value("cells", ""), OptValue::Str(String::new()));
        assert_eq!(
            option_value("cells", "99999999999999999999"),
            OptValue::Str("99999999999999999999".into()),
            "a number past i64 is not a number"
        );
    }

    #[test]
    fn an_option_that_takes_text_keeps_text_that_reads_as_a_number_or_a_boolean() {
        for name in ["123", "007", "true", "false"] {
            assert_eq!(
                request(&format!("workspace.switch name={name}")),
                LayoutRequest::Run(Action::WorkspaceSwitch { name: name.to_string() }),
            );
        }
        assert_eq!(option_value("module", "5"), OptValue::Str("5".into()));
        assert_eq!(option_value("cwd", "true"), OptValue::Str("true".into()));
    }

    #[test]
    fn a_bad_option_is_refused_with_the_action_parsers_words() {
        assert_eq!(
            refusal("resize.left cells=0"),
            ":Eitri option \"cells\" must be 1 to 500"
        );
        assert_eq!(
            refusal("resize.left cells=wide"),
            ":Eitri option \"cells\" must be a whole number, not a string"
        );
        assert_eq!(
            refusal("zoom cells=2"),
            ":Eitri option \"cells\" is not an option of zoom"
        );
        assert_eq!(refusal("zoomm"), ":Eitri \"zoomm\" is not an action");
        assert!(refusal("module.nope").starts_with(":Eitri \"module.nope\" names no module"));
        assert_eq!(
            refusal("resize.left 5"),
            ":Eitri resize.left: \"5\" is not an option; an option is written key=value"
        );
        assert_eq!(
            refusal("resize.left cells=1 cells=2"),
            ":Eitri resize.left: option \"cells\" is given twice"
        );
    }

    #[test]
    fn repeatable_is_refused_as_eitri_layout_run_refuses_it() {
        assert_eq!(
            refusal("zoom repeatable=true"),
            ":Eitri zoom: repeatable is a key binding's option"
        );
        assert_eq!(
            refusal("resize.left repeatable=false"),
            ":Eitri resize.left: repeatable is a key binding's option"
        );
    }

    #[test]
    fn the_waits_and_every_non_layout_action_are_refused_with_run_refusals_words() {
        for (line, words) in [
            (
                "split.right",
                "split.right waits for a module key; eitri.layout.split(id, 'right'|'below') places one",
            ),
            (
                "split.below cwd=pane",
                "split.below waits for a module key; eitri.layout.split(id, 'right'|'below') places one",
            ),
            (
                "select.number",
                "select.number waits for a digit; eitri.layout.focus(id) gives a module the keys",
            ),
            (
                "workspace.choose",
                "workspace.choose waits for a key; eitri.layout.switch(name) switches workspaces",
            ),
            (
                "workspace.rename",
                "workspace.rename waits for a key; eitri.layout.switch(name) switches workspaces",
            ),
        ] {
            assert_eq!(refusal(line), format!(":Eitri {words}"));
        }
        let mut non_layout = 0;
        for name in NAMES {
            let parsed = action::parse(name, &required_options(name), &[]).unwrap().action;
            if parsed.is_layout() {
                continue;
            }
            non_layout += 1;
            assert_eq!(
                parse_words(&words_for(name), &[]),
                Err(format!(":Eitri {name} is not a layout action")),
                "{name}"
            );
        }
        // The agent panel's family: tab keys, the command line, copy mode, text size, the sheet.
        for name in [
            "tab.new",
            "tab.next",
            "tab.select",
            "panel.command-line",
            "copy-mode",
            "text.larger",
            "panel.keymap",
        ] {
            assert!(NAMES.contains(&name));
        }
        assert!(non_layout >= 20, "{non_layout}");
    }

    #[test]
    fn every_action_eitri_layout_run_runs_is_reachable() {
        for name in NAMES {
            let action = action::parse(name, &required_options(name), &[]).unwrap().action;
            if run_refusal(name, &action).is_some() {
                continue;
            }
            assert_eq!(
                parse_words(&words_for(name), &[]),
                Ok(NvimVerb::Request(LayoutRequest::Run(action))),
                "{name}"
            );
        }
    }

    #[test]
    fn words_the_wire_cannot_carry_are_refused_before_any_parse() {
        let words = |list: &[&str]| list.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        for bad in [
            words(&[]),
            words(&["zoom", "a=1", "b=2", "c=3", "d=4", "e=5", "f=6"]),
            words(&["workspace", "my study"]),
            words(&["workspace", "café"]),
            words(&["focus", "lua:my panel"]),
            words(&[""]),
            words(&["zoom;"]),
            words(&[&"a".repeat(65)]),
        ] {
            let why = parse_words(&bad, &[]).unwrap_err();
            assert!(why.starts_with(":Eitri takes 1 to 6 words"), "{bad:?}: {why}");
        }
        // A space inside a word reaches the host as two words, and what they make is refused.
        assert_eq!(
            refusal("zo om"),
            ":Eitri zo: \"om\" is not an option; an option is written key=value"
        );
        assert_eq!(
            refusal("workspace my study"),
            ":Eitri workspace takes one workspace name"
        );
    }

    #[test]
    fn no_action_name_is_a_verb_word() {
        for verb in VERBS {
            assert!(!NAMES.contains(&verb), "{verb} would hide an action of the same name");
            assert!(
                action::parse(verb, &[], &["notes".to_string()]).is_err(),
                "{verb} parses as an action"
            );
        }
    }

    #[test]
    fn no_verb_yields_a_float_request() {
        let mut lines: Vec<String> = [
            "split right agent",
            "split below lua:notes",
            "focus agent",
            "show terminal#2",
            "hide editor",
            "open terminal",
            "open agent",
            "workspace main",
            "keys",
            "float.toggle",
            "scratchpad.show",
            "scratchpad.show module=terminal#2",
            "scratchpad.move",
        ]
        .iter()
        .map(|line| line.to_string())
        .collect();
        lines.extend(NAMES.iter().map(|name| name.to_string()));
        for line in lines {
            let verb = parse(&line);
            assert!(
                !matches!(
                    verb,
                    Ok(NvimVerb::Request(LayoutRequest::Float(..) | LayoutRequest::Mark(_)))
                ),
                "{line}: {verb:?}"
            );
        }
    }

    #[test]
    fn completion_lists_the_verbs_and_the_actions_run_takes_and_apart_from_them_the_modules() {
        let actions: Vec<String> = [
            "zoom",
            "tab.new",
            "split.right",
            "resize.left",
            "zoom",
            "instance.new",
            "module.notes",
        ]
        .iter()
        .map(|name| name.to_string())
        .collect();
        let modules = [
            ModuleId::editor(),
            id("terminal#2"),
            ModuleId::lua("notes"),
            ModuleId::lua("my notes"),
        ];
        let words = completion_words(&actions, &modules);
        let mut expected: Vec<String> = VERBS.iter().map(|verb| verb.to_string()).collect();
        expected.extend(
            ["zoom", "resize.left", "instance.new", "module.notes"]
                .iter()
                .map(|word| word.to_string()),
        );
        assert_eq!(words.first, expected);
        assert_eq!(words.modules[..3], ["editor", "terminal#2", "lua:notes"]);
    }

    #[test]
    fn every_numbered_instance_completes_once_since_the_list_is_fixed_before_a_window_opens_one() {
        let words = completion_words(&[], &[ModuleId::terminal(), id("terminal#2")]);
        let numbered: Vec<&str> = words.modules.iter().map(String::as_str).skip(2).collect();
        assert_eq!(
            numbered,
            [
                "terminal#3",
                "terminal#4",
                "terminal#5",
                "terminal#6",
                "terminal#7",
                "terminal#8",
                "terminal#9",
                "agent#2",
                "agent#3",
                "agent#4",
                "agent#5",
                "agent#6",
                "agent#7",
                "agent#8",
                "agent#9",
            ]
        );
    }

    #[test]
    fn the_host_answers_every_first_word_the_snippet_lets_through() {
        let words = completion_words(&[], &[]);
        for word in VERBS.iter().chain(NAMES) {
            assert!(words.known.contains(&word.to_string()), "{word}");
        }
        for word in &words.known {
            let refused = parse_words(std::slice::from_ref(word), &[]).err().unwrap_or_default();
            assert!(!refused.contains("is not an action"), "{word}: {refused}");
        }
    }

    #[test]
    fn completion_of_every_name_holds_only_what_run_runs() {
        let names: Vec<String> = NAMES.iter().map(|name| name.to_string()).collect();
        let words = completion_words(&names, &[]).first;
        assert_eq!(&words[..VERBS.len()], VERBS.map(str::to_string).as_slice());
        for word in &words[VERBS.len()..] {
            let verb = parse_words(&words_for(word), &[]);
            assert!(
                matches!(verb, Ok(NvimVerb::Request(LayoutRequest::Run(_)))),
                "{word}: {verb:?}"
            );
        }
        for refused in [
            "split.right",
            "select.number",
            "workspace.choose",
            "tab.new",
            "send-keys",
            "hint",
        ] {
            assert!(!words.contains(&refused.to_string()), "{refused}");
        }
        assert!(words.contains(&"tab.break".to_string()));
    }

    #[test]
    fn eitri_runs_it_and_completion_lists_it() {
        assert_eq!(request("project.choose"), LayoutRequest::Run(Action::ProjectChoose));
        assert!(Action::ProjectChoose.is_layout());
        assert_eq!(run_refusal("project.choose", &Action::ProjectChoose), None);
        assert!(
            completion_words(&NAMES.iter().map(|name| name.to_string()).collect::<Vec<_>>(), &[])
                .first
                .contains(&"project.choose".to_string())
        );
    }

    #[test]
    fn eitri_refuses_a_word_with_a_slash() {
        assert!(refusal("project.choose /x").contains("characters A-Z"));
        assert!(parse_words(&["project.choose".into(), "/x".into()], &[]).is_err());
    }
}
