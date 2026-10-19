//! Where the window wires its plugin pages, read from `main.rs` as code: installed before the Lua panels are
//! read (so a plugin is placed, keyed and killed as one), given the window's one editor handle and its layout,
//! stopped by a kill of its module, and shut down when the window closes; told when its module holds the keys
//! and what the editor's `timeoutlen` is; its keys read for `prefix ?` before the chat takes them; and its host
//! given the Super filter and the edge rule for `Ctrl+h/j/k/l`.

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
        .find("plugin_panels::PluginPanels::install(&lua_engine,&keymap)")
        .expect("installed");
    let panels = code.find("letlua_panels:").expect("the Lua panels are read");
    assert!(install < panels);
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
    let focus = code[arm..]
        .find("focus_module(&chat);")
        .expect("the chat takes the keys");
    let open = code[arm..]
        .find("agent.open_keymap(plugin);")
        .expect("opened with the section");
    assert!(read < focus && focus < open);
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
