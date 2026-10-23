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

/// What each `grid.connect_changed(` and `grid.connect_changed_by_controller(` in `code` (whitespace removed) is
/// handed: the text up to its closing `)`. Both lists run after a layout change, so neither may hold what moved.
fn change_hooks(code: &str) -> Vec<&str> {
    let mut hooks = call_texts(code, "grid.connect_changed(");
    hooks.extend(call_texts(code, "grid.connect_changed_by_controller("));
    hooks
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

/// What the innermost block around `at` says before `at`: the bindings a closure made there captures, wherever among
/// them they stand.
fn enclosing_lets(code: &str, at: usize) -> &str {
    let open = code[..at].rfind('{').expect("a block around the call");
    &code[open..at]
}

#[test]
fn plugins_are_installed_before_the_lua_panels_are_read() {
    let code = build_ui_code();
    let install = code
        .find("plugin_panels::PluginPanels::install(&lua_engine,&keymap,&PageTheme::fallback())")
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
    // The freshly derived tokens: the page set carries no size, so a size-only change sends the page nothing.
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

#[test]
fn a_committed_document_gets_the_theme_and_a_host_its_module_key() {
    let source = scan::read_required("shell/src/plugin_panels.rs");
    let code = scan::code_only(&source.text);
    let code: String = scan::without(&code, &scan::modules(&code, "tests"))
        .split_whitespace()
        .collect();
    let at = code.find("on_committed:").expect("the commit hook");
    let hook = scan::braced(&code, at + code[at..].find('{').unwrap());
    assert!(hook.contains("pump.host.page_committed()"), "{hook}");
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
    let code = build_ui_code();
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
    let close = code.find("window.connect_close_request(").unwrap();
    assert!(code[close..].contains("plugin_panels.shutdown(&app_for_close);"));
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
fn prefix_question_mark_reads_the_plugin_before_the_chat_takes_the_keys() {
    let code = build_ui_code();
    let arm = code.find("Action::PanelKeymap=>{").expect("the prefix ? arm");
    let read = code[arm..]
        .find("keymap_plugins.declared_rows(&module)")
        .expect("the plugin's rows are read");
    let focus = code[arm..].find("give_keys(&chat);").expect("the chat takes the keys");
    let open = code[arm..]
        .find("agent.open_keymap(view,plugin,opening);")
        .expect("opened with the section");
    assert!(read < focus && focus < open);
}

#[test]
fn prefix_question_mark_keeps_what_it_opened_from() {
    let code = build_ui_code();
    let arm = code.find("Action::PanelKeymap=>{").expect("the prefix ? arm");
    let body = &code[arm..];
    let show = body.find("show_on_screen(&chat)").expect("the chat is put on screen");
    let kept = body.find("driver.keymap_opened(").expect("what it opened from is kept");
    let focus = body.find("give_keys(&chat);").expect("the chat takes the keys");
    assert!(
        show < kept && kept < focus,
        "kept only after the chat is on screen, before it takes the keys"
    );
    // The sheet's view is the one holding the keys (else the base view's), read before anything moves, and the
    // covered check and the opening use that same view.
    let view = body
        .find("letview=from.as_ref().and_then(ViewId::of_module).unwrap_or(ViewId::BASE);")
        .expect("the view comes from the module that held the keys");
    let chat = body
        .find("letchat=view.module();")
        .expect("the sheet's module is that view's");
    let covered = body
        .find("chat_covered(view)")
        .expect("the covered check asks about that view");
    let open = body
        .find("agent.open_keymap(view,plugin,opening);")
        .expect("the sheet opens in that view's page");
    assert!(view < chat && chat < show && show < covered && covered < kept && kept < open);

    let arm = code.find("Action::PanelKeymap=>{").expect("the prefix ? arm");
    let arm_body = scan::braced(&code, arm + "Action::PanelKeymap=>".len());
    assert!(
        arm_body.contains("layout.zoom().cloned()") && !arm_body.contains("zoomed()"),
        "the opening keeps either kind of zoom"
    );

    // A command line opened after the sheet forgets it, before the chat takes the keys.
    let line = code.find("Action::PanelCommandLine=>{").expect("the prefix : arm");
    let line_body = scan::braced(&code, line + "Action::PanelCommandLine=>".len());
    let forget = line_body
        .find("driver.keymap_superseded(WinId::MAIN);")
        .expect("the newer request forgets the sheet's pending return");
    let take = line_body.find("give_keys(&chat);").expect("the chat takes the keys");
    assert!(forget < take);
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
/// prefix exist before what asks them.
#[test]
fn no_late_bound_cell_is_left_in_build_ui() {
    let code = build_ui_code();
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
        body[quit..].contains("editor_quitting.cancelled(g)"),
        "a cancelled quit stays the editor quit's"
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
    assert!(
        code.contains("eitri_core::layout::persist::load_for_role(dir,project_root,pair.pair_role(),&decls,&deferred)")
    );
    assert!(code.contains("layout_saver.set_deferred(deferred.clone());"));
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

    let show = code
        .find("letshow_on_screen:Rc<dynFn(&ModuleId)->Result<(),LayoutError>>=")
        .expect("show_on_screen");
    let show = scan::braced(&code, show + code[show..].find('{').unwrap());
    assert!(
        show.contains("open_plugin_module("),
        "show_on_screen opens a hidden plugin by its rule"
    );
    // `eitri.layout.show` and its module key are the controller's, which opens it by its rule too.
    let verbs = scan::rust_sources(&["core/src/wm/verbs"]);
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

    let sources = scan::rust_sources(&["shell/src"]);
    let (_, opener) = scan::function_in(&sources, "open_plugin_module");
    assert!(opener.contains("plugin_open_rule(") && opener.contains("add_everywhere_at("));
    assert!(
        !opener.contains("build_page("),
        "an open the grid refuses builds no page"
    );
    let (_, rule) = scan::function_in(&sources, "plugin_open_rule");
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
    let apply = closure.find("apply_follow_commands(").expect("the commands applied");
    let route = closure.find("plugin_panels.route_follow(").expect("the lines routed");
    assert!(apply < route, "the routed commands are what is applied");

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
    let acted = body.find(".nvim_exited();").expect("the exit is acted on");
    assert!(told < acted);
    let retire = code
        .find("letretire_editor:Rc<dynFn()->Result<(),LayoutError>>=")
        .expect("retire_editor");
    let body = scan::braced(&code, retire + code[retire..].find('{').unwrap());
    let killed = body.find("grid.kill_module(").expect("the editor is killed");
    let told = body.find("plugin_panels.editor_closed();").expect("the hosts are told");
    assert!(killed < told, "told only once the kill succeeded");

    let close = code.find("window.connect_close_request(").unwrap();
    assert!(code[close..].contains("socket.cleanup();"));

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
    // The two arms still in the shell that need a new instance ask the controller for it.
    assert!(
        code.contains("driver.scratch_new(WinId::MAIN,&id);"),
        "scratchpad.show's missing instance"
    );
    assert!(code.contains("driver.tab_break(WinId::MAIN);"), "tab.break's new view");
    // A plugin module is hosted from startup: its open makes nothing.
    let sources = scan::rust_sources(&["shell/src"]);
    let (_, opener) = scan::function_in(&sources, "open_plugin_module");
    assert!(!opener.contains("CreateSurface"));
}

/// The module verbs are the controller's: the shell's arm answers a module key, a hide, a new instance or a split
/// key only as a bug, and its kill only for the editor (its quit) and the last module (the window close).
#[test]
fn no_module_verb_is_left_in_run_verb() {
    let code = build_ui_code();
    let body = let_block(&code, "letrun_verb:Rc<dynFn(Action)>=");
    let bug_arm = "Action::Module(_)|Action::ModuleHide|Action::InstanceNew{..}|Action::Split{..}=>{eprintln!(";
    assert_eq!(body.matches(bug_arm).count(), 1, "one arm, which reports a bug");
    for verb in [
        "Action::Module(",
        "Action::ModuleHide",
        "Action::InstanceNew",
        "Action::Split",
    ] {
        assert_eq!(body.matches(verb).count(), 1, "`{verb}` only in the bug arm");
    }
    for call in [
        "open_module(",
        "place_and_arrive(",
        "place_split(",
        "kill_terminal(",
        "grid.show_module(",
        "toggle_action(",
        "prompt_closing_tab(",
    ] {
        assert!(!body.contains(call), "run_verb still calls `{call}`");
    }
    let kill = let_block(
        &code,
        "letkill_now:Rc<dynFn(&ModuleId,Option<String>)->Result<(),String>>=",
    );
    for call in [
        "grid.kill_module(",
        "close_view_tabs(",
        ".retire(",
        "HostEvent::SurfaceEnded{",
    ] {
        assert!(!kill.contains(call), "the shell's kill still does `{call}`");
    }
    assert!(kill.contains(
        "(KillScope::Window,ModuleKind::Editor)=>editor_quitting.kill_editor(KillScope::Window,confirmed)?,"
    ));
    assert!(kill.contains("(KillScope::Window,_)=>editor_quitting.close_after(confirmed),"));
    assert!(
        kill.contains("(KillScope::Module,ModuleKind::Editor)=>editor_quitting.kill_editor(KillScope::Module,None)?,")
    );
    assert!(
        !code.contains("ShellCall::Place"),
        "the split's placement is the controller's"
    );
    // The follow commands and a chip are the controller's too.
    assert!(code.contains("tray.on_activate("));
    let chip = call_texts(&code, "tray.on_activate(");
    assert!(chip[0].contains("driver.run(WinId::MAIN,Action::Module(id.clone()),Origin::Chip);"));
    let main = file_code("shell/src/main.rs");
    let at = main
        .find("fnfollow(&self,module:&ModuleId,show:bool){")
        .expect("the follow window's commands");
    let follow = scan::braced(&main, at + main[at..].find('{').unwrap());
    assert!(follow.contains("driver.follow(WinId::MAIN,module,show);"), "{follow}");
}

/// The window verbs are the controller's: the shell's arm answers them only as a bug, and no longer refuses them
/// itself. Starting the second window and raising it are the host's ops, carried out through the pair.
#[test]
fn no_window_verb_is_left_in_run_verb() {
    let code = build_ui_code();
    let body = let_block(&code, "letrun_verb:Rc<dynFn(Action)>=");
    let bug_arm = "Action::WindowNew|Action::WindowOther|Action::MoveToWindow{..}|Action::WorkspaceToWindow{..}|Action::WorkspaceSwapWindow=>{eprintln!(";
    assert_eq!(body.matches(bug_arm).count(), 1, "one arm, which reports a bug");
    for verb in [
        "Action::WindowNew",
        "Action::WindowOther",
        "Action::MoveToWindow",
        "Action::WorkspaceToWindow",
        "Action::WorkspaceSwapWindow",
    ] {
        assert_eq!(body.matches(verb).count(), 1, "`{verb}` only in the bug arm");
    }
    assert!(code.contains("wm_host.set_linked_windows(pair.clone());"));
    assert!(code.contains("link.attach_controller(WinId::MAIN,peer,&wm);"));
}

/// The answer gate the panel asks at each card answer is the controller's query, and its refusal is posted for the
/// toast: the shell reads no layout to judge it.
#[test]
fn the_answer_gate_is_the_controllers_query() {
    let code = build_ui_code();
    let gate = call_texts(&code, "panel_hooks.set_answer_gate=Some(");
    assert_eq!(gate.len(), 1);
    let gate = gate[0];
    assert!(gate.contains("driver.query("), "{gate}");
    assert!(gate.contains("c.answer_gate(WinId::MAIN,view)"), "{gate}");
    assert!(gate.contains("HostEvent::AnswerRefused{"), "{gate}");
    assert!(!gate.contains("module_layout"), "{gate}");
    assert!(!gate.contains("view_covering("), "{gate}");
}

/// The callers of the grid's own hide and kill outside the grid: each an arm still in the shell, whose layout write
/// the controller re-reads. A new one is a deliberate edit of this list.
#[test]
fn every_remaining_hide_or_kill_caller_is_named() {
    let mut found: Vec<(String, String)> = Vec::new();
    for source in scan::rust_sources(&["shell/src"]) {
        let path = source.path.replace('\\', "/");
        if path.ends_with("shell/src/module_grid.rs") {
            continue;
        }
        let code = scan::code_only(&source.text);
        let flat: String = scan::without(&code, &scan::modules(&code, "tests"))
            .split_whitespace()
            .collect();
        let file = path.rsplit("shell/src/").next().unwrap_or(&path).to_string();
        for method in [".hide_module(", ".kill_module("] {
            for call in call_texts(&flat, method) {
                found.push((file.clone(), format!("{}{call}", &method[1..method.len() - 1])));
            }
        }
    }
    found.sort();
    let mut named: Vec<(String, String)> = [
        // The editor's quit: its hide in place and its kill once nvim is gone.
        ("main.rs", "hide_module(&ModuleId::editor(),&*self.focus_module)"),
        (
            "main.rs",
            "kill_module(&ModuleId::editor(),Reopen::InPlace,&*self.focus_module)",
        ),
        (
            "main.rs",
            "kill_module(&ModuleId::editor(),Reopen::Never,&*focus_module)",
        ),
        // An extra chat view the panel says holds no tab.
        ("main.rs", "kill_module(id,Reopen::Remove,&*focus_module)"),
        // `prefix ?`'s rollback of the chat it showed when a float still covers it.
        ("main.rs", "hide_module(&chat,&*focus_module)"),
        // The grid probe's own driving.
        ("grid_probe/probes.rs", "hide_module(&term,&|id|self.give_keys(id))"),
    ]
    .iter()
    .map(|(file, call)| (file.to_string(), call.to_string()))
    .collect();
    named.sort();
    assert_eq!(found, named);
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
    // The editor's retirement and an empty chat view's removal, still the shell's, tell the controller after the
    // grid's kill.
    for anchor in [
        "letretire_editor:Rc<dynFn()->Result<(),LayoutError>>=",
        "letdrop_chat_view:Rc<dynFn(&ModuleId)->Result<bool,LayoutError>>=",
    ] {
        let body = let_block(&code, anchor);
        let killed = body.find("grid.kill_module(").expect("the grid's kill");
        let told = body.find("HostEvent::SurfaceEnded{").expect("told to the controller");
        assert!(killed < told, "{anchor}");
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

/// Every key action, Lua run and panel tab verb reaches the controller's runner, and only through it: no
/// shell runner of its own is left, the shell's arms are reached only as the runner's shell verbs, and
/// each call of the runner names where it came from as one of the routes the shell has. A new route is a
/// deliberate edit of this list.
#[test]
fn the_runner_is_the_only_door() {
    const ROUTES: &[&str] = &["Key", "PanelTab", "Lua", "Follow", "Chip"];
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
        if !path.ends_with("shell/src/main.rs") {
            assert!(!flat.contains("run_verb("), "{path} calls the shell's arms");
        }
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
    let code = build_ui_code();
    let arm = call_texts(&code, "wm_host.set_shell_verb_arm(");
    assert_eq!(arm.len(), 1, "one shell verb arm");
    assert_eq!(code.matches("run_verb(").count(), 1, "the shell's arms are called once");
    assert!(
        arm[0].contains("run_verb("),
        "and that call is the shell verb arm: {}",
        arm[0]
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

/// The link's end loads again every page `pages_back` picks from what came back, after the layout took it back:
/// a page built before it was held away would otherwise show what its store said then.
#[test]
fn the_link_ended_hook_loads_again_every_page_that_comes_back() {
    let code = build_ui_code();
    let ended = let_block(&code, "letended:linked_window::spawn::OnEnded=");
    let taken_back = ended
        .find("linked_window::apply_end(&plan,")
        .expect("the end plan is carried out in the layout");
    let reloads = ended
        .find(
            "foridinlinked_window::pages_back(&plan){iflinked_window::reload_page_back(&plugin_panels,&lua_pages,&id)",
        )
        .expect("every page pages_back names is loaded again");
    assert!(taken_back < reloads, "after the layout took them back");
    assert!(
        code.contains("pair.set_hooks(linked_window::spawn::PairHooks{linked,raise,ended})"),
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
/// workspace, reads no chooser pick, and its shell verb arm answers a workspace verb only as a bug; the grid has no
/// switch of its own, and the shell keeps no second copy of the pick's or the made workspace's rules.
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
    let body = let_block(&code, "letrun_verb:Rc<dynFn(Action)>=");
    let bug_arm = "Action::WorkspaceStep{..}|Action::WorkspaceLast|Action::WorkspaceNew|Action::WorkspaceSwitch{..}|Action::WorkspaceChoose|Action::WorkspaceRename=>{eprintln!(";
    assert_eq!(body.matches(bug_arm).count(), 1, "one arm, which reports a bug");
    // `eitri.layout.switch` goes whole to the controller, as every request but a float's.
    assert!(code.contains("ifeitri_core::wm::lua::moved(&request){driver.lua_request(WinId::MAIN,request);continue;}"));
    assert!(eitri_core::wm::lua::moved(
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
    let main = file_code("shell/src/main.rs");
    for helper in ["fnresolve_pick(", "fnnew_workspace_layout(", "enumPick{"] {
        assert!(!main.contains(helper), "main.rs keeps `{helper}`");
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
