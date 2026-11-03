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
fn answer(c: &mut Chooser, text: &str, answer: PathAnswer) -> Vec<Effect> {
    let effects = line(c, text);
    let Effect::Path { serial, .. } = effects[0] else {
        panic!("expected a path request")
    };
    c.event(Ev::PathAnswer { serial, answer })
}
fn found(exact: Option<PathKind>, folders: &[&str], common: &str) -> PathAnswer {
    PathAnswer::Found {
        path: "/absolute/target".into(),
        exact,
        folders: folders.iter().map(PathBuf::from).collect(),
        common: common.into(),
    }
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
    for path_reply in [false, true] {
        let mut c = window();
        let Effect::List { serial } = c.shown()[0] else {
            unreachable!()
        };
        let before = c.view();
        let ev = if path_reply {
            Ev::PathAnswer {
                serial: serial + 1,
                answer: PathAnswer::Missing,
            }
        } else {
            Ev::Rows {
                serial: serial + 1,
                rows: vec![row("/a", Fact::Open)],
            }
        };
        assert!(c.event(ev).is_empty());
        assert_eq!(c.view(), before);
    }
    let mut c = window();
    let Effect::Path { serial, .. } = line(&mut c, "/old")[0] else {
        unreachable!()
    };
    line(&mut c, "/new");
    let before = c.view();
    assert!(c
        .event(Ev::PathAnswer {
            serial,
            answer: found(Some(PathKind::Dir), &[], "")
        })
        .is_empty());
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
fn path_text_asks_for_a_path_answer_and_filter_text_does_not() {
    for (mode, base) in [
        (Mode::Launch, "/home/me"),
        (Mode::Window { root: "/here".into() }, "/here"),
    ] {
        let mut c = chooser(mode);
        assert!(line(&mut c, "app").is_empty());
        for text in ["~/app", "/app", "./app", "../app"] {
            assert!(
                matches!(line(&mut c, text).as_slice(), [Effect::Path { text: t, base: b, .. }] if t == text && b == Path::new(base))
            );
        }
    }
}
#[test]
fn enter_on_a_found_dir_or_file_opens_the_answers_absolute_path() {
    for kind in [PathKind::Dir, PathKind::File] {
        let mut c = window();
        answer(&mut c, "~/target", found(Some(kind), &[], ""));
        assert_eq!(enter(&mut c), vec![Effect::Open("/absolute/target".into())]);
    }
}
#[test]
fn enter_on_a_listed_folder_opens_its_absolute_path() {
    for index in [0, 1] {
        let mut c = window();
        answer(&mut c, "~/t", found(None, &["/absolute/one", "/absolute/two"], "~/t"));
        c.event(Ev::Line(LineEv::Step(index)));
        assert_eq!(
            enter(&mut c),
            vec![Effect::Open(PathBuf::from(if index == 0 {
                "/absolute/one"
            } else {
                "/absolute/two"
            }))]
        );
    }
}
#[test]
fn enter_on_missing_says_no_such_folder_and_stays() {
    for (lang, message) in [
        (Lang::En, "no such folder: ~/lost"),
        (Lang::ZhCn, "没有这个文件夹：~/lost"),
    ] {
        let mut c = Chooser::new(Mode::Launch, None, lang, 1);
        answer(&mut c, "~/lost", PathAnswer::Missing);
        assert!(enter(&mut c).is_empty());
        assert_eq!(c.view().message.as_deref(), Some(message));
    }
}
#[test]
fn enter_held_for_a_path_answer_runs_on_it_and_stuck_says_cannot_read() {
    for (reply, expected) in [
        (
            found(Some(PathKind::Dir), &[], ""),
            vec![Effect::Open("/absolute/target".into())],
        ),
        (PathAnswer::Stuck, vec![]),
    ] {
        let mut c = window();
        let Effect::Path { serial, .. } = line(&mut c, "/a")[0] else {
            unreachable!()
        };
        assert!(enter(&mut c).is_empty());
        assert_eq!(c.event(Ev::PathAnswer { serial, answer: reply }), expected);
        if expected.is_empty() {
            assert_eq!(c.view().message.as_deref(), Some("cannot read that folder now"));
        }
    }
}
#[test]
fn tab_sets_the_line_to_the_common_prefix_and_adds_a_slash_on_one_match() {
    for (folders, common, expected) in [
        (vec!["/one"], "~/one", "~/one/"),
        (vec!["/one", "/only"], "~/on", "~/on"),
        (vec!["/one"], "~/one/", "~/one/"),
    ] {
        let mut c = window();
        answer(&mut c, "~/o", found(None, &folders, common));
        let effects = c.event(Ev::Line(LineEv::Tab));
        assert!(effects.contains(&Effect::SetLine(expected.into())));
        assert_eq!(c.view().line, expected);
    }
}
#[test]
fn tab_keeps_the_typed_form_of_the_line_and_extends_it_from_the_absolute_answer() {
    for (typed, folders, common, expected) in [
        ("~/b", vec!["/home/me/beta"], "/home/me/beta", "~/beta/"),
        ("~/b", vec!["/home/me/beta", "/home/me/bin"], "/home/me/b", "~/b"),
        ("~/.c", vec!["/home/me/.config"], "/home/me/.config", "~/.config/"),
        ("~/..", vec!["/home/me/../you"], "/home/me/../you", "~/../you/"),
        ("~", vec!["/home/me"], "/home/me", "~/"),
        // A sibling of home alone (home missing, or past the entry cap) cannot be written from `~`.
        ("~", vec!["/home/me-old"], "/home/me-old", "/home/me-old/"),
        (
            "./nested/ap",
            vec!["/w/nested/apple", "/w/nested/apricot"],
            "/w/nested/ap",
            "./nested/ap",
        ),
        (
            "./nested/ap",
            vec!["/w/nested/apple"],
            "/w/nested/apple",
            "./nested/apple/",
        ),
        ("../sib", vec!["/w/../sibling"], "/w/../sibling", "../sibling/"),
        ("./", vec!["/w/a", "/w/b"], "/w/", "./"),
        ("/u", vec!["/usr"], "/usr", "/usr/"),
        (
            "~/应",
            vec!["/home/me/应用甲", "/home/me/应用乙"],
            "/home/me/应用",
            "~/应用",
        ),
    ] {
        let mut c = window();
        answer(&mut c, typed, found(None, &folders, common));
        let effects = c.event(Ev::Line(LineEv::Tab));
        assert!(
            effects.contains(&Effect::SetLine(expected.into())),
            "{typed}: {effects:?}"
        );
        assert_eq!(c.view().line, expected, "{typed}");
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
        assert_eq!(c.view().rows[at].name, "Choose a folder…");
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
        assert_eq!(c.view().rows[0].name, "Choose a folder…");
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
            "Enter open · j k move · gg G ends · / filter · Ctrl+o folder · q Esc close",
            "Choose a folder…",
            "Home folder",
            "No recent projects",
            "open",
            "gone",
            "Opening app…",
            "cannot read that folder now",
        ),
        (
            Lang::ZhCn,
            "打开项目",
            "按 / 筛选或输入路径",
            "Enter 打开 · j k 移动 · gg G 首尾 · / 筛选 · Ctrl+o 文件夹 · q Esc 关闭",
            "选择文件夹…",
            "主目录",
            "没有最近的项目",
            "已打开",
            "不在了",
            "正在打开 app…",
            "现在读不了这个文件夹",
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
        answer(&mut c, "/bad", PathAnswer::Stuck);
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
fn a_path_answer_that_lands_during_a_start_is_there_when_the_start_fails() {
    let mut c = chooser(Mode::Launch);
    list(&mut c, vec![]);
    let effects = line(&mut c, "/wo");
    let Effect::Path { serial, .. } = effects[0] else {
        panic!("expected a path request")
    };
    assert_eq!(
        c.event(Ev::Line(LineEv::ChooseFolder)),
        vec![Effect::FolderDialog { at: None }]
    );
    assert_eq!(
        c.event(Ev::Folder(Some("/picked".into()))),
        vec![Effect::Open("/picked".into())]
    );
    c.opening("picked");

    let answer = found(Some(PathKind::Dir), &[], "");
    assert_eq!(
        c.event(Ev::PathAnswer { serial, answer }),
        vec![],
        "an answer starts nothing"
    );

    c.failed("failed".into());
    assert_eq!(enter(&mut c), vec![Effect::Open("/absolute/target".into())]);
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
        ("/home/me/<b> 中文\n", "<b> 中文?", "~/<b> 中文?"),
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
fn folder_clicks_work_while_a_path_answer_is_pending_or_missing() {
    for reply in [None, Some(PathAnswer::Missing), Some(PathAnswer::Stuck)] {
        let mut c = window();
        let Effect::Path { serial, .. } = line(&mut c, "/absent")[0] else {
            unreachable!()
        };
        if let Some(answer) = reply {
            c.event(Ev::PathAnswer { serial, answer });
        }
        let index = c.view().rows.len() - 1;
        assert_eq!(
            c.event(Ev::Pick {
                index,
                rows_rev: c.view().rows_rev
            }),
            vec![Effect::FolderDialog { at: None }]
        );
    }
}
#[test]
fn held_enter_never_opens_a_dialog_for_an_empty_path_answer() {
    let mut c = window();
    let Effect::Path { serial, .. } = line(&mut c, "/absent")[0] else {
        unreachable!()
    };
    enter(&mut c);
    assert!(c
        .event(Ev::PathAnswer {
            serial,
            answer: found(None, &[], "")
        })
        .is_empty());
}
#[test]
fn a_new_filter_invalidates_a_pending_path_and_held_enter() {
    for next in ["/other", "app"] {
        let mut c = window();
        let Effect::Path { serial, .. } = line(&mut c, "/first")[0] else {
            unreachable!()
        };
        enter(&mut c);
        line(&mut c, next);
        let before = c.view();
        assert!(c
            .event(Ev::PathAnswer {
                serial,
                answer: found(Some(PathKind::Dir), &[], "")
            })
            .is_empty());
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
        assert!(v.footer.contains("j k move") && v.footer.contains("q Esc close"));
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
    assert_eq!(c.view().rows[3].name, "Choose a folder…");
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
    assert_eq!(c.view().rows[c.view().selected.unwrap()].name, "Choose a folder…");
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
    assert_eq!(enter(&mut c), vec![Effect::FolderDialog { at: None }]);
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
fn esc_in_the_line_drops_a_pending_path_and_a_held_enter() {
    let mut c = window();
    let Effect::List { serial } = c.shown()[0] else {
        unreachable!()
    };
    let effects = line(&mut c, "~/x");
    let Effect::Path { serial: path, .. } = effects[0] else {
        panic!("a path request")
    };
    assert!(enter(&mut c).is_empty(), "held for the path answer");
    assert!(esc(&mut c).is_empty());
    assert!(c
        .event(Ev::PathAnswer {
            serial: path,
            answer: found(Some(PathKind::Dir), &[], "")
        })
        .is_empty());
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
fn a_tilde_in_the_list_goes_to_the_line_with_it_typed() {
    for (mode, base) in [
        (Mode::Window { root: "/here".into() }, "/here"),
        (Mode::Launch, "/home/me"),
    ] {
        let mut c = chooser(mode);
        list(&mut c, vec![row("/a", Fact::Present)]);
        let effects = key(&mut c, '~');
        let Effect::Path { serial, .. } = effects[1] else {
            panic!("a path request: {effects:?}")
        };
        assert_eq!(
            effects,
            vec![
                Effect::SetLine("~".into()),
                Effect::Path {
                    serial,
                    text: "~".into(),
                    base: base.into()
                }
            ]
        );
        let v = c.view();
        assert!(v.in_line);
        assert_eq!(v.line, "~");
        // The entry reporting the same text changes nothing.
        assert!(line(&mut c, "~").is_empty());
        line(&mut c, "~/");
        assert!(c.view().in_line);
    }
}

#[test]
fn other_plain_keys_in_the_list_do_nothing() {
    let mut c = three();
    key(&mut c, 'j');
    let before = c.view();
    for ch in "abcdefhilmnoprstuvwxyz0123456789 .,:;!?-_=+*#@$%^&()[]{}<>|\\'\"ABCDEFHIJKLMNOPQRSTUVWXYZ中".chars() {
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
    assert_eq!(c.view().rows[0].name, "Choose a folder…");
    key(&mut c, 'j');
    assert_eq!(c.view().rows[1].name, "Home folder");
    assert_eq!(c.view().selected, Some(1));
    key(&mut c, 'k');
    assert_eq!(enter(&mut c), vec![Effect::FolderDialog { at: None }]);
    assert!(c.event(Ev::Folder(None)).is_empty());
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
fn rows_rev_changes_for_path_answers_and_opening_state() {
    let mut c = window();
    list(&mut c, vec![row("/a", Fact::Present)]);
    let before = c.view();
    let effects = line(&mut c, "./app");
    let [Effect::Path { serial, .. }] = effects[..] else {
        panic!("a path asks for its answer");
    };
    let waiting = c.view();
    assert!(waiting.rows_rev > before.rows_rev);
    assert!(!std::sync::Arc::ptr_eq(&before.rows, &waiting.rows));
    c.event(Ev::PathAnswer {
        serial: serial + 1,
        answer: PathAnswer::Missing,
    });
    assert_eq!(c.view().rows_rev, waiting.rows_rev);
    c.event(Ev::PathAnswer {
        serial,
        answer: found(Some(PathKind::Dir), &[], ""),
    });
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
