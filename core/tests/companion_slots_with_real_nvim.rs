#[path = "support/embed_nvim.rs"]
mod embed_nvim;
use eitri_core::plugin::editor::PLUGIN_FOLLOW_LUA;
use embed_nvim::EmbedNvim;
use rmpv::Value;

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn the_embed_harness_needs_no_socket() {
    let nvim = EmbedNvim::start();
    nvim.manual_timers();
    let first = nvim.helper_channel();
    let second = nvim.helper_channel();
    assert_ne!(first, second);
    nvim.close_channel(first);
    assert_eq!(
        nvim.exec_lua("return vim.api.nvim_get_chan_info(...).id", vec![Value::from(second)])
            .as_u64(),
        Some(second)
    );
    nvim.close_channel(second);
}

#[path = "support/nvim_snapshot.rs"]
mod nvim_snapshot;
use eitri_core::companion::{
    install_args, page_id_ok, parse_install_report, InstallError, InstallKind, InstallRefusal, Sockets, INSTALL_LUA,
    PAGE_TEARDOWN_LUA, SLOT_CALL_LUA, TEARDOWN_LUA,
};
use nvim_snapshot::{assert_same, snapshot};

fn sockets() -> Sockets {
    let absent = agent::socket_path::in_dir(
        &std::env::temp_dir(),
        &format!("es-{}.sock", uuid::Uuid::new_v4().simple()),
    )
    .unwrap();
    Sockets {
        editor_context: Some(absent.clone()),
        theme: Some(absent.clone()),
        keys: Some(absent.clone()),
        pane_switch: Some(absent),
        reload_always: false,
    }
}

fn install(nvim: &EmbedNvim, chan: u64, kind: InstallKind) {
    let reply = nvim.exec_lua(INSTALL_LUA, install_args(chan, &sockets(), &kind));
    let report = parse_install_report(&reply).unwrap();
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    let (slot, parts): (&str, &[&str]) = match kind {
        InstallKind::Agent => (
            "agent",
            &[
                "editor_context",
                "theme",
                "keys",
                "nav_fallback",
                "scratch",
                "buffer_reload",
            ],
        ),
        InstallKind::Page { .. } => ("page", &["theme"]),
    };
    assert_eq!(report.slot.as_deref(), Some(slot));
    assert_eq!(report.installed, parts);
}

fn page(nvim: &EmbedNvim, chan: u64, id: &str) {
    install(nvim, chan, InstallKind::Page { id: id.into() });
}

fn teardown(nvim: &EmbedNvim, chan: u64, id: Option<&str>) -> Value {
    match id {
        Some(id) => nvim.exec_lua(PAGE_TEARDOWN_LUA, vec![Value::from(id), Value::from(chan)]),
        None => nvim.exec_lua(TEARDOWN_LUA, vec![Value::from(chan)]),
    }
}

fn settle(nvim: &EmbedNvim) {
    nvim.advance_timers(180);
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn an_agent_and_a_page_slot_coexist() {
    let nvim = EmbedNvim::start();
    nvim.manual_timers();
    let agent = nvim.helper_channel();
    let p = nvim.helper_channel();
    install(&nvim, agent, InstallKind::Agent);
    page(&nvim, p, "markdown");
    assert_eq!(
        nvim.exec_lua(
            r#"
local a, p = _G.__eitri_companion, _G.__eitri_pages.markdown
return type(a) == 'table' and type(p) == 'table' and a.timer ~= p.timer
  and a.timer:is_active() and p.timer:is_active() and a.timer:get_repeat() == 500 and p.timer:get_repeat() == 500
"#,
            vec![]
        )
        .as_bool(),
        Some(true)
    );
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn theme_groups_of_two_slots_coexist() {
    let nvim = EmbedNvim::start();
    nvim.manual_timers();
    install(&nvim, nvim.helper_channel(), InstallKind::Agent);
    let before = nvim.exec_lua("return #vim.api.nvim_get_autocmds({group='EitriThemeFeed'})", vec![]);
    page(&nvim, nvim.helper_channel(), "markdown");
    assert!(before.as_i64().unwrap() > 0);
    assert_eq!(
        nvim.exec_lua("return #vim.api.nvim_get_autocmds({group='EitriThemeFeed'})", vec![]),
        before
    );
    assert_eq!(
        nvim.exec_lua(
            "return #vim.api.nvim_get_autocmds({group='EitriThemeFeed_page_markdown'})",
            vec![]
        ),
        before
    );
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn an_agent_install_leaves_page_slots_alone() {
    let nvim = EmbedNvim::start();
    nvim.manual_timers();
    page(&nvim, nvim.helper_channel(), "markdown");
    nvim.exec_lua("_G.saved_page = __eitri_pages.markdown", vec![]);
    install(&nvim, nvim.helper_channel(), InstallKind::Agent);
    install(&nvim, nvim.helper_channel(), InstallKind::Agent);
    assert_eq!(
        nvim.exec_lua(
            "return saved_page == __eitri_pages.markdown and saved_page.timer:is_active()",
            vec![]
        )
        .as_bool(),
        Some(true)
    );
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn a_second_page_install_of_one_id_tears_the_first_slot_down() {
    let nvim = EmbedNvim::start();
    nvim.manual_timers();
    install(&nvim, nvim.helper_channel(), InstallKind::Agent);
    let first = nvim.helper_channel();
    page(&nvim, first, "markdown");
    page(&nvim, nvim.helper_channel(), "other");
    nvim.exec_lua("_G.saved_agent = __eitri_companion; _G.saved_page = __eitri_pages.markdown; _G.saved_other = __eitri_pages.other; saved_page.adopt(function() _G.was_torn = true end)", vec![]);
    let second = nvim.helper_channel();
    page(&nvim, second, "markdown");
    assert_eq!(
        nvim.exec_lua(
            r#"
return was_torn and saved_page.torn and saved_page.timer:is_closing()
  and not saved_page.adopt(function() error('a torn slot cannot adopt') end)
  and saved_agent == __eitri_companion and saved_agent.timer:is_active()
  and saved_other == __eitri_pages.other and saved_other.timer:is_active()
  and saved_page ~= __eitri_pages.markdown and __eitri_pages.markdown.chan == ...
"#,
            vec![Value::from(second)]
        )
        .as_bool(),
        Some(true)
    );
    assert_eq!(teardown(&nvim, first, Some("markdown")).as_bool(), Some(false));
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn two_page_slots_in_two_nvims_are_independent() {
    let first = EmbedNvim::start();
    first.manual_timers();
    let second = EmbedNvim::start();
    second.manual_timers();
    let a = first.helper_channel();
    let b = second.helper_channel();
    assert_eq!(a, b, "channel numbers are local to each nvim");
    page(&first, a, "markdown");
    page(&second, b, "markdown");
    install(&second, second.helper_channel(), InstallKind::Agent);
    first.close_channel(a);
    first.exec_lua(
        "assert(test_timers.until_ready(function() return __eitri_pages == nil end))",
        vec![],
    );
    assert_eq!(
        second
            .exec_lua(
                "return __eitri_pages.markdown.timer:is_active() and __eitri_companion.timer:is_active()",
                vec![]
            )
            .as_bool(),
        Some(true)
    );
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn page_teardown_and_slot_call_from_another_channel_change_nothing() {
    let nvim = EmbedNvim::start();
    nvim.manual_timers();
    let chan = nvim.helper_channel();
    let other = nvim.helper_channel();
    page(&nvim, chan, "markdown");
    nvim.exec_lua("__eitri_pages.markdown.calls.echo = function(x) return x end", vec![]);
    settle(&nvim);
    let before = snapshot(&|code| nvim.exec_lua(code, vec![]));
    assert_eq!(teardown(&nvim, other, Some("markdown")).as_bool(), Some(false));
    let call = |slot: &str, c| {
        nvim.exec_lua(
            SLOT_CALL_LUA,
            vec![
                Value::from(slot),
                Value::from(c),
                Value::from("echo"),
                Value::Array(vec![Value::from(19)]),
            ],
        )
    };
    assert_eq!(call("page:markdown", other).as_bool(), Some(false));
    assert_eq!(call("page:missing", chan).as_bool(), Some(false));
    assert_eq!(call("page:markdown", chan).as_i64(), Some(19));
    assert_same(&before, &snapshot(&|code| nvim.exec_lua(code, vec![])));
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn a_closed_page_channel_takes_its_slot_within_one_second() {
    let nvim = EmbedNvim::start();
    nvim.manual_timers();
    let chan = nvim.helper_channel();
    page(&nvim, chan, "markdown");
    nvim.exec_lua("_G.saved_page = __eitri_pages.markdown", vec![]);
    nvim.close_channel(chan);
    nvim.exec_lua(
        r#"
assert(test_timers.until_ready(function() return __eitri_pages == nil end))
assert(saved_page.torn and saved_page.timer:is_closing())
assert(not pcall(vim.api.nvim_get_autocmds, {group='EitriThemeFeed_page_markdown'}))
"#,
        vec![],
    );
}

/// nvim 0.10 answers `nvim_get_chan_info` for a closed channel with a typed empty dictionary,
/// `{[vim.type_idx] = vim.types.dictionary}`, which `next()` does not see as empty; the slot still goes.
#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn a_closed_channel_answered_as_a_typed_empty_dict_takes_the_slot() {
    let nvim = EmbedNvim::start();
    nvim.manual_timers();
    let chan = nvim.helper_channel();
    page(&nvim, chan, "markdown");
    nvim.exec_lua(
        r#"
local chan = ...
_G.saved_page = __eitri_pages.markdown
local real = vim.api.nvim_get_chan_info
vim.api.nvim_get_chan_info = function(c)
  if c == chan then return { [vim.type_idx] = vim.types.dictionary } end
  return real(c)
end
local gone = test_timers.until_ready(function() return __eitri_pages == nil end)
vim.api.nvim_get_chan_info = real
assert(gone, 'the slot of a channel answered as an empty dictionary is still there')
assert(saved_page.torn and saved_page.timer:is_closing())
"#,
        vec![Value::from(chan)],
    );
    nvim.close_channel(chan);
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn an_eitri_window_refuses_a_page_install() {
    refusal("_G.__eitri_window = {}", InstallRefusal::EitriWindow);
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn a_takeover_glue_refuses_a_page_install() {
    refusal("_G.__eitri_companion = {mode='takeover'}", InstallRefusal::Takeover);
}

fn refusal(setup: &str, expected: InstallRefusal) {
    let nvim = EmbedNvim::start();
    nvim.manual_timers();
    let chan = nvim.helper_channel();
    nvim.exec_lua(setup, vec![]);
    settle(&nvim);
    let before = snapshot(&|code| nvim.exec_lua(code, vec![]));
    let reply = nvim.exec_lua(
        INSTALL_LUA,
        install_args(chan, &sockets(), &InstallKind::Page { id: "markdown".into() }),
    );
    assert_eq!(parse_install_report(&reply), Err(InstallError::Refused(expected)));
    assert_same(&before, &snapshot(&|code| nvim.exec_lua(code, vec![])));
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn a_bad_page_id_raises() {
    let nvim = EmbedNvim::start();
    nvim.manual_timers();
    let chan = nvim.helper_channel();
    let before = snapshot(&|code| nvim.exec_lua(code, vec![]));
    for id in ["", "-a", "a-", &"a".repeat(41), "A", "a/b", "a_b", "你好"] {
        assert!(!page_id_ok(id), "{id}");
        let args = install_args(chan, &sockets(), &InstallKind::Page { id: id.into() });
        assert!(nvim.try_exec_lua(INSTALL_LUA, args).is_err(), "{id}");
        assert_same(&before, &snapshot(&|code| nvim.exec_lua(code, vec![])));
    }
    for id in ["a", "0", "a-b", &"a".repeat(40)] {
        assert!(page_id_ok(id), "{id}");
    }
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn leftover_snapshot_cases() {
    for case in [
        "page-close",
        "page-killed",
        "agent-first",
        "page-first",
        "both-killed",
        "agent-replaced",
    ] {
        let nvim = EmbedNvim::start();
        nvim.manual_timers();
        settle(&nvim);
        let before = snapshot(&|code| nvim.exec_lua(code, vec![]));
        let p = nvim.helper_channel();
        let mut a = None;
        if matches!(case, "agent-first" | "page-first" | "both-killed") {
            let chan = nvim.helper_channel();
            install(&nvim, chan, InstallKind::Agent);
            a = Some(chan);
        }
        page(&nvim, p, "markdown");
        if case == "agent-replaced" {
            let first = nvim.helper_channel();
            install(&nvim, first, InstallKind::Agent);
            let second = nvim.helper_channel();
            install(&nvim, second, InstallKind::Agent);
            assert_eq!(teardown(&nvim, first, None).as_bool(), Some(false));
            nvim.close_channel(first);
            a = Some(second);
        }
        if matches!(case, "page-close" | "agent-first" | "page-first" | "agent-replaced") {
            if case == "agent-first" {
                assert_eq!(teardown(&nvim, a.unwrap(), None).as_bool(), Some(true));
            }
            assert_eq!(teardown(&nvim, p, Some("markdown")).as_bool(), Some(true));
            if case != "agent-first" {
                if let Some(a) = a {
                    assert_eq!(teardown(&nvim, a, None).as_bool(), Some(true));
                }
            }
        }
        nvim.close_channel(p);
        if let Some(a) = a {
            nvim.close_channel(a);
        }
        nvim.exec_lua(
            "assert(test_timers.until_ready(function() return __eitri_pages == nil and __eitri_companion == nil end))",
            vec![],
        );
        settle(&nvim);
        assert_same(&before, &snapshot(&|code| nvim.exec_lua(code, vec![])));
    }
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn parts_export_calls_and_adopted_teardowns_run_in_reverse_order() {
    let nvim = EmbedNvim::start();
    nvim.manual_timers();
    let chan = nvim.helper_channel();
    let mut args = install_args(chan, &Sockets::default(), &InstallKind::Agent);
    let part = |name: &str, source: &str| {
        Value::Map(vec![
            (Value::from("name"), Value::from(name)),
            (Value::from("src"), Value::from(source)),
        ])
    };
    args[1] = Value::Array(vec![
        part("one", "local opts = ...; assert(type(opts) == 'table'); local torn = false; return {teardown = function() torn = true; table.insert(_G.order, 1) end, calls = {echo = function(x) return x end}}"),
        part("two", "return function() table.insert(_G.order, 2) end"),
        part("broken", "error('unavailable')"),
    ]);
    nvim.exec_lua("_G.order = {}", vec![]);
    let report = parse_install_report(&nvim.exec_lua(INSTALL_LUA, args)).unwrap();
    assert_eq!(report.installed, ["one", "two"]);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, "broken");
    nvim.exec_lua("__eitri_companion.adopt(function() table.insert(order, 3) end)", vec![]);
    assert_eq!(
        nvim.exec_lua(
            SLOT_CALL_LUA,
            vec![
                Value::from("agent"),
                Value::from(chan),
                Value::from("echo"),
                Value::Array(vec![Value::from(42)])
            ]
        )
        .as_i64(),
        Some(42)
    );
    teardown(&nvim, chan, None);
    assert_eq!(
        nvim.exec_lua("return order", vec![]),
        Value::Array(vec![Value::from(3), Value::from(2), Value::from(1)])
    );
}

fn follow_slot(chan: u64) -> Value {
    Value::Map(vec![("id".into(), "markdown".into()), ("chan".into(), chan.into())])
}

fn follow_op(nvim: &EmbedNvim, op: &str, slot: Option<Value>, mut opts: Vec<(&str, Value)>) -> Value {
    opts.push(("owner", "markdown".into()));
    if let Some(slot) = slot {
        opts.push(("slot", slot));
    }
    nvim.exec_lua(
        PLUGIN_FOLLOW_LUA,
        vec![
            op.into(),
            Value::Map(opts.into_iter().map(|(k, v)| (k.into(), v)).collect()),
        ],
    )
}

fn install_follow(nvim: &EmbedNvim, slot: Option<Value>, prefix: Option<Value>) -> Value {
    let mut opts = vec![
        ("socket", Value::from(sockets().theme.unwrap().to_str().unwrap())),
        ("filetypes", Value::Array(vec!["markdown".into()])),
        (
            "commands",
            Value::Array(vec![Value::Array(vec!["EitriMarkdown".into(), "show".into()])]),
        ),
    ];
    if let Some(prefix) = prefix {
        opts.push(("scratch_prefix", prefix));
    }
    follow_op(nvim, "install", slot, opts)
}

fn answer_field<'a>(answer: &'a Value, name: &str) -> Option<&'a Value> {
    answer
        .as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
}

fn assert_follow_refused(answer: Value) {
    assert_eq!(answer_field(&answer, "ok"), Some(&Value::from(false)));
    assert_eq!(answer_field(&answer, "slot"), Some(&Value::from(false)));
}

fn follow_state(nvim: &EmbedNvim) -> Value {
    nvim.exec_lua(
        r#"
local s = __eitri_plugin_follow.markdown
local v = vim.fn.winsaveview()
return {s.slot_chan, s.following, s.buf, s.seq, s.torn, vim.api.nvim_win_get_cursor(0),
  {v.topline, v.lnum, v.col, v.coladd, v.curswant, v.leftcol, v.skipcol, v.topfill}}
"#,
        vec![],
    )
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn follow_install_without_a_matching_slot_defines_nothing() {
    let nvim = EmbedNvim::start();
    nvim.manual_timers();
    let chan = nvim.helper_channel();
    for present in [false, true] {
        if present {
            page(&nvim, nvim.helper_channel(), "markdown");
        }
        settle(&nvim);
        let before = snapshot(&|code| nvim.exec_lua(code, vec![]));
        assert_follow_refused(install_follow(&nvim, Some(follow_slot(chan)), None));
        assert_same(&before, &snapshot(&|code| nvim.exec_lua(code, vec![])));
    }
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn follow_install_in_a_slot_records_its_channel_and_adopts() {
    let nvim = EmbedNvim::start();
    nvim.manual_timers();
    let chan = nvim.helper_channel();
    settle(&nvim);
    let before = snapshot(&|code| nvim.exec_lua(code, vec![]));
    page(&nvim, chan, "markdown");
    assert_eq!(
        answer_field(&install_follow(&nvim, Some(follow_slot(chan)), None), "ok"),
        Some(&true.into())
    );
    assert_eq!(
        nvim.exec_lua("return __eitri_plugin_follow.markdown.slot_chan", vec![])
            .as_u64(),
        Some(chan)
    );
    nvim.exec_lua(
        "assert(#__eitri_pages.markdown.teardowns == 2); _G.saved_follow = __eitri_plugin_follow.markdown",
        vec![],
    );
    teardown(&nvim, chan, Some("markdown"));
    nvim.exec_lua("assert(saved_follow.torn); assert(not __eitri_plugin_follow or not __eitri_plugin_follow.markdown); assert(vim.fn.exists(':EitriMarkdown') == 0); _G.saved_follow = nil", vec![]);
    settle(&nvim);
    assert_same(&before, &snapshot(&|code| nvim.exec_lua(code, vec![])));
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn a_page_slots_ops_never_touch_another_channels_state() {
    let nvim = EmbedNvim::start();
    nvim.manual_timers();
    let chan = nvim.helper_channel();
    let foreign = nvim.helper_channel();
    page(&nvim, chan, "markdown");
    install_follow(&nvim, Some(follow_slot(chan)), None);
    nvim.exec_lua(
        "vim.bo.filetype = 'markdown'; vim.api.nvim_buf_set_lines(0, 0, -1, false, {'one', 'two', 'three'})",
        vec![],
    );
    follow_op(&nvim, "follow", Some(follow_slot(chan)), vec![("on", true.into())]);
    settle(&nvim);
    for slot in [
        Some(follow_slot(foreign)),
        Some(Value::Map(vec![
            ("id".into(), "missing".into()),
            ("chan".into(), chan.into()),
        ])),
    ] {
        for op in ["install", "follow", "text", "reveal", "jump", "teardown"] {
            let before = snapshot(&|code| nvim.exec_lua(code, vec![]));
            let state = follow_state(&nvim);
            let answer = if op == "install" {
                install_follow(&nvim, slot.clone(), None)
            } else {
                follow_op(
                    &nvim,
                    op,
                    slot.clone(),
                    vec![
                        ("on", false.into()),
                        ("buf", 1.into()),
                        ("line", 3.into()),
                        ("max_bytes", 100.into()),
                        ("max_lines", 100.into()),
                    ],
                )
            };
            assert_follow_refused(answer);
            assert_eq!(follow_state(&nvim), state, "{op} leaves the holder's state alone");
            assert_same(&before, &snapshot(&|code| nvim.exec_lua(code, vec![])));
        }
    }
    // A full window may replace a page's follow state while that page is still disconnecting.
    install_follow(&nvim, None, None);
    for op in ["follow", "text", "reveal", "jump", "teardown"] {
        assert_follow_refused(follow_op(&nvim, op, Some(follow_slot(chan)), vec![]));
    }
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn a_live_slot_of_another_id_cannot_drive_a_pages_follow_state() {
    let nvim = EmbedNvim::start();
    nvim.manual_timers();
    let chan = nvim.helper_channel();
    page(&nvim, chan, "markdown");
    page(&nvim, chan, "other");
    install_follow(&nvim, Some(follow_slot(chan)), None);
    nvim.exec_lua(
        "vim.bo.filetype = 'markdown'; vim.api.nvim_buf_set_lines(0, 0, -1, false, {'one', 'two', 'three'})",
        vec![],
    );
    follow_op(&nvim, "follow", Some(follow_slot(chan)), vec![("on", true.into())]);
    settle(&nvim);
    let other = Value::Map(vec![("id".into(), "other".into()), ("chan".into(), chan.into())]);
    for op in ["follow", "text", "reveal", "jump", "teardown"] {
        let before = snapshot(&|code| nvim.exec_lua(code, vec![]));
        let state = follow_state(&nvim);
        assert_follow_refused(follow_op(
            &nvim,
            op,
            Some(other.clone()),
            vec![("on", false.into()), ("buf", 1.into()), ("line", 3.into())],
        ));
        assert_eq!(
            follow_state(&nvim),
            state,
            "{op} from another live slot leaves the holder's state alone"
        );
        assert_same(&before, &snapshot(&|code| nvim.exec_lua(code, vec![])));
    }
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn the_adopted_teardown_leaves_a_state_another_holder_installed() {
    let nvim = EmbedNvim::start();
    nvim.manual_timers();
    let chan = nvim.helper_channel();
    page(&nvim, chan, "markdown");
    install_follow(&nvim, Some(follow_slot(chan)), None);
    install_follow(&nvim, None, None);
    follow_op(&nvim, "follow", None, vec![("on", true.into())]);
    nvim.exec_lua(
        "_G.new_follow = __eitri_plugin_follow.markdown; assert(not new_follow.torn)",
        vec![],
    );
    nvim.close_channel(chan);
    nvim.exec_lua("assert(test_timers.until_ready(function() return __eitri_pages == nil end)); assert(__eitri_plugin_follow.markdown == new_follow and not new_follow.torn); assert(vim.fn.exists(':EitriMarkdown') == 2); assert(new_follow.following and not new_follow.timer:is_closing()); assert(#vim.api.nvim_get_autocmds({group='EitriPluginFollow_markdown'}) > 0)", vec![]);
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn his_own_command_of_the_same_name_survives_the_slot() {
    let nvim = EmbedNvim::start();
    nvim.manual_timers();
    nvim.exec_lua("vim.api.nvim_create_user_command('EitriMarkdown', function() _G.own_command_ran = true end, {desc='my preview'})", vec![]);
    let chan = nvim.helper_channel();
    page(&nvim, chan, "markdown");
    let reply = install_follow(&nvim, Some(follow_slot(chan)), None);
    assert_eq!(
        answer_field(&reply, "skipped"),
        Some(&Value::Array(vec!["EitriMarkdown".into()]))
    );
    nvim.exec_lua("vim.bo.filetype = 'markdown'", vec![]);
    let followed = follow_op(&nvim, "follow", Some(follow_slot(chan)), vec![("on", true.into())]);
    assert_eq!(answer_field(&followed, "ok"), Some(&true.into()));
    nvim.exec_lua(
        "assert(__eitri_plugin_follow.markdown.buf == vim.api.nvim_get_current_buf())",
        vec![],
    );
    nvim.close_channel(chan);
    nvim.exec_lua("assert(test_timers.until_ready(function() return __eitri_pages == nil end)); assert(not __eitri_plugin_follow or not __eitri_plugin_follow.markdown); vim.cmd.EitriMarkdown(); assert(own_command_ran); assert(vim.api.nvim_get_commands({}).EitriMarkdown.definition == 'my preview')", vec![]);
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn a_buffer_under_the_scratch_prefix_is_never_followed() {
    let nvim = EmbedNvim::start();
    nvim.manual_timers();
    let chan = nvim.helper_channel();
    page(&nvim, chan, "markdown");
    let prefix = std::env::temp_dir().join("nv-sc-");
    install_follow(&nvim, Some(follow_slot(chan)), Some(prefix.to_str().unwrap().into()));
    let draft = std::env::temp_dir().join("nv-sc-other-1/1-draft.md");
    nvim.exec_lua(
        "vim.api.nvim_buf_set_name(0, ...); vim.bo.filetype = 'markdown'",
        vec![draft.to_str().unwrap().into()],
    );
    follow_op(&nvim, "follow", Some(follow_slot(chan)), vec![("on", true.into())]);
    nvim.exec_lua(
        "assert(__eitri_plugin_follow.markdown.buf == nil); assert(__eitri_plugin_follow.markdown.seq == 0)",
        vec![],
    );
    let text = follow_op(
        &nvim,
        "text",
        Some(follow_slot(chan)),
        vec![("buf", 1.into()), ("max_bytes", 100.into()), ("max_lines", 100.into())],
    );
    assert_eq!(answer_field(&text, "gone"), Some(&true.into()));
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn an_agent_slot_can_adopt_follow_glue() {
    let nvim = EmbedNvim::start();
    nvim.manual_timers();
    let chan = nvim.helper_channel();
    install(&nvim, chan, InstallKind::Agent);
    let slot = Value::Map(vec![("chan".into(), chan.into())]);
    let installed = install_follow(&nvim, Some(slot.clone()), None);
    assert_eq!(answer_field(&installed, "ok"), Some(&true.into()));
    nvim.exec_lua("vim.bo.filetype = 'markdown'", vec![]);
    let followed = follow_op(&nvim, "follow", Some(slot.clone()), vec![("on", true.into())]);
    assert_eq!(answer_field(&followed, "ok"), Some(&true.into()));
    let text = follow_op(
        &nvim,
        "text",
        Some(slot),
        vec![("buf", 1.into()), ("max_bytes", 100.into()), ("max_lines", 100.into())],
    );
    assert!(answer_field(&text, "lines").is_some());
    nvim.close_channel(chan);
    nvim.exec_lua(
        r#"
assert(test_timers.until_ready(function() return __eitri_companion == nil end))
assert(not __eitri_plugin_follow or not __eitri_plugin_follow.markdown)
assert(vim.fn.exists(':EitriMarkdown') == 0)
"#,
        vec![],
    );
}
