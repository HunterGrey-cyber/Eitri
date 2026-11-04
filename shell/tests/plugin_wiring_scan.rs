//! Where the window wires its plugin pages, read from `build_ui` as code: installed before the Lua panels are
//! read (so a plugin is placed, keyed and killed as one), given the window's one editor handle and its layout,
//! stopped by a kill of its module, and shut down when the window closes; told when its module holds the keys
//! and what the editor's `timeoutlen` is; its keys read for `prefix ?` before the chat takes them; and every
//! host's controllers, the Super filter and the edge rule for `Ctrl+h/j/k/l` among them, installed by `wire_host`
//! alone. Also, from `plugin_page.rs`: the kit's reserved path is answered before the plugin's site is looked up,
//! and a page build applies and logs its policy and proxy plan, and every answer of the scheme carries `nosniff`.
//! And the pair's wiring: the startup host check asserts the hosts `hosted_at_start` says the controller makes, the
//! link's end loads again every page that comes back, window 2's path clicks are checked by the panel's own rule,
//! and window 2's follow lines are carried out by the follow window over the controller.

use eitri_core::source_scan as scan;

fn build_ui_code() -> String {
    let (_, code) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
    code.split_whitespace().collect()
}

/// A shell file as code (comments and literals blanked, its tests cut), whitespace removed.
fn file_code(path: &str) -> String {
    let source = scan::read_required(path);
    let code = scan::code_only(&source.text);
    scan::without(&code, &scan::modules(&code, "tests"))
        .split_whitespace()
        .collect()
}

/// What each `grid.connect_changed_by_controller(` in `code` (whitespace removed) is handed: the text up to its
/// closing `)`. The hooks run after a layout change, so none may hold what moved.
fn change_hooks(code: &str) -> Vec<&str> {
    call_texts(code, "grid.connect_changed_by_controller(")
}

/// Each call of `call` (ending in its `(`) in `code` (whitespace removed): its parenthesised arguments.
fn call_texts<'a>(code: &'a str, call: &str) -> Vec<&'a str> {
    code.match_indices(call)
        .map(|(at, call)| {
            let open = at + call.len() - 1;
            let mut depth = 0usize;
            for (i, c) in code[open..].char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            return &code[open..=open + i];
                        }
                    }
                    _ => {}
                }
            }
            panic!("unbalanced parentheses from byte {open}");
        })
        .collect()
}

/// The match arm that starts at `pattern` in `code`, up to the next arm of the same enum (`next`) or the end.
fn arm<'a>(code: &'a str, pattern: &str, next: &str) -> &'a str {
    let at = code.find(pattern).unwrap_or_else(|| panic!("`{pattern}`"));
    let rest = &code[at + pattern.len()..];
    let end = rest.find(next).map_or(code.len(), |end| at + pattern.len() + end);
    &code[at..end]
}

/// The block a `let` binds in `code` (whitespace removed), from the first `{` after its anchor.
fn let_block<'a>(code: &'a str, anchor: &str) -> &'a str {
    let at = code.find(anchor).unwrap_or_else(|| panic!("`{anchor}`"));
    scan::braced(code, at + code[at..].find('{').unwrap())
}

#[test]
fn apply_layout_requests_decides_nothing() {
    let code = build_ui_code();
    let body = let_block(&code, "letapply_layout_requests:Rc<dynFn()>=");
    assert!(body.contains("letrequests=lua_engine.layout.borrow_mut().take_requests();"));
    assert!(body.contains("if!requests.is_empty(){driver.post(HostEvent::Lua("));
    assert!(body.contains("LuaEvent::LayoutRequests{win:WinId::MAIN,requests,"));
    assert_eq!(call_texts(body, "driver.post(").len(), 1);
    for decision in ["LayoutRequest::", "lua_request(", "::moved(", "match", "forrequest"] {
        assert!(!body.contains(decision), "the drain still decides with {decision}");
    }
}

/// What the innermost block around `at` says before `at`: the bindings a closure made there captures, wherever among
/// them they stand.
fn enclosing_lets(code: &str, at: usize) -> &str {
    let open = code[..at].rfind('{').expect("a block around the call");
    &code[open..at]
}

#[test]
fn build_ui_starts_every_module_from_the_start_theme() {
    let code = build_ui_code();
    for fallback in ["ThemeTokens::fallback(", "PageTheme::fallback("] {
        assert!(!code.contains(fallback), "build_ui still starts from {fallback}");
    }
    assert_eq!(code.matches("StartTheme::read(").count(), 1);
    assert_eq!(code.matches("ThemeCss::install(").count(), 1);
    let read = code.find("StartTheme::read(").unwrap();
    let install = code.find("ThemeCss::install(").unwrap();
    let spawn = code.find("EditorSpawn{").expect("the editor spawn");
    let window = code.find("StartStep::Window").expect("the window phase");
    assert!(read < install && install < spawn && install < window);
    let spawn = scan::braced(&code, spawn + code[spawn..].find('{').unwrap());
    assert!(spawn.contains("background:start_theme.editor_background()"));
    let panels = call_texts(&code, "make_panel_handle(");
    assert_eq!(panels.len(), 1);
    assert!(read < code.find("make_panel_handle(").unwrap());
    assert!(panels[0].contains("start_theme"));
}

#[test]
fn only_a_window_with_its_own_editor_and_without_clean_remembers_the_theme() {
    let code = build_ui_code();
    assert_eq!(code.matches("ThemeWriter::new(").count(), 1);
    let writer = code.find("ThemeWriter::new(").unwrap();
    let gate = &code[writer.saturating_sub(200)..writer];
    assert!(gate.contains("owns_editor") && gate.contains("!want_clean"));
    let at = code.find("listen_theme(").expect("the theme listener");
    let listener = scan::braced(&code, at + code[at..].find('{').unwrap());
    assert_eq!(listener.matches(".remember(").count(), 1);
    let remembers: usize = scan::rust_sources(&["shell/src"])
        .iter()
        .map(|source| {
            let code = scan::code_only(&source.text);
            let code = scan::without(&code, &scan::modules(&code, "tests"));
            let code: String = code.split_whitespace().collect();
            code.matches(".remember(").count()
        })
        .sum();
    assert_eq!(remembers, 1);
}

#[test]
fn plugins_are_installed_before_the_lua_panels_are_read() {
    let code = build_ui_code();
    let install = code
        .find("plugin_panels::PluginPanels::install(&lua_engine,&keymap,&PageTheme::from_tokens(&start_theme.tokens),)")
        .expect("installed");
    let panels = code.find("letlua_panels:").expect("the Lua panels are read");
    assert!(install < panels);
}

#[test]
fn the_theme_listener_feeds_every_plugin_page() {
    let code = build_ui_code();
    // One consumer of nvim's theme, whether this window's own feed or the link from the window with the editor
    // reports it.
    let body = let_block(&code, "letapply_theme:");
    let terminal = body
        .find("pane.set_colors(colors);")
        .expect("every terminal follows the theme");
    // The freshly derived tokens: a change of the panel's size alone leaves the page set equal and sends nothing.
    let pages = body
        .find("plugin_panels.set_theme(&tokens);")
        .expect("every plugin page follows the theme");
    assert!(terminal < pages);
    let at = code.find("feed_pump.listen_theme(feed,").expect("the theme listener");
    let listener = scan::braced(&code, at + code[at..].find('{').unwrap());
    assert!(
        enclosing_lets(&code, at).contains("letapply=apply_theme.clone();"),
        "the feed's listener takes the shared consumer"
    );
    assert!(
        listener.contains("apply(payload);"),
        "the feed's theme reaches the consumer: {listener}"
    );
    assert!(
        code.contains("two.on_theme(apply_theme.clone());"),
        "the theme told over the link reaches the same consumer"
    );
}

/// The pages' text size is the editor's own: told once before any page is built, again when `guifont` moves and
/// when the desktop's text scaling does (both change the editor's size in the pages' CSS px), and never in a page
/// window beside a person's own nvim, whose font Eitri does not know.
#[test]
fn the_pages_are_told_the_editors_text_size_except_beside_a_persons_nvim() {
    let code = build_ui_code();
    assert!(
        code.contains("letpages_follow_editor_text=!pair.is_beside();"),
        "only a window with an editor font of its own"
    );
    let first = code
        .find(
            "if pages_follow_editor_text{plugin_panels.set_text_size(Some(editor_font.get().css_px()));}"
                .replace(' ', "")
                .as_str(),
        )
        .expect("the start size reaches the pages");
    let built = code.find("plugin_panels.build_page(").expect("a page is built");
    assert!(first < built, "before any page is built");
    let body = let_block(&code, "letapply_theme:");
    let size = body
        .find("plugin_panels.store_text_size(Some(font.css_px()));")
        .expect("a guifont change reaches the pages, stored for the theme call that follows");
    let theme = body.find("plugin_panels.set_theme(&tokens);").unwrap();
    assert!(
        size < theme,
        "the size first: the theme that follows already carries it"
    );
    assert!(
        body.contains("ifmoved.is_some()&&pages_follow_editor_text{"),
        "only a change is sent, and only to a window that follows the editor"
    );
    let panel_follows = code
        .find("letpanel_follows=config.panel_font_follows_editor;")
        .expect("the text-scaling listener's setup");
    let at = panel_follows
        + code[panel_follows..]
            .find("connect_gtk_xft_dpi_notify(")
            .expect("the text-scaling listener");
    let listener = scan::braced(&code, at + code[at..].find('{').unwrap());
    assert!(
        listener.contains("plugin_panels.set_text_size(Some(font.css_px()));"),
        "a text-scaling change moves the editor's size in CSS px: {listener}"
    );
}

#[test]
fn a_committed_document_gets_the_theme_and_a_host_its_module_key() {
    let source = scan::read_required("shell/src/plugin_panels.rs");
    let code = scan::code_only(&source.text);
    let code: String = scan::without(&code, &scan::modules(&code, "tests"))
        .split_whitespace()
        .collect();
    let at = code.find("on_committed:").expect("the commit hook");
    let hook = scan::braced(&code, at + code[at..].find('{').unwrap());
    assert!(hook.contains("pump.committed()"), "{hook}");
    let committed = code.find("fncommitted(&self)").expect("the pump's commit handler");
    let handler = scan::braced(&code, committed + code[committed..].find('{').unwrap());
    assert!(handler.contains("self.host.page_committed()"), "{handler}");
    let new = code.find("PluginHost::new(").expect("the host is made");
    assert!(
        code[new..].contains("host.set_theme(theme);"),
        "the host starts with the window's theme"
    );
    assert!(
        code[new..].contains("host.set_module_key(key.as_deref());"),
        "the host knows its module key"
    );
}

#[test]
fn plugins_share_the_windows_editor_handle_and_get_the_layout() {
    let code = build_ui_code();
    assert!(code.contains("panel_hooks.set_editor_rpc=Some(editor_rpc.clone());"));
    assert!(code.contains("plugin_panels.set_editor(editor_rpc);"));
    assert_eq!(
        code.matches("EmbeddedEditorRpc::new(").count(),
        1,
        "one editor handle for the window"
    );
    assert!(code.contains("plugin_panels.set_layout("));
}

#[test]
fn a_killed_plugin_module_stops_and_a_closing_window_shuts_plugins_down() {
    // The kill is the controller's: off screen first, then its lifecycle hears the kill.
    let verbs = file_code("core/src/wm/verbs/modules.rs");
    assert!(verbs.contains("ModuleKind::LuaWebview=>leave_killed(w,cx,win,id,reopen,batch),"));
    let (_, leave) = scan::function_in(&scan::rust_sources(&["core/src/wm/verbs"]), "leave_killed");
    let leave: String = leave.split_whitespace().collect();
    let killed = leave.find("layout::kill(").expect("off screen first");
    let told = leave
        .find("Ended::Killed{reopen}")
        .expect("the lifecycle hears the kill");
    assert!(killed < told, "told once the module is off screen");
    // What the kill ends is the controller's batch: the stop, then the web process's end.
    let services = file_code("shell/src/gtk_host/services.rs");
    assert!(arm(&services, "PluginOp::Terminate{", "PluginOp::").contains("self.plugin_panels.stop(module);"));
    let surfaces = file_code("shell/src/gtk_host/surfaces.rs");
    assert!(surfaces.contains("_if*why==DropWhy::Killed=>DropRoute::EndWebProcess,"));
    assert!(
        arm(&surfaces, "DropRoute::EndWebProcess=>", "DropRoute::").contains("self.surfaces.end_web_process(module)")
    );
    let raw = scan::code_only(&scan::read_required("shell/src/gtk_host/surfaces.rs").text);
    let end = scan::functions(&raw, "end_web_process");
    assert_eq!(end.len(), 1, "one end of a killed page's web process");
    assert!(end[0].contains("terminate_web_process()"));
    // The window's teardown ends every plugin task, with the application held for them; the controller's teardown
    // batch asks for it.
    assert!(file_code("core/src/wm/close.rs").contains("HostOp::Plugin(PluginOp::ShutdownAll{win}),"));
    assert!(
        arm(&services, "PluginOp::ShutdownAll{", "PluginOp::").contains("self.plugin_panels.shutdown(&self.app()?);")
    );
}

#[test]
fn a_plugin_hears_its_focus_and_the_editors_timeoutlen() {
    let code = build_ui_code();
    // One consumer of nvim's keys, whether this window's own feed or the link from the window with the editor
    // reports them.
    let consumer = let_block(&code, "letapply_nvim_keys:");
    assert!(consumer.contains("plugin_panels.set_timeoutlen(report.timeoutlen);"));
    let listen = code.find("feed_pump.listen_keys(feed,").expect("the keys feed");
    let listener = scan::braced(&code, listen + code[listen..].find('{').unwrap());
    assert!(
        enclosing_lets(&code, listen).contains("letapply=apply_nvim_keys.clone();"),
        "the feed's listener takes the shared consumer"
    );
    assert!(
        listener.contains("apply(report);"),
        "the feed's keys reach the consumer: {listener}"
    );
    assert!(
        code.contains("two.on_keys(apply_nvim_keys.clone());"),
        "the keys told over the link reach the same consumer"
    );
}

#[test]
fn no_panel_verb_is_left_in_build_ui() {
    let code = build_ui_code();
    for gone in [
        "run_tab_action",
        "tab_verb_view",
        "tab_break(",
        "reveal_chat(",
        "keymap_opened(",
        "keymap_superseded(",
    ] {
        assert!(!code.contains(gone), "build_ui still has {gone}");
    }
}

/// Every action is the controller's, so nothing hands one back to the shell: the contract has no shell verb, the host
/// runs none, and `build_ui` installs no arm for one. The window's mode, the last thing such an arm carried out, is
/// the host's `SetMode`, toggled through the function `build_ui` gives it.
#[test]
fn the_shell_verb_path_is_gone() {
    let code = build_ui_code();
    for gone in ["letrun_verb", "set_shell_verb_arm(", "ShellCall"] {
        assert!(!code.contains(gone), "build_ui still has `{gone}`");
    }
    let host = file_code("shell/src/gtk_host/mod.rs");
    for gone in ["run_shell_verb", "shell_verb_arm", "ShellCall"] {
        assert!(!host.contains(gone), "the GTK host still has `{gone}`");
    }
    let contract = file_code("core/src/wm/host.rs");
    for gone in ["ShellVerb", "ShellCall", "run_shell_verb", "ShellRefused", "shell_verb"] {
        assert!(!contract.contains(gone), "the contract still has `{gone}`");
    }
    assert!(
        host.contains("HostOp::SetMode{win,..}=>self.set_mode(op,*win),"),
        "the host carries out the window's mode"
    );
    let toggle = call_texts(&code, "wm_host.set_mode_toggle(");
    assert_eq!(toggle.len(), 1, "one mode toggle");
    assert!(toggle[0].contains("window_modes.toggle_immersive()"), "{}", toggle[0]);
}

/// The shell reports text-size inputs and carries out writes; the controller owns the model.
#[test]
fn no_text_size_decision_is_left_in_the_shell() {
    let code = file_code("shell/src/text_size.rs").replace("HostEvent::TextSize", "HostEvent::");
    for gone in ["TextSize", "apply_both", "apply_editor", "apply_panel", "apply_page"] {
        let found = if gone == "TextSize" {
            code.split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|word| word == gone)
        } else {
            code.contains(gone)
        };
        assert!(!found, "text_size.rs still holds {gone}");
    }
}

#[test]
fn no_panel_hook_decides_in_build_ui() {
    let code = build_ui_code();
    for hook in [
        "on_show_view",
        "on_view_empty",
        "on_review_open",
        "on_editor_done",
        "on_edit_selection",
        "on_choose_project",
        "on_editor_request",
        "on_tab_verb",
    ] {
        let calls = call_texts(&code, &format!("panel_hooks.{hook}=Some("));
        assert_eq!(calls.len(), 1, "{hook}");
        let body = calls[0];
        for forbidden in PANEL_DECISIONS {
            assert!(!body.contains(forbidden), "{hook}: {forbidden}");
        }
        if hook == "on_editor_request" {
            assert!(
                body.contains("returntwo.request(request,&||activation_token(&asking));"),
                "a linked request uses its transport: {body}"
            );
            assert!(
                body.ends_with("drive_editor(Some(request))}))"),
                "a local request returns the editor drive's outcome: {body}"
            );
        } else if hook == "on_choose_project" {
            assert!(
                body.contains("driver.run(WinId::MAIN,Action::ProjectChoose,Origin::Chip)"),
                "the project row runs the window's chooser action"
            );
        } else {
            assert!(body.contains("HostEvent::Chat(ChatEvent::"), "{hook} posts its event");
        }
    }
    let drive = let_block(&code, "letdrive_editor:Rc<dynFn(");
    assert!(editor_drive_returns_outcome(drive), "{drive}");
}

const PANEL_DECISIONS: [&str; 5] = [
    "give_keys(",
    "show_on_screen(",
    "focus_and_arrive(",
    "drop_chat_view(",
    "send_keys(",
];

fn editor_drive_returns_outcome(body: &str) -> bool {
    body.ends_with(
        "driver.editor_request(WinId::MAIN,request.map(|request|request.input_keys()).unwrap_or_default(),)})}",
    ) && !body.contains("driver.post(")
        && !PANEL_DECISIONS.iter().any(|forbidden| body.contains(forbidden))
}

#[test]
fn an_editor_drive_that_discards_the_outcome_is_rejected() {
    let drive = "{ Rc::new(move |request| {
        driver.editor_request(WinId::MAIN,
            request.map(|request| request.input_keys()).unwrap_or_default(),)
    }) }";
    let code: String = scan::code_only(drive).split_whitespace().collect();
    assert!(editor_drive_returns_outcome(&code));
    let discarded = code.replace(")})}", ");Ok(())})}");
    assert!(!editor_drive_returns_outcome(&discarded));
    let posted = code.replace("driver.editor_request(", "driver.post(");
    assert!(!editor_drive_returns_outcome(&posted));
    for forbidden in PANEL_DECISIONS {
        let deciding = code.replace(
            "driver.editor_request(",
            &format!("{forbidden});driver.editor_request("),
        );
        assert!(!editor_drive_returns_outcome(&deciding), "{forbidden}");
    }
}

/// A page shuts its keys sheet, its `:` line and a tab's details whenever it is told where the keys are, and when
/// the batch that opens one moved the keys into that chat, the page is told only after the batch: so all three open
/// from an idle, never inside the batch, or they close the moment they open.
#[test]
fn the_keys_sheet_and_the_command_line_open_after_the_page_hears_it_has_the_keys() {
    let code = file_code("shell/src/window_services.rs");
    let services = code
        .find("implgtk_host::ServiceSetforPanelServices{")
        .expect("the window's panel services");
    let chat = services + code[services..].find("fnchat(&self,").expect("its chat ops");
    let body = scan::braced(&code, chat + code[chat..].find('{').unwrap());
    for (pattern, next, call) in [
        ("ChatOp::OpenKeymap{", "ChatOp::CommandLine{", "agent.open_keymap("),
        ("ChatOp::CommandLine{", "ChatOp::Tab{", "agent.open_command_line("),
        (
            "ChatOp::Tab{call:eitri_core::wm::verbs::tabs::TabCall::Info(view),",
            "ChatOp::Reload{",
            "agent.open_detail(",
        ),
    ] {
        let arm = arm(body, pattern, next);
        let idle = arm
            .find("glib::idle_add_local_once(")
            .unwrap_or_else(|| panic!("{pattern} opens from an idle: {arm}"));
        let opened = arm
            .find(call)
            .unwrap_or_else(|| panic!("{pattern} calls {call}: {arm}"));
        assert!(opened > idle, "{pattern} opens inside the idle: {arm}");
        assert_eq!(arm.matches(call).count(), 1, "{pattern} opens once: {arm}");
    }
}

/// The sheet's close is the controller's: the hook posts the opening it closes, and whether its own keys closed it.
#[test]
fn the_keymap_closed_hook_posts_its_opening() {
    let code = build_ui_code();
    let hook = code
        .find("panel_hooks.on_keymap_closed=Some(Box::new({")
        .expect("the close hook is installed");
    let body = scan::braced(&code, hook + "panel_hooks.on_keymap_closed=Some(Box::new(".len());
    assert!(body.contains("move|opening:u64,return_keys:bool|"), "{body}");
    assert!(
        body.contains("ChatEvent::KeymapClosed{"),
        "the hook posts the close: {body}"
    );
    assert!(body.contains("opening,"), "with the opening it closes: {body}");
    assert!(
        body.contains("forced:!return_keys,"),
        "and whether its own keys closed it: {body}"
    );
    for decided in ["keys_return", "give_keys(", "hide_module(", "restore_zoom("] {
        assert!(
            !body.contains(decided),
            "the hook decides nothing itself: `{decided}` in {body}"
        );
    }
}

/// The window's construction has no cell filled in later to break a cycle: the prompt, the text size and the
/// prefix exist before what asks them. The one cell that stays is the terminal claude's wiring: it exists only
/// once `init.lua` has said whether the key is on, which is after the editor's context hook is installed, and
/// that hook reads it with `get`.
#[test]
fn no_late_bound_cell_is_left_in_build_ui() {
    let code = build_ui_code().replace("std::cell::OnceCell<Rc<ide_wiring::IdeWiring>>", "");
    for cell in ["prefix_cell", "kill_prompt", "text_size_slot", "OnceCell<"] {
        assert!(!code.contains(cell), "`{cell}` in build_ui");
    }
}

/// Focus and both key stages are the controller's: `build_ui` installs no focus tracker, no prefix and no key
/// capture that navigates, and nvim's edge letter is posted to the controller as a navigation.
#[test]
fn no_focus_or_key_decision_is_left_in_build_ui() {
    let code = build_ui_code();
    for decision in [
        "pane_focus::install(",
        "prefix::install(",
        "grid.navigate(",
        "editor_start_failure::navigation(",
        "move_focus(",
    ] {
        assert!(!code.contains(decision), "`{decision}` in build_ui");
    }
    let listener = code
        .find("feed_pump.listen_pane_switch(ps,")
        .expect("the editor's edge letters are listened to");
    let body = &code[listener..];
    let letter = body
        .find("pane_switch::PaneMessage::Direction(letter)=>")
        .expect("a direction letter");
    let quit = body
        .find("pane_switch::PaneMessage::QuitCancelled(g)=>")
        .expect("a cancelled quit");
    assert!(
        body[letter..quit].contains("EditorEvent::Navigate{"),
        "a direction letter posts the editor's navigation"
    );
    assert!(
        body[quit..].contains("EditorEvent::QuitCancelled{"),
        "a cancelled quit is posted to the controller's quit flow"
    );
    let layers = code.matches("gtk_host::keys::install_window_layers(").count();
    assert_eq!(layers, 1, "the window's key layers are installed by one call");
}

/// The non-test code of every shell source (comments and literals blanked), with each file's path.
fn shell_code() -> Vec<(String, String)> {
    scan::rust_sources(&["shell/src"])
        .into_iter()
        .map(|source| {
            let code = scan::code_only(&source.text);
            let code = scan::without(&code, &scan::modules(&code, "tests"));
            (source.path, code)
        })
        .collect()
}

/// How many times `code` calls each controller installer a host gets.
fn installer_calls(code: &str) -> [(&'static str, usize); 2] {
    let plain: String = code.split_whitespace().collect();
    [
        ("module_key", scan::calls(code, "module_key")),
        ("wheel_zoom::install", plain.matches("wheel_zoom::install(").count()),
    ]
}

#[test]
fn every_host_gets_its_controllers_from_wire_host() {
    let files = shell_code();
    let (_, wire) = files
        .iter()
        .find(|(path, _)| path.ends_with("gtk_host/wire.rs"))
        .expect("the wiring's file");
    let wire_host = scan::functions(wire, "wire_host");
    assert_eq!(wire_host.len(), 1, "one wire_host");

    // The installers each host once had of its own are gone: the module stage holds their rules.
    for (path, code) in &files {
        let plain: String = code.split_whitespace().collect();
        for gone in ["install_module_nav(", "panel_super::install(", "terminal::navigation("] {
            assert!(!plain.contains(gone), "`{gone}` in {path}");
        }
    }

    // Every controller a host gets is installed from inside wire_host and nowhere else.
    let inside = installer_calls(wire_host[0]);
    for (i, (name, count)) in inside.iter().enumerate() {
        assert!(*count > 0, "wire_host installs `{name}`");
        let everywhere: usize = files.iter().map(|(_, code)| installer_calls(code)[i].1).sum();
        assert_eq!(everywhere, *count, "`{name}` is called outside wire_host");
    }

    // Each host-creating site wires its host once: the startup loop over every host, the companion's window, the
    // controller's surface maker (every module made at run time). Nothing else calls wire_host.
    let (_, build_ui) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
    assert_eq!(scan::calls(&build_ui, "wire_host"), 1, "the startup loop alone");
    let (_, companion) = files
        .iter()
        .find(|(path, _)| path.ends_with("companion/window.rs"))
        .expect("the companion's window");
    assert_eq!(
        scan::calls(companion, "wire_host"),
        1,
        "the companion wires its one host once"
    );
    // The controller's `CreateSurface` wires the host it makes: a terminal's, a chat view's.
    let (_, surfaces) = files
        .iter()
        .find(|(path, _)| path.ends_with("gtk_host/surfaces.rs"))
        .expect("the surface constructors");
    assert_eq!(
        scan::calls(surfaces, "wire_host"),
        2,
        "the surface maker wires a terminal and a chat view"
    );
    let total: usize = files.iter().map(|(_, code)| scan::calls(code, "wire_host")).sum();
    assert_eq!(total, 4, "no other path wires a host");

    // A plugin page built on its first show fills a slot whose host was wired at startup.
    let (_, build_page) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_page");
    assert!(
        installer_calls(&build_page).iter().all(|(_, count)| *count == 0),
        "build_page installs a controller"
    );
    assert_eq!(scan::calls(&build_page, "wire_host"), 0, "build_page wires a host");
}

#[test]
fn the_kit_path_is_answered_before_any_site_file_is_looked_up() {
    let source = scan::read_required("shell/src/plugin_page.rs");
    let code = scan::code_only(&source.text);
    let serve = scan::functions(&code, "serve");
    assert_eq!(serve.len(), 1, "one scheme handler");
    let serve: String = serve[0].split_whitespace().collect();
    let routed = serve
        .find("route(id,&uri,")
        .expect("serve asks route which kind of path it is");
    let kit = serve.find("Route::Kit=>").expect("the kit is answered in serve");
    let doc = serve
        .find("Route::Doc{dir,rel}=>")
        .expect("the document route is answered in serve");
    let site = serve.find("page_policy::site_file(").expect("the site lookup");
    // A plugin's own `.eitri/keys.js`, or a site file under `.eitri/doc/`, would stand in for what the host serves
    // if the site were looked up first.
    assert!(routed < kit && kit < site && doc < site);
    // The document's project is resolved on disk, which must never happen on the GTK thread.
    let worker = serve
        .find("gio::spawn_blocking(move||doc_route::doc_file(&dir,&rel))")
        .expect("the document file is checked on a worker");
    assert!(doc < worker && worker < site);

    let route = scan::functions(&code, "route");
    assert_eq!(route.len(), 1, "one router");
    let route: String = route[0].split_whitespace().collect();
    let kit = route
        .find("ifpage_policy::kit_request(id,uri){returnRoute::Kit;}")
        .expect("the router checks the kit first");
    let doc = route
        .find("doc_route::doc_request(")
        .expect("and then the document route");
    assert!(kit < doc);
}

/// The source text (literals kept) of the `code` slice `part`, whitespace removed.
fn source_of(text: &str, code: &str, part: &str) -> String {
    let start = part.as_ptr() as usize - code.as_ptr() as usize;
    text[start..start + part.len()].split_whitespace().collect()
}

#[test]
fn a_page_build_logs_its_policy_and_proxy_and_uses_the_plan() {
    let source = scan::read_required("shell/src/plugin_page.rs");
    let code = scan::code_only(&source.text);
    let build = scan::functions(&code, "build");
    assert_eq!(build.len(), 1, "one page build");
    let plain: String = build[0].split_whitespace().collect();
    assert!(
        plain.contains("page_policy::proxy_plan(&spec.origins,spec.remote_images)"),
        "{plain}"
    );
    assert!(
        plain.contains("page_policy::csp_floor(&spec.origins,spec.remote_images)"),
        "{plain}"
    );
    // The log lines carry literals, which `code_only` blanks: they are read from the source text of the same
    // function.
    let text = source_of(&source.text, &code, build[0]);
    assert!(text.contains("println!(\"[plugin{}]csp:{}\",spec.id,csp)"), "{text}");
    assert!(
        text.contains("println!(\"[plugin{}]proxy:{}\",spec.id,plan.summary())"),
        "{text}"
    );

    // The direct route for `https` exists only where the plan asks for it.
    let ends = scan::word_ends(&code, "add_proxy_for_scheme");
    assert_eq!(ends.len(), 1, "one per-scheme proxy entry in the file");
    let call = ends[0] - "add_proxy_for_scheme".len();
    let guard = scan::word_ends(&code, "https_direct")
        .into_iter()
        .find(|end| {
            let before = code[..end - "https_direct".len()].trim_end();
            before.ends_with("plan.") && before[..before.len() - "plan.".len()].trim_end().ends_with("if")
        })
        .expect("an `if plan.https_direct` block");
    let open = guard + code[guard..].find('{').unwrap();
    let block = scan::braced(&code, open);
    assert!(
        open < call && call < open + block.len(),
        "the per-scheme entry sits inside `if plan.https_direct`"
    );
    let args_end = call + code[call..].find(')').unwrap() + 1;
    let at = &code[call..args_end];
    assert_eq!(
        source_of(&source.text, &code, at),
        "add_proxy_for_scheme(\"https\",\"direct://\")"
    );
}

#[test]
fn every_answer_of_the_scheme_carries_nosniff() {
    let source = scan::read_required("shell/src/plugin_page.rs");
    let code = scan::code_only(&source.text);
    let code = scan::without(&code, &scan::modules(&code, "tests"));
    // A file is answered only through `respond`, which sets the type and `nosniff`; errors carry no body.
    assert_eq!(
        code.matches("request.finish(").count(),
        0,
        "an answer that bypasses respond"
    );
    assert_eq!(
        code.matches(".finish_with_response(").count(),
        1,
        "one place answers with a body"
    );
    let respond = scan::functions(&code, "respond");
    assert_eq!(respond.len(), 1, "one respond");
    assert!(
        respond[0].contains(".finish_with_response("),
        "the body is sent from respond"
    );
    let text: String = source.text.split_whitespace().collect();
    assert!(
        text.contains("headers.append(\"X-Content-Type-Options\",\"nosniff\");response.set_http_headers(headers);"),
        "respond sets nosniff"
    );
}

#[test]
fn a_page_with_no_view_here_is_built_before_it_is_given_the_keys_whatever_its_start() {
    // A page that starts shown has no view either once it has left for the other window and come back: the grab
    // and `PluginOp::Build` build any page that has none, never only one that starts hidden.
    let code = build_ui_code();
    let focus = code
        .find("letgrab_module:Rc<dynFn(&ModuleId)->bool>=")
        .expect("grab_module");
    let focus = scan::braced(&code, focus + code[focus..].find('{').unwrap());
    assert!(
        focus.contains("if!plugin_panels.is_built(id){plugin_panels.activate_holding(id);plugin_panels.build_page(id,"),
        "{focus}"
    );
    assert!(!focus.contains("starts_hidden("), "{focus}");
    let services = file_code("shell/src/gtk_host/services.rs");
    let build = arm(&services, "PluginOp::Build{", "PluginOp::");
    assert!(build.contains("if!self.plugin_panels.is_built(module){"), "{build}");
    assert!(!build.contains("starts_hidden("), "{build}");
}

/// A plugin whose module starts hidden: kept out of what the window places by itself, its page built when its
/// module is first shown (a check right after startup for a layout that reopens it shown, then a change hook for
/// every later route), and opened by its own rule through one function from every show path.
#[test]
fn a_start_hidden_plugin_is_built_when_first_shown_and_opened_by_its_rule() {
    let code = build_ui_code();
    let off_screen = code
        .find("grid.take_every_host_off_screen();")
        .expect("every host starts off screen");
    let apply = code
        .find("driver.first_apply(WinId::MAIN,")
        .expect("the startup layout is the controller's first layout batch");
    assert!(off_screen < apply, "the first batch maps what the layout shows");
    let builds: Vec<usize> = code
        .match_indices("plugin_panels.build_page(")
        .map(|(at, _)| at)
        .collect();
    assert!(
        builds.iter().any(|at| *at < apply),
        "a layout that reopens the module shown builds its page at startup"
    );
    assert!(
        change_hooks(&code).iter().all(|hook| !hook.contains("build_page(")),
        "no change hook builds a page: the controller's lifecycle does, on the module's first show"
    );
    // Declared as a page that waits for its first show.
    assert!(code.contains("deferred:plugin_panels.starts_hidden(&id),kind:SurfaceKind::PluginPage{plugin},"));
    let services = file_code("shell/src/gtk_host/services.rs");
    let build = arm(&services, "PluginOp::Build{", "PluginOp::");
    assert!(build.contains("self.plugin_panels.build_page(module,"), "{build}");
    assert!(code.contains(
        "eitri_core::layout::persist::load_for_role(dir,project_root,pair.pair_role(),&layout_decls,&layout_deferred)"
    ));
    assert!(code.contains("layout_saver.set_deferred(layout_deferred.clone());"));
    // The lists the file is read with are the window's own, with an own window's page read as deferred.
    assert!(code.contains("own_window_lists(plugin_window.as_ref().map(|window|&window.page),&decls,&deferred)"));
    // A switch or an open gives the keys before the change hooks run: the grab, the controller's and the shell
    // arms' alike, builds the page first.
    let focus = code
        .find("letgrab_module:Rc<dynFn(&ModuleId)->bool>=")
        .expect("grab_module");
    let focus = scan::braced(&code, focus + code[focus..].find('{').unwrap());
    assert!(
        focus.contains("plugin_panels.build_page("),
        "the keys never go to a slot with no page"
    );
    // A workspace made by name is the controller's, which keeps a start-hidden module its definition names.
    let workspaces = file_code("core/src/wm/verbs/workspaces.rs");
    assert!(
        workspaces.contains("new_workspace_layout(template,&live,&setup.decls,&deferred)"),
        "a workspace made from a definition keeps a start-hidden module it names"
    );

    // Every show of it is the controller's, which opens it by its rule: `eitri.layout.show`, its module key, a
    // plugin page's `layout.focus` and a card's view alike go through `show_on_screen`.
    let verbs = scan::rust_sources(&["core/src/wm/verbs"]);
    let (_, show) = scan::function_in(&verbs, "show_on_screen");
    assert!(
        show.contains("open_plugin_module("),
        "show_on_screen opens a hidden plugin by its rule"
    );
    let (_, lua_show) = scan::function_in(&verbs, "lua_show");
    assert!(
        lua_show.contains("open_plugin_module("),
        "eitri.layout.show opens it by its rule"
    );
    let (_, open) = scan::function_in(&verbs, "open_module");
    let open: String = open.split_whitespace().collect();
    assert!(
        open.contains("KeyAction::Place|KeyAction::ShowAndFocusifstarts_hidden(cx,id)=>{open_plugin_module("),
        "its module key opens it by its rule"
    );

    let (_, opener) = scan::function_in(&verbs, "open_plugin_module");
    assert!(opener.contains("plugin_open_rule(") && opener.contains("add_everywhere_at("));
    assert!(
        !opener.contains("build_page(") && !opener.contains("PluginOp::Build"),
        "an open the layout refuses builds no page: the lifecycle builds it once it is shown"
    );
    let (_, rule) = scan::function_in(&verbs, "plugin_open_rule");
    assert!(rule.contains("resolve_own("));

    // The page is built in one closure, which logs the build after it.
    let source = scan::read_required("shell/src/plugin_panels.rs");
    let code = scan::code_only(&source.text);
    let code = scan::without(&code, &scan::modules(&code, "tests"));
    let flat: String = code.split_whitespace().collect();
    assert_eq!(flat.matches("plugin_page::build(").count(), 1, "one page build");
    let closure = flat.find("letbuild:BuildPage=").expect("the BuildPage closure");
    let closure = scan::braced(&flat, closure + flat[closure..].find('{').unwrap());
    let built = closure
        .find("plugin_page::build(")
        .expect("the build is inside the closure");
    let traced = closure.find("trace::line(").expect("the build is traced");
    assert!(built < traced);
    let text: String = source.text.split_whitespace().collect();
    assert!(text.contains("trace::line(&id,\"pagebuilt\",\"\");"));
}

/// The follow socket exists only for a window whose plugins want it; its lines are routed by owner and its commands
/// applied by the window; each host hears whether its module is drawn (from a change hook), when the editor's session
/// may have started (the theme and keys feeds report at VimEnter) and when the editor ended (before its exit is acted
/// on); and the trace switch is a probe build's alone.
#[test]
fn the_follow_feed_is_wired_only_for_plugins_that_want_it() {
    let code = build_ui_code();
    assert_eq!(code.matches("FollowSocket::bind()").count(), 1, "one follow socket");
    let wants = code
        .find("ifplugin_panels.wants_follow_feed(){")
        .expect("bound only when a plugin wants it");
    let block = scan::braced(&code, wants + "ifplugin_panels.wants_follow_feed()".len());
    assert!(block.contains("FollowSocket::bind()"), "{block}");
    assert!(code.contains("plugin_panels.set_follow_feed(socket.socket_path(),scratch_path.as_deref());"));
    assert!(code.contains("plugin_panels.set_window_root(project_root);"));

    let listen = code.find("feed_pump.listen_follow(").expect("the follow listener");
    let closure = scan::braced(&code, listen + code[listen..].find('{').unwrap());
    let route = closure
        .find("letcommands=plugin_panels.route_follow(")
        .expect("the lines routed");
    let apply = closure
        .find("plugin_panels::apply_follow_commands(&window,commands)")
        .expect("the commands applied");
    assert!(route < apply, "the routed commands are what is applied");
    // Beside nvim the same routed commands go to the controller one by one, unsorted.
    assert!(closure.contains("forcmdincommands{") || closure.contains("for(module,cmd)incommands{"));
    assert!(closure.contains("window.command(&module,cmd);"), "{closure}");

    // Each host is told whether its module is drawn by the controller's lifecycle, after every change.
    assert!(
        change_hooks(&code).iter().all(|hook| !hook.contains("set_on_screen(")),
        "no change hook in build_ui tells a host whether it is drawn"
    );
    let services = file_code("shell/src/gtk_host/services.rs");
    assert!(
        arm(&services, "PluginOp::OnScreen{", "PluginOp::").contains("self.plugin_panels.set_on_screen(module,*on);")
    );

    for listener in ["feed_pump.listen_theme(feed,", "feed_pump.listen_keys(feed,"] {
        let at = code.find(listener).expect(listener);
        let body = scan::braced(&code, at + code[at..].find('{').unwrap());
        assert!(body.contains("plugin_panels.editor_session();"), "{listener}");
    }

    let exit = code.find("pane.on_exited_unrequested(").expect("the editor's own exit");
    let body = scan::braced(&code, exit + code[exit..].find('{').unwrap());
    let told = body.find("plugin_panels.editor_closed();").expect("the hosts are told");
    let acted = body
        .find("EditorEvent::Exited{")
        .expect("the exit is posted to the quit flow");
    assert!(told < acted);
    // The editor's retirement is the controller's: the hosts hear it only once the layout took the kill.
    let close = file_code("core/src/wm/close.rs");
    let at = close
        .find("fnend_editor(&mutself)->Result<(),crate::layout::LayoutError>{")
        .expect("the controller ending of the editor");
    let retire = scan::braced(&close, at + close[at..].find('{').unwrap());
    let killed = retire.find("self.kill_editor_layout(").expect("the editor is killed");
    let told = retire.find("PluginOp::EditorClosed{").expect("the hosts are told");
    assert!(killed < told, "told only once the kill succeeded");
    let services = file_code("shell/src/gtk_host/services.rs");
    assert!(arm(&services, "PluginOp::EditorClosed{", "PluginOp::").contains("self.plugin_panels.editor_closed();"));

    // The plugins' follow socket goes with the editor's feeds, at the window's teardown.
    let at = services.find("fnfeeds(&self){").expect("the teardown's feeds step");
    let feeds = scan::braced(&services, at + services[at..].find('{').unwrap());
    assert!(feeds.contains("socket.cleanup();"));
    assert!(arm(&services, "CleanupOp::Feeds=>", "}").contains("teardown.feeds()"));

    // The switch's attribute and variable are literals, which the code-only text blanks: read the source.
    let (_, raw) = scan::function_in_raw(&scan::rust_sources(&["shell/src"]), "build_ui");
    let text: String = raw.split_whitespace().collect();
    assert_eq!(text.matches("trace::set_enabled(true)").count(), 1);
    assert!(text.contains(
        "#[cfg(feature=\"probe\")]ifstd::env::var_os(\"EITRI_PLUGIN_TRACE\").is_some_and(|v|v==\"1\"){eitri_core::plugin::trace::set_enabled(true);}"
    ));

    // A followed document's project goes into the same slot the page's scheme handler reads.
    let source = scan::read_required("shell/src/plugin_panels.rs");
    let code = scan::code_only(&source.text);
    let code: String = scan::without(&code, &scan::modules(&code, "tests"))
        .split_whitespace()
        .collect();
    assert!(code.contains("doc_slot:doc_slot.clone(),"));
    assert!(code.contains("ifletSome(slot)=&doc_slot{host.set_doc_slot(slot.clone());}"));
}

/// Every module the window makes at run time is the controller's to make: a new terminal or chat view, a split's,
/// a named one made for the scratchpad and a broken-out tab's view alike, through its `CreateSurface`; no maker of
/// the shell's own is left, and nothing tells the controller of a module the shell made.
#[test]
fn a_runtime_open_is_the_runners() {
    let code = build_ui_code();
    for maker in [
        "new_terminal",
        "new_chat_view",
        "new_scratch_terminal",
        "new_scratch_chat_view",
    ] {
        assert!(
            !code.contains(&format!("let{maker}:")),
            "build_ui still makes instances itself: {maker}"
        );
        assert!(!code.contains(&format!("{maker}(")), "build_ui still calls {maker}");
    }
    for source in scan::rust_sources(&["shell/src", "core/src"]) {
        let flat: String = scan::code_only(&source.text).split_whitespace().collect();
        assert!(
            !flat.contains("HostEvent::Declared"),
            "{} posts or reads a shell's declaration",
            source.path
        );
    }
    // Both instance-making verbs reach the controller's module creation path.
    assert!(!code.contains("driver.scratch_new("));
    let floats = file_code("core/src/wm/verbs/floats.rs");
    assert!(
        floats.contains("modules::scratch_new("),
        "scratchpad.show's missing instance"
    );
    let chat = file_code("core/src/wm/verbs/chat.rs");
    assert!(chat.contains("modules::tab_break("), "tab.break's new view");
    // A plugin module is hosted from startup: its open makes nothing.
    let (_, opener) = scan::function_in(&scan::rust_sources(&["core/src/wm/verbs"]), "open_plugin_module");
    assert!(!opener.contains("CreateSurface"));
}

/// The module verbs are the controller's: a module key, a hide, a kill, a new instance or a split key, the editor's
/// kill (its quit) and the last module's (the window close) included.
#[test]
fn no_module_verb_is_left_in_build_ui() {
    let code = build_ui_code();
    for call in [
        "place_and_arrive(",
        "place_split(",
        "kill_terminal(",
        "toggle_action(",
        "prompt_closing_tab(",
    ] {
        assert!(!code.contains(call), "build_ui still calls `{call}`");
    }
    // The kill is the controller's too, the editor's (its quit) and the last module's (the window close) included:
    // the shell keeps no kill of its own.
    assert!(!code.contains("letkill_now"), "build_ui keeps a kill of its own");
    for call in [
        "grid.kill_module(",
        "close_view_tabs(",
        ".retire(",
        "HostEvent::SurfaceEnded{",
        "close_prompt.ask(",
    ] {
        assert!(!code.contains(call), "build_ui still does `{call}`");
    }
    assert!(
        !code.contains("ShellCall::Place"),
        "the split's placement is the controller's"
    );
    // The follow commands and a chip are the controller's too.
    assert!(code.contains("tray.on_activate("));
    let chip = call_texts(&code, "tray.on_activate(");
    assert!(chip[0].contains("driver.run(WinId::MAIN,Action::Module(id.clone()),Origin::Chip);"));
    let main = file_code("shell/src/window_services.rs");
    let at = main
        .find("fnfollow(&self,module:&ModuleId,show:bool){")
        .expect("the follow window's commands");
    let follow = scan::braced(&main, at + main[at..].find('{').unwrap());
    assert!(follow.contains("driver.follow(WinId::MAIN,module,show);"), "{follow}");
}

/// The window verbs are the controller's: the shell neither runs nor refuses them itself. Starting the second window
/// and raising it are the host's ops, carried out through the pair.
#[test]
fn no_window_verb_is_left_in_build_ui() {
    let code = build_ui_code();
    for verb in [
        "Action::WindowNew",
        "Action::WindowOther",
        "Action::MoveToWindow",
        "Action::WorkspaceToWindow",
        "Action::WorkspaceSwapWindow",
    ] {
        assert!(!code.contains(verb), "build_ui still names `{verb}`");
    }
    assert!(code.contains("wm_host.set_linked_windows(pair.clone());"));
    assert!(code.contains("link.attach_controller(WinId::MAIN,peer,name.clone(),&wm);"));
}

/// The answer gate the panel asks at each card answer is the controller's query, and its refusal is posted for the
/// toast: the shell reads no layout to judge it.
#[test]
fn the_answer_gate_is_the_controllers_query() {
    let code = build_ui_code();
    let gate = call_texts(&code, "panel_hooks.set_answer_gate=Some(");
    assert_eq!(gate.len(), 1);
    let gate = gate[0];
    assert!(gate.contains("driver.query_or("), "{gate}");
    assert!(gate.contains("c.answer_gate(WinId::MAIN,view)"), "{gate}");
    assert!(gate.contains("HostEvent::AnswerRefused{"), "{gate}");
    assert!(!gate.contains("module_layout"), "{gate}");
    assert!(!gate.contains("view_covering("), "{gate}");
}

/// Every hide, kill, show, open, tab switch and unzoom is the controller's: the grid has none of its own, and no
/// shell code calls one, the grid probe's own driving included (it asks the controller as `eitri.layout.show` and
/// `hide` do).
#[test]
fn no_shell_code_hides_or_kills_through_the_grid() {
    let grid = file_code("shell/src/module_grid.rs");
    for gone in [
        "fnhide_module(",
        "fnkill_module(",
        "fnshow_module(",
        "fnopen_module(",
        "fnswitch_tab(",
        "fnunzoom(",
        "fnhide_then_unmap(",
        "fnleave_then_unmap(",
        "pub(crate)fnlayout(",
        "fnconnect_changed(",
    ] {
        assert!(!grid.contains(gone), "the grid still has `{gone}`");
    }
    for source in scan::rust_sources(&["shell/src"]) {
        let code = scan::code_only(&source.text);
        let flat: String = scan::without(&code, &scan::modules(&code, "tests"))
            .split_whitespace()
            .collect();
        for method in [
            ".hide_module(",
            ".kill_module(",
            "grid.show_module(",
            "grid.open_module(",
            "grid.switch_tab(",
            "grid.unzoom(",
        ] {
            assert!(!flat.contains(method), "{} calls `{method}`", source.path);
        }
    }
}

/// What starts a terminal's shell, builds a deferred page, tells a plugin it is drawn and brings a killed module
/// back is the controller's module lifecycle; no change hook in `build_ui` does any of them.
#[test]
fn no_lifecycle_hook_is_left_in_build_ui() {
    let code = build_ui_code();
    let hooks = change_hooks(&code);
    assert!(!hooks.is_empty(), "build_ui still registers change hooks");
    for hook in hooks {
        for call in [".start()", "build_page(", "set_on_screen(", "open_chooser("] {
            assert!(!hook.contains(call), "a change hook calls `{call}`: {hook}");
        }
    }
    let services = file_code("shell/src/gtk_host/services.rs");
    assert!(arm(&services, "TermOp::Start{", "TermOp::").contains("start_pane(&self.terminals,module)"));
    assert!(arm(&services, "ChatOp::OpenChooser{", "ChatOp::").contains("self.agent.open_chooser(*view);"));
    let reload = arm(&services, "PluginOp::Reload{", "PluginOp::");
    assert!(reload.contains("webview.load_uri(url);") && reload.contains("crash_guard.borrow_mut().reset();"));
}

/// The selection, the tray, the mark and the workspace's name are the controller's chrome model, drawn when its
/// `Chrome` op announces a change: `build_ui` registers no change hook that redraws one, the closures that did are
/// gone, and the grid has no second list of hooks for them.
#[test]
fn no_chrome_hook_is_left_in_build_ui() {
    let code = build_ui_code();
    for hook in change_hooks(&code) {
        for call in [
            "refresh_selection(",
            "tray.refresh(",
            "refresh_mark(",
            "refresh_workspace(",
        ] {
            assert!(!hook.contains(call), "a change hook calls `{call}`: {hook}");
        }
    }
    for closure in ["letrefresh_selection", "letrefresh_mark", "letrefresh_workspace"] {
        assert!(!code.contains(closure), "`{closure}` is still made in build_ui");
    }
    for redraw in [
        "refresh_selection(",
        "tray.refresh(",
        "refresh_mark(",
        "refresh_workspace(",
        "refresh_strips(",
    ] {
        assert!(
            !code.contains(redraw),
            "build_ui still redraws the chrome itself: `{redraw}`"
        );
    }
    assert!(!code.contains("connect_chrome_changed("));
    let grid = file_code("shell/src/module_grid.rs");
    assert!(!grid.contains("fnconnect_chrome_changed(") && !grid.contains("fnnotify_chrome_changed("));
    // What draws it instead: the model, read by the host's `Chrome` arm onto the window's widgets.
    assert!(code.contains("wm_host.set_chrome_model(") && code.contains("wm_host.set_chrome_widgets("));
    let host = file_code("shell/src/gtk_host/mod.rs");
    assert!(arm(&host, "HostOp::Chrome{", "HostOp::").contains("self.chrome(op,*win)"));
}

/// The window's start is the controller's start plan, walked in its order from `build_ui`: each phase one
/// `driver.start` call, the Lua layout requests accepted before the requests phase and applied after it, and
/// nothing left in `build_ui` that the plan's ops now do (building the window, loading `init.lua`, showing the
/// window, emitting `shell:ready`).
#[test]
fn build_ui_walks_the_start_plan() {
    use eitri_core::wm::start::{start_plan, StartPhase};
    let code = build_ui_code();
    let name = |phase: &StartPhase| match phase {
        StartPhase::Feeds => "Feeds",
        StartPhase::Window => "Window",
        StartPhase::Editor => "Editor",
        StartPhase::Kernel => "Kernel",
        StartPhase::Config => "Config",
        StartPhase::Layout => "Layout",
        StartPhase::Reopen => "Reopen",
        StartPhase::Surfaces => "Surfaces",
        StartPhase::Chrome => "Chrome",
        StartPhase::Present => "Present",
        StartPhase::Requests => "Requests",
    };
    let mut at = Vec::new();
    for phase in start_plan() {
        let call = format!("driver.start(WinId::MAIN,StartStep::{}", name(phase));
        let found: Vec<usize> = code.match_indices(&call).map(|(i, _)| i).collect();
        assert_eq!(found.len(), 1, "`{call}` appears once in build_ui");
        // The next character ends the variant's name, so `Feeds` is not found inside a longer name.
        let next = code[found[0] + call.len()..].chars().next();
        assert!(matches!(next, Some('{' | ')')), "`{call}` names the phase itself");
        at.push(found[0]);
    }
    assert!(
        at.windows(2).all(|w| w[0] < w[1]),
        "the phases run in the plan's order: {at:?}"
    );
    // The base page's place: the panel is made without a page before `init.lua`, the controller is told what it needs
    // to say when the page is built, and what it built is taken once the layout phase is over, before any surface.
    let handle = code
        .find("agent_panel::make_panel_handle(")
        .expect("the panel's handle");
    let maker = code
        .find("wm_host.set_base_page_maker(")
        .expect("the host can build the page");
    let told = code.find("driver.set_early_facts(").expect("the start's facts");
    let kernel = code.find("StartStep::Kernel").unwrap();
    assert!(handle < maker && maker < told && told < kernel);
    let told_again = code.rfind("driver.set_early_facts(").unwrap();
    assert!(
        kernel < told_again && told_again < code.find("StartStep::Config").unwrap(),
        "the config phase knows what `window.tiling` says"
    );
    // Reopening the editor's files: `editor.restore` is read once `init.lua` has run, the record and nvim's input are
    // prepared after it, and the controller takes them at its reopen step, before any surface or the window's showing.
    let restore = code
        .find("eitri_core::lua::config::parse_editor_restore(")
        .expect("`editor.restore` is read");
    let prepared = code
        .find("eitri_core::editor_reopen_start::prepare_start(")
        .expect("the reopen is prepared");
    let reopen = code
        .find("StartStep::Reopen{prepared:reopen_prepared")
        .expect("the reopen step");
    assert!(kernel < restore && restore < prepared && prepared < reopen);
    assert_eq!(code.matches("prepare_start(").count(), 1);
    let taken = code.find("base_page.take()").expect("the page's place");
    assert!(code.find("StartStep::Layout").unwrap() < taken);
    assert!(taken < code.find("StartStep::Surfaces").unwrap());
    let own = code
        .find("plugin_panels.set_own_window(")
        .expect("the hosts learn window ownership");
    let surfaces = code.find("StartStep::Surfaces").unwrap();
    assert!(own < surfaces, "ownership is set before any surface can say hello");
    assert!(code.find("wm::start::own_window(").unwrap() < own);
    for (build, _) in code.match_indices("plugin_panels.build_page(") {
        assert!(own < build, "page construction follows the ownership decision");
    }
    // The top bar and the window title name the window alike: both read the one name an own window takes from its
    // declared title.
    assert!(code.contains("chrome::build_top_bar(&window,&chrome_project)"));
    assert!(code.contains("project:chrome_project.clone(),"));
    let (_, install) = scan::function_in(&[scan::read_required("shell/src/plugin_panels.rs")], "install");
    let install: String = install.split_whitespace().collect();
    assert!(
        !install.contains("build(theme)"),
        "install registers hosts before building pages"
    );
    let requests = *at.last().unwrap();
    let accept = code
        .find(".accept_requests();")
        .expect("the layout requests are accepted");
    let apply = code
        .rfind("apply_layout_requests();")
        .expect("the queued requests are applied");
    assert!(accept < requests, "accepted before shell:ready is emitted");
    assert!(requests < apply, "applied after shell:ready's handlers queued theirs");
    // What the plan's ops do is not done again here. Read with literals kept and whole-line comments dropped.
    let (_, raw) = scan::function_in_raw(&scan::rust_sources(&["shell/src"]), "build_ui");
    let raw: String = raw
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<String>()
        .split_whitespace()
        .collect();
    for gone in [
        "ApplicationWindow::builder",
        "load_init_file(",
        "window.present()",
        "emit(\"shell:ready\")",
        ".open_window(",
    ] {
        assert!(!raw.contains(gone), "build_ui still has `{gone}`");
    }
}

/// The base chat page has one way to exist: the controller's `BuildBasePage`, which a hub never emits. Its one caller
/// is the maker the host's arm for that op runs, and that arm is the maker's one way in.
#[test]
fn the_base_page_has_one_call_site() {
    let mut calls = Vec::new();
    for (path, code) in shell_code() {
        for (at, _) in code.match_indices("build_base_page(") {
            if !code[..at].ends_with("fn ") {
                calls.push(path.clone());
            }
        }
    }
    assert_eq!(calls, ["shell/src/gtk_host/surfaces.rs"], "{calls:?}");
    let surfaces = file_code("shell/src/gtk_host/surfaces.rs");
    let maker = "implBasePageMakerforBasePage{";
    let maker = surfaces.find(maker).expect("the maker") + maker.len() - 1;
    assert!(
        scan::braced(&surfaces, maker).contains("build_base_page("),
        "only the maker's `build` builds it"
    );
    let host = file_code("shell/src/gtk_host/mod.rs");
    assert_eq!(
        host.matches("maker.build()").count(),
        1,
        "the host runs the maker in one place"
    );
    let arm = host.find("HostOp::BuildBasePage{..}=>{").expect("the op's arm");
    let built = host.find("maker.build()").unwrap();
    assert!(arm < built && built < arm + 300, "that place is the op's arm");
    assert_eq!(
        file_code("core/src/wm/start.rs")
            .matches("HostOp::BuildBasePage")
            .count(),
        1,
        "the controller emits it in one place"
    );
}

/// Giving a panel its tab files reads them and claims other windows' tabs at once, so a hub never does; it turns the
/// tab writer off before the config can turn anything on, and its handle gets no page to run a writer from.
#[test]
fn a_hub_reads_and_claims_no_tabs_file() {
    let code = build_ui_code();
    let given = code
        .find("agent_panel_handle.set_tab_files(")
        .expect("the window's tabs");
    assert_eq!(code.matches("set_tab_files(").count(), 1);
    assert!(
        code[..given].ends_with("if!hub{"),
        "the tab files are given only outside a hub"
    );
    let off = code
        .find("agent_panel_handle.keep_no_tabs()")
        .expect("a hub's writer is off");
    assert!(code[..off].ends_with("ifhub{"));
    assert!(off < code.find("config.apply_to_panel(").unwrap());
}

/// Every key action, Lua run and panel tab verb reaches the controller's runner, and only through it: no
/// shell runner of its own is left, no arm of the shell's is reached at all, and each call of the runner names
/// where it came from as one of the routes the shell has. A new route is a deliberate edit of this list.
#[test]
fn the_runner_is_the_only_door() {
    // `Hub`: what window 1 sent a module window over the link, run there as it is.
    const ROUTES: &[&str] = &["Key", "PanelTab", "Lua", "Follow", "Chip", "Hub"];
    let mut runs = 0;
    for (path, code) in shell_code() {
        let flat: String = code.split_whitespace().collect();
        assert!(!flat.contains("run_action"), "{path} still has a runner of its own");
        // `Origin` itself, not a name that ends in it (`SurfaceOrigin::`).
        let origins: Vec<&str> = flat
            .match_indices("Origin::")
            .filter(|(at, _)| {
                !flat[..*at]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
            })
            .map(|(at, found)| {
                let rest = &flat[at + found.len()..];
                let end = rest
                    .find(|c: char| !c.is_alphanumeric() && c != '_')
                    .unwrap_or(rest.len());
                &rest[..end]
            })
            .collect();
        for origin in &origins {
            assert!(
                ROUTES.contains(origin),
                "{path}: the runner is called from Origin::{origin}"
            );
        }
        for call in call_texts(&flat, "driver.run(") {
            assert!(
                ROUTES.iter().any(|route| call.contains(&format!("Origin::{route}"))),
                "{path}: a runner call that names no route: {call}"
            );
            runs += 1;
        }
        assert!(!flat.contains("run_verb("), "{path} calls an arm of the shell's");
    }
    assert!(
        runs >= 2,
        "the chooser's new workspace and the tab verbs call the runner"
    );
    // The prefix's commands run inside the controller, through the same runner as a key's.
    let controller = file_code("core/src/wm/controller.rs");
    assert!(
        controller.contains("PrefixCommand::Run(action)=>returnself.run(win,action,Origin::Key{keycode}),"),
        "the prefix's command runs through the runner as a key's"
    );
    assert!(
        !build_ui_code().contains("set_shell_verb_arm("),
        "no arm is left for the runner to hand an action back to"
    );
}

/// The tiling verbs are the controller's: no arm in `build_ui` changes the tiled tree, the mark, the zoom or
/// the selection through the grid's own verbs any more.
#[test]
fn a_scan_finds_no_tiling_arm_in_run_verb() {
    let code = build_ui_code();
    for verb in [
        "grid.move_far(",
        "grid.toggle_split(",
        "grid.rotate(",
        "grid.flip(",
        "grid.preset(",
        "grid.cycle_preset(",
        "grid.spread(",
        "grid.swap_marked(",
        "grid.undo(",
        "grid.move_dir(",
        "grid.container_mode(",
        "grid.focus_parent(",
        "grid.focus_child(",
        "grid.toggle_zoom(",
        "grid.resize(",
        "grid.swap_modules(",
        "grid.swap_adjacent(",
    ] {
        assert!(!code.contains(verb), "build_ui still calls `{verb}`");
    }
}

/// The startup check asserts a host for exactly the modules `hosted_at_start` picks from the surfaces declared, and
/// runs after the controller has made them: a window with no editor would otherwise fail its own start.
#[test]
fn the_startup_host_check_is_built_by_hosted_at_start() {
    let code = build_ui_code();
    let built = code
        .find("letstartup_modules:Vec<ModuleId>=linked_window::hosted_at_start(&startup_surfaces,")
        .expect("the startup host list is built by hosted_at_start over the surfaces declared");
    let declared = code
        .find("StartStep::Surfaces{decls:startup_surfaces,}")
        .expect("the same surfaces are declared to the controller");
    let checked = code
        .find("foridin&startup_modules{assert!(grid.has_host(id),")
        .expect("each module of that list must have its host");
    assert!(
        built < declared && declared < checked,
        "the list is taken before the surfaces move into the controller, and checked once it made them"
    );
}

/// Returned pages read their stores at their next show; failed restores reload pages after taking them back.
#[test]
fn the_link_ended_hook_loads_again_every_page_that_comes_back() {
    let code = build_ui_code();
    let ended = let_block(&code, "letended:linked_window::spawn::OnEnded=");
    assert!(
        ended.contains("wm.post(HostEvent::Pair(eitri_core::wm::PairEvent::LinkEnded{"),
        "the link's end is the controller's: {ended}"
    );
    for gone in [
        "apply_end(",
        "end_plan(",
        "pages_back(",
        "reload_page_back(",
        "grid.apply()",
    ] {
        assert!(!ended.contains(gone), "the hook still carries out `{gone}`");
    }
    let (_, end) = scan::function_in(&scan::rust_sources(&["core/src/wm"]), "pair_event");
    let end: String = end.split_whitespace().collect();
    let taken_back = end
        .find("crate::window_link::end::apply_end(&plan,")
        .expect("the end plan is carried out in the layout");
    let returned = end
        .find("window.lifecycle.returned(id);")
        .expect("returned modules rejoin the lifecycle before layout");
    let hidden = end
        .find("foridin&plan.back_hidden{")
        .expect("hidden returns rejoin the lifecycle");
    let marked = end
        .find("window.lifecycle.reload_at_show(id);")
        .expect("hidden pages reload at their next show");
    assert!(taken_back < hidden && hidden < marked);
    let pages = end
        .find("letpages:Vec<_>=crate::window_link::end::pages_back(&plan)")
        .expect("the returned pages are selected before the lifecycle reopens them");
    let selection = &end[pages..];
    let selection = &selection[..selection.find(".collect();").expect("the page selection is collected")];
    assert!(
        selection
            .contains(".filter(|id|!matches!(window.lifecycle.state(id),Some(super::module::ModuleState::Killed(_))))"),
        "a Lua panel reopened by the lifecycle must not also get ReloadPage: {selection}"
    );
    let laid_out = end.find("window.changed_by_controller(win,batch);").expect("laid out");
    let reloads = end
        .find("formoduleinpages{batch.push_effect(HostOp::Pair(PairOp::ReloadPage{win,module}));}")
        .expect("every selected page gets one reload");
    assert_eq!(
        end.matches("PairOp::ReloadPage").count(),
        1,
        "only the selected pages reload"
    );
    assert!(
        taken_back < returned && returned < pages && pages < laid_out && laid_out < reloads,
        "select after the return, before the lifecycle reopens panels, and reload after layout"
    );
    let services = file_code("shell/src/gtk_host/services.rs");
    let reload = arm(&services, "PairOp::ReloadPage{", "}Ok(())");
    assert!(
        reload.contains("crate::linked_window::reload_page_back(&self.plugin_panels,&self.lua_pages,module)"),
        "{reload}"
    );
    assert!(
        code.contains("pair.set_hooks(linked_window::spawn::PairHooks{linked,post,ended})"),
        "and that hook is the one the link's end runs"
    );
}

/// A path window 2 asks window 1 to open passes the panel's own rule, the same function a click in window 1's
/// own chat goes through, so the two can never drift apart.
#[test]
fn window_2s_paths_follow_the_panels_own_rule() {
    let code = file_code("shell/src/linked_window/editor.rs");
    let open = arm(
        &code,
        "EditorRequest::OpenPath{path,line,col:_}=>",
        "EditorRequest::ReviewOpen",
    );
    assert!(
        open.contains("eitri_panel::agent_panel::open_path_target(&self.root,&path)"),
        "window 2's open request is checked by the panel's rule: {open}"
    );
    for own in ["fnopen_target", "fnopen_path_target"] {
        assert!(
            !code.contains(own),
            "the editor proxy keeps no path rule of its own (`{own}`)"
        );
    }
}

/// Window 2's follow lines, which window 1 sends it, are carried out as the lines of a window's own socket are: by
/// the follow window over the controller, so a plugin's `show` or `hide` in window 2 passes the same gates.
#[test]
fn window_2s_follow_lines_go_through_the_controllers_follow_window() {
    let code = build_ui_code();
    let follow_window = "letwindow=GridFollowWindow{driver:Rc::downgrade(&driver),};";
    for listener in ["feed_pump.listen_follow(", "two.on_follow("] {
        let at = code.find(listener).unwrap_or_else(|| panic!("`{listener}`"));
        let made = code[..at]
            .rfind(follow_window)
            .unwrap_or_else(|| panic!("`{listener}` has a follow window over the controller"));
        let between = &code[made + follow_window.len()..at];
        assert!(
            !between.contains("letwindow=") && between.len() < 200,
            "the follow window `{listener}` applies is that one: {between}"
        );
        let handed = call_texts(&code[at..], listener)[0];
        assert!(
            handed.contains("plugin_panels::apply_follow_commands(&window,"),
            "{handed}"
        );
    }
}

/// The workspace verbs are the controller's: `build_ui` makes no closure that switches, makes, renames or deletes a
/// workspace and reads no chooser pick; the grid has no switch of its own, and the shell keeps no second copy of the
/// pick's or the made workspace's rules.
#[test]
fn no_workspace_verb_is_left_in_build_ui() {
    let code = build_ui_code();
    for closure in [
        "letswitch_workspace",
        "letswitch_or_make",
        "letstart_rename",
        "letpick_workspace",
        "letworkspaces_changed",
    ] {
        assert!(!code.contains(closure), "`{closure}` is still made in build_ui");
    }
    for call in [
        "grid.switch_workspace(",
        "Pick::",
        "resolve_pick(",
        "new_workspace_layout(",
        "rename_current(",
        "create_copy(",
        ".delete(&name)",
        "ShellCall::WorkspacePick",
        "start_rename(",
    ] {
        assert!(!code.contains(call), "build_ui still does `{call}`");
    }
    // Every Lua layout request goes whole to the controller, including a workspace switch.
    assert!(code.contains("LuaEvent::LayoutRequests{win:WinId::MAIN,requests,"));
    assert!(eitri_core::wm::lua::decided(
        &eitri_core::lua::layout::LayoutRequest::Switch("study".into())
    ));
    // A defined name is made from `init.lua`'s definitions, given to the controller once, after `init.lua` ran.
    let config = code.find("StartStep::Config{").expect("the config phase");
    let setups = call_texts(&code, "driver.set_module_setup(");
    assert_eq!(setups.len(), 1, "one module setup for the window");
    assert!(setups[0].contains("workspaces:lua_engine.layout.borrow().defined_workspaces().to_vec(),"));
    assert!(config < code.find("driver.set_module_setup(").unwrap());
    let grid = file_code("shell/src/module_grid.rs");
    assert!(!grid.contains("fnswitch_workspace(") && !grid.contains("fnswitch_then_unmap"));
    for path in [
        "shell/src/main.rs",
        "shell/src/launch.rs",
        "shell/src/window_services.rs",
    ] {
        let main = file_code(path);
        for helper in ["fnresolve_pick(", "fnnew_workspace_layout(", "enumPick{"] {
            assert!(!main.contains(helper), "{path} keeps `{helper}`");
        }
    }
}

/// The rename entry only reports: its wiring posts what happened to it to the controller and renames, hides or
/// refills nothing, and `build_ui` wires it through that one function.
#[test]
fn the_rename_entry_reports_and_never_renames_itself() {
    let chrome = file_code("shell/src/chrome.rs");
    let at = chrome.find("fnwire_rename_entry(").expect("the rename entry's wiring");
    let wiring = scan::braced(&chrome, at + chrome[at..].find('{').unwrap());
    for posts in [
        "HostEvent::TopBarEntry{win:WinId::MAIN,ev}",
        "EntryEv::Enter(entry.text().to_string())",
        "EntryEv::Escape",
        "EntryEv::LostKeys",
    ] {
        assert!(wiring.contains(posts), "the entry reports `{posts}`: {wiring}");
    }
    for call in [
        "rename_current(",
        "Workspaces",
        "workspaces",
        ".set_visible(",
        ".set_text(",
        "select_region(",
        "return_to_pane",
        "grab_focus(",
    ] {
        assert!(!wiring.contains(call), "the entry's wiring does `{call}` itself");
    }
    let code = build_ui_code();
    assert_eq!(code.matches("chrome::wire_rename_entry(&top_bar.rename,").count(), 1);
    for call in [
        "rename.connect_activate(",
        "entry.connect_activate(",
        "rename.add_controller(",
    ] {
        assert!(!code.contains(call), "build_ui wires the entry itself: `{call}`");
    }
}

/// Float decisions belong to the controller, including a new named scratch instance and Lua floats.
#[test]
fn no_float_verb_is_left_in_build_ui() {
    let code = build_ui_code();
    for gone in ["move_float(", "scratch_new("] {
        assert!(!code.contains(gone), "build_ui still has `{gone}`");
    }
    assert!(!code.contains("grid.float_at("));
    assert!(eitri_core::wm::lua::decided(
        &eitri_core::lua::layout::LayoutRequest::Float(eitri_core::layout::ModuleId::terminal(), None)
    ));
}

/// Gesture callbacks only report pointer events; layout changes are computed by the controller.
#[test]
fn the_grids_gestures_write_no_layout() {
    let mut handlers = 0;
    for path in ["shell/src/module_grid.rs", "shell/src/module_grid/floatbars.rs"] {
        let code = file_code(path);
        for signal in ["connect_drag_begin(", "connect_drag_update(", "connect_drag_end("] {
            for handler in call_texts(&code, signal) {
                handlers += 1;
                for forbidden in [
                    "move_divider(",
                    "set_float_rect(",
                    ".raise(",
                    "try_borrow_mut(",
                    "float_dragged(",
                    "float_bar_pressed(",
                ] {
                    assert!(!handler.contains(forbidden), "{path}: a gesture calls `{forbidden}`");
                }
            }
        }
        for gone in ["fnfloat_dragged(", "fnfloat_bar_pressed(", "fnraise_float("] {
            assert!(
                !code.contains(gone),
                "{path}: `{gone}` still changes layout for a gesture"
            );
        }
    }
    assert!(handlers >= 6, "both divider and float gestures report every phase");
}

/// Whether window 2 comes back at a launch depends on `window.restore`, which `init.lua` sets: the pair file's
/// decision and the saved tabs it chooses are made once the config phase has read it, and the panel is given its
/// tabs then rather than reading them as it is built.
#[test]
fn window_restore_is_read_before_the_pair_files_decision() {
    let code = build_ui_code();
    let config = code.find("StartStep::Config").expect("the config phase");
    let restore_key = code.find("WINDOW_RESTORE_KEY").expect("window.restore is read");
    assert!(config < restore_key, "window.restore is read after init.lua ran");
    for decision in ["read_for_restore(", "tab_files(", "second_restore("] {
        let at: Vec<usize> = code.match_indices(decision).map(|(at, _)| at).collect();
        assert!(!at.is_empty(), "build_ui calls `{decision}`");
        assert!(
            at.iter().all(|at| *at > restore_key),
            "`{decision}` comes after window.restore is read"
        );
    }
    let layout = code.find("StartStep::Layout").expect("the layout phase");
    assert!(
        code.find("read_for_restore(").unwrap() < code.find("load_for_role(").expect("the layout files are read"),
        "the pair file is read before the layout files"
    );
    assert!(
        code.find("set_tab_files(").unwrap() < layout,
        "the panel has its tabs before the layout phase"
    );
    let panels = call_texts(&code, "make_panel_handle(");
    assert!(
        !panels[0].contains("tab_files"),
        "the panel is built with no saved tabs chosen yet"
    );
}

/// The link's end as the window carries it out: an orphaned window 2 waits for a new window 1, window 1 settles
/// the pair file through the pair (which leaves it writing when its window 2 never linked), and a window 1 that
/// held its automatic tab restore for a window 2 started again lets it go once that window is back or is not.
#[test]
fn the_links_end_and_the_restore_of_window_2_are_carried_out_by_the_window() {
    let code = build_ui_code();
    let between = |from: &str, to: &str| -> String {
        let start = code.find(from).unwrap_or_else(|| panic!("`{from}` in build_ui"));
        let end = start
            + code[start..]
                .find(to)
                .unwrap_or_else(|| panic!("`{to}` after `{from}`"));
        code[start..end].to_string()
    };
    let ended = between("letended:linked_window::spawn::OnEnded=", "pair.set_hooks(");
    for call in ["pair.wait_for_relink(", "pair.window2_link_ended(link,end)"] {
        assert!(ended.contains(call), "the `ended` hook calls `{call}`");
    }
    // An orphaned window 2 keeps the pair file once the controller has taken back what it held away.
    let services = file_code("shell/src/gtk_host/services.rs");
    let orphaned = arm(&services, "PairOp::Orphaned{", "PairOp::");
    for call in ["writer.window2_holds(held);", "writer.take_over();"] {
        assert!(orphaned.contains(call), "{orphaned}");
    }
    assert!(
        !ended.contains("hand_over"),
        "window 1's pair file at a link's end is the pair's to settle"
    );
    let linked = between("letlinked:linked_window::spawn::OnLinked=", "letpost:");
    assert!(
        linked.contains("panel.release_launch_restore()"),
        "a linked window 2 lets the restore go"
    );
    let failed = between("pair.on_restore_failed(", "driver.start(");
    assert!(
        failed.contains("wm.post(HostEvent::Pair(eitri_core::wm::PairEvent::RestoreFailed{win:WinId::MAIN"),
        "a window 2 that does not come back is the controller's: {failed}"
    );
    let unrestored = arm(&services, "PairOp::Unrestored{", "PairOp::");
    assert!(unrestored.contains("pair_files::take_back_unrestored("), "{unrestored}");
    let given = unrestored
        .find("self.agent.set_tab_files(")
        .expect("window 2's tabs are given back");
    let released = unrestored
        .rfind("self.agent.release_launch_restore()")
        .expect("and the restore let go");
    assert!(
        given < released,
        "window 2's tabs are given before the restore is let go"
    );
    let held = code
        .find("hold_launch_restore()")
        .expect("the restore is held for window 2");
    assert!(
        held < code.find("StartStep::Layout").unwrap(),
        "held before anything can be restored"
    );
    assert!(
        code.contains(concat!(
            "pair_raise.used_for_window(pair.holds_pair()||pair.is_second()||pair.is_beside(),",
            "pair.restored()||module_window.is_some(),incoming_opens.is_some(),)"
        )),
        "a restored window 2's first show keeps window 1's keys, so does a module window's (one beside nvim too, \
         which presents through the same raise), and a window holding the open socket can be raised"
    );
}

/// Only the controller writes a window's layout: no shell code names a mutable layout or workspaces (a `RefCell` of
/// either, or a `WindowParts` built by hand), the grid and every reader hold the controller's views, nothing posts a
/// change made behind the controller, and `build_ui` hands the window's start values over by value.
#[test]
fn the_shell_names_no_mutable_layout() {
    for source in scan::rust_sources(&["shell/src"]) {
        let code = scan::code_only(&source.text);
        let flat: String = scan::without(&code, &scan::modules(&code, "tests"))
            .split_whitespace()
            .collect();
        for named in [
            "RefCell<Layout>",
            "RefCell<eitri_core::layout::Layout>",
            "RefCell<Workspaces>",
            "RefCell<eitri_core::layout::Workspaces>",
            "RefCell<eitri_core::layout::workspace::Workspaces>",
            "RefCell::new(Layout::",
            "WindowParts{",
            "LayoutChangedByShell",
            "module_layout.borrow",
            "module_layout.try_borrow",
            "workspaces.borrow_mut(",
            "workspaces.try_borrow_mut(",
        ] {
            assert!(!flat.contains(named), "{} names `{named}`", source.path);
        }
    }
    let code = build_ui_code();
    assert!(
        code.contains("letparts=eitri_core::wm::WindowParts::new(start_layout,start_workspaces,layout_saver);"),
        "the window's start values go to the controller by value"
    );
    assert!(code.contains("let(module_layout,workspaces)=parts.views();"));
    let contract = file_code("core/src/wm/host.rs");
    assert!(
        !contract.contains("LayoutChangedByShell"),
        "the contract has no change made behind the controller"
    );
    let controller = file_code("core/src/wm/controller.rs");
    for gone in ["shell_revision", "fnlayout_changed("] {
        assert!(!controller.contains(gone), "the controller still has `{gone}`");
    }
}

#[test]
fn window_new_reads_only_the_monitor_of_the_pair_file() {
    let code = build_ui_code();
    let restore = &code[code.find("letsecond_restore=").unwrap()..code.find("letsecond_placement=").unwrap()];
    assert!(restore.contains(".filter(|_|pair.is_second()&&pair.restored())"));
    assert_eq!(call_texts(restore, "pair_files::second_restore(").len(), 1);
    assert!(restore.contains("None=>linked_window::pair_files::SecondRestore::default(),"));
    let placement = &code[code.find("letsecond_placement=").unwrap()..code.find("letwindow2_restored=").unwrap()];
    assert!(placement.contains(".filter(|_|pair.is_second()&&!pair.restored()&&!want_clean)"));
    assert_eq!(call_texts(placement, "pair::read_pair(").len(), 1);
    assert_eq!(call_texts(placement, "pair_files::second_placement(").len(), 1);
    for forbidden in ["second_restore(", ".holders", "tab_files(", "PairWriter", "write_pair("] {
        assert!(!placement.contains(forbidden), "placement reads {forbidden}");
    }
    assert!(placement.contains("None=>None,"));
    let start = &code[code.find("letstart=ifwriting").unwrap()..code.find("ifwriting||pair.is_second()").unwrap()];
    assert!(start.contains("elseifpair.restored(){second_restore.file.clone()}"));
    assert!(start.contains("PairFile{window2_open:true,..Default::default()}"));
    let calls = call_texts(&code, "driver.set_placement(");
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains("elseifpair.restored(){second_restore.remembered.clone()}else{second_placement}"));
    assert!(code.find("letsecond_placement=").unwrap() < code.find("driver.set_placement(").unwrap());
}

#[test]
fn the_busy_hook_posts_chat_busy() {
    let code = build_ui_code();
    let calls = call_texts(&code, "agent_panel_handle.on_busy(");
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains("HostEvent::Chat(eitri_core::wm::ChatEvent::Busy{win:WinId::MAIN,view,busy,"));
    assert!(calls[0].contains("for&(view,busy)inchanged"));
}
