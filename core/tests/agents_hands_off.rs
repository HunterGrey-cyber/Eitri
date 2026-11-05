//! The agents layer moves the keys and shows state; it never answers. No row, verb or event of it may answer a
//! permission card, a question or a trust prompt, send a turn or change a mode: those stay with keys pressed in
//! the chat page that shows the tab, which `TabSet::may_answer` decides.
//!
//! This reads the layer's code as source text and fails on any mention of an answer, send or mode path in it.
//! Comments and literals are blanked first, and each region is found by its name, so formatting cannot hide a
//! use or invent one. Every file and region must be found and must hold a token it is known to contain, so a
//! moved file turns this red rather than leaving it scanning nothing.

use eitri_core::source_scan as scan;

/// What reaching an answer, a send or a mode change looks like in source.
const FORBIDDEN: [&str; 10] = [
    "may_answer",
    "answer_card",
    "answer_question",
    "answer_trust",
    "approve_pending",
    "respond_permission",
    "confirm_bypass",
    "cycle_mode",
    "set_mode",
    "send_turn",
];

/// Whole files that are the layer's, each with a token it is known to hold.
const FILES: [(&str, &str); 6] = [
    ("core/src/tab_set/agent_rows.rs", "RowContext"),
    ("core/src/wm/reveal.rs", "RevealRequest"),
    ("core/src/plugin/host/agents.rs", "fn agents_verb"),
    ("panel/src/agents_glue.rs", "AgentsGlue"),
    ("shell/src/agents_host.rs", "fn focus_remote"),
    ("core/src/wm/agents_round.rs", "pub enum RoundKey"),
];

/// Each whole-word use of a forbidden name in `code`, as `name`.
fn uses(code: &str) -> Vec<&'static str> {
    FORBIDDEN
        .iter()
        .copied()
        .filter(|name| !scan::word_ends(code, name).is_empty())
        .collect()
}

/// Files of the layer that are not scanned whole, each with why.
const NOT_SCANNED: [(&str, &str); 1] = [(
    "core/src/plugin/host/agents/tests.rs",
    "the host's tests: one of them proves a page's view never satisfies `may_answer`",
)];

/// The match arms of `body` (code, comments and literals blanked) that name an agents verb, each whole: from its
/// first `Verb::Agents` through its pattern and the body after `=>`, a braced block or an expression up to the `,`
/// or the closing brace that ends the arm.
fn agents_arms(body: &str) -> Vec<&str> {
    let mut arms: Vec<(usize, usize)> = Vec::new();
    for (at, _) in body.match_indices("Verb::Agents") {
        if arms.iter().any(|&(start, end)| (start..end).contains(&at)) {
            continue;
        }
        let arrow = at + body[at..].find("=>").expect("an agents arm has a body");
        let mut end = body.len();
        let mut depth = 0i32;
        for (i, c) in body[arrow + 2..].char_indices() {
            let i = arrow + 2 + i;
            match c {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' if depth == 0 => {
                    end = i;
                    break;
                }
                ')' | ']' | '}' => {
                    depth -= 1;
                    if depth == 0 && c == '}' && body[arrow + 2..i].trim_start().starts_with('{') {
                        end = i + 1;
                        break;
                    }
                }
                ',' if depth == 0 => {
                    end = i + 1;
                    break;
                }
                _ => {}
            }
        }
        arms.push((at, end));
    }
    arms.into_iter().map(|(start, end)| &body[start..end]).collect()
}

/// Every Rust file under `dirs` whose name says it is the agents layer's (`agents*.rs`, or one under an `agents/`
/// directory), outside `core/src/agents` (scanned as a whole).
fn agents_named(dirs: &[&str]) -> Vec<String> {
    scan::rust_sources(dirs)
        .into_iter()
        .map(|source| source.path)
        .filter(|path| !path.starts_with("core/src/agents/"))
        .filter(|path| {
            let name = path.rsplit('/').next().unwrap_or(path);
            name.starts_with("agents") || path.contains("/agents/")
        })
        .collect()
}

#[test]
fn the_agents_layer_never_reaches_an_answer_path() {
    let mut wrong = Vec::new();

    let layer = scan::rust_sources(&["core/src/agents"]);
    scan::require_anchors(
        &layer,
        &[
            "pub struct Reader",
            "pub struct Publisher",
            "pub struct SharedStore",
            "pub enum Request",
        ],
    );
    for source in &layer {
        for name in uses(&scan::code_only(&source.text)) {
            wrong.push(format!("{}: {name}", source.path));
        }
    }

    // A new file of the layer is scanned or says why not: a file added later cannot slip past by not being listed.
    for path in agents_named(&["core/src", "panel/src", "shell/src"]) {
        assert!(
            FILES.iter().any(|(listed, _)| *listed == path) || NOT_SCANNED.iter().any(|(listed, _)| *listed == path),
            "{path} is named for the agents layer but this scan does not list it: add it to FILES with a token it holds"
        );
    }

    for (path, anchor) in FILES {
        let source = scan::read_required(path);
        scan::require_anchors(std::slice::from_ref(&source), &[anchor]);
        for name in uses(&scan::code_only(&source.text)) {
            wrong.push(format!("{path}: {name}"));
        }
    }

    // The plugin host's agents arms: each hands the verb to the agents module and does nothing else.
    let host = [scan::read_required("core/src/plugin/host.rs")];
    let (_, handle) = scan::function_in(&host, "handle");
    let arms = agents_arms(&handle);
    assert!(
        !arms.is_empty() && handle.contains("self.agents_verb("),
        "the host's `handle` no longer routes the agents verbs through `agents_verb`"
    );
    for arm in arms {
        for name in uses(arm) {
            wrong.push(format!("core/src/plugin/host.rs handle: {name}"));
        }
    }

    // The window's half of the plugin pages' agents: the port it hands every host, and the hand-over.
    let panels = [scan::read_required("shell/src/plugin_panels.rs")];
    let code = scan::code_only(&panels[0].text);
    let ports = scan::impl_blocks(&code, "WindowAgents");
    assert!(
        !ports.is_empty(),
        "shell/src/plugin_panels.rs no longer implements the agents port on WindowAgents"
    );
    let (_, set_agents) = scan::function_in(&panels, "set_agents");
    for region in ports.into_iter().chain([set_agents.as_str()]) {
        for name in uses(region) {
            wrong.push(format!("shell/src/plugin_panels.rs agents: {name}"));
        }
    }

    assert!(
        wrong.is_empty(),
        "the agents layer names an answer, send or mode path: {wrong:#?}"
    );
}

#[test]
fn a_forbidden_name_in_code_is_found_and_one_in_a_comment_is_not() {
    let code = scan::code_only("fn f(t: &TabSet) { t.may_answer(a, b); } // set_mode\nlet s = \"send_turn\";");
    assert_eq!(uses(&code), ["may_answer"]);
    assert!(uses(&scan::code_only("fn may_answers_list() {}")).is_empty());
    let arms = agents_arms("match v {\n Verb::AgentsWatch(_) => self.agents_verb(re, v, now),\n Verb::Other => x,\n}");
    assert_eq!(arms, ["Verb::AgentsWatch(_) => self.agents_verb(re, v, now),"]);
}

#[test]
fn an_agents_arm_is_scanned_to_its_end() {
    // A body on the lines after the arrow, a call on its second line.
    let code = "match v {\n Verb::AgentsFocus(_) =>\n  self.agents_verb(\n   re, v,\n  ).then(t.may_answer(a)),\n Verb::Other => x,\n}";
    let arms = agents_arms(code);
    assert_eq!(arms.len(), 1, "{arms:?}");
    assert_eq!(uses(arms[0]), ["may_answer"], "{arms:?}");
    // A block body, and an or-pattern of several verbs: one arm.
    let code = "match v {\n v @ (Verb::AgentsWatch(_)\n | Verb::AgentsNew(_)) => {\n  self.agents_verb(re, v, now);\n  t.send_turn(x)\n }\n Verb::Other => { t.may_answer(a) }\n}";
    let arms = agents_arms(code);
    assert_eq!(arms.len(), 1, "{arms:?}");
    assert_eq!(uses(arms[0]), ["send_turn"], "the next arm is not part of it: {arms:?}");
    // The last arm, closed by the match's own brace.
    let arms = agents_arms("match v {\n Verb::AgentsStop(_) => t.set_mode(m)\n}");
    assert_eq!(arms, ["Verb::AgentsStop(_) => t.set_mode(m)\n"]);
    assert!(agents_named(&["core/src/plugin"]).contains(&"core/src/plugin/host/agents.rs".to_owned()));
}
