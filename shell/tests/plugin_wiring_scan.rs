//! Where the window wires its plugin pages, read from `main.rs` as code: installed before the Lua panels are
//! read (so a plugin is placed, keyed and killed as one), given the window's one editor handle and its layout,
//! stopped by a kill of its module, and shut down when the window closes; told when its module holds the keys
//! and what the editor's `timeoutlen` is; its keys read for `prefix ?` before the chat takes them; and its host
//! given the Super filter and the edge rule for `Ctrl+h/j/k/l`. Also, from `plugin_page.rs`: the kit's reserved
//! path is answered before the plugin's site is looked up.

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
    let kit = serve
        .find("ifpage_policy::kit_request(id,&uri){")
        .expect("the kit path is checked in serve");
    let answered = serve[kit..].find("return;").expect("and answered there") + kit;
    let site = serve.find("page_policy::site_file(").expect("the site lookup");
    // A plugin's own `.eitri/keys.js` would replace the host's kit if the site were looked up first.
    assert!(kit < answered && answered < site);
}
