//! Takeover glue in a private embedded nvim; socket cases also check the real notification wire.
#[path = "support/embed_nvim.rs"]
mod embed_nvim;
#[path = "support/nvim_snapshot.rs"]
mod nvim_snapshot;

use eitri_core::companion::{
    install_args, parse_install_report, takeover_install_args, InstallError, InstallKind, InstallRefusal, Sockets,
    INSTALL_LUA, PAGE_TEARDOWN_LUA, TEARDOWN_LUA,
};
use eitri_core::takeover::glue::{CommandsPart, TakeoverParts, TmuxSwap};
use embed_nvim::EmbedNvim;
use rmpv::Value;
use std::path::PathBuf;

fn parts() -> TakeoverParts {
    let absent = std::env::temp_dir().join(format!("takeover-{}", &uuid::Uuid::new_v4().simple().to_string()[..16]));
    let socket = agent::socket_path::in_dir(&absent, "absent.sock").unwrap();
    TakeoverParts {
        sockets: Sockets {
            editor_context: Some(socket.clone()),
            theme: Some(socket.clone()),
            keys: Some(socket.clone()),
            pane_switch: Some(socket.clone()),
            reload_always: false,
        },
        commands: CommandsPart {
            socket,
            words: eitri_core::nvim_verbs::completion_words(&[], &[]),
            group: Some((
                "<leader>e".into(),
                vec![
                    eitri_core::nvim_verbs::leader::LeaderKey {
                        suffix: "f".into(),
                        words: vec!["focus".into(), "editor".into()],
                        desc: "focus editor".into(),
                    },
                    eitri_core::nvim_verbs::leader::LeaderKey {
                        suffix: "<C-o>".into(),
                        words: vec!["focus".into(), "editor".into()],
                        desc: "focus editor".into(),
                    },
                ],
            )),
        },
        tmux_swap: None,
    }
}

fn install(nvim: &EmbedNvim, chan: u64) {
    let value = nvim.exec_lua(INSTALL_LUA, takeover_install_args(chan, &parts()));
    let report = parse_install_report(&value).unwrap();
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(
        &report.installed[..6],
        [
            "editor_context",
            "theme",
            "keys",
            "nav_fallback",
            "scratch",
            "buffer_reload"
        ]
    );
}

fn detach(nvim: &EmbedNvim, chan: u64) {
    assert_eq!(nvim.exec_lua(TEARDOWN_LUA, vec![chan.into()]).as_bool(), Some(true));
    settle(nvim);
}

fn settle(nvim: &EmbedNvim) {
    nvim.exec_lua("vim.wait(180)", vec![]);
}

fn snapshot(nvim: &EmbedNvim) -> (nvim_snapshot::Snapshot, serde_json::Value) {
    let base = nvim_snapshot::snapshot(&|code| nvim.exec_lua(code, vec![]));
    // These fields widen the shared snapshot without changing other suites' observations.
    let extra = nvim.exec_lua(
        r#"
local buffers = {}
for _, b in ipairs(vim.api.nvim_list_bufs()) do
  local marks = {}
  for name, ns in pairs(vim.api.nvim_get_namespaces()) do
    marks[name] = vim.api.nvim_buf_get_extmarks(b, ns, 0, -1, {details=true})
  end
  buffers[#buffers+1] = { b, vim.bo[b].buftype, vim.bo[b].buflisted, marks }
end
local clients = {}
for _, c in ipairs(vim.lsp.get_clients()) do clients[#clients+1] = {c.id, c.name} end
return vim.json.encode({buffers=buffers, wins=vim.api.nvim_list_wins(), tabs=vim.api.nvim_list_tabpages(),
  socket=vim.env.EITRI_PANE_SWITCH_SOCKET or vim.NIL, clients=clients, diagnostics=vim.diagnostic.config()})
"#,
        vec![],
    );
    (base, serde_json::from_str(extra.as_str().unwrap()).unwrap())
}

fn assert_same(
    before: &(nvim_snapshot::Snapshot, serde_json::Value),
    after: &(nvim_snapshot::Snapshot, serde_json::Value),
) {
    nvim_snapshot::assert_same(&before.0, &after.0);
    assert_eq!(before.1, after.1);
}

fn ready() -> EmbedNvim {
    let nvim = EmbedNvim::start();
    // Load the APIs the widened observation itself uses before taking either snapshot.
    snapshot(&nvim);
    settle(&nvim);
    nvim
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn a_takeover_install_refuses_a_second_takeover_a_companion_and_a_page() {
    let nvim = ready();
    let first = nvim.helper_channel();
    let second = nvim.helper_channel();
    install(&nvim, first);
    settle(&nvim);
    let before = snapshot(&nvim);
    for args in [
        takeover_install_args(second, &parts()),
        install_args(second, &parts().sockets, &InstallKind::Agent),
        install_args(second, &parts().sockets, &InstallKind::Page { id: "markdown".into() }),
    ] {
        let answer = nvim.exec_lua(INSTALL_LUA, args);
        assert_eq!(answer, Value::Map(vec![("refused".into(), "takeover".into())]));
        assert_eq!(
            parse_install_report(&answer),
            Err(InstallError::Refused(InstallRefusal::Takeover))
        );
        assert_same(&before, &snapshot(&nvim));
    }
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn a_takeover_install_replaces_a_companion_and_tears_down_page_slots() {
    let nvim = ready();
    let old = nvim.helper_channel();
    let page = nvim.helper_channel();
    let new = nvim.helper_channel();
    nvim.exec_lua(INSTALL_LUA, install_args(old, &parts().sockets, &InstallKind::Agent));
    nvim.exec_lua(
        INSTALL_LUA,
        install_args(page, &parts().sockets, &InstallKind::Page { id: "markdown".into() }),
    );
    nvim.exec_lua(
        r#"
_G.takeover_notices = {}
_G.takeover_notify = vim.rpcnotify
vim.rpcnotify = function(chan, method, ...)
  takeover_notices[#takeover_notices+1] = {chan, method, __eitri_pages ~= nil}
  return takeover_notify(chan, method, ...)
end
"#,
        vec![],
    );
    install(&nvim, new);
    assert_eq!(
        nvim.exec_lua(
            r#"
local old, page = ...
local seen = {}
for _, n in ipairs(takeover_notices) do
  if n[2] == 'eitri_replaced' then seen[n[1]] = true; if n[1] == page then assert(n[3]) end end
end
return seen[old] and seen[page] and __eitri_pages == nil and __eitri_companion.mode == 'takeover'
"#,
            vec![old.into(), page.into()]
        )
        .as_bool(),
        Some(true)
    );
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn the_liveness_timer_tears_down_when_either_channel_goes() {
    for fork_gone in [false, true] {
        let nvim = ready();
        let link = nvim.helper_channel();
        let fork = nvim.helper_channel();
        install(&nvim, link);
        nvim.exec_lua("_G.__neovide_attach = {chan=...}", vec![fork.into()]);
        nvim.close_channel(if fork_gone { fork } else { link });
        nvim.exec_lua(
            "assert(vim.wait(1000, function() return __eitri_companion == nil end, 10))",
            vec![],
        );
        assert_eq!(
            nvim.exec_lua("return __eitri_window == nil and vim.g.eitri == nil", vec![])
                .as_bool(),
            Some(true)
        );
    }
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn take_back_tears_down_and_closes_the_fork_channel() {
    let nvim = ready();
    let link = nvim.helper_channel();
    let fork = nvim.helper_channel();
    install(&nvim, link);
    nvim.exec_lua(
        r#"
local fork = ...
_G.__neovide_attach = {chan=fork, cleanup=function(chan)
  assert(chan == fork)
  _G.takeover_cleaned = true
  _G.__neovide_attach = nil
end}
_G.takeover_back = __eitri_companion.take_back
assert(takeover_back())
assert(__eitri_companion == nil and __eitri_window == nil)
assert(takeover_cleaned)
assert(vim.wait(1000, function() return vim.api.nvim_get_chan_info(fork).id == nil end, 10))
assert(takeover_back() == false)
"#,
        vec![fork.into()],
    );
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn ui_enter_for_another_channel_notifies_once_and_the_forks_own_does_not() {
    let nvim = ready();
    let link = nvim.helper_channel();
    let fork = nvim.helper_channel();
    let other = nvim.helper_channel();
    nvim.exec_lua(
        r#"
_G.takeover_ui_notices = 0
_G.takeover_notify = vim.rpcnotify
vim.rpcnotify = function(chan, method, ...)
  if method == 'eitri_ui_entered' then takeover_ui_notices = takeover_ui_notices + 1 end
  return takeover_notify(chan, method, ...)
end
"#,
        vec![],
    );
    install(&nvim, link);
    // Ask the helper to send ui_attach on its RPC channel back to its parent.
    nvim.exec_lua(
        r#"
local fork, other = ...
_G.__neovide_attach = {chan=fork}
local attach = "vim.rpcnotify(1, 'nvim_ui_attach', 80, 24, {rgb=true})"
vim.rpcrequest(fork, 'nvim_exec_lua', attach, {})
assert(vim.wait(1000, function() return #vim.api.nvim_list_uis() == 1 end, 10))
assert(takeover_ui_notices == 0)
vim.rpcrequest(other, 'nvim_exec_lua', attach, {})
assert(vim.wait(1000, function() return #vim.api.nvim_list_uis() == 2 end, 10))
assert(takeover_ui_notices == 1)
__eitri_companion.teardown()
vim.rpcrequest(other, 'nvim_exec_lua', "vim.rpcnotify(1, 'nvim_ui_detach')", {})
vim.wait(50)
vim.rpcrequest(other, 'nvim_exec_lua', attach, {})
vim.wait(50)
assert(takeover_ui_notices == 1)
"#,
        vec![fork.into(), other.into()],
    );
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn the_env_swap_is_made_and_undone() {
    let nvim = ready();
    let chan = nvim.helper_channel();
    nvim.exec_lua("vim.env.TMUX='/real,1,0'; vim.env.TMUX_PANE='%3'; vim.env.PATH='/one:/two'; vim.env.EITRI_PANE_SWITCH_SOCKET='/earlier'", vec![]);
    let mut p = parts();
    p.tmux_swap = Some(TmuxSwap {
        tmux: "/swap,2,0".into(),
        tmux_pane: "%9".into(),
        pane_switch_socket: p.commands.socket.clone(),
        shim_dir: PathBuf::from("/shim"),
    });
    let report = nvim.exec_lua(INSTALL_LUA, takeover_install_args(chan, &p));
    assert!(parse_install_report(&report).unwrap().failed.is_empty());
    nvim.exec_lua("assert(vim.env.TMUX == '/swap,2,0' and vim.env.TMUX_PANE == '%9'); assert(vim.env.PATH == '/shim:/one:/two'); assert(vim.env.EITRI_PANE_SWITCH_SOCKET == ...); vim.env.PATH='/plugin:'..vim.env.PATH", vec![p.commands.socket.to_str().unwrap().into()]);
    detach(&nvim, chan);
    nvim.exec_lua("assert(vim.env.PATH == '/plugin:/one:/two'); assert(vim.env.TMUX == '/real,1,0' and vim.env.TMUX_PANE == '%3' and vim.env.EITRI_PANE_SWITCH_SOCKET == '/earlier')", vec![]);
    // A later writer owns the three scalar values; a shim entry deliberately moved stays put.
    nvim.exec_lua(INSTALL_LUA, takeover_install_args(chan, &p));
    nvim.exec_lua("vim.env.TMUX='new'; vim.env.TMUX_PANE='new'; vim.env.EITRI_PANE_SWITCH_SOCKET='new'; vim.env.PATH='/plugin:/one:/shim:/two'", vec![]);
    detach(&nvim, chan);
    nvim.exec_lua("assert(vim.env.PATH == '/plugin:/one:/shim:/two'); assert(vim.env.TMUX == 'new' and vim.env.TMUX_PANE == 'new' and vim.env.EITRI_PANE_SWITCH_SOCKET == 'new')", vec![]);
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn outside_tmux_nothing_is_swapped() {
    let nvim = ready();
    let chan = nvim.helper_channel();
    let before = nvim.exec_lua("return {vim.env.PATH, vim.env.TMUX or false, vim.env.TMUX_PANE or false, vim.env.EITRI_PANE_SWITCH_SOCKET or false}", vec![]);
    let mut p = parts();
    p.tmux_swap = Some(TmuxSwap {
        tmux: "/swap,2,0".into(),
        tmux_pane: "%9".into(),
        pane_switch_socket: p.commands.socket.clone(),
        shim_dir: PathBuf::from("/shim"),
    });
    nvim.exec_lua(INSTALL_LUA, takeover_install_args(chan, &p));
    assert_eq!(before, nvim.exec_lua("return {vim.env.PATH, vim.env.TMUX or false, vim.env.TMUX_PANE or false, vim.env.EITRI_PANE_SWITCH_SOCKET or false}", vec![]));
    detach(&nvim, chan);
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn the_cursor_part_restores_only_its_own_value() {
    for previous in [Value::Nil, 7.into()] {
        let nvim = ready();
        let chan = nvim.helper_channel();
        nvim.exec_lua(
            "vim.g.neovide_cursor_unfocused_outline_width=...",
            vec![previous.clone()],
        );
        install(&nvim, chan);
        assert_eq!(
            nvim.exec_lua("return vim.g.neovide_cursor_unfocused_outline_width", vec![])
                .as_i64(),
            Some(0)
        );
        detach(&nvim, chan);
        assert_eq!(
            nvim.exec_lua("return vim.g.neovide_cursor_unfocused_outline_width", vec![]),
            previous
        );
        install(&nvim, chan);
        nvim.exec_lua("vim.g.neovide_cursor_unfocused_outline_width=9", vec![]);
        detach(&nvim, chan);
        assert_eq!(
            nvim.exec_lua("return vim.g.neovide_cursor_unfocused_outline_width", vec![])
                .as_i64(),
            Some(9)
        );
    }
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn the_reload_part_runs_always_in_a_takeover() {
    let nvim = ready();
    let chan = nvim.helper_channel();
    nvim.exec_lua("_G.takeover_checks=0; vim.api.nvim_buf_set_name(0, vim.env.HOME..'/reload'); vim.fn.writefile({'one'}, vim.api.nvim_buf_get_name(0)); vim.cmd('edit'); vim.o.autoread=true; vim.cmd=function() takeover_checks=takeover_checks+1 end", vec![]);
    install(&nvim, chan);
    nvim.exec_lua("assert(vim.wait(1500, function() return takeover_checks > 0 end, 10)); __eitri_companion.calls.busy(false); local n=takeover_checks; assert(vim.wait(1500, function() return takeover_checks > n end, 10))", vec![]);
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn the_persons_own_eitri_command_and_keys_survive() {
    let nvim = ready();
    let chan = nvim.helper_channel();
    nvim.exec_lua(
        r#"
vim.api.nvim_create_user_command('Eitri', 'let g:person_ran = 1', {})
vim.keymap.set('x', '<leader>ae', '<Nop>', {desc='selection'})
vim.keymap.set('n', '<leader>ex', '<Nop>', {desc='person'})
"#,
        vec![],
    );
    let before = snapshot(&nvim);
    install(&nvim, chan);
    detach(&nvim, chan);
    assert_same(&before, &snapshot(&nvim));
    // Replacements made after the install survive too, even with the same descriptions.
    nvim.exec_lua(
        "vim.api.nvim_del_user_command('Eitri'); vim.keymap.del('n','<leader>ex')",
        vec![],
    );
    install(&nvim, chan);
    nvim.exec_lua("vim.api.nvim_create_user_command('Eitri', 'echo 42', {force=true, desc='eitri: run a window or layout action'}); vim.keymap.set('n', '<leader>ef', '<Nop>', {desc='Eitri: focus editor'}); _G.__eitri_window={person=true}; vim.g.eitri=9", vec![]);
    detach(&nvim, chan);
    nvim.exec_lua("assert(vim.api.nvim_get_commands({}).Eitri.definition == 'echo 42'); assert(vim.fn.maparg('<leader>ef','n') == '<Nop>'); assert(__eitri_window.person and vim.g.eitri == 9)", vec![]);
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn leftover_after_detach_and_after_link_kill() {
    for killed in [false, true] {
        let nvim = ready();
        let before = snapshot(&nvim);
        for _ in 0..3 {
            let chan = nvim.helper_channel();
            install(&nvim, chan);
            if !killed {
                detach(&nvim, chan);
            }
            nvim.close_channel(chan);
            nvim.exec_lua(
                "assert(vim.wait(1000, function() return __eitri_companion == nil end, 10))",
                vec![],
            );
            settle(&nvim);
            assert_same(&before, &snapshot(&nvim));
        }
    }
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn leftover_with_page_slots() {
    for case in ["existing", "closed", "refused-killed"] {
        let nvim = ready();
        let before = snapshot(&nvim);
        let page = nvim.helper_channel();
        let chan = nvim.helper_channel();
        if case != "refused-killed" {
            nvim.exec_lua(
                INSTALL_LUA,
                install_args(page, &parts().sockets, &InstallKind::Page { id: "markdown".into() }),
            );
            if case == "closed" {
                nvim.exec_lua(PAGE_TEARDOWN_LUA, vec!["markdown".into(), page.into()]);
            }
        }
        install(&nvim, chan);
        if case == "refused-killed" {
            let reply = nvim.exec_lua(
                INSTALL_LUA,
                install_args(page, &parts().sockets, &InstallKind::Page { id: "markdown".into() }),
            );
            assert_eq!(
                parse_install_report(&reply),
                Err(InstallError::Refused(InstallRefusal::Takeover))
            );
        }
        // jobstop closes only the helper process owned by this test, as a killed page's RPC EOF does.
        nvim.close_channel(page);
        assert_eq!(
            nvim.exec_lua("return __eitri_companion.chan", vec![]).as_u64(),
            Some(chan)
        );
        detach(&nvim, chan);
        nvim.close_channel(chan);
        assert_eq!(
            nvim.exec_lua("return __eitri_pages == nil", vec![]).as_bool(),
            Some(true)
        );
        assert_same(&before, &snapshot(&nvim));
    }
}

fn answer(pending: eitri_core::nvim_rpc::Pending) -> Value {
    pending
        .wait(std::time::Duration::from_secs(2))
        .expect("nvim must answer")
        .unwrap()
}

#[test]
#[ignore = "needs real nvim and private Unix sockets; no display"]
fn socket_notices_and_ui_enter_are_delivered_to_the_link() {
    use eitri_core::nvim_rpc::{LinkEvent, NvimLink};
    use std::time::Duration;
    let nvim = ready();
    let home = nvim.exec_lua("return vim.env.HOME", vec![]);
    let path = agent::socket_path::in_dir(&PathBuf::from(home.as_str().unwrap()), "origin.sock").unwrap();
    nvim.exec_lua("return vim.fn.serverstart(...)", vec![path.to_str().unwrap().into()]);
    let (page, page_events) = NvimLink::connect(&path, Duration::from_secs(2)).unwrap();
    let (link, events) = NvimLink::connect(&path, Duration::from_secs(2)).unwrap();
    let (fork, _fork_events) = NvimLink::connect(&path, Duration::from_secs(2)).unwrap();
    let (other, _other_events) = NvimLink::connect(&path, Duration::from_secs(2)).unwrap();
    let r = answer(page.exec_lua(
        INSTALL_LUA,
        install_args(
            page.channel_id(),
            &parts().sockets,
            &InstallKind::Page { id: "markdown".into() },
        ),
    ));
    assert!(parse_install_report(&r).unwrap().failed.is_empty());
    let handle = eitri_core::takeover::glue::takeover_install_link(&link, &parts()).unwrap();
    assert_eq!(handle.link_chan, link.channel_id());
    assert!(matches!(
        eitri_core::takeover::glue::takeover_install_link(&page, &parts()),
        Err(InstallError::Refused(InstallRefusal::Takeover))
    ));
    assert!(page.is_alive(), "a refusal preserves the caller's connection");
    assert!(
        matches!(page_events.recv_timeout(Duration::from_secs(1)).unwrap(), LinkEvent::Notification {method, ..} if method == "eitri_replaced")
    );
    answer(link.exec_lua("_G.__neovide_attach={chan=...}", vec![fork.channel_id().into()]));
    let attach = |ui: &NvimLink| {
        answer(ui.call(
            "nvim_ui_attach",
            vec![80.into(), 24.into(), Value::Map(vec![("rgb".into(), true.into())])],
        ));
    };
    attach(&fork);
    assert!(events.recv_timeout(Duration::from_millis(100)).is_err());
    attach(&other);
    assert!(
        matches!(events.recv_timeout(Duration::from_secs(1)).unwrap(), LinkEvent::Notification {method, ..} if method == "eitri_ui_entered")
    );
    assert!(events.recv_timeout(Duration::from_millis(100)).is_err());
    link.close();
    nvim.exec_lua(
        "assert(vim.wait(1000, function() return __eitri_companion == nil end, 10))",
        vec![],
    );
    fork.close();
    other.close();
    page.close();
    nvim.exec_lua("vim.fn.serverstop(...)", vec![path.to_str().unwrap().into()]);
}

#[test]
#[ignore = "needs real nvim and private Unix sockets; no display"]
fn socket_a_refused_page_killed_with_sigkill_leaves_the_takeover_and_no_pages() {
    let nvim = ready();
    let home = nvim.exec_lua("return vim.env.HOME", vec![]);
    let path = agent::socket_path::in_dir(&PathBuf::from(home.as_str().unwrap()), "origin.sock").unwrap();
    nvim.exec_lua("return vim.fn.serverstart(...)", vec![path.to_str().unwrap().into()]);
    let before = snapshot(&nvim);
    let chan = nvim.helper_channel();
    install(&nvim, chan);
    let page_process = EmbedNvim::start();
    let page_chan = page_process
        .exec_lua(
            "return vim.fn.sockconnect('pipe', ..., {rpc=true})",
            vec![path.to_str().unwrap().into()],
        )
        .as_u64()
        .unwrap();
    let origin_chan = page_process
        .exec_lua(
            "return vim.rpcrequest(..., 'nvim_get_api_info')[1]",
            vec![page_chan.into()],
        )
        .as_u64()
        .unwrap();
    let r = nvim.exec_lua(
        INSTALL_LUA,
        install_args(
            origin_chan,
            &parts().sockets,
            &InstallKind::Page { id: "markdown".into() },
        ),
    );
    assert_eq!(
        parse_install_report(&r),
        Err(InstallError::Refused(InstallRefusal::Takeover))
    );
    drop(page_process);
    nvim.exec_lua(
        "vim.wait(600); assert(__eitri_companion.chan == ... and __eitri_pages == nil)",
        vec![chan.into()],
    );
    detach(&nvim, chan);
    nvim.close_channel(chan);
    assert_same(&before, &snapshot(&nvim));
    nvim.exec_lua("vim.fn.serverstop(...)", vec![path.to_str().unwrap().into()]);
}

fn takeover_ready() -> EmbedNvim {
    let nvim = ready();
    // Builtin modules and namespace ids persist for nvim's lifetime. Start with its file and LSP APIs initialized.
    nvim.exec_lua("require('editorconfig'); vim.filetype.match({filename='sample.txt'}); vim.lsp.inlay_hint.is_enabled({bufnr=0}); vim.diagnostic.get(0); vim.text.hexencode(''); vim.api.nvim_create_namespace('eitri_review'); vim.api.nvim_create_namespace('eitri_agent_view')", vec![]);
    nvim
}

struct TakeoverTransport<'a>(&'a EmbedNvim);
impl eitri_core::editor_rpc::EditorRpc for TakeoverTransport<'_> {
    fn target(&self) -> Option<u64> {
        Some(1)
    }
    fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> eitri_core::nvim_rpc::Pending {
        let (answer, pending) = eitri_core::nvim_rpc::Pending::pair();
        answer.send(
            self.0
                .try_exec_lua(code, args)
                .map_err(|e| eitri_core::nvim_rpc::RpcError::Nvim(e.to_string())),
        );
        pending
    }
}

fn run(rpc: &dyn eitri_core::editor_rpc::EditorRpc, code: &'static str, args: Vec<Value>) -> Value {
    rpc.exec_lua(code, args).try_take().unwrap().unwrap()
}

fn tool_args(path: &str, seq: u64) -> Vec<Value> {
    vec![
        Value::Map(vec![
            ("scope".into(), "takeover-test".into()),
            ("seq".into(), seq.into()),
            ("deadline_ms".into(), (u64::MAX / 2).into()),
        ]),
        Value::Map(vec![
            ("path".into(), path.into()),
            ("load".into(), true.into()),
            ("id".into(), format!("takeover-{seq}").into()),
            ("kind".into(), "hover".into()),
        ]),
    ]
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn read_tool_buffers_are_unloaded_at_teardown_and_opened_files_stay() {
    use eitri_core::editor_tools::tools::{lsp::LSP_ASK_LUA, open_file::OPEN_FILE_LUA};
    let nvim = takeover_ready();
    let chan = nvim.helper_channel();
    let paths = nvim.exec_lua(
        r#"
local paths = {}
for _, name in ipairs({'hidden', 'opened', 'modified', 'shown'}) do
  local path = vim.fn.getcwd() .. '/' .. name
  vim.fn.writefile({'hello'}, path)
  paths[#paths+1] = path
end
return paths
"#,
        vec![],
    );
    let paths = paths.as_array().unwrap();
    install(&nvim, chan);
    let transport = std::rc::Rc::new(TakeoverTransport(&nvim));
    let rpc = eitri_core::editor_rpc::TakeoverRpc::new(transport, chan);
    for (i, path) in paths.iter().enumerate() {
        run(
            &rpc,
            if i == 1 { OPEN_FILE_LUA } else { LSP_ASK_LUA },
            tool_args(path.as_str().unwrap(), i as u64 + 1),
        );
    }
    nvim.exec_lua(
        r#"
local paths = ...
vim.api.nvim_buf_set_lines(vim.fn.bufnr(paths[3]), 0, -1, false, {'unsaved'})
vim.api.nvim_open_win(vim.fn.bufnr(paths[4]), false, {split='below'})
"#,
        vec![Value::Array(paths.clone())],
    );
    detach(&nvim, chan);
    assert_eq!(
        nvim.exec_lua(
            r#"
local paths = ...
return {
  not vim.api.nvim_buf_is_loaded(vim.fn.bufnr(paths[1])),
  vim.api.nvim_buf_is_loaded(vim.fn.bufnr(paths[2])),
  vim.api.nvim_buf_is_loaded(vim.fn.bufnr(paths[3])),
  vim.api.nvim_buf_is_loaded(vim.fn.bufnr(paths[4])),
  __eitri_agent == nil,
}
"#,
            vec![Value::Array(paths.clone())]
        ),
        Value::Array(vec![true.into(); 5])
    );
}

fn has_key(value: &Value, key: &str) -> bool {
    value
        .as_map()
        .is_some_and(|map| map.iter().any(|(k, _)| k.as_str() == Some(key)))
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn a_buffer_the_person_used_or_the_open_tool_showed_stays_after_it_is_hidden() {
    use eitri_core::editor_tools::tools::{lsp::LSP_ASK_LUA, open_file::OPEN_FILE_LUA};
    let nvim = takeover_ready();
    let chan = nvim.helper_channel();
    let paths = nvim.exec_lua(
        r#"
local paths = {}
for _, name in ipairs({'read_only', 'person', 'person_away', 'opened', 'read_then_opened', 'away_one', 'away_two'}) do
  local path = vim.fn.getcwd() .. '/' .. name
  vim.fn.writefile({'hello'}, path)
  paths[#paths+1] = path
end
return paths
"#,
        vec![],
    );
    let paths = paths.as_array().unwrap();
    let path = |i: usize| paths[i].as_str().unwrap();
    install(&nvim, chan);
    let transport = std::rc::Rc::new(TakeoverTransport(&nvim));
    let rpc = eitri_core::editor_rpc::TakeoverRpc::new(transport, chan);
    // The read tools load these three; the person then opens one of them for themselves.
    run(&rpc, LSP_ASK_LUA, tool_args(path(0), 1));
    run(&rpc, LSP_ASK_LUA, tool_args(path(1), 2));
    run(&rpc, LSP_ASK_LUA, tool_args(path(4), 3));
    nvim.exec_lua(
        "vim.cmd.edit(vim.fn.fnameescape((...)[1])); vim.cmd.edit(vim.fn.fnameescape((...)[2]))",
        vec![Value::Array(vec![paths[1].clone(), paths[2].clone()])],
    );
    // The open tool shows a file in the agent's window and reuses that window for the next one, so the earlier
    // file is hidden by the time the takeover ends. One of them had been loaded by a read tool first.
    for (seq, i) in [(4, 3), (5, 4), (6, 5), (7, 6)] {
        let shown = run(&rpc, OPEN_FILE_LUA, tool_args(path(i), seq));
        assert!(has_key(&shown, "shown"), "{shown}");
    }
    detach(&nvim, chan);
    assert_eq!(
        nvim.exec_lua(
            r#"
local paths = ...
local function loaded(path)
  local buf = vim.fn.bufnr(path)
  return buf >= 0 and vim.api.nvim_buf_is_loaded(buf)
end
return {
  not loaded(paths[1]),
  loaded(paths[2]),
  loaded(paths[4]),
  loaded(paths[5]),
}
"#,
            vec![Value::Array(paths.clone())]
        ),
        Value::Array(vec![true.into(); 4])
    );
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn a_preloaded_buffer_the_person_opened_gets_its_own_policy_back_at_detach() {
    use eitri_core::plugin::editor as plugin;
    let nvim = takeover_ready();
    let chan = nvim.helper_channel();
    let file = nvim.exec_lua(
        "local p = vim.fn.getcwd() .. '/preloaded.txt'; vim.fn.writefile({'x'}, p); return vim.uv.fs_realpath(p)",
        vec![],
    );
    let path = std::path::PathBuf::from(file.as_str().unwrap());
    install(&nvim, chan);
    let transport = std::rc::Rc::new(TakeoverTransport(&nvim));
    let rpc = eitri_core::editor_rpc::TakeoverRpc::new(transport, chan);
    let preloaded = plugin::preload(&rpc, "takeover-plugin", &path, false, false)
        .try_take()
        .unwrap()
        .unwrap();
    assert!(plugin::decode_preload(&preloaded).unwrap().buf > 0, "{preloaded}");
    let state = r#"
local buf = vim.fn.bufnr((...))
return { vim.diagnostic.is_enabled({ bufnr = buf }), vim.b[buf].eitri_plugin_preload == nil }
"#;
    assert_eq!(
        nvim.exec_lua(state, vec![file.clone()]),
        Value::Array(vec![false.into(), false.into()])
    );
    nvim.exec_lua("vim.cmd.edit(vim.fn.fnameescape((...)))", vec![file.clone()]);
    detach(&nvim, chan);
    assert_eq!(
        nvim.exec_lua(state, vec![file]),
        Value::Array(vec![true.into(), true.into()]),
        "the person's buffer keeps none of the plugin's settings"
    );
}

fn scratch_diff(nvim: &EmbedNvim) {
    nvim.exec_lua(
        r#"
local base, proposed = vim.fn.getcwd() .. '/base', vim.fn.getcwd() .. '/proposed'
vim.fn.writefile({'before'}, base)
vim.fn.writefile({'after'}, proposed)
local text = vim.json.encode({op='diff', base=base, proposed=proposed, name='sample.txt'})
EitriScratch.call((text:gsub('.', function(c) return ('%02x'):format(c:byte()) end)))
"#,
        vec![],
    );
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn the_scratch_teardown_closes_its_tabs_and_wipes_its_buffers() {
    let nvim = takeover_ready();
    let chan = nvim.helper_channel();
    let before = snapshot(&nvim);
    install(&nvim, chan);
    scratch_diff(&nvim);
    assert_eq!(
        nvim.exec_lua("return #vim.api.nvim_list_tabpages()", vec![]).as_u64(),
        Some(2)
    );
    detach(&nvim, chan);
    assert_same(&before, &snapshot(&nvim));
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn three_takeovers_in_a_row_leave_nothing() {
    let nvim = takeover_ready();
    let chan = nvim.helper_channel();
    let before = snapshot(&nvim);
    for _ in 0..3 {
        install(&nvim, chan);
        scratch_diff(&nvim);
        detach(&nvim, chan);
        assert_same(&before, &snapshot(&nvim));
    }
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn leftover_with_every_module_in_use() {
    use eitri_core::{ide_server::view, plugin::editor as plugin, review_editor as review};
    let nvim = takeover_ready();
    let chan = nvim.helper_channel();
    // Namespace ids are permanent in nvim; allocate them before observing their contents.
    nvim.exec_lua(
        "vim.api.nvim_create_namespace('eitri_review'); vim.api.nvim_create_namespace('eitri_agent_view')",
        vec![],
    );
    // The person's own file is in place before the first snapshot, so the one comparison is against the state
    // before any takeover at all.
    let file = nvim.exec_lua("return vim.fn.getcwd() .. '/sample.txt'", vec![]);
    nvim.exec_lua("local path = ...; vim.fn.writefile({'after'}, path); vim.api.nvim_buf_set_name(0, path); vim.api.nvim_buf_set_lines(0,0,-1,false,{'after'}); vim.bo.modified=false", vec![file.clone()]);
    let before = snapshot(&nvim);
    install(&nvim, chan);
    let transport = std::rc::Rc::new(TakeoverTransport(&nvim));
    let rpc = eitri_core::editor_rpc::TakeoverRpc::new(transport, chan);
    run(&rpc, review::REVIEW_LUA, review::install_args(review::Owner::Embedded));
    let path = std::path::Path::new(file.as_str().unwrap());
    let meta = review::ShowMeta {
        tab: 1,
        session: "takeover".into(),
        turn: 1,
        scope: eitri_core::turn_review::Scope::Turn,
    };
    let shown = run(
        &rpc,
        review::SHOW_LUA,
        review::show_args(
            path,
            &meta,
            &[review::ShowHunk::from_file_lines(
                1,
                (1, 1),
                (1, 1),
                &[b"before\n".to_vec()],
                &[b"after\n".to_vec()],
            )],
        ),
    );
    assert!(
        shown
            .as_map()
            .unwrap()
            .iter()
            .any(|(k, v)| k.as_str() == Some("drawn") && v.as_u64() == Some(1)),
        "{shown}"
    );
    plugin::assist_apply(&rpc, "takeover-plugin", path.parent().unwrap(), false, false)
        .try_take()
        .unwrap()
        .unwrap();
    run(
        &rpc,
        plugin::PLUGIN_BUFFERS_LUA,
        vec![
            "diff".into(),
            Value::Map(vec![
                ("owner".into(), "takeover-plugin".into()),
                (
                    "left".into(),
                    Value::Map(vec![("title".into(), "old".into()), ("text".into(), "before".into())]),
                ),
                (
                    "right".into(),
                    Value::Map(vec![("title".into(), "new".into()), ("text".into(), "after".into())]),
                ),
            ]),
        ],
    );
    let mut args = tool_args(path.to_str().unwrap(), 10);
    args[1] = Value::Map(vec![
        ("view".into(), 1.into()),
        ("real".into(), file),
        ("rel".into(), "sample.txt".into()),
        ("file_name".into(), "eitri-file://takeover".into()),
        ("proposal_name".into(), "eitri-proposal://takeover".into()),
        ("file_lines".into(), Value::Array(vec!["before".into()])),
        ("proposal_lines".into(), Value::Array(vec!["after".into()])),
        ("file_text".into(), "before\n".into()),
        ("crlf".into(), false.into()),
        ("eol".into(), true.into()),
    ]);
    run(&rpc, view::IDE_DIFF_OPEN, args);
    scratch_diff(&nvim);
    detach(&nvim, chan);
    assert_same(&before, &snapshot(&nvim));
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn window_2s_editor_ops_go_through_the_same_check() {
    use eitri_core::window_link::{
        editor_ops::{self, Dispatch, OpContext},
        EditorOp,
    };
    struct Context(PathBuf);
    impl OpContext for Context {
        fn project_root(&self) -> &std::path::Path {
            &self.0
        }
        fn peer(&self) -> eitri_core::wm::PeerId {
            eitri_core::wm::PeerId(2)
        }
        fn session_in_window2(&self, _: &str) -> bool {
            true
        }
        fn plugin_roots_in_window2(&self, _: &str) -> Option<std::collections::BTreeMap<String, PathBuf>> {
            None
        }
        fn drawn_for_window2(&self) -> Vec<PathBuf> {
            vec![]
        }
    }
    let nvim = takeover_ready();
    let chan = nvim.helper_channel();
    install(&nvim, chan);
    let root = nvim.exec_lua("return vim.fn.getcwd()", vec![]);
    let transport = std::rc::Rc::new(TakeoverTransport(&nvim));
    let rpc = eitri_core::editor_rpc::TakeoverRpc::new(transport, chan);
    let op = serde_json::from_value::<EditorOp>(serde_json::json!({"op":"agent_editor_state","session":"takeover","stamp":{"scope":"takeover","seq":1,"deadline_ms":4000000000000_u64}})).unwrap();
    let Dispatch::Rpc(pending) = editor_ops::dispatch(1, op, &Context(root.as_str().unwrap().into()), &rpc).unwrap()
    else {
        panic!("state must ask the editor")
    };
    pending.try_take().unwrap().unwrap();
    assert_eq!(
        nvim.exec_lua("return __eitri_agent ~= nil", vec![]).as_bool(),
        Some(true)
    );
    detach(&nvim, chan);
    assert_eq!(
        nvim.exec_lua("return __eitri_agent == nil", vec![]).as_bool(),
        Some(true)
    );
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn registered_modules_reject_calls_after_their_takeover_ended() {
    use eitri_core::{editor_tools::tools::lsp::LSP_ASK_LUA, plugin::editor as plugin};
    let nvim = takeover_ready();
    let old = nvim.helper_channel();
    let new = nvim.helper_channel();
    let transport = std::rc::Rc::new(TakeoverTransport(&nvim));
    let rpc = eitri_core::editor_rpc::TakeoverRpc::new(transport, old);
    install(&nvim, old);
    detach(&nvim, old);
    for replacement in [false, true] {
        if replacement {
            install(&nvim, new);
        }
        let before = snapshot(&nvim);
        for value in [
            run(
                &rpc,
                plugin::PLUGIN_ASSIST_LUA,
                vec![
                    "apply".into(),
                    Value::Map(vec![("owner".into(), "takeover-plugin".into())]),
                ],
            ),
            run(&rpc, LSP_ASK_LUA, tool_args("/absent", 1)),
        ] {
            assert!(value
                .as_map()
                .unwrap()
                .iter()
                .any(|(k, _)| k.as_str() == Some("refused")));
        }
        assert_same(&before, &snapshot(&nvim));
    }
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn mention_commands_are_registered_without_replacing_personal_commands() {
    use eitri_core::ide_server::mention::IDE_MENTION_DEFINE;
    let nvim = takeover_ready();
    let chan = nvim.helper_channel();
    let transport = std::rc::Rc::new(TakeoverTransport(&nvim));
    let rpc = eitri_core::editor_rpc::TakeoverRpc::new(transport, chan);
    let before = snapshot(&nvim);
    install(&nvim, chan);
    assert_eq!(
        run(&rpc, IDE_MENTION_DEFINE, vec!["/absent".into()]).as_bool(),
        Some(true)
    );
    run(&rpc, eitri_core::ide_server::mention::IDE_MENTION_REMOVE, vec![]);
    assert_eq!(
        run(&rpc, IDE_MENTION_DEFINE, vec!["/absent".into()]).as_bool(),
        Some(true)
    );
    detach(&nvim, chan);
    assert_same(&before, &snapshot(&nvim));
    nvim.exec_lua(
        "vim.api.nvim_create_user_command('EitriMention', function() end, {desc='personal mention'})",
        vec![],
    );
    let before = snapshot(&nvim);
    install(&nvim, chan);
    assert_eq!(
        run(&rpc, IDE_MENTION_DEFINE, vec!["/absent".into()]).as_bool(),
        Some(false)
    );
    detach(&nvim, chan);
    assert_same(&before, &snapshot(&nvim));
}
