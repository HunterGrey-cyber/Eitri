use super::*;

fn chooser(mode: Mode) -> Chooser {
    Chooser::new(mode, Some("/home/me".into()), Lang::En, 7)
}
fn window() -> Chooser {
    chooser(Mode::Window { root: "/here".into() })
}
fn row(path: &str, fact: Fact) -> Row {
    Row::new(path.into(), Some(Path::new("/home/me")), "2 h".into(), fact)
}
fn list(c: &mut Chooser, rows: Vec<Row>) -> Vec<Effect> {
    let effects = c.shown();
    let Effect::List { serial } = effects[0] else {
        panic!("expected a list request")
    };
    c.event(Ev::Rows { serial, rows })
}
fn line(c: &mut Chooser, text: &str) -> Vec<Effect> {
    c.event(Ev::Line(LineEv::Changed(text.into())))
}
fn enter(c: &mut Chooser) -> Vec<Effect> {
    c.event(Ev::Line(LineEv::Enter))
}
fn answer(c: &mut Chooser, text: &str, answer: Listing) -> Vec<Effect> {
    let effects = line(c, text);
    deliver(c, &effects, answer)
}

#[test]
fn a_filter_matches_name_and_shown_path_ignoring_case() {
    for (query, names) in [("APP", vec!["App"]), ("WORK", vec!["App", "other"]), ("absent", vec![])] {
        let mut c = window();
        list(
            &mut c,
            vec![
                row("/home/me/Work/App", Fact::Present),
                row("/home/me/Work/other", Fact::Open),
            ],
        );
        line(&mut c, query);
        assert_eq!(
            c.view()
                .rows
                .iter()
                .take(names.len())
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            names
        );
        assert_eq!(c.view().rows.len(), names.len() + 1);
    }
}
#[test]
fn is_path_reads_the_four_prefixes_only() {
    for (text, expected) in [
        ("/a", true),
        ("~", true),
        ("~/a", true),
        ("./a", true),
        ("../a", true),
        ("a/b", false),
        (".a", false),
        ("", false),
    ] {
        assert_eq!(is_path(text), expected, "{text}");
    }
}
#[test]
fn the_windows_own_project_is_not_listed() {
    for path in ["/here", "/elsewhere"] {
        let mut c = window();
        list(&mut c, vec![row(path, Fact::Present)]);
        assert_eq!(c.view().rows.len(), if path == "/here" { 1 } else { 2 });
    }
}
#[test]
fn step_moves_and_does_not_wrap() {
    for (steps, expected) in [(vec![-1], 0), (vec![1], 1), (vec![99], 2), (vec![99, -99], 0)] {
        let mut c = window();
        list(&mut c, vec![row("/a", Fact::Present), row("/b", Fact::Present)]);
        for step in steps {
            c.event(Ev::Line(LineEv::Step(step)));
        }
        assert_eq!(c.view().selected, Some(expected));
    }
}
#[test]
fn enter_opens_the_selected_row() {
    for index in [0, 1] {
        let mut c = window();
        list(&mut c, vec![row("/a", Fact::Present), row("/b", Fact::Open)]);
        c.event(Ev::Line(LineEv::Step(index)));
        assert_eq!(
            enter(&mut c),
            vec![Effect::Open(PathBuf::from(if index == 0 { "/a" } else { "/b" }))]
        );
    }
}
#[test]
fn enter_on_gone_stays_and_says_it_is_not_there() {
    for (lang, message) in [(Lang::En, "~/lost is not there"), (Lang::ZhCn, "~/lost 不在了")] {
        let mut c = Chooser::new(Mode::Launch, Some("/home/me".into()), lang, 1);
        list(&mut c, vec![row("/home/me/lost", Fact::Gone)]);
        assert!(enter(&mut c).is_empty());
        assert_eq!(c.view().message.as_deref(), Some(message));
        assert!(c.view().rows[0].dimmed);
    }
}
#[test]
fn enter_on_unknown_opens() {
    for fact in [Fact::Unknown, Fact::Present, Fact::Open] {
        let mut c = window();
        list(&mut c, vec![row("/a", fact)]);
        assert_eq!(enter(&mut c), vec![Effect::Open("/a".into())]);
    }
}
#[test]
fn held_enter_runs_on_the_first_row_when_the_line_did_not_change() {
    for query in ["", "app"] {
        let mut c = window();
        let Effect::List { serial } = c.shown()[0] else {
            unreachable!()
        };
        line(&mut c, query);
        assert!(enter(&mut c).is_empty());
        assert_eq!(
            c.event(Ev::Rows {
                serial,
                rows: vec![row("/app", Fact::Present)]
            }),
            vec![Effect::Open("/app".into())]
        );
    }
}
#[test]
fn held_enter_is_dropped_when_the_line_changed_or_no_project_row_arrived() {
    for (changed, rows) in [(true, vec![row("/app", Fact::Present)]), (false, vec![])] {
        let mut c = chooser(Mode::Launch);
        let Effect::List { serial } = c.shown()[0] else {
            unreachable!()
        };
        enter(&mut c);
        if changed {
            line(&mut c, "app");
        }
        assert!(c.event(Ev::Rows { serial, rows }).is_empty());
    }
}
#[test]
fn a_stale_serial_changes_nothing() {
    let mut c = window();
    let Effect::List { serial } = c.shown()[0] else {
        unreachable!()
    };
    let before = c.view();
    c.event(Ev::Rows {
        serial: serial + 1,
        rows: vec![row("/a", Fact::Open)],
    });
    assert_eq!(c.view(), before);
    let old = line(&mut c, "/old/");
    line(&mut c, "/new/");
    let before = c.view();
    assert!(deliver(&mut c, &old, listing(&["old"])).is_empty());
    assert_eq!(c.view(), before);
}

#[test]
fn a_reply_to_an_earlier_chooser_is_dropped() {
    for generation in [8, 9] {
        let mut old = window();
        let Effect::List { serial } = old.shown()[0] else {
            unreachable!()
        };
        let mut c = Chooser::new(Mode::Launch, None, Lang::En, generation);
        c.shown();
        let before = c.view();
        assert!(c
            .event(Ev::Rows {
                serial,
                rows: vec![row("/a", Fact::Open)]
            })
            .is_empty());
        assert_eq!(c.view(), before);
    }
}
#[test]
fn path_text_asks_for_a_listing_and_filter_text_does_not() {
    for mode in modes() {
        for typed in ["~/app", "/app", "./app", "../app"] {
            let mut c = chooser(mode.clone());
            assert!(line(&mut c, "app").is_empty());
            let effects = line(&mut c, typed);
            let (_, dir) = read_request(&effects);
            let expected = match typed {
                "~/app" => PathBuf::from("/home/me"),
                "/app" => PathBuf::from("/"),
                "./app" => {
                    if mode == Mode::Launch {
                        "/home/me".into()
                    } else {
                        "/here".into()
                    }
                }
                _ => {
                    if mode == Mode::Launch {
                        "/home/me/..".into()
                    } else {
                        "/here/..".into()
                    }
                }
            };
            assert_eq!(dir, expected);
        }
    }
}

#[test]
fn a_found_dir_is_entered_and_a_found_file_opens_by_its_walked_path() {
    for kind in [EntryKind::Dir { mount_point: false }, EntryKind::File] {
        let mut c = window();
        let effects = line(&mut c, "~/target");
        let mut e = entry("target");
        e.kind = kind.clone();
        deliver(
            &mut c,
            &effects,
            Listing::Read {
                dir: "/physical".into(),
                entries: vec![e],
                capped: None,
                exact: None,
            },
        );
        if kind == EntryKind::File {
            assert_eq!(enter(&mut c), vec![Effect::Open("/home/me/target".into())]);
        } else {
            let effects = enter(&mut c);
            assert_eq!(read_request(&effects).1, PathBuf::from("/home/me/target"));
            deliver(&mut c, &effects, listing(&[]));
            assert_eq!(enter(&mut c), vec![Effect::Open("/home/me/target".into())]);
        }
    }
}

#[test]
fn enter_on_a_listed_folder_enters_before_opening_its_walked_path() {
    for (index, name) in [(0, "target1"), (1, "target2")] {
        let mut c = window();
        browse(&mut c, "~/t", &["target1", "target2"]);
        c.event(Ev::Line(LineEv::Step(index)));
        let effects = enter(&mut c);
        deliver(&mut c, &effects, listing(&[]));
        assert_eq!(enter(&mut c), vec![Effect::Open(PathBuf::from("/home/me").join(name))]);
    }
}

#[test]
fn enter_on_missing_says_no_such_folder_and_stays() {
    for (lang, message) in [
        (Lang::En, "no such folder: ~/lost"),
        (Lang::ZhCn, "没有这个文件夹：~/lost"),
    ] {
        let mut c = Chooser::new(Mode::Launch, Some("/home/me".into()), lang, 1);
        answer(&mut c, "~/lost", Listing::Missing);
        assert!(enter(&mut c).is_empty());
        assert_eq!(c.view().message.as_deref(), Some(message));
    }
}

#[test]
fn enter_held_for_a_listing_runs_on_it_and_stuck_says_cannot_read() {
    for reply in [listing(&[]), Listing::Stuck] {
        let mut c = window();
        let effects = line(&mut c, "/a/");
        enter(&mut c);
        let expected = if reply == Listing::Stuck {
            vec![]
        } else {
            vec![Effect::Open("/a".into())]
        };
        assert_eq!(deliver(&mut c, &effects, reply), expected);
        if expected.is_empty() {
            assert_eq!(c.view().message.as_deref(), Some("cannot read /a now"));
        }
    }
}

#[test]
fn tab_sets_the_line_to_the_common_prefix_and_adds_a_slash_on_one_match() {
    for (names, expected) in [(vec!["one"], "~/one/"), (vec!["one", "only"], "~/on")] {
        let mut c = window();
        browse(&mut c, "~/o", &names);
        let effects = c.event(Ev::Line(LineEv::Tab));
        assert!(effects.contains(&Effect::SetLine(expected.into())));
        assert_eq!(c.view().line, expected);
    }
}

#[test]
fn tab_keeps_the_typed_form_of_the_line_and_extends_it_from_the_absolute_answer() {
    for (typed, names, expected) in [
        ("~/b", vec!["beta"], "~/beta/"),
        ("~/b", vec!["beta", "bin"], "~/b"),
        ("~/.c", vec![".config"], "~/.config/"),
        ("~/..", vec!["you"], "~/../you/"),
        ("~", vec!["me"], "~/"),
        ("~", vec!["me-old"], "/home/me-old/"),
        ("./nested/ap", vec!["apple", "apricot"], "./nested/ap"),
        ("./nested/ap", vec!["apple"], "./nested/apple/"),
        ("../sib", vec!["sibling"], "../sibling/"),
        ("./", vec!["a", "b"], "./"),
        ("/u", vec!["usr"], "/usr/"),
        ("~/应", vec!["应用甲", "应用乙"], "~/应用"),
    ] {
        let mut c = window();
        browse(&mut c, typed, &names);
        let effects = c.event(Ev::Line(LineEv::Tab));
        assert!(
            effects.contains(&Effect::SetLine(expected.into())),
            "{typed}: {effects:?}"
        );
        assert_eq!(c.view().line, expected);
    }
}

#[test]
fn tab_in_a_filter_does_nothing() {
    for text in ["", "app", "a/b"] {
        let mut c = window();
        line(&mut c, text);
        let before = c.view();
        assert!(c.event(Ev::Line(LineEv::Tab)).is_empty());
        assert_eq!(c.view(), before);
    }
}
#[test]
fn the_folder_row_is_last_but_for_a_launch_first_run_and_choose_folder_asks_the_dialog() {
    for (mode, rows, at) in [
        (Mode::Launch, vec![], 0),
        (Mode::Launch, vec![row("/a", Fact::Present)], 2),
        (Mode::Window { root: "/here".into() }, vec![], 0),
    ] {
        let mut c = chooser(mode);
        list(&mut c, rows);
        assert_eq!(c.view().rows[at].name, "Browse folders…");
        assert_eq!(
            c.event(Ev::Line(LineEv::ChooseFolder)),
            vec![Effect::FolderDialog { at: None }]
        );
    }
}
#[test]
fn folder_none_returns_to_the_line_as_it_was() {
    for text in ["app", "~/app"] {
        let mut c = window();
        line(&mut c, text);
        let before = c.view();
        c.event(Ev::Line(LineEv::ChooseFolder));
        assert!(c.event(Ev::Folder(None)).is_empty());
        assert_eq!(c.view(), before);
    }
}
#[test]
fn folder_some_opens_it() {
    for path in ["/folder", "/<b> 中文\n"] {
        let mut c = window();
        c.event(Ev::Line(LineEv::ChooseFolder));
        assert_eq!(c.event(Ev::Folder(Some(path.into()))), vec![Effect::Open(path.into())]);
    }
}
#[test]
fn dismiss_and_keys_lost_are_ignored_while_a_dialog_is_open() {
    for ev in [Ev::Dismiss, Ev::Line(LineEv::KeysLost), Ev::Line(LineEv::Escape)] {
        let mut c = window();
        c.event(Ev::Line(LineEv::ChooseFolder));
        assert!(c.event(ev).is_empty());
    }
}
#[test]
fn esc_and_dismiss_close() {
    for ev in [Ev::Dismiss, Ev::Line(LineEv::Escape), Ev::Line(LineEv::KeysLost)] {
        let mut c = window();
        assert_eq!(c.event(ev), vec![Effect::Close]);
    }
}
#[test]
fn launch_first_run_selects_choose_a_folder_above_home_folder() {
    for home in [Some(PathBuf::from("/home/me")), None] {
        let mut c = Chooser::new(Mode::Launch, home.clone(), Lang::En, 1);
        list(&mut c, vec![]);
        assert_eq!(c.view().selected, Some(0));
        assert_eq!(c.view().rows[0].name, "Browse folders…");
        assert_eq!(c.view().message.as_deref(), Some("No recent projects"));
        if home.is_some() {
            assert_eq!(c.view().rows[1].name, "Home folder");
        }
    }
}
#[test]
fn launch_home_row_only_when_no_row_is_home() {
    for (path, synthetic) in [("/home/me", false), ("/home/me/app", true)] {
        let mut c = chooser(Mode::Launch);
        list(&mut c, vec![row(path, Fact::Present)]);
        assert_eq!(c.view().rows.iter().any(|r| r.name == "Home folder"), synthetic);
    }
}
#[test]
fn window_mode_has_no_home_row() {
    for rows in [vec![], vec![row("/app", Fact::Present)]] {
        let mut c = window();
        list(&mut c, rows);
        assert!(!c.view().rows.iter().any(|r| r.name == "Home folder"));
    }
}
#[test]
fn every_string_has_both_languages() {
    for (lang, title, placeholder, footer, choose, home, empty, open, gone, opening, stuck) in [
        (
            Lang::En,
            "Open a project",
            "press / to filter or type a path",
            "Enter open · j k move · h l browse · / filter · Ctrl+o dialog · q close",
            "Browse folders…",
            "Home folder",
            "No recent projects",
            "open",
            "gone",
            "Opening app…",
            "cannot read / now",
        ),
        (
            Lang::ZhCn,
            "打开项目",
            "按 / 筛选或输入路径",
            "Enter 打开 · j k 移动 · h l 浏览 · / 筛选 · Ctrl+o 对话框 · q 关闭",
            "浏览文件夹…",
            "主目录",
            "没有最近的项目",
            "已打开",
            "不在了",
            "正在打开 app…",
            "现在读不了 /",
        ),
    ] {
        let mut c = Chooser::new(Mode::Launch, Some("/home/me".into()), lang, 1);
        list(&mut c, vec![]);
        let v = c.view();
        assert_eq!(v.title, title);
        assert_eq!(v.placeholder, placeholder);
        assert_eq!(v.footer, footer);
        assert_eq!(v.rows[0].name, choose);
        assert_eq!(v.rows[1].name, home);
        assert_eq!(v.message.as_deref(), Some(empty));
        c.event(Ev::Line(LineEv::Key('/')));
        let v = c.view();
        assert_eq!(
            v.placeholder,
            if lang == Lang::En {
                "filter, or a path: ~/src/app"
            } else {
                "筛选，或输入路径：~/src/app"
            }
        );
        assert_eq!(
            v.footer,
            if lang == Lang::En {
                "Enter open · ↑↓ move · Tab complete · Ctrl+o folder · Esc list"
            } else {
                "Enter 打开 · ↑↓ 移动 · Tab 补全 · Ctrl+o 文件夹 · Esc 返回列表"
            }
        );
        list(&mut c, vec![row("/a", Fact::Open), row("/b", Fact::Gone)]);
        assert_eq!(c.view().rows[0].right, open);
        assert_eq!(c.view().rows[1].right, gone);
        answer(&mut c, "/bad", Listing::Stuck);
        enter(&mut c);
        assert_eq!(c.view().message.as_deref(), Some(stuck));
        c.opening("app");
        assert!(c.view().opening);
        assert_eq!(c.view().message.as_deref(), Some(opening));
        for ev in [
            Ev::Line(LineEv::Enter),
            Ev::Line(LineEv::Changed("x".into())),
            Ev::Dismiss,
        ] {
            assert!(c.event(ev).is_empty());
        }
        c.failed("failed".into());
        assert!(!c.view().opening);
        assert_eq!(c.view().message.as_deref(), Some("failed"));
    }
}
#[test]
fn a_list_that_lands_during_a_start_is_there_when_the_start_fails() {
    let mut c = chooser(Mode::Launch);
    let effects = c.shown();
    let Effect::List { serial } = effects[0] else {
        panic!("expected a list request")
    };
    assert_eq!(
        c.event(Ev::Pick {
            index: 1,
            rows_rev: c.view().rows_rev
        }),
        vec![Effect::Open("/home/me".into())],
        "the home row"
    );
    c.opening("me");

    let rows = vec![row("/a", Fact::Present)];
    assert_eq!(c.event(Ev::Rows { serial, rows }), vec![], "an answer starts nothing");
    assert!(c.view().opening);
    assert_eq!(c.view().message.as_deref(), Some("Opening me…"));

    c.failed("failed".into());
    assert_eq!(c.view().rows[0].name, "a");
    assert_eq!(c.view().message.as_deref(), Some("failed"));
    assert_eq!(
        enter(&mut c),
        vec![Effect::Open("/a".into())],
        "Enter does not wait for an answer that already came"
    );
}
#[test]
fn a_listing_that_lands_during_a_start_is_there_when_the_start_fails() {
    let mut c = chooser(Mode::Launch);
    list(&mut c, vec![]);
    let effects = line(&mut c, "/work/");
    c.event(Ev::Line(LineEv::ChooseFolder));
    assert_eq!(
        c.event(Ev::Folder(Some("/picked".into()))),
        vec![Effect::Open("/picked".into())]
    );
    c.opening("picked");
    assert!(deliver(&mut c, &effects, listing(&[])).is_empty());
    c.failed("failed".into());
    assert_eq!(enter(&mut c), vec![Effect::Open("/work".into())]);
}

#[test]
fn step_keys_match_the_session_chooser() {
    use crate::source_scan::{code_only, read_required};
    let source = read_required("agent-ui/web/src/chooser.ts");
    let code = code_only(&source.text);
    // The first brace is the event type; the function's body follows its return type.
    let start = code.find("function pickerStep").expect("pickerStep exists");
    let tail = &code[start..];
    let body_start = tail.find("null {").expect("pickerStep returns null for other keys") + "null ".len();
    let body = crate::source_scan::braced(tail, body_start);
    let offset = body.as_ptr() as usize - code.as_ptr() as usize;
    let raw = &source.text[offset..offset + body.len()];
    for (key, direction) in [("ArrowDown", 1), ("ArrowUp", -1), ("n", 1), ("p", -1)] {
        assert!(raw.contains(&format!("\"{key}\"")));
        let mut c = window();
        list(&mut c, vec![row("/a", Fact::Present), row("/b", Fact::Present)]);
        c.event(Ev::Line(LineEv::Step(1)));
        c.event(Ev::Line(LineEv::Step(direction)));
        assert_eq!(c.view().selected, Some(if direction == 1 { 2 } else { 0 }));
    }
    let compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    for (key, ctrl, direction) in [
        ("ArrowDown", false, 1),
        ("ArrowUp", false, -1),
        ("n", true, 1),
        ("p", true, -1),
    ] {
        let condition = if ctrl {
            format!("event.ctrlKey&&event.key===\"{key}\"")
        } else {
            format!("event.key===\"{key}\"")
        };
        let branch = compact
            .split("if(")
            .skip(1)
            .find(|branch| branch.contains(&condition))
            .unwrap();
        assert!(branch
            .split(';')
            .next()
            .unwrap()
            .ends_with(&format!("return{direction}")));
    }
}
#[test]
fn row_labels_are_safe_text_and_paths_keep_their_bytes() {
    for (path, name, shown) in [
        ("/home/me/<b> 中文\n", "<b> 中文\\u{a}", "~/<b> 中文\\u{a}"),
        ("/home/me", "me", "~"),
        ("/home/mesh/app", "app", "/home/mesh/app"),
        ("/", "/", "/"),
    ] {
        let r = row(path, Fact::Present);
        assert_eq!(r.name, name);
        assert_eq!(r.shown, shown);
        assert_eq!(r.path, Path::new(path));
    }
}
#[test]
fn ages_use_minutes_hours_days_and_weeks_with_future_times_clamped() {
    use std::time::Duration;
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000_000);
    for (seconds, expected) in [
        (0, "0 m"),
        (300, "5 m"),
        (7200, "2 h"),
        (259200, "3 d"),
        (2419200, "4 w"),
    ] {
        assert_eq!(age_text(now - Duration::from_secs(seconds), now), expected);
    }
    assert_eq!(age_text(now + Duration::from_secs(1), now), "0 m");
}

#[test]
fn the_dialog_works_while_a_listing_is_pending_or_missing() {
    for reply in [None, Some(Listing::Missing), Some(Listing::Stuck)] {
        let mut c = window();
        let effects = line(&mut c, "/absent");
        if let Some(reply) = reply {
            deliver(&mut c, &effects, reply);
        }
        assert_eq!(
            c.event(Ev::Line(LineEv::ChooseFolder)),
            vec![Effect::FolderDialog { at: None }]
        );
    }
}

#[test]
fn held_enter_never_opens_a_dialog_for_an_empty_listing() {
    let mut c = window();
    let effects = line(&mut c, "/absent");
    enter(&mut c);
    assert!(deliver(&mut c, &effects, listing(&[])).is_empty());
}

#[test]
fn a_new_folder_or_projects_filter_invalidates_a_pending_read_and_held_enter() {
    for next in ["/other/", "app"] {
        let mut c = window();
        let effects = line(&mut c, "/first/");
        enter(&mut c);
        line(&mut c, next);
        let before = c.view();
        assert!(deliver(&mut c, &effects, listing(&[])).is_empty());
        assert_eq!(c.view(), before);
    }
}

#[test]
fn a_failed_start_logs_its_code_or_unknown_and_never_a_path() {
    assert_eq!(start_failed_line(Some(1)), "[project] start failed 1");
    assert_eq!(start_failed_line(Some(-9)), "[project] start failed -9");
    assert_eq!(start_failed_line(None), "[project] start failed unknown");
}
#[test]
fn a_failed_start_says_a_gone_folder_as_a_gone_row_does_and_else_points_at_its_log() {
    let mut c = chooser(Mode::Launch);
    c.opening("app");
    c.start_failed("/home/me/src/app".into(), true);
    let view = c.view();
    assert!(!view.opening, "the keys are back");
    assert_eq!(view.message.as_deref(), Some("~/src/app is not there"));

    c.opening("app");
    c.start_failed("/home/me/src/app".into(), false);
    assert_eq!(
        c.view().message.as_deref(),
        Some("could not open app: see its window log")
    );

    let mut zh = Chooser::new(Mode::Launch, None, Lang::ZhCn, 1);
    zh.start_failed("/srv/app".into(), true);
    assert_eq!(zh.view().message.as_deref(), Some("/srv/app 不在了"));
}

// The list state (the state the chooser opens in) and the line state.

fn key(c: &mut Chooser, ch: char) -> Vec<Effect> {
    c.event(Ev::Line(LineEv::Key(ch)))
}
fn esc(c: &mut Chooser) -> Vec<Effect> {
    c.event(Ev::Line(LineEv::Escape))
}
fn three() -> Chooser {
    let mut c = window();
    list(
        &mut c,
        vec![
            row("/a", Fact::Present),
            row("/b", Fact::Present),
            row("/c", Fact::Present),
        ],
    );
    c
}

#[test]
fn it_opens_in_the_list_with_the_list_keys_and_a_placeholder_that_names_the_slash() {
    for mode in [Mode::Launch, Mode::Window { root: "/here".into() }] {
        let mut c = chooser(mode);
        c.shown();
        let v = c.view();
        assert!(!v.in_line);
        assert_eq!(v.placeholder, "press / to filter or type a path");
        assert!(v.footer.contains("j k move") && v.footer.contains("q close"));
    }
}

#[test]
fn j_k_and_the_four_step_keys_move_the_list_without_wrapping() {
    let mut c = three();
    key(&mut c, 'j');
    key(&mut c, 'j');
    assert_eq!(c.view().selected, Some(2));
    c.event(Ev::Line(LineEv::Step(1)));
    key(&mut c, 'j');
    key(&mut c, 'j');
    assert_eq!(
        c.view().selected,
        Some(3),
        "the folder row is the last, and nothing wraps"
    );
    key(&mut c, 'k');
    c.event(Ev::Line(LineEv::Step(-1)));
    assert_eq!(c.view().selected, Some(1));
    for _ in 0..9 {
        key(&mut c, 'k');
    }
    assert_eq!(c.view().selected, Some(0));
    assert!(!c.view().in_line);
}

#[test]
fn gg_goes_to_the_first_row_and_capital_g_to_the_last() {
    let mut c = three();
    assert!(key(&mut c, 'G').is_empty());
    assert_eq!(c.view().selected, Some(3));
    assert_eq!(c.view().rows[3].name, "Browse folders…");
    key(&mut c, 'g');
    assert_eq!(c.view().selected, Some(3), "a lone g only waits");
    key(&mut c, 'g');
    assert_eq!(c.view().selected, Some(0));
    // The wait ends with the second g: the next g waits again rather than going anywhere.
    key(&mut c, 'j');
    key(&mut c, 'g');
    key(&mut c, 'j');
    assert_eq!(c.view().selected, Some(1), "g then j is swallowed");
}

#[test]
fn capital_g_on_a_launch_first_run_lands_on_the_last_row() {
    let mut c = chooser(Mode::Launch);
    list(&mut c, vec![]);
    key(&mut c, 'G');
    assert_eq!(c.view().rows[c.view().selected.unwrap()].name, "Home folder");
    assert_eq!(enter(&mut c), vec![Effect::Open("/home/me".into())]);
    key(&mut c, 'j');
    let mut c = chooser(Mode::Launch);
    list(&mut c, vec![]);
    key(&mut c, 'j');
    assert_eq!(c.view().selected, Some(1));
    key(&mut c, 'g');
    key(&mut c, 'g');
    assert_eq!(c.view().rows[c.view().selected.unwrap()].name, "Browse folders…");
}

/// The key after a waiting `g` does nothing else, whatever it is: `gx`, `g<Enter>`, `g<Esc>` and `g<Down>` act as
/// nothing, and the wait is over.
#[test]
fn the_key_after_a_waiting_g_is_swallowed() {
    for next in [
        Ev::Line(LineEv::Key('x')),
        Ev::Line(LineEv::Other),
        Ev::Line(LineEv::Key('j')),
        Ev::Line(LineEv::Key('q')),
        Ev::Line(LineEv::Key('/')),
        Ev::Line(LineEv::Key('~')),
        Ev::Line(LineEv::Enter),
        Ev::Line(LineEv::Escape),
        Ev::Line(LineEv::Step(1)),
        Ev::Line(LineEv::Tab),
        Ev::Line(LineEv::ChooseFolder),
    ] {
        let mut c = three();
        key(&mut c, 'j');
        let before = c.view();
        key(&mut c, 'g');
        assert!(c.event(next.clone()).is_empty(), "{next:?}");
        assert_eq!(c.view(), before, "{next:?}");
        // The wait is over: a later g waits afresh, and j moves.
        key(&mut c, 'j');
        assert_eq!(c.view().selected, Some(2), "{next:?}");
    }
}

#[test]
fn another_key_does_nothing_and_ends_a_waiting_g() {
    let mut c = three();
    key(&mut c, 'j');
    let before = c.view();
    assert!(c.event(Ev::Line(LineEv::Other)).is_empty());
    assert_eq!(c.view(), before);
    // g, Backspace, `/`: the Backspace ended the wait, so the slash goes to the line.
    key(&mut c, 'g');
    c.event(Ev::Line(LineEv::Other));
    key(&mut c, '/');
    assert!(c.view().in_line);
    // g, Backspace, Esc closes: the wait is over when Esc comes.
    let mut c = three();
    key(&mut c, 'g');
    c.event(Ev::Line(LineEv::Other));
    assert_eq!(esc(&mut c), vec![Effect::Close]);
}

#[test]
fn a_click_ends_a_waiting_g() {
    let mut c = three();
    key(&mut c, 'g');
    assert!(
        c.event(Ev::Pick {
            index: 2,
            rows_rev: c.view().rows_rev
        })
        .len()
            == 1,
        "the click opens its row"
    );
    let mut c = three();
    key(&mut c, 'g');
    c.event(Ev::Pick {
        index: 99,
        rows_rev: c.view().rows_rev,
    });
    key(&mut c, 'j');
    assert_eq!(
        c.view().selected,
        Some(0),
        "a click on no row is not a key, and the wait stands"
    );
    let mut c = three();
    key(&mut c, 'g');
    c.event(Ev::Line(LineEv::Changed("a".into())));
    assert!(c.view().in_line);
    c.event(Ev::Line(LineEv::Step(1)));
    assert_eq!(c.view().selected, Some(1), "typing ends the wait");
}

#[test]
fn a_dismiss_or_lost_keys_still_close_while_g_waits() {
    for ev in [Ev::Dismiss, Ev::Line(LineEv::KeysLost)] {
        let mut c = three();
        key(&mut c, 'g');
        assert_eq!(c.event(ev), vec![Effect::Close]);
    }
}

#[test]
fn enter_in_the_list_opens_the_selected_row() {
    let mut c = three();
    key(&mut c, 'j');
    assert_eq!(enter(&mut c), vec![Effect::Open("/b".into())]);
    let mut c = three();
    key(&mut c, 'G');
    let effects = enter(&mut c);
    assert_eq!(read_request(&effects).1, PathBuf::from("/"));
}

#[test]
fn slash_goes_to_the_empty_line_and_typing_filters_there() {
    let mut c = three();
    assert!(key(&mut c, '/').is_empty());
    let v = c.view();
    assert!(v.in_line);
    assert_eq!(v.line, "");
    assert_eq!(v.placeholder, "filter, or a path: ~/src/app");
    assert_eq!(v.rows.len(), 4);
    line(&mut c, "b");
    assert_eq!(c.view().rows.len(), 2);
    assert_eq!(c.view().rows[0].name, "b");
    // In the line the four step keys, Tab and Enter work as before.
    c.event(Ev::Line(LineEv::Step(1)));
    assert_eq!(c.view().selected, Some(1));
    c.event(Ev::Line(LineEv::Step(-1)));
    assert_eq!(enter(&mut c), vec![Effect::Open("/b".into())]);
}

#[test]
fn esc_in_the_line_clears_it_and_returns_to_the_list_and_a_second_esc_closes() {
    for typed in ["b", "~/x", ""] {
        let mut c = three();
        key(&mut c, '/');
        if !typed.is_empty() {
            line(&mut c, typed);
        }
        c.event(Ev::Line(LineEv::Step(1)));
        let effects = esc(&mut c);
        assert!(!effects.contains(&Effect::Close), "{typed}");
        let v = c.view();
        assert!(!v.in_line, "{typed}");
        assert_eq!(v.line, "", "{typed}");
        assert_eq!(v.rows.len(), 4, "the whole list is back: {typed}");
        assert_eq!(v.selected, Some(0), "{typed}");
        assert_eq!(v.placeholder, "press / to filter or type a path");
        assert_eq!(esc(&mut c), vec![Effect::Close], "{typed}");
    }
}

#[test]
fn esc_in_the_line_drops_a_pending_read_and_a_held_enter() {
    let mut c = window();
    let Effect::List { serial } = c.shown()[0] else {
        unreachable!()
    };
    let effects = line(&mut c, "~/x");
    enter(&mut c);
    esc(&mut c);
    assert!(deliver(&mut c, &effects, listing(&[])).is_empty());
    assert!(c
        .event(Ev::Rows {
            serial,
            rows: vec![row("/a", Fact::Present)]
        })
        .is_empty());
    assert_eq!(c.view().rows[0].name, "a");
    assert!(!c.view().in_line);
}

#[test]
fn q_and_esc_close_the_list_with_nothing_opened() {
    for ch in [Some('q'), None] {
        let mut c = three();
        key(&mut c, 'j');
        let effects = match ch {
            Some(ch) => key(&mut c, ch),
            None => esc(&mut c),
        };
        assert_eq!(effects, vec![Effect::Close]);
        assert!(c.event(Ev::Line(LineEv::Enter)).is_empty(), "closed for good");
    }
}

#[test]
fn other_plain_keys_in_the_list_do_nothing() {
    let mut c = three();
    key(&mut c, 'j');
    let before = c.view();
    for ch in "abcdefimnoprstuvwxyz0123456789 .,:;!?_=+*#@$%^&()[]{}<>|\\'\"ABCDEFHIJKLMNOPQRSTUVWXYZ中".chars() {
        assert!(key(&mut c, ch).is_empty(), "{ch}");
        assert_eq!(c.view(), before, "{ch}");
    }
    assert!(!c.view().in_line);
}

#[test]
fn tab_does_nothing_in_the_list_and_ctrl_o_asks_the_dialog_in_both_states() {
    let mut c = three();
    assert!(c.event(Ev::Line(LineEv::Tab)).is_empty());
    assert_eq!(
        c.event(Ev::Line(LineEv::ChooseFolder)),
        vec![Effect::FolderDialog { at: None }]
    );
    // A cancelled dialog leaves the state where it was.
    assert!(c.event(Ev::Folder(None)).is_empty());
    assert!(!c.view().in_line);
    key(&mut c, 'j');
    assert_eq!(c.view().selected, Some(1));
    key(&mut c, '/');
    assert_eq!(
        c.event(Ev::Line(LineEv::ChooseFolder)),
        vec![Effect::FolderDialog { at: None }]
    );
    assert!(c.event(Ev::Folder(None)).is_empty());
    assert!(c.view().in_line);
    let mut c = three();
    c.event(Ev::Line(LineEv::ChooseFolder));
    assert_eq!(
        c.event(Ev::Folder(Some("/picked".into()))),
        vec![Effect::Open("/picked".into())]
    );
}

#[test]
fn text_that_reaches_the_model_in_the_list_is_typing() {
    let mut c = three();
    line(&mut c, "b");
    assert!(c.view().in_line);
    assert_eq!(c.view().rows.len(), 2);
}

#[test]
fn a_command_key_that_arrives_in_the_line_is_text() {
    let mut c = three();
    key(&mut c, '/');
    line(&mut c, "a");
    let effects = key(&mut c, 'j');
    assert_eq!(effects, vec![Effect::SetLine("aj".into())]);
    assert_eq!(c.view().line, "aj");
    assert!(c.view().in_line);
}

#[test]
fn enter_before_the_rows_arrive_is_held_in_the_list_and_runs_on_the_first_row() {
    for mode in [Mode::Launch, Mode::Window { root: "/here".into() }] {
        let mut c = chooser(mode);
        let Effect::List { serial } = c.shown()[0] else {
            unreachable!()
        };
        assert!(enter(&mut c).is_empty());
        assert_eq!(
            c.event(Ev::Rows {
                serial,
                rows: vec![row("/last", Fact::Present), row("/older", Fact::Present)]
            }),
            vec![Effect::Open("/last".into())]
        );
    }
}

#[test]
fn a_list_key_before_the_rows_arrive_drops_the_held_enter() {
    for ch in ['j', 'G'] {
        let mut c = window();
        let Effect::List { serial } = c.shown()[0] else {
            unreachable!()
        };
        key(&mut c, ch);
        assert!(enter(&mut c).is_empty());
        assert!(key(&mut c, 'k').is_empty());
        assert!(
            c.event(Ev::Rows {
                serial,
                rows: vec![row("/a", Fact::Present)]
            })
            .is_empty(),
            "{ch}"
        );
    }
}

#[test]
fn one_esc_from_the_opening_state_leaves_nothing_changed() {
    for mode in [Mode::Launch, Mode::Window { root: "/here".into() }] {
        let mut c = chooser(mode);
        list(&mut c, vec![row("/a", Fact::Present)]);
        assert_eq!(esc(&mut c), vec![Effect::Close]);
    }
}

#[test]
fn a_launch_first_run_works_with_the_same_keys() {
    let mut c = chooser(Mode::Launch);
    list(&mut c, vec![]);
    assert_eq!(c.view().selected, Some(0));
    assert_eq!(c.view().rows[0].name, "Browse folders…");
    key(&mut c, 'j');
    assert_eq!(c.view().rows[1].name, "Home folder");
    assert_eq!(c.view().selected, Some(1));
    key(&mut c, 'k');
    let effects = enter(&mut c);
    deliver(&mut c, &effects, listing(&[]));
    esc(&mut c);
    key(&mut c, '/');
    line(&mut c, "zzz");
    assert_eq!(c.view().rows.len(), 1);
    assert_eq!(esc(&mut c).len(), 0);
    assert_eq!(c.view().rows.len(), 2);
    assert_eq!(esc(&mut c), vec![Effect::Close]);
}

#[test]
fn list_keys_are_ignored_while_a_start_is_under_way() {
    let mut c = three();
    c.opening("a");
    for ev in [
        Ev::Line(LineEv::Key('j')),
        Ev::Line(LineEv::Key('q')),
        Ev::Line(LineEv::Key('/')),
    ] {
        assert!(c.event(ev).is_empty());
    }
    assert!(c.view().opening);
    assert!(!c.view().in_line);
}

#[test]
fn rows_rev_moves_when_rows_arrive_and_not_on_a_step() {
    let mut c = window();
    let effects = c.shown();
    let Effect::List { serial } = effects[0] else {
        panic!("expected a list request")
    };
    let before = c.view().rows_rev;
    c.event(Ev::Rows {
        serial,
        rows: vec![row("/a", Fact::Present), row("/b", Fact::Present)],
    });
    let arrived = c.view().rows_rev;
    assert!(arrived > before);
    c.event(Ev::Line(LineEv::Step(1)));
    assert_eq!(c.view().rows_rev, arrived);
    line(&mut c, "a");
    assert!(c.view().rows_rev > arrived);
}

#[test]
fn the_rows_arc_is_reused_after_a_step() {
    let mut c = window();
    list(&mut c, vec![row("/a", Fact::Present), row("/b", Fact::Present)]);
    let first = c.view().rows;
    for key in ['j', 'G'] {
        c.event(Ev::Line(LineEv::Key(key)));
        assert!(std::sync::Arc::ptr_eq(&first, &c.view().rows));
    }
}

#[test]
fn a_pick_with_an_old_rows_rev_is_dropped() {
    let mut c = window();
    let effects = c.shown();
    let Effect::List { serial } = effects[0] else {
        panic!("expected a list request")
    };
    let old = c.view().rows_rev;
    c.event(Ev::Rows {
        serial,
        rows: vec![row("/a", Fact::Present)],
    });
    let before = c.view();
    assert_ne!(old, before.rows_rev);
    assert!(c
        .event(Ev::Pick {
            index: 0,
            rows_rev: old
        })
        .is_empty());
    assert_eq!(c.view(), before);
    assert_eq!(
        c.event(Ev::Pick {
            index: 0,
            rows_rev: before.rows_rev
        }),
        vec![Effect::Open("/a".into())]
    );
}

#[test]
fn rows_rev_changes_for_listing_answers_and_opening_state() {
    let mut c = window();
    list(&mut c, vec![row("/a", Fact::Present)]);
    let before = c.view();
    let effects = line(&mut c, "./app/");
    let (serial, dir) = read_request(&effects);
    let waiting = c.view();
    assert!(waiting.rows_rev > before.rows_rev);
    assert!(!Arc::ptr_eq(&before.rows, &waiting.rows));
    c.event(Ev::Listing {
        serial: serial + 1,
        dir,
        listing: Listing::Missing,
    });
    assert_eq!(c.view().rows_rev, waiting.rows_rev);
    deliver(&mut c, &effects, listing(&["x"]));
    let answered = c.view();
    assert!(answered.rows_rev > waiting.rows_rev);
    c.opening("app");
    let opening = c.view();
    assert!(opening.rows_rev > answered.rows_rev);
    c.failed("could not start".into());
    assert!(c.view().rows_rev > opening.rows_rev);
}

#[test]
fn a_pick_from_a_previous_opening_is_stale() {
    let mut old = Chooser::new(Mode::Launch, None, Lang::En, 1);
    let mut current = Chooser::new(Mode::Launch, None, Lang::En, 2);
    list(&mut old, vec![row("/a", Fact::Present)]);
    list(&mut current, vec![row("/b", Fact::Present)]);
    let before = current.view();
    assert!(current
        .event(Ev::Pick {
            index: 0,
            rows_rev: old.view().rows_rev
        })
        .is_empty());
    assert_eq!(current.view(), before);
}

fn modes() -> [Mode; 2] {
    [Mode::Launch, Mode::Window { root: "/here".into() }]
}
fn entry(name: &str) -> Entry {
    Entry {
        raw: name.into(),
        name: name.into(),
        kind: EntryKind::Dir { mount_point: false },
        physical: Some(PathBuf::from("/physical").join(name)),
    }
}
fn read_request(effects: &[Effect]) -> (u64, PathBuf) {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Read { serial, dir, .. } => Some((*serial, dir.clone())),
            _ => None,
        })
        .expect("a folder read was requested")
}
fn deliver(c: &mut Chooser, effects: &[Effect], listing: Listing) -> Vec<Effect> {
    let (serial, dir) = read_request(effects);
    c.event(Ev::Listing { serial, dir, listing })
}
fn listing(names: &[&str]) -> Listing {
    Listing::Read {
        dir: "/physical".into(),
        entries: names.iter().map(|name| entry(name)).collect(),
        capped: None,
        exact: None,
    }
}
fn browse(c: &mut Chooser, path: &str, names: &[&str]) {
    let effects = line(c, path);
    deliver(c, &effects, listing(names));
}
fn side(c: &mut Chooser, direction: i32) -> Vec<Effect> {
    c.event(Ev::Line(LineEv::Side(direction)))
}
fn selected_name(c: &Chooser) -> String {
    let view = c.view();
    view.rows[view.selected.unwrap()].name.clone()
}

macro_rules! transition_project {
    ($name:ident, $key:expr, $dir:expr, $cursor:expr) => {
        #[test]
        fn $name() {
            for mode in modes() {
                let mut c = chooser(mode);
                list(&mut c, vec![row("/projects/app", Fact::Present)]);
                let effects = c.event(Ev::Line($key));
                assert_eq!(read_request(&effects).1, PathBuf::from($dir));
                assert!(c.view().browsing);
                assert!(!c.view().in_line);
                deliver(&mut c, &effects, listing(&["app", "other"]));
                assert_eq!(selected_name(&c), $cursor);
            }
        }
    };
}
transition_project!(
    transition_list_h_on_project_browses_parent_cursor_on_project,
    LineEv::Key('h'),
    "/projects",
    "app/"
);
transition_project!(
    transition_list_left_on_project_browses_parent_cursor_on_project,
    LineEv::Side(-1),
    "/projects",
    "app/"
);
transition_project!(
    transition_list_l_on_project_browses_project_cursor_on_here,
    LineEv::Key('l'),
    "/projects/app",
    "."
);
transition_project!(
    transition_list_right_on_project_browses_project_cursor_on_here,
    LineEv::Side(1),
    "/projects/app",
    "."
);
transition_project!(
    transition_list_tilde_browses_home_cursor_on_here,
    LineEv::Key('~'),
    "/home/me",
    "."
);

#[test]
fn transition_list_minus_in_launch_browses_home() {
    for mode in modes() {
        for ev in [LineEv::Key('-'), LineEv::Enter, LineEv::Key('l'), LineEv::Side(1)] {
            let mut c = chooser(mode.clone());
            list(&mut c, vec![]);
            let expected = if mode == Mode::Launch { "/home/me" } else { "/" };
            let effects = c.event(Ev::Line(ev));
            assert_eq!(read_request(&effects).1, PathBuf::from(expected));
        }
    }
}
#[test]
fn transition_list_h_on_browse_stays_in_the_list() {
    for mode in modes() {
        let mut c = chooser(mode);
        list(&mut c, vec![]);
        assert!(key(&mut c, 'h').is_empty());
        assert!(!c.view().browsing);
    }
}
#[test]
fn transition_list_side_on_home_browses_home_or_its_parent() {
    for (direction, expected, cursor) in [(-1, "/home", "me/"), (1, "/home/me", ".")] {
        let mut c = chooser(Mode::Launch);
        list(&mut c, vec![]);
        c.event(Ev::Line(LineEv::Step(1)));
        let effects = side(&mut c, direction);
        assert_eq!(read_request(&effects).1, PathBuf::from(expected));
        deliver(&mut c, &effects, listing(&["me"]));
        assert_eq!(selected_name(&c), cursor);
    }
}
#[test]
fn transition_filter_typing_a_path_browses_the_folder_and_filters() {
    for mode in modes() {
        for typed in ["~/s", "/mnt/", "./", "../"] {
            let mut c = chooser(mode.clone());
            list(&mut c, vec![]);
            key(&mut c, '/');
            let effects = line(&mut c, typed);
            assert!(c.view().browsing && c.view().in_line);
            assert!(effects.iter().any(|e| matches!(e, Effect::Read { debounce: true, .. })));
        }
    }
}
#[test]
fn transition_path_non_path_text_goes_to_the_projects_filter() {
    for mode in modes() {
        let mut c = chooser(mode);
        list(&mut c, vec![row("/kv", Fact::Open)]);
        browse(&mut c, "~/", &["src"]);
        assert!(line(&mut c, "kv").is_empty());
        assert!(!c.view().browsing);
        assert!(c.view().in_line);
        assert_eq!(selected_name(&c), "kv");
    }
}
#[test]
fn transition_path_backspace_past_tilde_slash_lists_homes_parent_filtered_by_its_name() {
    for mode in modes() {
        let mut c = chooser(mode);
        browse(&mut c, "~/", &[]);
        let effects = line(&mut c, "~");
        assert_eq!(read_request(&effects).1, PathBuf::from("/home"));
        deliver(&mut c, &effects, listing(&["me", "you"]));
        assert_eq!(selected_name(&c), "me/");
    }
}
#[test]
fn transition_path_trailing_dot_dot_names_the_parent() {
    for mode in modes() {
        for (typed, expected) in [("~/src/..", "/home/me/src/.."), ("~/src/.", "/home/me/src")] {
            let mut c = chooser(mode.clone());
            let effects = line(&mut c, typed);
            assert_eq!(read_request(&effects).1, PathBuf::from(expected));
            deliver(&mut c, &effects, listing(&[]));
            assert_eq!(selected_name(&c), ".");
        }
    }
}
#[test]
fn transition_path_esc_returns_to_the_folder_last_shown() {
    for mode in modes() {
        let mut c = chooser(mode);
        browse(&mut c, "~/", &["src"]);
        line(&mut c, "~/missing/half");
        esc(&mut c);
        assert!(c.view().browsing && !c.view().in_line);
        assert_eq!(c.view().line, "~/");
        assert_eq!(selected_name(&c), ".");
    }
}
#[test]
fn transition_path_esc_with_no_folder_shown_returns_to_the_list() {
    for mode in modes() {
        let mut c = chooser(mode);
        let effects = line(&mut c, "/missing/");
        deliver(&mut c, &effects, Listing::Missing);
        esc(&mut c);
        assert!(!c.view().browsing && !c.view().in_line);
        assert_eq!(c.view().line, "");
    }
}
#[test]
fn transition_browse_slash_edits_the_path_and_esc_returns_to_browse() {
    for mode in modes() {
        let mut c = chooser(mode);
        let effects = key(&mut c, '~');
        deliver(&mut c, &effects, listing(&["src"]));
        assert!(key(&mut c, '/').is_empty());
        assert!(c.view().in_line);
        line(&mut c, "~/sr");
        esc(&mut c);
        assert_eq!(c.view().line, "~/");
        assert!(!c.view().in_line);
        assert_eq!(selected_name(&c), ".");
        esc(&mut c);
        assert!(!c.view().browsing);
        assert_eq!(c.view().line, "");
    }
}
#[test]
fn transition_browse_here_right_does_nothing_and_home_keys_browse_home() {
    for mode in modes() {
        let mut c = chooser(mode);
        browse(&mut c, "/else/", &[]);
        esc(&mut c);
        assert!(side(&mut c, 1).is_empty());
        key(&mut c, 'g');
        let effects = key(&mut c, 'h');
        assert_eq!(read_request(&effects).1, PathBuf::from("/home/me"));
        deliver(&mut c, &effects, listing(&[]));
        assert_eq!(selected_name(&c), ".");
        assert_eq!(key(&mut c, 'q'), vec![Effect::Close]);
    }
}
#[test]
fn transition_browse_up_keys_browse_parent_cursor_on_folder_left() {
    for mode in modes() {
        for ev in [LineEv::Key('h'), LineEv::Side(-1), LineEv::Key('-'), LineEv::Up] {
            let mut c = chooser(mode.clone());
            browse(&mut c, "/parent/child/", &[]);
            esc(&mut c);
            let effects = c.event(Ev::Line(ev.clone()));
            assert_eq!(read_request(&effects).1, PathBuf::from("/parent"));
            deliver(&mut c, &effects, listing(&["child"]));
            assert_eq!(selected_name(&c), "child/");
        }
        let mut c = chooser(mode);
        browse(&mut c, "/", &[]);
        esc(&mut c);
        assert!(key(&mut c, 'h').is_empty());
    }
}
#[test]
fn enter_on_a_folder_goes_in_and_lands_on_here() {
    for mode in modes() {
        for in_line in [false, true] {
            let mut c = chooser(mode.clone());
            browse(&mut c, "~/", &["src"]);
            if !in_line {
                esc(&mut c);
            }
            c.event(Ev::Line(LineEv::Step(1)));
            let effects = enter(&mut c);
            assert_eq!(read_request(&effects).1, PathBuf::from("/home/me/src"));
            assert!(!effects.iter().any(|e| matches!(e, Effect::Open(_))));
            deliver(&mut c, &effects, listing(&[]));
            assert_eq!(selected_name(&c), ".");
            assert_eq!(c.view().in_line, in_line);
        }
    }
}
#[test]
fn enter_on_here_opens_only_after_read_or_denied() {
    for mode in modes() {
        for reply in [listing(&[]), Listing::Denied { exact: None }] {
            let mut c = chooser(mode.clone());
            browse(&mut c, "~/", &[]);
            let effects = line(&mut c, "~/src/");
            assert!(enter(&mut c).is_empty());
            assert_eq!(
                deliver(&mut c, &effects, reply),
                vec![Effect::Open("/home/me/src".into())]
            );
        }
    }
}
#[test]
fn enter_on_here_while_stuck_says_cannot_read_and_stays() {
    for reply in [
        Listing::Stuck,
        Listing::Missing,
        Listing::Loop,
        Listing::TooManyStuck,
        Listing::NotAFolder,
    ] {
        let mut c = window();
        let effects = line(&mut c, "/blocked/");
        deliver(&mut c, &effects, reply.clone());
        assert!(enter(&mut c).is_empty());
        assert!(c.view().browsing);
        if reply == Listing::Stuck {
            assert_eq!(c.view().message.as_deref(), Some("cannot read /blocked now"));
        }
    }
}
#[test]
fn held_enter_on_here_waits_for_the_listing() {
    let mut c = window();
    let effects = line(&mut c, "/read/");
    assert!(enter(&mut c).is_empty());
    let (serial, _) = read_request(&effects);
    c.event(Ev::Slow { serial });
    assert!(c.view().message.as_deref().unwrap().contains("reading"));
    assert_eq!(
        deliver(&mut c, &effects, listing(&[])),
        vec![Effect::Open("/read".into())]
    );
}
#[test]
fn two_held_enters_go_in_then_open() {
    for mode in modes() {
        let mut c = chooser(mode);
        let effects = line(&mut c, "~/src/li");
        enter(&mut c);
        enter(&mut c);
        let effects = deliver(&mut c, &effects, listing(&["linux", "little"]));
        assert_eq!(read_request(&effects).1, PathBuf::from("/home/me/src/linux"));
        assert_eq!(
            deliver(&mut c, &effects, listing(&[])),
            vec![Effect::Open("/home/me/src/linux".into())]
        );
    }
}
#[test]
fn a_third_enter_is_not_held_and_a_move_drops_what_is_held() {
    for move_cursor in [false, true] {
        let mut c = window();
        let effects = line(&mut c, "~/src/li");
        enter(&mut c);
        enter(&mut c);
        enter(&mut c);
        assert!(matches!(&c.place, Place::Folder(b) if b.held_enters == 2));
        if move_cursor {
            c.event(Ev::Line(LineEv::Step(1)));
        }
        let effects = deliver(&mut c, &effects, listing(&["linux"]));
        if move_cursor {
            assert!(effects.is_empty());
        } else {
            assert_eq!(
                deliver(&mut c, &effects, listing(&[])),
                vec![Effect::Open("/home/me/src/linux".into())]
            );
            assert!(deliver(&mut c, &effects, listing(&[])).is_empty());
        }
    }
}
#[test]
fn a_typed_folder_with_a_slash_opens_on_one_enter() {
    let mut c = window();
    browse(&mut c, "~/src/", &[]);
    assert_eq!(enter(&mut c), vec![Effect::Open("/home/me/src".into())]);
}
#[test]
fn enter_on_a_mount_or_unknown_link_row_goes_in_and_never_opens() {
    for kind in [
        EntryKind::Dir { mount_point: true },
        EntryKind::Link {
            target: "/mount".into(),
            kind: LinkKind::Unknown,
        },
    ] {
        let mut c = window();
        let effects = line(&mut c, "/mnt/");
        let mut e = entry("remote");
        e.kind = kind;
        deliver(
            &mut c,
            &effects,
            Listing::Read {
                dir: "/mnt".into(),
                entries: vec![e],
                capped: None,
                exact: None,
            },
        );
        c.event(Ev::Line(LineEv::Step(1)));
        let effects = enter(&mut c);
        assert_eq!(read_request(&effects).1, PathBuf::from("/mnt/remote"));
        assert!(!effects.iter().any(|e| matches!(e, Effect::Open(_))));
        deliver(&mut c, &effects, Listing::Stuck);
        assert!(enter(&mut c).is_empty());
    }
}
#[test]
fn the_file_row_appears_only_on_an_exact_name_and_opens() {
    for filter in ["", "note", "notes.md", "NOTES.MD"] {
        let mut c = window();
        let effects = line(&mut c, &format!("~/notes/{filter}"));
        let mut file = entry("notes.md");
        file.kind = EntryKind::File;
        deliver(
            &mut c,
            &effects,
            Listing::Read {
                dir: "/physical".into(),
                entries: vec![file],
                capped: None,
                exact: None,
            },
        );
        let exact = filter == "notes.md";
        assert_eq!(c.view().rows.iter().any(|r| r.kind == RowKind::File), exact);
        if exact {
            assert_eq!(enter(&mut c), vec![Effect::Open("/home/me/notes/notes.md".into())]);
        }
    }
}
#[test]
fn a_link_to_a_file_is_the_file_row() {
    let mut c = window();
    let effects = line(&mut c, "~/link.md");
    let mut e = entry("link.md");
    e.kind = EntryKind::Link {
        target: "notes.md".into(),
        kind: LinkKind::Unknown,
    };
    let (serial, _) = read_request(&effects);
    deliver(
        &mut c,
        &effects,
        Listing::Read {
            dir: "/physical".into(),
            entries: vec![e],
            capped: None,
            exact: None,
        },
    );
    c.event(Ev::Later {
        serial,
        git: vec![],
        links: vec![("link.md".into(), LinkKind::File)],
    });
    assert_eq!(c.view().rows[0].kind, RowKind::File);
    assert_eq!(enter(&mut c), vec![Effect::Open("/home/me/link.md".into())]);
}
#[test]
fn filter_order_is_equal_then_prefix_then_contains_case_insensitive() {
    let mut c = window();
    browse(&mut c, "~/APP", &["xAPP", "app10", "App", "app2", "apple", "other"]);
    assert_eq!(
        c.view().rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        ["App/", "app2/", "app10/", "apple/", "xAPP/"]
    );
}
#[test]
fn hidden_toggle_and_a_dot_filter_read_nothing() {
    let mut c = window();
    browse(&mut c, "~/", &[".config", "src"]);
    esc(&mut c);
    assert!(!c.view().hidden);
    assert!(key(&mut c, '.').is_empty());
    assert!(c.view().hidden);
    assert_eq!(c.view().rows.len(), 3);
    key(&mut c, '.');
    key(&mut c, '/');
    assert!(line(&mut c, "~/.c").is_empty());
    assert_eq!(selected_name(&c), ".config/");
}
#[test]
fn typing_after_the_last_slash_reads_nothing() {
    let mut c = window();
    browse(&mut c, "~/src/", &["linux"]);
    for typed in ["~/src/l", "~/src/li", "~/src/LIN", "~/src/"] {
        assert!(line(&mut c, typed).is_empty());
    }
}
#[test]
fn a_change_to_the_folder_part_asks_one_debounced_read() {
    let mut c = window();
    browse(&mut c, "~/", &["src"]);
    let effects = line(&mut c, "~/src/");
    assert!(matches!(
        effects.as_slice(),
        [Effect::Read {
            debounce: true,
            filter: None,
            ..
        }]
    ));
}
#[test]
fn moves_ask_reads_that_are_not_debounced() {
    for mode in modes() {
        for key_event in [
            LineEv::Key('h'),
            LineEv::Key('l'),
            LineEv::Key('~'),
            LineEv::Key('-'),
            LineEv::Side(1),
        ] {
            let mut c = chooser(mode.clone());
            list(&mut c, vec![row("/projects/app", Fact::Open)]);
            let effects = c.event(Ev::Line(key_event.clone()));
            assert!(effects
                .iter()
                .any(|e| matches!(e, Effect::Read { debounce: false, .. })));
        }
    }
}
#[test]
fn going_up_asks_with_near_the_folder_left() {
    let mut c = window();
    browse(&mut c, "~/src/", &[]);
    esc(&mut c);
    let effects = key(&mut c, 'h');
    assert!(effects
        .iter()
        .any(|e| matches!(e, Effect::Read { near: Some(name), .. } if name == "src")));
}
#[test]
fn ctrl_w_deletes_back_to_the_previous_slash() {
    let mut c = window();
    browse(&mut c, "~/src/li", &["linux"]);
    for expected in ["~/src/", "~/", ""] {
        c.event(Ev::Line(LineEv::DeleteSegment));
        assert_eq!(c.view().line, expected);
    }
}
#[test]
fn tab_completes_and_goes_in_on_one_match() {
    let mut c = window();
    browse(&mut c, "~/src/li", &["linux"]);
    let effects = c.event(Ev::Line(LineEv::Tab));
    assert!(effects.contains(&Effect::SetLine("~/src/linux/".into())));
    assert_eq!(read_request(&effects).1, PathBuf::from("/home/me/src/linux"));
    assert!(effects
        .iter()
        .any(|e| matches!(e, Effect::Read { debounce: false, .. })));
}
#[test]
fn the_cache_shows_a_folder_at_once_and_reads_it_again_keeping_the_cursor_by_name() {
    let mut c = window();
    browse(&mut c, "~/", &["src", "z"]);
    esc(&mut c);
    c.event(Ev::Line(LineEv::Step(1)));
    let effects = enter(&mut c);
    deliver(&mut c, &effects, listing(&[]));
    let effects = key(&mut c, 'h');
    assert_eq!(selected_name(&c), "src/");
    assert!(c.view().rows.iter().any(|r| r.name == "z/"));
    deliver(&mut c, &effects, listing(&["aaa", "src", "z"]));
    assert_eq!(selected_name(&c), "src/");
}
#[test]
fn the_cache_holds_eight_folders() {
    let mut c = window();
    for i in 0..9 {
        browse(&mut c, &format!("/folder{i}/"), &["child"]);
    }
    let effects = line(&mut c, "/folder1/");
    assert!(c.view().rows.iter().any(|r| r.name == "child/"));
    deliver(&mut c, &effects, listing(&["child"]));
    line(&mut c, "/folder0/");
    assert!(!c.view().rows.iter().any(|r| r.name == "child/"));
}
#[test]
fn stuck_keeps_the_serial_and_a_late_listing_on_it_fills_the_rows() {
    let mut c = window();
    let effects = line(&mut c, "/slow/");
    let (serial, _) = read_request(&effects);
    c.event(Ev::Stuck { serial });
    assert!(c.view().message.as_deref().unwrap().contains("not answering"));
    assert!(enter(&mut c).is_empty());
    deliver(&mut c, &effects, listing(&["recovered"]));
    assert!(c.view().rows.iter().any(|r| r.name == "recovered/"));
    assert!(c.view().message.as_deref().is_none_or(|s| !s.contains("cannot read")));
}
#[test]
fn a_listing_stuck_closes_the_serial() {
    let mut c = window();
    let effects = line(&mut c, "/slow/");
    deliver(&mut c, &effects, Listing::Stuck);
    let before = c.view();
    deliver(&mut c, &effects, listing(&["late"]));
    assert_eq!(c.view(), before);
}
#[test]
fn any_freed_rereads_a_shown_stuck_folder() {
    for reply in [Listing::Stuck, Listing::TooManyStuck] {
        let mut c = window();
        let effects = line(&mut c, "~/slow/");
        deliver(&mut c, &effects, reply);
        let effects = c.event(Ev::Freed {
            mount: "/unrelated/physical/mount".into(),
        });
        assert_eq!(read_request(&effects).1, PathBuf::from("/home/me/slow"));
        assert!(c
            .event(Ev::Freed {
                mount: "/another".into()
            })
            .is_empty());
    }
}
#[test]
fn rows_rev_moves_with_the_folder_rows_and_not_on_a_step() {
    let mut c = window();
    browse(&mut c, "~/", &["a", "b", ".h"]);
    esc(&mut c);
    let before = c.view();
    key(&mut c, 'j');
    let after = c.view();
    assert_eq!(after.rows_rev, before.rows_rev);
    assert!(Arc::ptr_eq(&after.rows, &before.rows));
    key(&mut c, '.');
    assert!(c.view().rows_rev > after.rows_rev);
}
#[test]
fn a_pick_twice_with_the_first_press_rev_is_dropped_after_the_rows_changed() {
    let mut c = chooser(Mode::Launch);
    list(&mut c, vec![]);
    let rev = c.view().rows_rev;
    let effects = c.event(Ev::Pick {
        index: 0,
        rows_rev: rev,
    });
    assert!(c
        .event(Ev::PickTwice {
            index: 0,
            rows_rev: rev
        })
        .is_empty());
    deliver(&mut c, &effects, listing(&[]));
    let rev = c.view().rows_rev;
    assert_eq!(
        c.event(Ev::PickTwice {
            index: 0,
            rows_rev: rev
        }),
        vec![Effect::Open("/home/me".into())]
    );
}
#[test]
fn open_and_age_match_recent_rows_by_physical_path() {
    let mut c = window();
    list(
        &mut c,
        vec![
            row("/physical/open", Fact::Open),
            row("/physical/old", Fact::Present),
            row("/home/me/walked", Fact::Open),
        ],
    );
    browse(&mut c, "~/", &["open", "old", "walked"]);
    assert_eq!(c.view().rows.iter().find(|r| r.name == "open/").unwrap().right, "open");
    assert_eq!(c.view().rows.iter().find(|r| r.name == "old/").unwrap().right, "2 h");
    assert_eq!(c.view().rows.iter().find(|r| r.name == "walked/").unwrap().right, "");
}
#[test]
fn the_windows_own_root_reads_open() {
    let mut c = window();
    let effects = line(&mut c, "~/alias/");
    deliver(
        &mut c,
        &effects,
        Listing::Read {
            dir: "/here".into(),
            entries: vec![],
            capped: None,
            exact: None,
        },
    );
    assert!(c.view().rows[0].right.contains("open"));
}
#[test]
fn the_footer_says_into_on_a_folder_and_open_on_here() {
    for mode in modes() {
        for in_line in [false, true] {
            let mut c = chooser(mode.clone());
            browse(&mut c, "~/", &["src"]);
            if !in_line {
                esc(&mut c);
            }
            assert!(c.view().footer.starts_with("Enter open \u{2068}me\u{2069} ·"));
            c.event(Ev::Line(LineEv::Step(1)));
            assert!(c.view().footer.starts_with("Enter into \u{2068}src\u{2069}/ ·"));
        }
    }
}
#[test]
fn name_in_the_footer_is_capped_at_24_with_a_middle_ellipsis() {
    let mut c = window();
    let name = "abcdefghijklmno这是很长的文件夹pqrstuvwxyz";
    browse(&mut c, "~/", &[name]);
    c.event(Ev::Line(LineEv::Step(1)));
    let footer = c.view().footer;
    let shown = footer
        .strip_prefix("Enter into \u{2068}")
        .unwrap()
        .split("\u{2069}/ ·")
        .next()
        .unwrap();
    assert_eq!(shown.chars().count(), 24);
    assert!(shown.contains('…'));
    assert!(shown.starts_with("abcdef") && shown.ends_with("uvwxyz"));
}
#[test]
fn an_edited_escaped_line_reads_as_typed_and_answers_missing() {
    let mut c = window();
    browse(&mut c, "~/", &["bad\u{202e}\u{200b}\n<b>"]);
    c.event(Ev::Line(LineEv::Step(1)));
    let effects = enter(&mut c);
    assert_eq!(
        read_request(&effects).1,
        PathBuf::from("/home/me/bad\u{202e}\u{200b}\n<b>")
    );
    deliver(&mut c, &effects, listing(&[]));
    let shown = c.view().line;
    assert!(shown.contains("\\u{202e}\\u{200b}\\u{a}<b>"));
    assert!(!shown.contains('\u{2068}') && !shown.contains('\u{2069}'));
    let edited = format!("{}x/", shown.trim_end_matches('/'));
    let effects = line(&mut c, &edited);
    assert_eq!(
        read_request(&effects).1,
        PathBuf::from(edited.replacen("~/", "/home/me/", 1))
    );
    deliver(&mut c, &effects, Listing::Missing);
    assert!(enter(&mut c).is_empty());
    assert_eq!(c.view().message, Some(format!("no such folder: {edited}")));
}
#[test]
fn a_non_utf8_row_opens_by_its_raw_bytes() {
    use std::os::unix::ffi::OsStringExt;
    let mut c = window();
    let effects = line(&mut c, "~/");
    let raw = OsString::from_vec(vec![b'a', 0xff]);
    let mut e = entry("a�");
    e.raw = raw.clone();
    deliver(
        &mut c,
        &effects,
        Listing::Read {
            dir: "/physical".into(),
            entries: vec![e],
            capped: None,
            exact: None,
        },
    );
    c.event(Ev::Line(LineEv::Step(1)));
    let effects = enter(&mut c);
    assert_eq!(read_request(&effects).1, PathBuf::from("/home/me").join(&raw));
    deliver(&mut c, &effects, listing(&[]));
    assert_eq!(enter(&mut c), vec![Effect::Open(PathBuf::from("/home/me").join(raw))]);
}
#[test]
fn browse_folders_replaces_choose_a_folder_and_ctrl_o_still_asks_the_dialog() {
    for mode in modes() {
        let mut c = chooser(mode);
        list(&mut c, vec![]);
        assert!(c.view().rows.iter().any(|r| r.name == "Browse folders…"));
        assert_eq!(
            c.event(Ev::Line(LineEv::ChooseFolder)),
            vec![Effect::FolderDialog { at: None }]
        );
        c.event(Ev::Folder(None));
        let effects = enter(&mut c);
        assert!(effects.iter().any(|e| matches!(e, Effect::Read { .. })));
    }
}
#[test]
fn esc_esc_from_browse_in_a_first_run_launch_window_returns_to_its_two_rows_then_closes() {
    let mut c = chooser(Mode::Launch);
    list(&mut c, vec![]);
    let effects = enter(&mut c);
    deliver(&mut c, &effects, listing(&[]));
    esc(&mut c);
    assert_eq!(c.view().rows.len(), 2);
    assert_eq!(c.view().rows[0].name, "Browse folders…");
    assert_eq!(esc(&mut c), vec![Effect::Close]);
}
#[test]
fn ctrl_o_starts_the_dialog_at_a_folder_only_if_it_answered() {
    for reply in [
        None,
        Some(listing(&[])),
        Some(Listing::Denied { exact: None }),
        Some(Listing::Stuck),
        Some(Listing::Missing),
    ] {
        let mut c = window();
        let effects = line(&mut c, "~/src/");
        let answered = matches!(reply, Some(Listing::Read { .. } | Listing::Denied { .. }));
        if let Some(reply) = reply {
            deliver(&mut c, &effects, reply);
        }
        assert_eq!(
            c.event(Ev::Line(LineEv::ChooseFolder)),
            vec![Effect::FolderDialog {
                at: answered.then(|| PathBuf::from("/home/me/src"))
            }]
        );
    }
}
#[test]
fn every_new_string_has_both_languages() {
    for (lang, title, hidden, here, empty, denied, looped, too_many, not_folder, reading, stuck) in [
        (
            Lang::En,
            "Browse folders",
            " · hidden shown",
            "this folder",
            "no folders here · Enter opens this folder",
            "cannot read: permission denied · h goes back",
            "cannot read: too many links",
            "too many folders not answering · Ctrl+o for the system dialog",
            "not a folder",
            "reading",
            "not answering · h goes back",
        ),
        (
            Lang::ZhCn,
            "浏览文件夹",
            " · 显示隐藏",
            "此文件夹",
            "这里没有文件夹 · Enter 打开此文件夹",
            "读不了：没有权限 · h 返回",
            "读不了：链接太多",
            "太多文件夹没有响应 · Ctrl+o 用系统对话框",
            "不是文件夹",
            "正在读取",
            "没有响应 · h 返回",
        ),
    ] {
        let mut c = Chooser::new(Mode::Launch, Some("/home/me".into()), lang, 1);
        browse(&mut c, "~/", &[]);
        assert_eq!(c.view().title, title);
        assert_eq!(c.view().rows[0].right, here);
        assert_eq!(c.view().message.as_deref(), Some(empty));
        esc(&mut c);
        key(&mut c, '.');
        assert_eq!(c.view().title, format!("{title}{hidden}"));
        for (i, reply, expected) in [
            (0, Listing::Denied { exact: None }, denied),
            (1, Listing::Loop, looped),
            (2, Listing::TooManyStuck, too_many),
            (3, Listing::NotAFolder, not_folder),
            (4, Listing::Stuck, stuck),
        ] {
            let effects = line(&mut c, &format!("/folder{i}/"));
            deliver(&mut c, &effects, reply);
            let message = c.view().message.unwrap();
            assert_eq!(message.split('\t').next(), Some(expected));
        }
        let effects = line(&mut c, "/last/");
        let (serial, _) = read_request(&effects);
        c.event(Ev::Slow { serial });
        assert_eq!(c.view().message.as_deref(), Some(format!("{reading}\t/last").as_str()));
    }
}

#[test]
fn capped_and_denied_listings_reread_with_the_current_filter_and_keep_held_enters() {
    for reply in [
        Listing::Read {
            dir: "/physical".into(),
            entries: vec![],
            capped: Some(Cap::Folders),
            exact: None,
        },
        Listing::Denied { exact: None },
    ] {
        let mut c = window();
        let effects = line(&mut c, "~/src/proj");
        enter(&mut c);
        enter(&mut c);
        let effects = deliver(&mut c, &effects, reply);
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::Read { filter: Some(f), debounce: false, .. } if f == "proj")));
        let effects = deliver(
            &mut c,
            &effects,
            Listing::Denied {
                exact: Some(entry("proj")),
            },
        );
        assert_eq!(read_request(&effects).1, PathBuf::from("/home/me/src/proj"));
        assert_eq!(
            deliver(&mut c, &effects, Listing::Denied { exact: None }),
            vec![Effect::Open("/home/me/src/proj".into())]
        );
    }
}
#[test]
fn typing_in_a_capped_folder_rereads_and_filtered_answers_are_not_cached() {
    let mut c = window();
    let effects = line(&mut c, "~/src/");
    deliver(
        &mut c,
        &effects,
        Listing::Read {
            dir: "/physical".into(),
            entries: vec![entry("first")],
            capped: Some(Cap::Folders),
            exact: None,
        },
    );
    let effects = line(&mut c, "~/src/last");
    assert!(
        matches!(effects.as_slice(), [Effect::Read { filter: Some(filter), debounce: true, .. }] if filter == "last")
    );
    deliver(&mut c, &effects, listing(&["last"]));
    line(&mut c, "/else/");
    let effects = line(&mut c, "~/src/");
    assert!(c.view().rows.iter().any(|row| row.name == "first/"));
    assert!(!c.view().rows.iter().any(|row| row.name == "last/"));
    assert!(effects
        .iter()
        .any(|effect| matches!(effect, Effect::Read { filter: None, .. })));
}
#[test]
fn a_filter_changed_during_a_capped_read_is_used_for_the_next_read() {
    let mut c = window();
    let effects = line(&mut c, "/capped/");
    deliver(
        &mut c,
        &effects,
        Listing::Read {
            dir: "/physical".into(),
            entries: vec![],
            capped: Some(Cap::Scanned),
            exact: None,
        },
    );
    let old = line(&mut c, "/capped/old");
    let newer = line(&mut c, "/capped/new");
    assert!(newer
        .iter()
        .any(|e| matches!(e, Effect::Read { filter: Some(f), .. } if f == "new")));
    let before = c.view();
    assert!(deliver(&mut c, &old, listing(&["old"])).is_empty());
    assert_eq!(c.view(), before);
}
#[test]
fn a_stuck_timer_drops_held_enters_and_late_rows_never_open_by_themselves() {
    let mut c = window();
    let effects = line(&mut c, "/slow/");
    let (serial, _) = read_request(&effects);
    enter(&mut c);
    enter(&mut c);
    c.event(Ev::Stuck { serial });
    assert!(deliver(&mut c, &effects, listing(&[])).is_empty());
    assert_eq!(enter(&mut c), vec![Effect::Open("/slow".into())]);
}
#[test]
fn the_listing_envelope_matches_the_walked_folder_and_rejects_the_physical_folder() {
    let mut c = window();
    let effects = line(&mut c, "~/link/");
    let (serial, _) = read_request(&effects);
    let before = c.view();
    c.event(Ev::Listing {
        serial,
        dir: "/physical".into(),
        listing: listing(&["wrong"]),
    });
    assert_eq!(c.view(), before);
    deliver(&mut c, &effects, listing(&["right"]));
    assert!(c.view().rows.iter().any(|r| r.name == "right/"));
}
#[test]
fn editing_only_the_filter_of_an_escaped_folder_keeps_its_raw_path() {
    let mut c = window();
    browse(&mut c, "~/", &["a\n"]);
    c.event(Ev::Line(LineEv::Step(1)));
    let effects = enter(&mut c);
    deliver(&mut c, &effects, listing(&["child"]));
    assert!(line(&mut c, "~/a\\u{a}/ch").is_empty());
    let effects = enter(&mut c);
    assert_eq!(read_request(&effects).1, PathBuf::from("/home/me/a\n/child"));
}
#[test]
fn a_broken_link_says_not_a_folder_and_its_target_is_safe_text() {
    let mut c = window();
    let effects = line(&mut c, "~/");
    let mut e = entry("bad\u{202e}");
    e.kind = EntryKind::Link {
        target: "<b>\n\u{200b}".into(),
        kind: LinkKind::Other,
    };
    deliver(
        &mut c,
        &effects,
        Listing::Read {
            dir: "/physical".into(),
            entries: vec![e],
            capped: None,
            exact: None,
        },
    );
    assert_eq!(c.view().rows[1].shown, "-> <b>\\u{a}\\u{200b}");
    assert!(c.view().rows[1].dimmed);
    c.event(Ev::Line(LineEv::Step(1)));
    assert!(enter(&mut c).is_empty());
    assert_eq!(c.view().message.as_deref(), Some("not a folder"));
}
#[test]
fn exact_file_rows_in_denied_and_capped_folders_open_by_the_raw_path() {
    for denied in [false, true] {
        let mut c = window();
        let initial = line(&mut c, "/private/");
        deliver(
            &mut c,
            &initial,
            if denied {
                Listing::Denied { exact: None }
            } else {
                Listing::Read {
                    dir: "/physical".into(),
                    entries: vec![],
                    capped: Some(Cap::Scanned),
                    exact: None,
                }
            },
        );
        let effects = line(&mut c, "/private/notes.md");
        let mut e = entry("notes.md");
        e.kind = EntryKind::File;
        deliver(
            &mut c,
            &effects,
            if denied {
                Listing::Denied { exact: Some(e) }
            } else {
                Listing::Read {
                    dir: "/physical".into(),
                    entries: vec![],
                    capped: Some(Cap::Scanned),
                    exact: Some(e),
                }
            },
        );
        assert_eq!(selected_name(&c), "notes.md");
        assert_eq!(enter(&mut c), vec![Effect::Open("/private/notes.md".into())]);
    }
}
#[test]
fn later_marks_and_link_kinds_keep_the_cursor_and_ignore_old_requests() {
    let mut c = window();
    let effects = line(&mut c, "~/");
    let (serial, _) = read_request(&effects);
    deliver(&mut c, &effects, listing(&["a", "b"]));
    c.event(Ev::Line(LineEv::Step(2)));
    let before = c.view();
    c.event(Ev::Later {
        serial: serial + 1,
        git: vec!["b".into()],
        links: vec![],
    });
    assert_eq!(c.view(), before);
    c.event(Ev::Later {
        serial,
        git: vec!["b".into()],
        links: vec![],
    });
    assert_eq!(selected_name(&c), "b/");
    assert_eq!(c.view().rows[2].right, "git");
}

macro_rules! folder_transition {
    ($name:ident, $event:expr) => {
        #[test]
        fn $name() {
            for mode in modes() {
                for in_line in [false, true] {
                    let mut c = chooser(mode.clone());
                    browse(&mut c, "~/src/", &["app"]);
                    if !in_line {
                        esc(&mut c);
                    }
                    c.event(Ev::Line(LineEv::Step(1)));
                    let effects = c.event($event(c.view().rows_rev));
                    assert_eq!(read_request(&effects).1, PathBuf::from("/home/me/src/app"));
                    deliver(&mut c, &effects, listing(&[]));
                    assert_eq!(selected_name(&c), ".");
                    assert_eq!(c.view().in_line, in_line);
                }
            }
        }
    };
}
folder_transition!(transition_browse_enter_on_folder_enters_cursor_on_here, |_| Ev::Line(
    LineEv::Enter
));
folder_transition!(transition_browse_double_click_on_folder_enters_cursor_on_here, |rev| {
    Ev::PickTwice {
        index: 1,
        rows_rev: rev,
    }
});
#[test]
fn transition_browse_l_on_folder_enters_cursor_on_here() {
    for mode in modes() {
        for ev in [LineEv::Key('l'), LineEv::Side(1)] {
            let mut c = chooser(mode.clone());
            browse(&mut c, "~/src/", &["app"]);
            esc(&mut c);
            c.event(Ev::Line(LineEv::Step(1)));
            let effects = c.event(Ev::Line(ev.clone()));
            assert_eq!(read_request(&effects).1, PathBuf::from("/home/me/src/app"));
        }
    }
}
#[test]
fn transition_browse_click_selects_and_double_click_on_here_opens() {
    for mode in modes() {
        let mut c = chooser(mode);
        browse(&mut c, "~/src/", &["app"]);
        esc(&mut c);
        c.event(Ev::Line(LineEv::Step(1)));
        let rev = c.view().rows_rev;
        assert!(c
            .event(Ev::Pick {
                index: 0,
                rows_rev: rev
            })
            .is_empty());
        assert_eq!(selected_name(&c), ".");
        assert_eq!(
            c.event(Ev::PickTwice {
                index: 0,
                rows_rev: rev
            }),
            vec![Effect::Open("/home/me/src".into())]
        );
    }
}
#[test]
fn transition_list_click_on_browse_browses_without_opening_home() {
    for mode in modes() {
        let mut c = chooser(mode.clone());
        list(&mut c, vec![]);
        let effects = c.event(Ev::Pick {
            index: 0,
            rows_rev: c.view().rows_rev,
        });
        assert_eq!(
            read_request(&effects).1,
            PathBuf::from(if mode == Mode::Launch { "/home/me" } else { "/" })
        );
        assert!(!effects.iter().any(|e| matches!(e, Effect::Open(_))));
    }
}
#[test]
fn transition_filter_esc_returns_to_the_list() {
    for mode in modes() {
        let mut c = chooser(mode);
        list(&mut c, vec![row("/app", Fact::Present)]);
        key(&mut c, '/');
        line(&mut c, "ap");
        esc(&mut c);
        assert!(!c.view().browsing && !c.view().in_line);
        assert_eq!(c.view().line, "");
    }
}
#[test]
fn transition_list_slash_edits_the_projects_filter() {
    for mode in modes() {
        let mut c = chooser(mode);
        assert!(key(&mut c, '/').is_empty());
        assert!(c.view().in_line && !c.view().browsing);
        assert_eq!(c.view().line, "");
    }
}
#[test]
fn transition_list_esc_or_q_closes() {
    for mode in modes() {
        for ev in [LineEv::Escape, LineEv::Key('q')] {
            let mut c = chooser(mode.clone());
            assert_eq!(c.event(Ev::Line(ev.clone())), vec![Effect::Close]);
        }
    }
}
#[test]
fn transition_any_dismiss_or_lost_keys_closes() {
    for mode in modes() {
        for in_line in [false, true] {
            for ev in [Ev::Dismiss, Ev::Line(LineEv::KeysLost)] {
                let mut c = chooser(mode.clone());
                browse(&mut c, "~/", &[]);
                if !in_line {
                    esc(&mut c);
                }
                assert_eq!(c.event(ev.clone()), vec![Effect::Close]);
            }
        }
    }
}

#[test]
fn another_filter_after_a_filtered_listing_still_searches_the_whole_folder() {
    let mut c = window();
    let effects = line(&mut c, "/capped/");
    deliver(
        &mut c,
        &effects,
        Listing::Read {
            dir: "/physical".into(),
            entries: vec![entry("first")],
            capped: Some(Cap::Folders),
            exact: None,
        },
    );
    let effects = line(&mut c, "/capped/one");
    deliver(&mut c, &effects, listing(&["one"]));
    let effects = line(&mut c, "/capped/two");
    assert!(
        matches!(effects.as_slice(), [Effect::Read { filter: Some(filter), debounce: true, .. }] if filter == "two")
    );
}
#[test]
fn esc_from_a_filtered_read_restores_the_full_cached_listing_and_refreshes_it() {
    let mut c = window();
    let effects = line(&mut c, "/capped/");
    deliver(
        &mut c,
        &effects,
        Listing::Read {
            dir: "/physical".into(),
            entries: vec![entry("first")],
            capped: Some(Cap::Folders),
            exact: None,
        },
    );
    let effects = line(&mut c, "/capped/last");
    deliver(&mut c, &effects, listing(&["last"]));
    let effects = esc(&mut c);
    assert_eq!(c.view().line, "/capped/");
    assert_eq!(selected_name(&c), ".");
    assert!(c.view().rows.iter().any(|r| r.name == "first/"));
    assert!(effects.iter().any(|e| matches!(
        e,
        Effect::Read {
            filter: None,
            debounce: false,
            ..
        }
    )));
}

#[test]
fn a_double_click_in_projects_is_already_handled_by_its_first_press() {
    let mut c = three();
    let before = c.view();
    assert!(c
        .event(Ev::PickTwice {
            index: 0,
            rows_rev: before.rows_rev
        })
        .is_empty());
    assert_eq!(c.view(), before);
}
#[test]
fn a_late_listing_and_timers_keep_the_opening_message() {
    for capped in [false, true] {
        let mut c = window();
        let effects = line(&mut c, "/work/app");
        let (serial, _) = read_request(&effects);
        c.opening("picked");
        for ev in [Ev::Slow { serial }, Ev::Stuck { serial }] {
            c.event(ev);
            assert_eq!(c.view().message.as_deref(), Some("Opening picked…"));
        }
        deliver(
            &mut c,
            &effects,
            Listing::Read {
                dir: "/physical".into(),
                entries: vec![entry("app")],
                capped: capped.then_some(Cap::Folders),
                exact: None,
            },
        );
        assert_eq!(c.view().message.as_deref(), Some("Opening picked…"));
    }
}
#[test]
fn typing_a_trailing_dot_rereads_the_folder_and_keeps_an_escaped_prefix_raw() {
    let mut c = window();
    browse(&mut c, "~/", &["a\n"]);
    c.event(Ev::Line(LineEv::Step(1)));
    let effects = enter(&mut c);
    deliver(&mut c, &effects, listing(&[]));
    let effects = line(&mut c, "~/a\\u{a}/.");
    assert_eq!(read_request(&effects).1, PathBuf::from("/home/me/a\n"));
    assert!(effects.iter().any(|e| matches!(e, Effect::Read { debounce: true, .. })));
    deliver(&mut c, &effects, listing(&[]));
    let effects = line(&mut c, "~/a\\u{a}/./..");
    assert_eq!(read_request(&effects).1, PathBuf::from("/home/me/a\n/.."));
}
#[test]
fn all_browse_marks_caps_and_dynamic_copy_have_both_languages() {
    for (lang, marks, caps, no_match, cannot_read, browse_open, browse_into, path_open, path_into) in [
        (
            Lang::En,
            ["git", "link", "mount", "file"],
            [
                "first 2000 folders shown · / searches the whole folder",
                "first 50 000 entries read",
            ],
            "no folder matches \"app\"",
            "cannot read /dir now",
            "Enter open \u{2068}app\u{2069} · l in · h up · / filter · . hidden · Esc back · q close",
            "Enter into \u{2068}app\u{2069}/ · h up · / filter · . hidden · Esc back · q close",
            "Enter open \u{2068}app\u{2069} · Tab complete · ↑↓ move · Ctrl+w up · Esc clear",
            "Enter into \u{2068}app\u{2069}/ · Tab complete · ↑↓ move · Ctrl+w up · Esc clear",
        ),
        (
            Lang::ZhCn,
            ["git", "链接", "挂载点", "文件"],
            ["只显示前 2000 个文件夹 · / 搜索整个文件夹", "只读了前 50 000 项"],
            "没有匹配“app”的文件夹",
            "现在读不了 /dir",
            "Enter 打开 \u{2068}app\u{2069} · l 进入 · h 上一级 · / 筛选 · . 隐藏 · Esc 返回 · q 关闭",
            "Enter 进入 \u{2068}app\u{2069}/ · h 上一级 · / 筛选 · . 隐藏 · Esc 返回 · q 关闭",
            "Enter 打开 \u{2068}app\u{2069} · Tab 补全 · ↑↓ 移动 · Ctrl+w 上一级 · Esc 清除",
            "Enter 进入 \u{2068}app\u{2069}/ · Tab 补全 · ↑↓ 移动 · Ctrl+w 上一级 · Esc 清除",
        ),
    ] {
        let t = text::text(lang);
        assert_eq!([t.git, t.link, t.mount, t.file], marks);
        assert_eq!([t.folders_cap, t.scanned_cap], caps);
        assert_eq!(text::no_match(lang, "app"), no_match);
        assert_eq!(text::cannot_read(lang, "/dir"), cannot_read);
        assert_eq!(text::footer(lang, "app", text::Enter::Open, false), browse_open);
        assert_eq!(text::footer(lang, "app", text::Enter::Into, false), browse_into);
        assert_eq!(text::footer(lang, "app", text::Enter::Open, true), path_open);
        assert_eq!(text::footer(lang, "app", text::Enter::Into, true), path_into);
    }
}

#[test]
fn a_cached_refresh_keeps_here_selected_when_the_cursor_moved_off_the_near_folder() {
    let mut c = window();
    browse(&mut c, "~/", &["src", "z"]);
    esc(&mut c);
    c.event(Ev::Line(LineEv::Step(1)));
    let effects = enter(&mut c);
    deliver(&mut c, &effects, listing(&[]));
    let effects = key(&mut c, 'h');
    assert_eq!(selected_name(&c), "src/");
    key(&mut c, 'k');
    assert_eq!(selected_name(&c), ".");
    deliver(&mut c, &effects, listing(&["src", "z"]));
    assert_eq!(selected_name(&c), ".");
}
#[test]
fn tab_completes_a_shared_prefix_without_case() {
    let mut c = window();
    browse(&mut c, "~/AP", &["AppOne", "appTwo"]);
    c.event(Ev::Line(LineEv::Tab));
    assert_eq!(c.view().line, "~/App");
}
#[test]
fn a_launch_without_home_can_still_browse_the_root() {
    let mut c = Chooser::new(Mode::Launch, None, Lang::En, 1);
    list(&mut c, vec![]);
    let effects = enter(&mut c);
    assert_eq!(read_request(&effects).1, PathBuf::from("/"));
}
#[test]
fn going_up_from_a_folder_reached_by_dot_dot_walks_one_more_dot_dot() {
    for mode in modes() {
        for up in [LineEv::Key('h'), LineEv::Key('-'), LineEv::Side(-1), LineEv::Up] {
            let mut c = chooser(mode.clone());
            browse(&mut c, "~/src/..", &["a"]);
            esc(&mut c);
            let effects = c.event(Ev::Line(up.clone()));
            let (_, dir) = read_request(&effects);
            assert_eq!(dir, PathBuf::from("/home/me/src/../.."), "{up:?}");
            assert!(effects.iter().any(|e| matches!(e, Effect::Read { near: None, .. })));
        }
    }
}
#[test]
fn the_footer_names_only_the_keys_that_work_on_the_row() {
    let mut c = window();
    let effects = line(&mut c, "~/");
    let mut dead = entry("x");
    dead.kind = EntryKind::Link {
        target: "nowhere".into(),
        kind: LinkKind::Other,
    };
    deliver(
        &mut c,
        &effects,
        Listing::Read {
            dir: "/physical".into(),
            entries: vec![dead],
            capped: None,
            exact: None,
        },
    );
    esc(&mut c);
    // `.` opens and `l` goes in; a dead link has no Enter action at all.
    assert_eq!(selected_name(&c), ".");
    assert!(c.view().footer.starts_with("Enter open \u{2068}me\u{2069} · l in ·"));
    c.event(Ev::Line(LineEv::Step(1)));
    assert_eq!(selected_name(&c), "x/");
    assert!(c.view().rows[c.view().selected.unwrap()].dimmed);
    assert!(c.view().footer.starts_with("h up ·"), "{}", c.view().footer);
    assert!(!c.view().footer.contains("Enter"));
    // A file opens, but `l` does nothing on it.
    let mut c = window();
    let effects = line(&mut c, "~/notes.md");
    let mut file = entry("notes.md");
    file.kind = EntryKind::File;
    deliver(
        &mut c,
        &effects,
        Listing::Read {
            dir: "/physical".into(),
            entries: vec![],
            capped: None,
            exact: Some(file),
        },
    );
    assert_eq!(selected_name(&c), "notes.md");
    assert!(c
        .view()
        .footer
        .starts_with("Enter open \u{2068}notes.md\u{2069} · Tab complete"));
    for lang in [Lang::En, Lang::ZhCn] {
        let footer = text::footer(lang, "notes.md", text::Enter::OpenFile, false);
        assert!(
            footer.contains("h ") && !footer.contains("l in") && !footer.contains("l 进入"),
            "{footer}"
        );
        assert_ne!(footer, text::footer(lang, "notes.md", text::Enter::Open, false));
    }
}
#[test]
fn tab_does_not_write_hidden_characters_back_as_a_filter() {
    let mut c = window();
    browse(&mut c, "~/", &["a\u{200b}x", "a\u{200b}y"]);
    let effects = c.event(Ev::Line(LineEv::Tab));
    assert!(effects.contains(&Effect::SetLine("~/a".into())));
    let view = c.view();
    assert_eq!(view.line, "~/a");
    assert_eq!(view.rows.len(), 2);
    assert!(view.message.as_deref().is_none_or(|m| !m.contains("no folder matches")));
}
#[test]
fn a_later_with_nothing_to_add_keeps_the_rows_and_their_revision() {
    let mut c = window();
    let effects = line(&mut c, "~/");
    let (serial, _) = read_request(&effects);
    deliver(&mut c, &effects, listing(&["a", "b"]));
    esc(&mut c);
    let before = c.view();
    c.event(Ev::Later {
        serial,
        git: vec![],
        links: vec![],
    });
    let after = c.view();
    assert_eq!(after.rows_rev, before.rows_rev);
    assert!(Arc::ptr_eq(&after.rows, &before.rows));
    c.event(Ev::Later {
        serial,
        git: vec!["a".into()],
        links: vec![],
    });
    assert!(c.view().rows_rev > before.rows_rev);
    assert!(c.view().rows.iter().any(|r| r.right.contains("git")));
}
#[test]
fn the_dialog_starts_at_a_folder_only_when_its_fresh_read_has_answered() {
    let mut c = window();
    browse(&mut c, "~/", &["src"]);
    esc(&mut c);
    c.event(Ev::Line(LineEv::Step(1)));
    let effects = enter(&mut c);
    deliver(&mut c, &effects, listing(&[]));
    // Back in `~/`, shown from the cache while its re-read is still out.
    let reread = key(&mut c, 'h');
    assert!(reread.iter().any(|e| matches!(e, Effect::Read { .. })));
    assert_eq!(
        c.event(Ev::Line(LineEv::ChooseFolder)),
        vec![Effect::FolderDialog { at: None }]
    );
}

fn link_entry(name: &str, kind: LinkKind) -> Entry {
    Entry {
        kind: EntryKind::Link {
            target: "target".into(),
            kind,
        },
        ..entry(name)
    }
}
fn read_of(entries: Vec<Entry>, exact: Option<Entry>) -> Listing {
    Listing::Read {
        dir: "/physical".into(),
        entries,
        capped: None,
        exact,
    }
}
fn exact_read<'a>(effects: &'a [Effect]) -> Option<(&'a Path, &'a str)> {
    effects.iter().find_map(|effect| match effect {
        Effect::Read {
            dir,
            filter: Some(filter),
            debounce: false,
            ..
        } => Some((dir.as_path(), filter.as_str())),
        _ => None,
    })
}

#[test]
fn going_up_out_of_a_dot_folder_lands_on_it_with_hidden_folders_off() {
    for mode in modes() {
        let mut c = chooser(mode);
        browse(&mut c, "~/.config/", &["nvim"]);
        esc(&mut c);
        let effects = key(&mut c, 'h');
        assert_eq!(read_request(&effects).1, PathBuf::from("/home/me"));
        deliver(&mut c, &effects, listing(&[".cache", ".config", "src"]));
        assert!(!c.view().hidden);
        assert_eq!(selected_name(&c), ".config/");
        assert!(
            !c.view().rows.iter().any(|r| r.name == ".cache/"),
            "other hidden folders stay hidden"
        );
        let effects = enter(&mut c);
        assert_eq!(read_request(&effects).1, PathBuf::from("/home/me/.config"));
    }
}
#[test]
fn h_on_a_dot_named_project_lands_on_it_in_its_parent() {
    for mode in modes() {
        let mut c = chooser(mode);
        list(&mut c, vec![row("/home/me/.dotfiles", Fact::Present)]);
        let effects = key(&mut c, 'h');
        deliver(&mut c, &effects, listing(&[".cache", ".dotfiles", "src"]));
        assert_eq!(selected_name(&c), ".dotfiles/");
        assert!(!c.view().rows.iter().any(|r| r.name == ".cache/"));
    }
}
#[test]
fn reading_stuck_denied_and_loop_messages_name_their_folder_after_a_tab() {
    for (typed, folder) in [("/mnt/nas/", "/mnt/nas"), ("~/nas/", "~/nas")] {
        let words =
            |message: Option<String>| message.map(|m| m.split_once('\t').map(|(w, p)| (w.to_owned(), p.to_owned())));
        let mut c = window();
        let effects = line(&mut c, typed);
        let (serial, _) = read_request(&effects);
        c.event(Ev::Slow { serial });
        assert_eq!(words(c.view().message), Some(Some(("reading".into(), folder.into()))));
        c.event(Ev::Stuck { serial });
        assert_eq!(
            words(c.view().message),
            Some(Some(("not answering · h goes back".into(), folder.into())))
        );
        for (reply, expected) in [
            (Listing::Stuck, "not answering · h goes back"),
            (
                Listing::Denied { exact: None },
                "cannot read: permission denied · h goes back",
            ),
            (Listing::Loop, "cannot read: too many links"),
        ] {
            let mut c = window();
            answer(&mut c, typed, reply.clone());
            assert_eq!(words(c.view().message), Some(Some((expected.into(), folder.into()))));
            if reply == Listing::Loop {
                assert!(enter(&mut c).is_empty());
                assert_eq!(words(c.view().message), Some(Some((expected.into(), folder.into()))));
            }
        }
    }
}
#[test]
fn messages_without_a_folder_have_no_tab() {
    for reply in [Listing::TooManyStuck, Listing::NotAFolder, Listing::Missing] {
        let mut c = window();
        answer(&mut c, "/mnt/nas/", reply);
        assert!(!c.view().message.unwrap().contains('\t'));
    }
}
#[test]
fn the_footers_name_sits_in_its_own_isolate() {
    let mut c = window();
    browse(&mut c, "~/", &["שלום"]);
    esc(&mut c);
    assert!(c.view().footer.starts_with("Enter open \u{2068}me\u{2069} · l in ·"));
    c.event(Ev::Line(LineEv::Step(1)));
    assert!(c.view().footer.starts_with("Enter into \u{2068}שלום\u{2069}/ · h up ·"));
}
#[test]
fn a_held_enter_on_an_exactly_typed_link_has_it_classified_then_opens_a_file() {
    for mode in modes() {
        let mut c = chooser(mode);
        let effects = line(&mut c, "~/link.md");
        assert!(enter(&mut c).is_empty());
        let effects = deliver(
            &mut c,
            &effects,
            read_of(vec![link_entry("link.md", LinkKind::Unknown), entry("link.md.d")], None),
        );
        assert_eq!(exact_read(&effects), Some((Path::new("/home/me"), "link.md")));
        assert!(!effects
            .iter()
            .any(|e| matches!(e, Effect::Read { dir, .. } if dir.ends_with("link.md"))));
        let file = link_entry("link.md", LinkKind::File);
        assert_eq!(
            deliver(&mut c, &effects, read_of(vec![file.clone()], Some(file))),
            vec![Effect::Open("/home/me/link.md".into())]
        );
    }
}
#[test]
fn an_enter_before_the_links_kind_is_known_has_it_classified_first() {
    let mut c = window();
    let effects = line(&mut c, "~/x/link.md");
    deliver(
        &mut c,
        &effects,
        read_of(vec![link_entry("link.md", LinkKind::Unknown)], None),
    );
    let effects = enter(&mut c);
    assert_eq!(exact_read(&effects), Some((Path::new("/home/me/x"), "link.md")));
    let file = link_entry("link.md", LinkKind::File);
    assert_eq!(
        deliver(&mut c, &effects, read_of(vec![file.clone()], Some(file))),
        vec![Effect::Open("/home/me/x/link.md".into())]
    );
}
#[test]
fn a_classified_folder_link_is_entered_with_both_held_enters_kept() {
    let mut c = window();
    let effects = line(&mut c, "~/data");
    enter(&mut c);
    enter(&mut c);
    let effects = deliver(
        &mut c,
        &effects,
        read_of(vec![link_entry("data", LinkKind::Unknown)], None),
    );
    assert_eq!(exact_read(&effects), Some((Path::new("/home/me"), "data")));
    let dir = link_entry("data", LinkKind::Dir);
    let effects = deliver(&mut c, &effects, read_of(vec![dir.clone()], Some(dir)));
    assert_eq!(read_request(&effects).1, PathBuf::from("/home/me/data"));
    assert_eq!(
        deliver(&mut c, &effects, listing(&[])),
        vec![Effect::Open("/home/me/data".into())]
    );
}
#[test]
fn a_link_still_unknown_after_its_classification_is_entered_and_not_asked_again() {
    let mut c = window();
    let effects = line(&mut c, "~/nas");
    enter(&mut c);
    let effects = deliver(
        &mut c,
        &effects,
        read_of(vec![link_entry("nas", LinkKind::Unknown)], None),
    );
    assert!(exact_read(&effects).is_some());
    let unknown = link_entry("nas", LinkKind::Unknown);
    let effects = deliver(&mut c, &effects, read_of(vec![unknown.clone()], Some(unknown)));
    assert_eq!(read_request(&effects).1, PathBuf::from("/home/me/nas"));
    assert!(exact_read(&effects).is_none());
}
#[test]
fn a_classification_keeps_the_whole_listing_and_its_marks() {
    let mut c = window();
    let effects = line(&mut c, "~/odd");
    deliver(
        &mut c,
        &effects,
        read_of(
            vec![link_entry("odd", LinkKind::Unknown), entry("odds"), entry("other")],
            None,
        ),
    );
    let effects = enter(&mut c);
    let (serial, _) = read_request(&effects);
    let other = link_entry("odd", LinkKind::Other);
    assert!(deliver(&mut c, &effects, read_of(vec![other.clone()], Some(other))).is_empty());
    assert_eq!(c.view().message.as_deref(), Some("not a folder"));
    c.event(Ev::Later {
        serial,
        git: vec!["odds".into()],
        links: vec![],
    });
    assert!(c
        .view()
        .rows
        .iter()
        .any(|r| r.name == "odds/" && r.right.contains("git")));
    assert!(
        line(&mut c, "~/o").is_empty(),
        "the whole folder is still held: a shorter filter reads nothing"
    );
    assert_eq!(
        c.view().rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        ["odd/", "odds/", "other/"]
    );
}
/// A folder whose answer is `reply` (`None`: still being read, or stuck once `stuck` is set), in the language asked
/// for: typed (`in_line`), or walked into with `l` from a folder that read fine, so the browse state shows it.
fn folder_in(lang: Lang, reply: Option<Listing>, stuck: bool, in_line: bool) -> Chooser {
    let mut c = Chooser::new(Mode::Window { root: "/here".into() }, Some("/home/me".into()), lang, 7);
    let effects = if in_line {
        line(&mut c, "/probe/")
    } else {
        browse(&mut c, "~/", &["src"]);
        esc(&mut c);
        c.event(Ev::Line(LineEv::Step(1)));
        key(&mut c, 'l')
    };
    match reply {
        Some(reply) => {
            deliver(&mut c, &effects, reply);
        }
        None if stuck => {
            let (serial, _) = read_request(&effects);
            c.event(Ev::Stuck { serial });
        }
        None => {}
    }
    assert!(c.view().browsing && c.view().in_line == in_line);
    c
}
fn offers_enter(footer: &str) -> bool {
    footer.contains("Enter")
}
#[test]
fn the_footer_offers_enter_exactly_when_enter_acts() {
    let states: [(&str, Option<Listing>, bool); 9] = [
        ("read", Some(listing(&["src"])), false),
        ("empty", Some(listing(&[])), false),
        ("denied", Some(Listing::Denied { exact: None }), false),
        ("missing", Some(Listing::Missing), false),
        ("not a folder", Some(Listing::NotAFolder), false),
        ("loop", Some(Listing::Loop), false),
        ("stuck listing", Some(Listing::Stuck), false),
        ("too many stuck", Some(Listing::TooManyStuck), false),
        ("stuck while reading", None, true),
    ];
    for lang in [Lang::En, Lang::ZhCn] {
        for (name, reply, stuck) in &states {
            for in_line in [true, false] {
                let mut c = folder_in(lang, reply.clone(), *stuck, in_line);
                let footer = c.view().footer;
                let acts = !enter(&mut c).is_empty();
                assert_eq!(
                    offers_enter(&footer),
                    acts,
                    "{name} {lang:?} in_line={in_line}: {footer}"
                );
                if !acts {
                    // `l` goes where `Enter` goes, so it is not offered either.
                    assert!(
                        !footer.contains("l in") && !footer.contains("l 进入"),
                        "{name}: {footer}"
                    );
                }
            }
        }
    }
}
#[test]
fn a_missing_folder_footer_names_no_enter_in_both_languages_and_states() {
    for (lang, up, back) in [
        (Lang::En, "h up ·", "Tab complete"),
        (Lang::ZhCn, "h 上一级 ·", "Tab 补全"),
    ] {
        let browse = folder_in(lang, Some(Listing::Missing), false, false).view().footer;
        assert!(browse.starts_with(up), "{browse}");
        assert!(
            !offers_enter(&browse) && !browse.contains("l in") && !browse.contains("l 进入"),
            "{browse}"
        );
        let typing = folder_in(lang, Some(Listing::Missing), false, true).view().footer;
        assert!(typing.starts_with(back), "{typing}");
        assert!(!offers_enter(&typing), "{typing}");
    }
}
#[test]
fn a_read_in_progress_still_offers_the_enter_it_holds() {
    for lang in [Lang::En, Lang::ZhCn] {
        let mut c = folder_in(lang, None, false, true);
        assert!(offers_enter(&c.view().footer));
        assert!(enter(&mut c).is_empty());
    }
}
#[test]
fn a_filter_that_matches_nothing_offers_no_enter() {
    for lang in [Lang::En, Lang::ZhCn] {
        for in_line in [true, false] {
            let mut c = Chooser::new(Mode::Window { root: "/here".into() }, Some("/home/me".into()), lang, 7);
            let effects = line(&mut c, "/probe/zzz");
            deliver(&mut c, &effects, listing(&["src"]));
            if !in_line {
                esc(&mut c);
            }
            let footer = c.view().footer;
            let acts = !enter(&mut c).is_empty();
            assert_eq!(offers_enter(&footer), acts, "{lang:?} in_line={in_line}: {footer}");
        }
    }
}

#[test]
fn an_unknown_link_that_is_not_a_folder_opens_as_the_chosen_file() {
    for mode in modes() {
        let mut c = chooser(mode);
        let effects = line(&mut c, "~/link.md");
        deliver(
            &mut c,
            &effects,
            read_of(vec![link_entry("link.md", LinkKind::Unknown)], None),
        );
        let effects = enter(&mut c);
        let link = link_entry("link.md", LinkKind::Unknown);
        let effects = deliver(&mut c, &effects, read_of(vec![link.clone()], Some(link)));
        assert_eq!(
            deliver(&mut c, &effects, Listing::NotAFolder),
            vec![Effect::Open("/home/me/link.md".into())]
        );
    }
}

#[test]
fn an_unknown_link_read_failure_does_not_open_a_file() {
    for reply in [Listing::Missing, Listing::Stuck, Listing::Loop] {
        let mut c = window();
        let effects = line(&mut c, "~/link.md");
        deliver(
            &mut c,
            &effects,
            read_of(vec![link_entry("link.md", LinkKind::Unknown)], None),
        );
        let effects = enter(&mut c);
        let link = link_entry("link.md", LinkKind::Unknown);
        let effects = deliver(&mut c, &effects, read_of(vec![link.clone()], Some(link)));
        assert!(!deliver(&mut c, &effects, reply)
            .iter()
            .any(|e| matches!(e, Effect::Open(_))));
    }
}

#[test]
fn moving_selection_cancels_a_pending_unknown_link_file_choice() {
    let mut c = window();
    let effects = line(&mut c, "~/link.md");
    deliver(
        &mut c,
        &effects,
        read_of(vec![link_entry("link.md", LinkKind::Unknown)], None),
    );
    let effects = enter(&mut c);
    let link = link_entry("link.md", LinkKind::Unknown);
    let effects = deliver(&mut c, &effects, read_of(vec![link.clone()], Some(link)));
    c.event(Ev::Line(LineEv::Step(1)));
    assert!(!deliver(&mut c, &effects, Listing::NotAFolder)
        .iter()
        .any(|e| matches!(e, Effect::Open(_))));
}
