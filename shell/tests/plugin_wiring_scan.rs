//! Where the window wires its plugin pages, read from `main.rs` as code: installed before the Lua panels are
//! read (so a plugin is placed, keyed and killed as one), given the window's one editor handle and its layout,
//! stopped by a kill of its module, and shut down when the window closes; told when its module holds the keys
//! and what the editor's `timeoutlen` is; its keys read for `prefix ?` before the chat takes them; and its host
//! given the Super filter and the edge rule for `Ctrl+h/j/k/l`. Also, from `plugin_page.rs`: the kit's reserved
//! path is answered before the plugin's site is looked up, and a page build applies and logs its policy and
//! proxy plan, and every answer of the scheme carries `nosniff`.

use eitri_core::source_scan as scan;

fn main_rs() -> String {
    let source = scan::read_required("shell/src/main.rs");
    let code = scan::code_only(&source.text);
    scan::without(&code, &scan::modules(&code, "tests"))
        .split_whitespace()
        .collect()
}

#[test]
fn plugins_are_installed_before_the_lua_panels_are_read() {
    let code = main_rs();
    let install = code
        .find("plugin_panels::PluginPanels::install(&lua_engine,&keymap,&PageTheme::fallback())")
        .expect("installed");
    let panels = code.find("letlua_panels:").expect("the Lua panels are read");
    assert!(install < panels);
}

#[test]
fn the_theme_listener_feeds_every_plugin_page() {
    let code = main_rs();
    let at = code.find("feed_pump.listen_theme(feed,").expect("the theme listener");
    let body = scan::braced(&code, at + code[at..].find('{').unwrap());
    let terminal = body
        .find("pane.set_colors(colors);")
        .expect("every terminal follows the theme");
    // The freshly derived tokens: the page set carries no size, so a size-only change sends the page nothing.
    let pages = body
        .find("plugin_panels.set_theme(&tokens);")
        .expect("every plugin page follows the theme");
    assert!(terminal < pages);
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
    let code = main_rs();
    assert!(code.contains("agent_panel_handle.set_editor_rpc(editor_rpc.clone());"));
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
    let code = main_rs();
    let arm = code
        .find("ModuleKind::LuaWebview=>{grid.kill_module(")
        .expect("the Lua panel kill arm");
    let stop = code[arm..]
        .find("plugin_panels.stop(id);")
        .expect("stopped in that arm");
    let terminate = code[arm..].find("terminate_web_process()").unwrap();
    assert!(stop < terminate, "stopped before its web process ends");
    let close = code.find("window.connect_close_request(").unwrap();
    assert!(code[close..].contains("plugin_panels.shutdown(&app_for_close);"));
}

#[test]
fn a_plugin_hears_its_focus_and_the_editors_timeoutlen() {
    let code = main_rs();
    assert!(code.contains("ModuleKind::LuaWebview=>plugin_panels.set_has_keys(id,has_keys),"));
    let listen = code.find("feed_pump.listen_keys(feed,").expect("the keys feed");
    assert!(code[listen..].contains("plugin_panels.set_timeoutlen(report.timeoutlen);"));
}

#[test]
fn prefix_question_mark_reads_the_plugin_before_the_chat_takes_the_keys() {
    let code = main_rs();
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
    let code = main_rs();
    let arm = code.find("Action::PanelKeymap=>{").expect("the prefix ? arm");
    let body = &code[arm..];
    let show = body.find("show_on_screen(&chat)").expect("the chat is put on screen");
    let kept = body
        .find("keys_return.borrow_mut().opened(")
        .expect("what it opened from is kept");
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
        .find("chat_covered_refusal(view)")
        .expect("the covered check asks about that view");
    let open = body
        .find("agent.open_keymap(view,plugin,opening);")
        .expect("the sheet opens in that view's page");
    assert!(view < chat && chat < show && show < covered && covered < kept && kept < open);

    // Reported only for a move the layout accepted: inside the `Ok(())` arm of `layout.set_focus`.
    let install = code.find("pane_focus::install(").expect("the focus tracker");
    let accept = "layout.set_focus(id){Ok(())=>";
    let accepted = code[install..].find(accept).expect("the layout's accepted-move arm") + install;
    let arm = scan::braced(&code, accepted + accept.len());
    assert!(
        arm.contains("keys_return.borrow_mut().focus_moved("),
        "a focus move is reported from the arm that the layout accepted"
    );
    assert_eq!(
        code.matches("keys_return.borrow_mut().focus_moved(").count(),
        1,
        "and nowhere else, so a refused move is not reported"
    );

    // The close hook's own body asks what to give back.
    let hook = code
        .find("agent_panel_handle.on_keymap_closed({")
        .expect("the close hook is installed");
    let hook_body = scan::braced(&code, hook + "agent_panel_handle.on_keymap_closed(".len());
    assert!(
        hook_body.contains("keys_return.borrow_mut().closed("),
        "the hook asks KeysReturn what to give back"
    );
    // The zoom goes back after the keys: giving them to a page on a tab not drawn chooses that tab,
    // which ends any zoom, so a zoom put back first would be lost. Either kind goes back whole.
    let give = hook_body.find("give_keys(&ret.focus)").expect("the keys go back");
    let zoom = hook_body
        .find("grid.restore_zoom(zoom,&ret.focus)")
        .expect("the zoom goes back through restore_zoom");
    assert!(give < zoom, "the keys go back before the zoom");
    assert!(
        !hook_body.contains("toggle_zoom("),
        "a toggle reads the selection and the tab"
    );
    assert!(
        hook_body.contains("zoom:layout.zoom(),"),
        "the close reads either kind of zoom"
    );
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
        .find("keys_return.borrow_mut().superseded();")
        .expect("the newer request forgets the sheet's pending return");
    let take = line_body.find("give_keys(&chat);").expect("the chat takes the keys");
    assert!(forget < take);
}

#[test]
fn every_web_module_host_gets_the_super_filter_and_lua_pages_swallow_at_edges() {
    let code = main_rs();
    assert!(code.contains("letswallow_at_edge=id.kind()==ModuleKind::LuaWebview;"));
    // Straight after the nav controller and before the loop body ends: for every host, not under an `if`.
    assert!(code.contains(
        "install_module_nav(id.clone(),host,move_focus.clone(),intercept,swallow_at_edge);panel_super::install(host);}"
    ));
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

/// `code`'s byte offset `at` in `text`: `code_only` keeps one character for each character of the source, so the
/// two line up by character, not by byte.
fn source_offset(text: &str, code: &str, at: usize) -> usize {
    let chars = code[..at].chars().count();
    text.char_indices().nth(chars).map_or(text.len(), |(byte, _)| byte)
}

/// The source text (literals kept) of the `code` slice `part`, whitespace removed.
fn source_of(text: &str, code: &str, part: &str) -> String {
    let start = part.as_ptr() as usize - code.as_ptr() as usize;
    let (from, to) = (
        source_offset(text, code, start),
        source_offset(text, code, start + part.len()),
    );
    text[from..to].split_whitespace().collect()
}

#[test]
fn a_page_build_logs_its_policy_and_proxy_and_uses_the_plan() {
    let source = scan::read_required("shell/src/plugin_page.rs");
    // Not cut at the tests module: blanking it could change its byte length, and these offsets must line up with
    // the source by character.
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
    let code = main_rs();
    let hooks: Vec<usize> = code.match_indices("grid.connect_changed(").map(|(at, _)| at).collect();
    let first_hook = *hooks.first().expect("a change hook");
    let builds: Vec<usize> = code
        .match_indices("plugin_panels.build_page(")
        .map(|(at, _)| at)
        .collect();
    assert!(
        builds.iter().any(|at| *at < first_hook),
        "a layout that reopens the module shown builds its page at startup"
    );
    let built_in_a_hook = hooks.iter().any(|at| {
        let after = at + "grid.connect_changed(".len();
        code[after..].starts_with("move||{") && {
            let body = scan::braced(&code, after + "move||".len());
            body.contains("plugin_panels.build_page(")
        }
    });
    assert!(
        built_in_a_hook,
        "a change hook builds the page on the module's first show"
    );
    assert!(code.contains("eitri_core::layout::persist::load_with(dir,project_root,&decls,&deferred)"));
    assert!(code.contains("layout_saver.set_deferred(deferred.clone());"));
    // A switch or an open gives the keys before the change hooks run: the grab builds the page first.
    let focus = code
        .find("letfocus_module:Rc<dynFn(&ModuleId)->bool>=")
        .expect("focus_module");
    let focus = scan::braced(&code, focus + code[focus..].find('{').unwrap());
    assert!(
        focus.contains("plugin_panels.build_page("),
        "the keys never go to a slot with no page"
    );
    assert!(
        code.contains("new_workspace_layout(template,&live,&decls,&deferred)"),
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
    let arm = code
        .find("LayoutRequest::Show(id)if")
        .expect("eitri.layout.show's own arm");
    let arm = scan::braced(&code, arm + code[arm..].find('{').unwrap());
    assert!(
        arm.contains("open_plugin_module("),
        "eitri.layout.show opens it by its rule"
    );
    let open = code
        .find("letopen_module:Rc<dynFn(&ModuleId,KeyAction)>=")
        .expect("open_module");
    let open = scan::braced(&code, open + code[open..].find('{').unwrap());
    assert!(
        open.contains("KeyAction::Place|KeyAction::ShowAndFocusifplugin_panels.starts_hidden(id)=>{"),
        "its module key opens it by its rule"
    );

    let source = scan::read_required("shell/src/main.rs");
    let text = scan::code_only(&source.text);
    let opener = scan::functions(&text, "open_plugin_module");
    assert_eq!(opener.len(), 1, "one way to open it");
    assert!(opener[0].contains("plugin_open_rule(") && opener[0].contains("add_everywhere_at("));
    assert!(
        !opener[0].contains("build_page("),
        "an open the grid refuses builds no page"
    );
    let rule = scan::functions(&text, "plugin_open_rule");
    assert_eq!(rule.len(), 1);
    assert!(rule[0].contains("resolve_own("));

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
    let code = main_rs();
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

    let on_screen = code.match_indices("grid.connect_changed(").any(|(at, _)| {
        let after = at + "grid.connect_changed(".len();
        code[after..].starts_with("move||{")
            && scan::braced(&code, after + "move||".len()).contains("plugin_panels.set_on_screen(")
    });
    assert!(on_screen, "a change hook tells each host whether its module is drawn");

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
    let source = scan::read_required("shell/src/main.rs");
    let text: String = source.text.split_whitespace().collect();
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
