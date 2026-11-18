use super::*;

#[test]
fn a_clean_start_does_not_keep_the_compositor_record_to_rewrite() {
    use eitri_core::window_link::cm_file::CmFile;
    let file = CmFile::new(&eitri_core::layout::hub_tree().layout, &[]);
    assert_eq!(cm_record_kept(Some(file.clone()), false), Some(file.clone()));
    assert_eq!(cm_record_kept(Some(file), true), None);
}

/// The last tab closed by hand reaches the controller as a tab having left the window; a hand-over's answers go
/// over the link instead.
#[test]
fn a_closed_last_tab_reaches_the_controller_as_a_tab_that_left() {
    let event = chat_move_event(WinId::MAIN, agent_panel::ChatMoveEvent::LastTabClosed);
    assert!(matches!(event, Some(ChatEvent::TabLeft { win: WinId::MAIN })));
}

/// Only a run that follows compositor mode gives saved views tiles: a layout file `eitri` mode wrote says what it
/// wants, and a window with `window.restore` off asked for its default layout.
#[test]
fn only_a_way_back_from_compositor_mode_tiles_the_saved_views() {
    use eitri_core::source_scan as scan;
    let flat = |text: &str| text.split_whitespace().collect::<String>();
    let (_, code) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
    let code = flat(&code);
    let guarded = code
        .find(concat!("letsaved_views=(back_from_", "compositor"))
        .expect("the views are read only then");
    let tiled = code
        .find(concat!("tile_saved_", "views(&mutstart_layout"))
        .expect("the start layout gets them");
    let views = code
        .find(concat!("letstartup_chat_", "views"))
        .expect("the window builds its chat views from that layout");
    assert!(guarded < tiled && tiled < views);
}

/// Enumerating extra views merges workspaces without duplicating or including the base view.
#[test]
fn extra_chat_views_are_sorted_and_unique_across_workspaces() {
    use eitri_core::layout::{Layout, ModuleDecl, Placement};
    let id = |text: &str| ModuleId::parse(text).unwrap();
    let below = |ids: &[&str]| -> Layout {
        let decls: Vec<ModuleDecl> = ids
            .iter()
            .map(|text| ModuleDecl {
                id: id(text),
                placement: Placement::BelowEditor,
            })
            .collect();
        Layout::initial(&decls).unwrap()
    };
    let current = below(&["terminal"]);
    let other = below(&["terminal", "agent#2"]);
    let both = below(&["terminal", "agent#3", "agent#2"]);
    assert_eq!(
        extra_chat_views(&[&both, &other, &current]),
        vec![id("agent#2"), id("agent#3")],
        "the extra views every layout holds, once each and never the base view"
    );
    assert!(extra_chat_views(&[&current]).is_empty());
}

/// Checks that `build_application` sets `NON_UNIQUE`, which disables gio's
/// single-instance behavior. Removing the flag fails this assertion;
/// its runtime effect appears when a second window is launched.
/// This test does not launch a second `shell` or exercise gio registration, and
/// would still pass if another route ran `build_ui` twice in one process.
/// Separate processes on separate launches follow gio's semantics; demonstrating
/// that behavior requires launching two real `shell` processes.
#[test]
fn build_application_still_opts_out_of_gios_single_instance_behaviour() {
    for kind in [AppKind::Main, AppKind::Chat, AppKind::Page] {
        let app = build_application(kind);
        assert!(
            app.flags().contains(gtk4::gio::ApplicationFlags::NON_UNIQUE),
            "{kind:?}"
        );
    }
}

#[test]
fn each_kind_of_window_has_its_app_id() {
    assert_eq!(AppKind::Main.id(), "cn.huntergrey.eitri");
    assert_eq!(AppKind::Chat.id(), "cn.huntergrey.eitri.Panel");
    assert_eq!(AppKind::Page.id(), "cn.huntergrey.eitri.Page");
    let module = |name: WindowName| linked_window::WindowStart::Module {
        name,
        root: PathBuf::from("/p"),
        sock: std::os::fd::OwnedFd::from(std::fs::File::open("/dev/null").unwrap()),
        token: None,
        restore: false,
    };
    assert_eq!(AppKind::of(&module(WindowName::Chat(1))), AppKind::Chat);
    assert_eq!(AppKind::of(&module(WindowName::Chat(4))), AppKind::Chat);
    assert_eq!(AppKind::of(&module(WindowName::Page("markdown".into()))), AppKind::Page);
    assert_eq!(AppKind::of(&module(WindowName::Second)), AppKind::Main);
    assert_eq!(AppKind::of(&linked_window::WindowStart::Full), AppKind::Main);
    assert_eq!(
        AppKind::of(&linked_window::WindowStart::Beside {
            name: WindowName::Page("notes".into()),
            nvim: PathBuf::from("/run/user/1/nvim.7.0"),
            root: PathBuf::from("/p"),
        }),
        AppKind::Page,
        "a page window beside nvim is a page window to the compositor's rules"
    );
}

fn beside_pair() -> Rc<linked_window::spawn::Pair> {
    linked_window::spawn::Pair::beside(
        WindowName::Page("notes".into()),
        Path::new("/p"),
        linked_window::spawn::tests::idle_beside_link("notes"),
    )
}

/// No window 1 started a beside window, yet its mode is not in question: it is a module window from its start,
/// never a hub, so the start reads no mode and asks the compositor nothing.
#[test]
fn early_facts_of_a_beside_window_rule_out_a_hub() {
    let facts = early_facts(&beside_pair(), false);
    assert!(facts.started, "the mode is not in question");
    assert!(!facts.shows_chat);
    assert!(!facts.unpaired && !facts.alone && !facts.window2_orphan && !facts.module_orphans);
    let full = linked_window::spawn::Pair::full(Err(std::io::Error::other("no state")), Path::new("/p"), false, false);
    assert!(
        !early_facts(&full, true).started,
        "a full window's mode is still decided"
    );
}

/// The first-start screen and its onboarding belong to a full window; a page window beside nvim offers neither.
#[test]
fn a_beside_window_runs_no_onboarding() {
    assert!(!onboarding_full_window(&beside_pair()));
    let alone = linked_window::spawn::Pair::full(Err(std::io::Error::other("no state")), Path::new("/p"), false, false);
    assert!(onboarding_full_window(&alone), "a full window still offers it");
    let unpaired = linked_window::spawn::Pair::full(Ok(None), Path::new("/p"), false, false);
    assert!(!onboarding_full_window(&unpaired));
    // The first show presents a beside window through the pair's raise, as a module window's is.
    let src = include_str!("main.rs");
    assert!(src.contains(concat!(
        "pair_raise.used_for_window(\n        pair.holds_pair() || pair.is_second() || pair.",
        "is_beside(),"
    )));
}

/// `eitri page` reads `$NVIM` and its own arguments before the scrub, and never reaches the project resolver,
/// which would read `page` as a directory.
#[test]
fn the_page_subcommand_is_dispatched_before_the_project_resolver() {
    // Split literals, so this test's own text is not counted.
    let src = include_str!("main.rs");
    let start = src.find(concat!("fn main() -> glib", "::ExitCode {")).expect("main");
    let end = start
        + src[start..]
            .find(concat!("fn build_", "application"))
            .expect("build_application");
    let body = &src[start..end];
    let at = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("{needle} in main"));
    let order = [
        at(concat!("args_os().skip(1)", ".collect()")),
        at(concat!("companion::run(", "early_args")),
        at(concat!("page_window::run_", "args(&early_args[1..])")),
        at(concat!("drop_inherited", "_editor_env()")),
        at(concat!("tasks::install", "_guard(")),
        at(concat!("project_root::", "resolve_launch()")),
    ];
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
}

/// A chat or page window shows one module: `build_ui` makes no editor for it, reads and writes no layout file,
/// hangs no top bar in the window and builds no other module's host, and a chat window shows its chat in its
/// own base view.
#[test]
fn the_module_window_branch_builds_only_its_module() {
    use eitri_core::source_scan as scan;
    let flat = |text: &str| text.split_whitespace().collect::<String>();
    let (_, code) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
    for needle in [
        concat!("module_window.is_none() {\n        root.append(&top_bar", ".widget)"),
        concat!("made.retain(|decl| decl.module == ", "*module)"),
        concat!("reconcile_", "module("),
        concat!("Tiling::", "ModuleWindow"),
        concat!("set_page_", "roots("),
    ] {
        assert!(flat(&code).contains(&flat(needle)), "build_ui lacks {needle}");
    }
    assert!(
        !code.contains(concat!("set_transient", "_for")),
        "a module window is not a dialog of window 1"
    );
    let (_, modes) = scan::function_in(&[scan::read_required("shell/src/window_mode.rs")], "sync_widgets");
    assert!(
        flat(&modes).contains(&flat(concat!("self.top_bar.parent()", ".is_some() &&"))),
        "a top bar with no parent is never shown again by a window mode or a reveal"
    );
}

/// A `bye` to a module window is the group closing: the window closes instead of waiting for a window 1.
#[test]
fn a_module_window_that_hears_its_group_close_closes_rather_than_waits() {
    use eitri_core::source_scan as scan;
    let flat = |text: &str| text.split_whitespace().collect::<String>();
    let (_, code) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
    let code = flat(&code);
    let asked = code
        .find(concat!("waits_for_relink_", "after(end)"))
        .expect("the hook asks how the link ended");
    let closes = code[asked..]
        .find(concat!("HostEvent::", "GroupClosed"))
        .expect("a group's end closes the window");
    let waits = code[asked..]
        .find(concat!("pair.wait_for_", "relink("))
        .expect("any other end waits");
    assert!(closes < waits, "the close is the branch that does not wait");
}

/// A chat window saves nothing itself: its panel's writer is off from the moment the mode is known, each change of
/// its tabs goes to window 1, and a link that comes up hears the latest.
#[test]
fn a_module_window_tells_window_1_its_tabs_and_keeps_no_file() {
    use eitri_core::source_scan as scan;
    let flat = |text: &str| text.split_whitespace().collect::<String>();
    let (_, code) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
    let code = flat(&code);
    assert_eq!(
        code.matches(&flat(concat!("agent_panel_handle.keep_no", "_tabs();")))
            .count(),
        2
    );
    for wired in [
        concat!("agent_panel_handle.report_", "tabs(move |tabs|"),
        concat!("pair.tabs_", "changed(tabs.clone());"),
        concat!("pair.say_", "tabs();"),
        concat!("linked_window::group_tabs::beside_the_", "layout(open, tab_windows)"),
    ] {
        assert_eq!(code.matches(&flat(wired)).count(), 1, "{wired}");
    }
}

/// What a chat or page window needs of `build_ui` besides its module: its name for the controller, the keys left
/// in window 1 when it was started restored, no saved tabs of window 2's, the focus it gained before it linked,
/// and window 1's way to hear that a window it started never linked.
#[test]
fn the_module_window_branch_names_itself_and_wires_its_start_and_focus() {
    use eitri_core::source_scan as scan;
    let flat = |text: &str| text.split_whitespace().collect::<String>();
    let (_, code) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
    for needle in [
        concat!("driver.set_module_", "name(WinId::MAIN, name.clone())"),
        concat!("pair.restored() && module_window.", "is_none(),"),
        concat!("PlacementRole::", "NewModule"),
        concat!("pair.restored() || module_window.", "is_some()"),
        concat!("window_weak.upgrade().is_some_and(|window| window.", "is_active())"),
        concat!("pair.say_window_", "focused()"),
        concat!("pair.on_module_", "failed("),
        concat!("HostEvent::ModuleWindow", "Failed {"),
    ] {
        assert!(flat(&code).contains(&flat(needle)), "build_ui lacks {needle}");
    }
}

/// Keysyms named literally (not a single character and not an `Fnn` function key) that
/// this test knows how to read for a person, and how it spells each one. The zoom-together
/// design's text-size accelerators (`<Control>equal` and friends) are the first accelerators
/// in this crate whose key part isn't a bare letter/digit or a function key, so `spell` and
/// `is_accel` both need to know them by name -- this table is the one place that names them
/// for both.
const NAMED_KEYSYMS: [(&str, &str); 7] = [
    ("equal", "="),
    ("plus", "+"),
    ("minus", "-"),
    ("KP_Add", "Keypad+"),
    ("KP_Subtract", "Keypad-"),
    ("KP_0", "Keypad0"),
    // Keypad 0 with NumLock off reports as KP_Insert, not KP_0 -- text-reset binds both
    // (2026-09-23).
    ("KP_Insert", "Keypad0 (NumLock off)"),
];

/// `<Mod>…Key`, with at least one modifier or a function key. The shape the sources are
/// scanned for; anything else in a string literal is not an accelerator.
///
/// A literal with a valid modifier-bracket prefix (so it is clearly meant as an
/// accelerator) whose key part is neither a bare letter/digit, an `Fnn` function key, nor
/// one of `NAMED_KEYSYMS` is a scanner FAILURE, not a silent skip: it panics, naming the
/// literal and the file, rather than quietly leaving `found` blind to it. That is the
/// direction this whole test is written to fail in -- see its own doc comment on "both
/// directions" -- and the reason it exists concretely: before `NAMED_KEYSYMS` exhausted,
/// `<Control>KP_Add` (underscore is not alphanumeric) would otherwise have been dropped
/// from `found` with nothing here noticing the guard had gone blind to it.
fn is_accel(literal: &str) -> bool {
    let mut rest = literal;
    let mut modifiers = 0;
    while let Some(end) = rest.strip_prefix('<').and_then(|r| r.find('>')) {
        if !rest[1..end + 1].chars().all(|c| c.is_ascii_alphabetic()) {
            return false;
        }
        modifiers += 1;
        rest = &rest[end + 2..];
    }
    let function_key = rest.starts_with('F') && rest.len() > 1 && rest[1..].chars().all(|c| c.is_ascii_digit());
    if rest.is_empty() || (modifiers == 0 && !function_key) {
        return false;
    }
    if function_key
        || rest.chars().all(|c| c.is_ascii_alphanumeric())
        || NAMED_KEYSYMS.iter().any(|(keysym, _)| *keysym == rest)
    {
        return true;
    }
    panic!(
        "{literal:?} has a modifier bracket ({modifiers} of them) but its key part {rest:?} \
         is not one `spell`/`NAMED_KEYSYMS` knows how to read -- teach both, or this was \
         never meant to be an accelerator"
    );
}

/// Every `.rs` file of the window crates `shell` and `panel` and whatever joins them
/// (`eitri_core::source_scan::host_dirs_except` without `core` and `mac`), with its `#[cfg(test)]`
/// module and its line comments cut off -- so this test's own copies of the accelerators cannot satisfy it.
/// Fails when a directory is missing or the code the scan exists to cover is not in what it read.
fn sources() -> Vec<(String, String)> {
    use eitri_core::source_scan as scan;
    let read = scan::rust_sources(&scan::host_dirs_except(&["core/src", "mac/src"]));
    scan::require_anchors(
        &read,
        &["fn build_ui(", "fn handle_inbound_message(", "struct TrustGate"],
    );
    let test_only = scan::test_only_files(&read);
    read.into_iter()
        .filter(|source| !test_only.contains(&source.path))
        .map(|source| {
            let code = source.text.split("#[cfg(test)]").next().unwrap_or_default();
            let code: String = code
                .lines()
                .map(|line| line.split("//").next().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n");
            (source.path, code)
        })
        .collect()
}

/// Every accelerator-shaped string literal in `shell/src`, cut from `.rs` files by
/// [`sources`]/[`is_accel`]. Shared, so the two tests below scan the same tree the same way.
fn found_accelerators() -> Vec<(String, String)> {
    let mut found = Vec::new();
    for (name, code) in &sources() {
        for (index, chunk) in code.split('"').enumerate() {
            if index % 2 == 1 && is_accel(chunk) {
                found.push((name.clone(), chunk.to_string()));
            }
        }
    }
    found
}

/// Every accelerator is registered from `eitri_core::keymap::root`, so an
/// accelerator-shaped literal anywhere in `shell/src` is one registered around the keymap --
/// and one the no-`Ctrl+Shift` check in core never saw. `is_accel` is exercised directly, so
/// this cannot pass because the scanner went blind.
#[test]
fn shell_src_writes_no_accelerator_literal() {
    sources();
    assert!(is_accel("<Control><Shift>f") && is_accel("F11") && is_accel("<Control>KP_Add"));
    assert!(!is_accel("<C-b>") && !is_accel("Ctrl+b"));
    assert_eq!(
        found_accelerators(),
        Vec::<(String, String)>::new(),
        "register it through eitri_core::keymap::root instead"
    );
}

/// Every keyboard arrival lands in BROWSE, except the
/// new-tab path, which still opens the composer -- an empty tab has nothing to browse.
#[test]
fn keyboard_arrival_is_browse_and_only_a_new_tab_types() {
    let (_, src) =
        eitri_core::source_scan::function_in(&eitri_core::source_scan::rust_sources(&["shell/src"]), "build_ui");
    let calls = src.matches(concat!(".enter", "_input(")).count();
    assert_eq!(calls, 0, "the shell's wiring never enters INPUT");
    let services = eitri_core::source_scan::code_only(
        &eitri_core::source_scan::read_required("shell/src/gtk_host/services.rs").text,
    );
    let tab_calls = eitri_core::source_scan::functions(&services, "tab_call");
    assert_eq!(tab_calls.len(), 1);
    assert_eq!(tab_calls[0].matches(".enter_input(").count(), 1);
    assert!(tab_calls[0].contains("agent.enter_input(ViewId::BASE)"));
    // `Ctrl+h/j/k/l` moves are the controller's: they land through its `land_by_key`, whose arrival is the
    // one that lands where the reader left the chat.
    let focus =
        eitri_core::source_scan::code_only(&eitri_core::source_scan::read_required("core/src/wm/focus.rs").text);
    let move_focus = eitri_core::source_scan::functions(&focus, "move_focus");
    assert_eq!(move_focus.len(), 1, "one move between modules");
    assert!(
        move_focus[0].contains(concat!("land_by_", "key(w, win, &to, batch)")),
        "a Ctrl+h/j/k/l move must land through land_by_key"
    );
    let land = eitri_core::source_scan::functions(&focus, "land_by_key");
    assert!(
        land[0].contains("arrive_tail(win, to, false)"),
        "landing by key arrives where the reader left the chat"
    );
}

/// The full window drops the outer nvim's variables before anything can inherit them: after the
/// panel branch (which reads `$NVIM`), ahead of the backend check, the nvim probe and GTK. Byte
/// offsets, not lines, so the order holds however the lines are wrapped.
#[test]
fn main_drops_the_inherited_editor_env_before_any_child() {
    // Split literals, so this test's own text is not counted.
    let src = include_str!("main.rs");
    let start = src.find(concat!("fn main() -> glib", "::ExitCode {")).expect("main");
    let end = start
        + src[start..]
            .find(concat!("fn build_", "application"))
            .expect("build_application");
    let body = &src[start..end];
    let call = concat!("companion::env::drop_inherited", "_editor_env()");
    assert_eq!(body.matches(call).count(), 1, "called exactly once in main");
    let at = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("{needle} in main"));
    let order = [
        at(concat!("companion::run(", "early_args")),
        at(call),
        at(concat!("BackendKind::", "from_env")),
        at(concat!("nvim_bin::", "resolve()")),
        at(concat!("build_", "application(kind)")),
    ];
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
}

/// What a launch hands a file over with is gone from the environment after the scrub and after GTK, and a launch
/// that hands its file over must end before anything of a window exists.
#[test]
fn main_reads_the_open_socket_before_the_scrub_and_delivers_before_gtk() {
    // Split literals, so this test's own text is not counted.
    let src = include_str!("main.rs");
    let start = src.find(concat!("fn main() -> glib", "::ExitCode {")).expect("main");
    let end = start
        + src[start..]
            .find(concat!("fn build_", "application"))
            .expect("build_application");
    let body = &src[start..end];
    let at = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("{needle} in main"));
    let order = [
        at(concat!("open_requests::hint_", "from_env()")),
        at(concat!("open_requests::token_", "from_env()")),
        at(concat!("companion::env::drop_inherited", "_editor_env()")),
        at(concat!("resolve_", "launch()")),
        at(concat!("open_requests::hand_", "over(")),
        at(concat!("open_requests::", "raise(")),
        at(concat!("nvim_bin::", "resolve()")),
        at(concat!("build_", "application(kind)")),
    ];
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
}

/// Window 2's link end and log come first: before any other subcommand, before the editor environment is
/// touched, before any child or thread, and before the project resolver, which would read its arguments as a
/// directory.
#[test]
fn the_linked_window_subcommand_is_dispatched_before_the_project_resolver() {
    // Split literals, so this test's own text is not counted.
    let src = include_str!("main.rs");
    let start = src.find(concat!("fn main() -> glib", "::ExitCode {")).expect("main");
    let end = start
        + src[start..]
            .find(concat!("fn build_", "application"))
            .expect("build_application");
    let body = &src[start..end];
    let at = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("{needle} in main"));
    let order = [
        at(concat!("args_os().skip(1)", ".collect()")),
        at(concat!("start::", "SUBCOMMAND")),
        at(concat!("second_first_acts(", "&early_args[1..])")),
        at(concat!("run_guard::", "SUBCOMMAND")),
        at(concat!("companion::run(", "early_args")),
        at(concat!("drop_inherited", "_editor_env()")),
        at(concat!("tasks::install", "_guard(")),
        at(concat!("project_root::", "resolve_launch()")),
    ];
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
    assert_eq!(
        eitri_core::window_link::start::SUBCOMMAND,
        "linked-window",
        "the subcommand window 1 starts window 2 with"
    );
}

/// The image decoder child is served before anything in `main` reads the environment or a descriptor: its dispatch
/// is `main`'s first statement, so nothing can be added above it without this test seeing it.
#[test]
fn the_attachment_decoder_is_dispatched_first() {
    // Split literals, so this test's own text is not counted.
    let src = include_str!("main.rs");
    let start = src.find(concat!("fn main() -> glib", "::ExitCode {")).expect("main");
    let body = eitri_core::rust_mask::code_only(&src[start..]);
    let flat: String = body.split_whitespace().collect();
    let first = eitri_core::rust_mask::code_only(concat!(
        "fn main() -> glib::ExitCode {\n",
        "    #[cfg(target_os = \"linux\")]\n",
        "    if std::env::args_os().nth(1).is_some_and(|a| a == eitri_core::attachments::",
        "SUBCOMMAND) {\n",
        "        eitri_core::attachments::decoder_child",
        "_main();\n",
        "    }\n",
    ));
    let first: String = first.split_whitespace().collect();
    assert!(
        flat.starts_with(&first),
        "main must open with the decoder's dispatch: {}",
        &flat[..first.len().min(flat.len())]
    );
    assert_eq!(eitri_core::attachments::SUBCOMMAND, "attachment-decoder");
}

/// Window 2 builds every module but the editor, so nothing in `build_ui` may assume the editor exists: its
/// pane is an `Option` from the editor phase on, and nothing takes the pane out of it by force.
#[test]
fn pane_none_builds_every_module_but_the_editor() {
    use eitri_core::source_scan as scan;
    let (_, body) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
    assert!(
        body.contains("let (pane, editor_host, editor_start_failure_label) = match made_editor"),
        "the editor's pane is taken from the editor phase as an Option"
    );
    assert!(body.contains("Some(pane), Some(host), Some(failure)") && body.contains("None => (None, None, None)"));
    let forced: Vec<String> = scan::word_ends(&body, "pane")
        .into_iter()
        .map(|end| {
            let rest = &body[end..];
            let stop = rest.find([';', '{', '\n']).unwrap_or(rest.len());
            rest[..stop].to_string()
        })
        .filter(|rest| {
            ["unwrap", "expect"]
                .iter()
                .any(|forced| rest.starts_with(&format!(".{forced}(")) || rest.contains(&format!(").{forced}(")))
        })
        .collect();
    assert!(
        forced.is_empty(),
        "the editor's pane forced out of its Option: {forced:?}"
    );
    assert!(
        !body.contains("editor_maker.take().expect("),
        "the editor phase's answer is not expected to hold an editor"
    );
}

/// JavaScriptCore reads its options once, when WebKit first starts it, so the UI process's JIT
/// switch must come before GTK and every `WebView`: inside `main`, ahead of `build_application`
/// (whose `activate` builds the panel). The heap trim is scheduled from `build_ui`, once.
#[test]
fn main_turns_the_ui_jit_off_before_gtk_and_build_ui_schedules_the_trim() {
    // Split literals, so this test's own text is not counted.
    let src = include_str!("main.rs");
    let start = src.find(concat!("fn main() -> glib", "::ExitCode {")).expect("main");
    let end = start
        + src[start..]
            .find(concat!("fn build_", "application"))
            .expect("build_application");
    let body = &src[start..end];
    let jit = concat!("process_memory::ui_process_jsc", "_without_jit()");
    assert_eq!(body.matches(jit).count(), 1, "called exactly once in main");
    assert!(body.find(jit) < body.find(concat!("build_", "application(kind)")));
    let (_, ui) =
        eitri_core::source_scan::function_in(&eitri_core::source_scan::rust_sources(&["shell/src"]), "build_ui");
    assert_eq!(
        ui.matches(concat!("process_memory::trim_heap", "_after_startup()"))
            .count(),
        1
    );
    // The companion window (`eitri panel`, `eitri split`) builds the same panel in a process of its
    // own and does both too.
    let companion = include_str!("companion/mod.rs");
    let jit_at = companion
        .find(concat!("process_memory::ui_process_jsc", "_without_jit()"))
        .expect("the companion turns its JIT off");
    assert!(
        jit_at
            < companion
                .find(concat!("Application::", "builder()"))
                .expect("its application")
    );
    assert_eq!(
        companion
            .matches(concat!("process_memory::trim_heap", "_after_startup()"))
            .count(),
        1
    );
}

/// A shell source as code: comments and literals blanked, its test modules cut.
fn shell_file_code(path: &str) -> String {
    use eitri_core::source_scan as scan;
    let code = scan::code_only(&scan::read_required(path).text);
    scan::without(&code, &scan::modules(&code, "tests"))
}

/// The window's half of the terminal `claude` comes up where the font does: after `init.lua`, so the key has its
/// value, and before the layout, which can start a shell.
#[test]
fn the_key_is_read_after_the_config_step_as_the_terminal_font_is() {
    use eitri_core::source_scan as scan;
    let read = scan::rust_sources(&["shell/src"]);
    let (_, ui) = scan::function_in(&read, "build_ui");
    let at = |what: &str| ui.find(what).unwrap_or_else(|| panic!("`{what}` in build_ui"));
    let key = at("TERMINAL_CLAUDE_IDE_KEY");
    assert!(at("StartStep::Kernel") < at("TERMINAL_FONT_KEY"));
    assert!(at("TERMINAL_FONT_KEY") < key);
    assert!(key < at("IdeWiring::start"));
    assert!(at("IdeWiring::start") < at("make_terminal_pane"));
    assert!(at("IdeWiring::start") < at("StartStep::Layout"));
}

#[test]
fn a_bad_terminal_claude_ide_refuses_startup() {
    use eitri_core::source_scan as scan;
    let read = scan::rust_sources(&["shell/src"]);
    let (_, ui) = scan::function_in_raw(&read, "build_ui");
    let from = ui.find("let claude_ide").expect("the key's read");
    let read_of_it = &ui[from..from + ui[from..].find("IdeWiring::start").expect("what it decides")];
    assert!(
        read_of_it.contains("owns_editor"),
        "window 2 does not decide for itself"
    );
    assert!(read_of_it.contains("parse_terminal_claude_ide"));
    assert!(
        read_of_it.contains("eprintln!(\"eitri: {message}\")"),
        "the message names the key"
    );
    assert!(read_of_it.contains("std::process::exit(1)"));
}

#[test]
fn every_product_pane_is_made_by_the_one_closure() {
    use eitri_core::source_scan as scan;
    let making = concat!("TerminalPane", "::new(");
    let read = scan::rust_sources(&["shell/src"]);
    scan::require_anchors(&read, &["fn build_ui("]);
    let holding: Vec<String> = read
        .iter()
        .filter(|source| !source.path.starts_with("shell/src/grid_probe/"))
        .flat_map(|source| {
            let code = scan::code_only(&source.text);
            let product = scan::without(&code, &scan::modules(&code, "tests"));
            vec![source.path.clone(); product.matches(making).count()]
        })
        .collect();
    assert_eq!(holding, vec!["shell/src/main.rs".to_owned()]);
    let (_, ui) = scan::function_in(&read, "build_ui");
    assert!(closure_of(&ui, "let make_terminal_pane").contains(making));
}

/// The body of the closure handed to the first `call` (ending in its `(`) in `code`.
fn closure_of<'a>(code: &'a str, call: &str) -> &'a str {
    let at = code.find(call).unwrap_or_else(|| panic!("`{call}`"));
    eitri_core::source_scan::braced(code, at + code[at..].find('{').expect("a closure"))
}

/// A termination signal's handler only marks the close and says so, through the write that never blocks, then
/// posts the signal: the controller tears the window down, asking nothing. It closes nothing itself, and is
/// installed once the teardown has what it ends, so a signal at any moment after finds it there. A failed write
/// panics in `println!`, and a panic in this GLib callback aborts the process.
#[test]
fn the_end_signal_posts_and_closes_nothing_itself() {
    let (_, ui) =
        eitri_core::source_scan::function_in(&eitri_core::source_scan::rust_sources(&["shell/src"]), "build_ui");
    assert_eq!(ui.matches(concat!("end_signals::", "install(")).count(), 1);
    let install = ui.find(concat!("end_signals::", "install(")).unwrap();
    let handed = ui
        .find(concat!("*teardown.", "borrow_mut() = Some("))
        .expect("the teardown is handed what it ends");
    assert!(handed < install, "the teardown has what it ends before any signal");
    let handler = closure_of(&ui[install..], "install(");
    let post = handler
        .find(concat!("HostEvent::", "EndSignal { name }"))
        .expect("the handler posts the signal");
    assert!(
        !handler.contains(concat!("close", "(")),
        "the handler closes nothing itself"
    );
    let before = &handler[..post];
    assert!(
        !before.contains(concat!("println", "!(")),
        "the handler's own lines may not panic"
    );
    assert!(
        !before.contains(concat!("writeln", "!(")),
        "the handler's own lines may not block on a full pipe"
    );
    assert_eq!(
        before.matches(concat!("end_signals::", "write_if_writable(")).count(),
        2,
        "both lines go through the write that never blocks"
    );
    let marked = before
        .find(concat!("end_signals::", "mark_closing()"))
        .expect("the close is marked for the watchdog");
    let said = before.rfind(concat!("end_signals::", "write_if_writable(")).unwrap();
    assert!(marked < said, "marked before it is said");
    assert_eq!(
        end_signals::SIGNALS.map(|(signum, _)| signum),
        [libc::SIGTERM, libc::SIGINT, libc::SIGHUP]
    );
}

/// The window's close request decides nothing: a close the host's own `CloseWindow` started proceeds, and every
/// other one is the controller's to answer, which stops it and tears the window down later if it may go.
/// `build_ui` adds no close handler of its own.
#[test]
fn the_close_handler_decides_nothing() {
    let (_, ui) =
        eitri_core::source_scan::function_in(&eitri_core::source_scan::rust_sources(&["shell/src"]), "build_ui");
    assert!(
        !ui.contains(concat!("connect_", "close_request(")),
        "build_ui handles the close itself"
    );
    let host = shell_file_code("shell/src/gtk_host/mod.rs");
    assert_eq!(host.matches(concat!("connect_", "close_request(")).count(), 1);
    let handler: String = closure_of(&host, concat!("connect_", "close_request("))
        .split_whitespace()
        .collect();
    let flag = handler
        .find("host.closing.borrow().contains(&win)")
        .expect("the host's own close goes through");
    let asked = handler
        .find("sink.close_requested(win,CloseBy::User)")
        .expect("every other close is the controller's");
    assert!(flag < asked, "the host's own close never asks");
    for decision in [
        "shutdown(",
        "persist",
        "cleanup(",
        "remove_dir",
        ".close()",
        "Editor",
        "quit",
        "prompt",
    ] {
        assert!(!handler.contains(decision), "the close handler does `{decision}`");
    }
}

/// The editor's quit and the window's close are the controller's (`eitri_core::wm::{quit, close}`): nothing of
/// the flow the shell carried out is left in it, and `build_ui` closes no window itself.
#[test]
fn no_quit_flow_is_left_in_the_shell() {
    use eitri_core::source_scan as scan;
    for source in scan::rust_sources(&["shell/src"]) {
        let code = scan::code_only(&source.text);
        let code = scan::without(&code, &scan::modules(&code, "tests"));
        for name in [
            concat!("Editor", "Quitting"),
            concat!("Quit", "Host"),
            concat!("Shell", "QuitHost"),
            concat!("persist", "_now("),
            concat!("Prompt", "Withdrawn"),
        ] {
            assert!(!code.contains(name), "{} still has `{name}`", source.path);
        }
    }
    let (_, ui) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
    assert!(
        !ui.contains(concat!("window.", "close()")),
        "build_ui closes the window itself"
    );
    assert!(
        !ui.contains(concat!("retire", "_editor")),
        "build_ui retires the editor itself"
    );
}

/// The window's service set wraps `gtk_host_services::Services`: a service the wrapper leaves to the trait's
/// default is answered "not carried out" whatever the inner set does, so every one the trait declares is written
/// out in the wrapper.
#[test]
fn the_windows_service_set_forwards_every_service() {
    use eitri_core::source_scan as scan;
    let names = |code: &str| -> Vec<String> {
        scan::word_ends(code, "fn")
            .into_iter()
            .filter_map(|end| {
                let name: String = code[end..]
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                (!name.is_empty()).then_some(name)
            })
            .collect()
    };
    let block = |path: &str, head: &str| -> String {
        let code = scan::code_only(&scan::read_required(path).text);
        let code = code.split_whitespace().collect::<Vec<_>>().join(" ");
        let at = code.find(head).unwrap_or_else(|| panic!("{path} has no `{head}`"));
        let open = at + code[at..].find('{').expect("a body after it");
        scan::braced(&code, open).to_string()
    };
    let declared = names(&block("shell/src/gtk_host/mod.rs", "trait ServiceSet"));
    let written = names(&block("shell/src/window_services.rs", "ServiceSet for PanelServices"));
    assert!(
        declared.iter().any(|name| name == "restart_editor") && declared.len() > 5,
        "the trait was not read: {declared:?}"
    );
    let missing: Vec<&String> = declared.iter().filter(|name| !written.contains(name)).collect();
    assert!(
        missing.is_empty(),
        "PanelServices leaves {missing:?} to the trait's default"
    );
}

/// A terminal's shell ending is told to the controller, which judges it (the terminal's kill, or the window's
/// close as the last module on screen): the hook kills and closes nothing itself, and answers `false` on every
/// path, since the kill's own hang-up closes the pane.
#[test]
fn the_terminal_exit_hook_posts_and_answers_false() {
    let (_, ui) =
        eitri_core::source_scan::function_in(&eitri_core::source_scan::rust_sources(&["shell/src"]), "build_ui");
    assert_eq!(ui.matches(concat!("pane.on_", "shell_exit(")).count(), 1);
    let hook: String = closure_of(&ui, concat!("pane.on_", "shell_exit("))
        .split_whitespace()
        .collect();
    assert!(
        hook.contains("HostEvent::Terminal(TermEvent::Exited{"),
        "the end is posted"
    );
    for call in ["close(", "kill_confirmed(", "can_kill("] {
        assert!(!hook.contains(call), "the hook does `{call}`");
    }
    assert!(!hook.contains("true"), "the hook never asks its pane to close");
    let returns = hook.matches("return").count();
    assert_eq!(
        hook.matches("returnfalse;").count(),
        returns,
        "every early return answers false"
    );
    assert!(hook.ends_with("false}"), "and so does its end");
}
/// The overlay's "Anywhere in the window" rows come from `eitri_core::keymap::root`, and its
/// `Ctrl+j` row exists only because of the terminal: every chord the terminal gives up to
/// Eitri (the controller's `nav_direction` over the key as GTK delivers it, asked about each letter) must be
/// named there.
#[test]
fn every_chord_the_terminal_gives_up_is_in_the_root_help() {
    let documented: std::collections::HashSet<String> = eitri_core::keymap::root::help_rows()
        .into_iter()
        .flat_map(|row| row.keys.split(" / ").map(str::to_string).collect::<Vec<_>>())
        .collect();
    let mut taken = 0;
    for letter in 'a'..='z' {
        let key = gtk4::gdk::Key::from_name(letter.to_string()).expect("a letter keyval");
        let press = gtk_host::keys::key_press(key, 0, gtk4::gdk::ModifierType::CONTROL_MASK);
        if eitri_core::wm::keys::nav_direction(&press).is_some() {
            taken += 1;
            let name = format!("Ctrl+{letter}");
            assert!(
                documented.contains(&name),
                "the terminal gives up {name}, but no root help row names it"
            );
        }
    }
    assert_eq!(taken, 4, "Ctrl+h/j/k/l, and nothing else");
}

/// Every refusal is the controller's, drawn from its chrome model as a flash and a toast: `build_ui` keeps no way
/// of refusing a key itself, so no refusal there can blink without its words or differ from the controller's.
#[test]
fn build_ui_refuses_nothing_itself() {
    use eitri_core::source_scan as scan;
    let (_, code) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
    assert!(
        !code.contains(concat!("Shell", "Refused")),
        "build_ui posts a refusal of its own"
    );
    let flashes = code.matches(concat!("flash", "(")).count();
    let drawn = code.matches(concat!("show_flash", "(")).count();
    assert_eq!(flashes, drawn, "the only flash in build_ui is the chrome model's own");
}

/// ssh runs `eitri host known-hosts-probe` on every connection it makes for a remote project: `main` dispatches it
/// first, before the launcher's environment, the display, any subcommand, any thread or GTK, and it records the key
/// it is given and prints nothing. Any other command line is not `host`'s.
#[test]
fn known_hosts_probe_is_dispatched_before_gtk() {
    // Split literals, so this test's own text is not counted.
    let src = include_str!("main.rs");
    let start = src.find(concat!("fn main() -> glib", "::ExitCode {")).expect("main");
    let end = start
        + src[start..]
            .find(concat!("fn build_", "application"))
            .expect("build_application");
    let body = &src[start..end];
    let at = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("{needle} in main"));
    let order = [
        at(concat!("host_command::", "run(")),
        at(concat!("launch_env::", "take(")),
        at(concat!("display_env::", "resolve_wayland_display(")),
        at(concat!("start::", "SUBCOMMAND")),
        at(concat!("build_", "application(kind)")),
    ];
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");

    let scratch = eitri_core::plugin::test_support::Scratch::new("kh-probe");
    let record = scratch.0.join("kh-0123456789abcdef");
    let args = [
        "host",
        "known-hosts-probe",
        record.to_str().unwrap(),
        "example-host",
        "ssh-ed25519",
        "SHA256:abc",
    ]
    .map(OsString::from);
    assert_eq!(host_command::run(args), Some(glib::ExitCode::SUCCESS));
    let recorded: Vec<String> = serde_json::from_str(&std::fs::read_to_string(&record).unwrap()).unwrap();
    assert_eq!(recorded, ["example-host", "ssh-ed25519", "SHA256:abc"]);
    assert_eq!(
        host_command::run(["host", "known-hosts-probe"].map(OsString::from)),
        Some(glib::ExitCode::from(1))
    );
    assert_eq!(host_command::run(["open"].map(OsString::from)), None);
    assert_eq!(host_command::run(Vec::<OsString>::new()), None);
}
