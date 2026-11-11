use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::*;
use crate::test_scratch_dir::ScratchDir;
use crate::window_link::pair::Role;

const TOKEN: &str = "0123456789abcdef";

fn root() -> PathBuf {
    PathBuf::from("/home/someone/project")
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn leaf(file: &[u8], cur: bool) -> Node {
    Node::Leaf(Leaf {
        file: file.to_vec(),
        lnum: 1,
        col: 0,
        want: 0,
        top: 1,
        left: 0,
        w: 80,
        h: 24,
        cur,
    })
}

fn tab(tree: Node) -> Tab {
    Tab {
        cols: 200,
        lines: 48,
        tree,
    }
}

fn record(tabs: Vec<Tab>, bufs: Vec<Vec<u8>>) -> Record {
    Record {
        root: root().as_os_str().as_bytes().to_vec(),
        tab: None,
        tabs,
        bufs,
    }
}

/// An absolute name of exactly `len` bytes, different for each `n`.
fn name(n: usize, len: usize) -> Vec<u8> {
    let mut name = format!("/{n}/").into_bytes();
    name.resize(len, b'a');
    name
}

fn one_leaf_with(change: impl FnOnce(&mut Leaf)) -> Record {
    let mut l = Leaf {
        file: b"/p/a.rs".to_vec(),
        lnum: 1,
        col: 0,
        want: 0,
        top: 1,
        left: 0,
        w: 80,
        h: 24,
        cur: true,
    };
    change(&mut l);
    record(vec![tab(Node::Leaf(l))], vec![])
}

fn reparse(r: &Record) -> Result<Record, RecordError> {
    parse(&r.to_json(), &root())
}

/// A tree `depth` levels deep: rows and columns alternating around one window.
fn deep(depth: usize) -> Node {
    if depth == 1 {
        leaf(b"/p/deep.rs", true)
    } else if depth.is_multiple_of(2) {
        Node::Row(vec![deep(depth - 1)])
    } else {
        Node::Col(vec![deep(depth - 1)])
    }
}

fn state_home(dir: &ScratchDir) -> &OsStr {
    dir.as_os_str()
}

fn main_file(dir: &ScratchDir) -> RecordFile {
    RecordFile::for_window(Some(state_home(dir)), None, &root(), Role::Main).unwrap()
}

fn mode(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn a_record_round_trips() {
    let max = 1u32 << 31;
    let mut odd = Leaf {
        file: b"/p/\xff\xfe odd \"name\".rs".to_vec(),
        lnum: max,
        col: max,
        want: 2147483647,
        top: max,
        left: max,
        w: 10000,
        h: 1,
        cur: true,
    };
    odd.col = 3;
    let r = Record {
        root: root().as_os_str().as_bytes().to_vec(),
        tab: Some(2),
        tabs: vec![
            tab(Node::Row(vec![
                leaf(b"/p/a.rs", true),
                Node::Col(vec![leaf(b"/p/b.rs", false), leaf(b"/p/c.rs", false)]),
            ])),
            Tab {
                cols: 80,
                lines: 24,
                tree: Node::Leaf(odd),
            },
            tab(Node::Col(vec![
                Node::Row(vec![leaf(b"/p/d.rs", false), leaf(b"/p/a.rs", true)]),
                leaf(b"/p/e.rs", false),
            ])),
        ],
        bufs: vec![b"/p/f.rs".to_vec(), b"/p/\xc3(".to_vec()],
    };
    assert_eq!(reparse(&r), Ok(r.clone()));
    assert!(r.to_json().contains("2147483647"));
    assert_eq!(r.file_count(), 8, "a.rs is in two windows and counts once");
}

#[test]
fn each_bound_is_held_at_its_edge_and_refused_one_past() {
    let tabs = |n: usize| record((0..n).map(|i| tab(leaf(&name(i, 10), true))).collect(), vec![]);
    assert!(reparse(&tabs(16)).is_ok());
    assert_eq!(reparse(&tabs(17)), Err(RecordError::Bound("tabs")));

    let leaves = |n: usize| {
        let mut row: Vec<Node> = (0..n).map(|i| leaf(&name(i, 10), false)).collect();
        row.push(Node::Col(vec![]));
        record(vec![tab(Node::Row(row))], vec![])
    };
    assert!(reparse(&leaves(32)).is_ok());
    assert_eq!(reparse(&leaves(33)), Err(RecordError::Bound("leaves in a tab")));

    let depth = |d: usize| record(vec![tab(deep(d))], vec![]);
    assert!(reparse(&depth(16)).is_ok());
    assert_eq!(reparse(&depth(17)), Err(RecordError::Bound("depth")));

    let bufs = |n: usize| record(vec![], (0..n).map(|i| name(i, 10)).collect());
    assert!(reparse(&bufs(256)).is_ok());
    assert_eq!(reparse(&bufs(257)), Err(RecordError::Bound("bufs")));

    let long = |len: usize| one_leaf_with(|l| l.file = name(0, len));
    assert!(reparse(&long(4096)).is_ok());
    assert_eq!(reparse(&long(4097)), Err(RecordError::Bound("name length")));
    let long_buf = |len: usize| record(vec![], vec![name(0, len)]);
    assert!(reparse(&long_buf(4096)).is_ok());
    assert_eq!(reparse(&long_buf(4097)), Err(RecordError::Bound("name length")));

    assert!(reparse(&one_leaf_with(|l| l.w = 10000)).is_ok());
    assert_eq!(reparse(&one_leaf_with(|l| l.w = 10001)), Err(RecordError::Bound("w")));
    assert_eq!(reparse(&one_leaf_with(|l| l.w = 0)), Err(RecordError::Bound("w")));
    assert!(reparse(&one_leaf_with(|l| l.h = 10000)).is_ok());
    assert_eq!(reparse(&one_leaf_with(|l| l.h = 10001)), Err(RecordError::Bound("h")));

    assert_eq!(reparse(&one_leaf_with(|l| l.lnum = 0)), Err(RecordError::Bound("lnum")));
    assert!(reparse(&one_leaf_with(|l| l.lnum = 1 << 31)).is_ok());
    assert_eq!(
        reparse(&one_leaf_with(|l| l.lnum = (1 << 31) + 1)),
        Err(RecordError::Bound("lnum"))
    );
    assert_eq!(reparse(&one_leaf_with(|l| l.top = 0)), Err(RecordError::Bound("top")));
    assert!(reparse(&one_leaf_with(|l| l.col = 1 << 31)).is_ok());
    assert_eq!(
        reparse(&one_leaf_with(|l| l.col = (1 << 31) + 1)),
        Err(RecordError::Bound("col"))
    );
    assert_eq!(
        reparse(&one_leaf_with(|l| l.left = (1 << 31) + 1)),
        Err(RecordError::Bound("left"))
    );
    assert_eq!(
        reparse(&one_leaf_with(|l| l.want = (1 << 31) + 1)),
        Err(RecordError::Bound("want"))
    );

    // Every number is judged as the JSON gave it, before it is narrowed: 2^32 + 1 is not line 1.
    let json = one_leaf_with(|_| {}).to_json();
    assert!(json.contains("\"lnum\":1,"), "{json}");
    for bad in ["4294967297", "-1", "1.0", "\"1\"", "null"] {
        let text = json.replace("\"lnum\":1,", &format!("\"lnum\":{bad},"));
        assert!(parse(&text, &root()).is_err(), "{bad} was read");
    }

    let two_cur = record(
        vec![tab(Node::Row(vec![leaf(b"/p/a.rs", true), leaf(b"/p/b.rs", true)]))],
        vec![],
    );
    assert_eq!(reparse(&two_cur), Err(RecordError::Bound("cur")));
    let one_each = record(vec![tab(leaf(b"/p/a.rs", true)), tab(leaf(b"/p/b.rs", true))], vec![]);
    assert!(reparse(&one_each).is_ok());

    let mut at = one_each.clone();
    at.tab = Some(2);
    assert!(reparse(&at).is_ok());
    at.tab = Some(3);
    assert_eq!(reparse(&at), Err(RecordError::Bound("tab")));
    at.tab = Some(0);
    assert_eq!(reparse(&at), Err(RecordError::Bound("tab")));
    let mut no_tabs = record(vec![], vec![]);
    no_tabs.tab = Some(1);
    assert_eq!(reparse(&no_tabs), Err(RecordError::Bound("tab")));

    let mut size = one_each.clone();
    size.tabs[0].cols = 10001;
    assert_eq!(reparse(&size), Err(RecordError::Bound("cols")));
    size.tabs[0].cols = 0;
    assert_eq!(reparse(&size), Err(RecordError::Bound("cols")));
}

#[test]
fn a_record_at_every_count_bound_with_4096_byte_names_fits_after_fit_budget_and_keeps_a_tab() {
    let mut n = 0;
    let mut next = || {
        n += 1;
        name(n, 4096)
    };
    let tabs: Vec<Tab> = (0..16)
        .map(|_| tab(Node::Row((0..32).map(|_| leaf(&next(), false)).collect())))
        .collect();
    let bufs: Vec<Vec<u8>> = (0..256).map(|_| next()).collect();
    let mut r = record(tabs, bufs);
    r.tab = Some(1);
    assert!(r.to_json().len() > RECORD_MAX);

    let dropped = r.fit_budget(RECORD_MAX);
    let json = r.to_json();
    assert!(json.len() <= RECORD_MAX, "{} bytes", json.len());
    assert!(!r.tabs.is_empty(), "a tab is kept");
    assert!(r.bufs.is_empty(), "listed buffers go before any tab");
    assert_eq!(r.tab, Some(1));
    assert!(!dropped.is_empty());
    assert_eq!(parse(&json, &root()), Ok(r.clone()));

    let before = r.clone();
    assert!(
        r.fit_budget(RECORD_MAX).is_empty(),
        "a record within the budget is left as it is"
    );
    assert_eq!(r, before);
}

#[test]
fn a_relative_name_or_a_nul_is_refused() {
    assert_eq!(
        reparse(&one_leaf_with(|l| l.file = b"p/a.rs".to_vec())),
        Err(RecordError::Bound("relative name"))
    );
    assert_eq!(
        reparse(&one_leaf_with(|l| l.file = b"/p/a\0.rs".to_vec())),
        Err(RecordError::Bound("NUL in name"))
    );
    assert_eq!(
        reparse(&one_leaf_with(|l| l.file = Vec::new())),
        Err(RecordError::Bound("name length"))
    );
    assert_eq!(
        reparse(&record(vec![], vec![b"rel".to_vec()])),
        Err(RecordError::Bound("relative name"))
    );
    assert_eq!(
        reparse(&record(vec![], vec![b"/a\0".to_vec()])),
        Err(RecordError::Bound("NUL in name"))
    );

    // Names are standard base64 with padding; anything else is not a name.
    let json = record(vec![], vec![b"/ab".to_vec()]).to_json();
    let good = b64(b"/ab");
    assert!(json.contains(&good));
    for bad in ["L2Fi", "L2F", "L2F=", "L2F*Yg==", "L2_i"] {
        let text = json.replace(&format!("\"{good}\""), &format!("\"{bad}\""));
        if bad == "L2Fi" {
            // "/ab" itself in another spelling: the canonical one decodes.
            assert!(parse(&text, &root()).is_ok());
            continue;
        }
        assert!(matches!(parse(&text, &root()), Err(RecordError::Syntax(_))), "{bad}");
    }
}

#[test]
fn empty_lists_read_as_brackets_or_braces() {
    let r = b64(root().as_os_str().as_bytes());
    for (tabs, bufs) in [("[]", "[]"), ("{}", "{}"), ("[]", "{}"), ("{}", "[]")] {
        let text = format!(r#"{{"v":1,"root":"{r}","tabs":{tabs},"bufs":{bufs}}}"#);
        assert_eq!(parse(&text, &root()), Ok(Record::empty(&root())), "{text}");
    }
    for children in ["[]", "{}"] {
        let text = format!(
            r#"{{"v":1,"root":"{r}","tabs":[{{"cols":80,"lines":24,"tree":{{"row":{children}}}}}],"bufs":[]}}"#
        );
        let got = parse(&text, &root()).unwrap();
        assert_eq!(got.tabs[0].tree, Node::Row(vec![]));
    }
    let text = format!(r#"{{"v":1,"root":"{r}","tabs":{{"a":1}},"bufs":[]}}"#);
    assert!(
        matches!(parse(&text, &root()), Err(RecordError::Syntax(_))),
        "only an empty object stands for a list"
    );
}

#[test]
fn tab_may_be_absent() {
    let r = record(vec![tab(leaf(b"/p/a.rs", true))], vec![]);
    let json = r.to_json();
    assert!(!json.contains("\"tab\""), "{json}");
    assert_eq!(parse(&json, &root()), Ok(r.clone()));
    let with_null = json.replacen('{', r#"{"tab":null,"#, 1);
    assert_eq!(parse(&with_null, &root()), Ok(r));
}

#[test]
fn unknown_keys_are_refused() {
    let json = one_leaf_with(|_| {}).to_json();
    for (from, to) in [
        ("{\"v\":1", "{\"extra\":1,\"v\":1"),
        ("\"cols\":", "\"x\":1,\"cols\":"),
        ("\"cur\":", "\"x\":1,\"cur\":"),
        ("{\"leaf\":", "{\"row\":[],\"leaf\":"),
    ] {
        assert!(json.contains(from), "{from} in {json}");
        let text = json.replacen(from, to, 1);
        assert!(matches!(parse(&text, &root()), Err(RecordError::Syntax(_))), "{text}");
    }
}

#[test]
fn another_version_is_nothing_not_set_aside() {
    let json = record(vec![tab(leaf(b"/p/a.rs", true))], vec![]).to_json();
    let v2 = json.replace("\"v\":1", "\"v\":2");
    assert_ne!(json, v2);
    assert_eq!(parse(&v2, &root()), Err(RecordError::Version(2)));
    // A later version may hold keys this one does not know; it is still only another version.
    let v2_more = v2.replacen('{', r#"{"new":[1,2],"#, 1);
    assert_eq!(parse(&v2_more, &root()), Err(RecordError::Version(2)));

    let dir = ScratchDir::new("nv-editor-record", "version");
    let file = main_file(&dir);
    crate::state_paths::create_state_dir(file.path().parent().unwrap()).unwrap();
    agent::private_fs::write_private(file.path(), v2.as_bytes()).unwrap();
    assert_eq!(file.read_start(&root()), StartRead::Nothing);
    assert_eq!(
        std::fs::read_to_string(file.path()).unwrap(),
        v2,
        "left in place for the next write"
    );
    assert!(!file.path().with_extension("json.unusable").exists());
}

#[test]
fn another_root_is_nothing_not_set_aside() {
    let json = record(vec![tab(leaf(b"/p/a.rs", true))], vec![]).to_json();
    assert_eq!(
        parse(&json, Path::new("/home/someone/other")),
        Err(RecordError::OtherRoot)
    );

    let dir = ScratchDir::new("nv-editor-record", "root");
    let other = Path::new("/home/someone/other");
    let file = RecordFile::for_window(Some(state_home(&dir)), None, other, Role::Main).unwrap();
    crate::state_paths::create_state_dir(file.path().parent().unwrap()).unwrap();
    agent::private_fs::write_private(file.path(), json.as_bytes()).unwrap();
    assert_eq!(file.read_start(other), StartRead::Nothing);
    assert_eq!(std::fs::read_to_string(file.path()).unwrap(), json);
    assert!(!file.path().with_extension("json.unusable").exists());
}

#[test]
fn nesting_past_forty_is_refused_before_serde() {
    let nested = |levels: usize| format!(r#"{{"v":1,"x":{}{}}}"#, "[".repeat(levels - 1), "]".repeat(levels - 1));
    assert_eq!(parse(&nested(41), &root()), Err(RecordError::TooDeep));
    assert_ne!(parse(&nested(40), &root()), Err(RecordError::TooDeep));
    // Past serde_json's own limit the answer is still the record's own, not a syntax error.
    assert_eq!(parse(&nested(500), &root()), Err(RecordError::TooDeep));
    // Brackets inside strings, escaped quotes included, are text.
    let text = format!(r#"{{"v":1,"x":"\"{}"}}"#, "[{".repeat(100));
    assert_ne!(parse(&text, &root()), Err(RecordError::TooDeep));
    let brackets = format!(",\"y\":{}{}", "[".repeat(41), "]".repeat(41));
    let text = format!(r#"{{"v":1,"x":"\\"{brackets}}}"#);
    assert_eq!(
        parse(&text, &root()),
        Err(RecordError::TooDeep),
        "an escaped backslash ends no string"
    );
    // The handoff holds the same bound.
    let handoff = format!(r#"{{"same":true,"record":{}}}"#, nested(41));
    assert_eq!(parse_handoff(&handoff, &root()), Err(RecordError::TooDeep));
}

#[test]
fn a_record_past_256_kib_is_too_large() {
    let json = record(vec![tab(leaf(b"/p/a.rs", true))], vec![]).to_json();
    let padded = |len: usize| format!("{json}{}", " ".repeat(len - json.len()));
    assert!(parse(&padded(RECORD_MAX), &root()).is_ok());
    assert_eq!(parse(&padded(RECORD_MAX + 1), &root()), Err(RecordError::TooLarge));
    let handoff = format!(r#"{{"same":false,"record":{}}}"#, padded(RECORD_MAX));
    assert_eq!(parse_handoff(&handoff, &root()), Err(RecordError::TooLarge));

    // Nothing writes what its reader refuses.
    let dir = ScratchDir::new("nv-editor-record", "large");
    let file = main_file(&dir);
    let mut big = record(
        vec![tab(leaf(b"/p/a.rs", true))],
        (0..256).map(|i| name(i, 4096)).collect(),
    );
    assert!(big.to_json().len() > RECORD_MAX);
    assert!(file.write(&big).is_err());
    assert!(!file.path().exists());
    big.fit_budget(RECORD_MAX);
    file.write(&big).unwrap();
    assert_eq!(file.read_start(&root()), StartRead::Record(big));
}

#[test]
fn fit_budget_drops_bufs_from_the_end_then_whole_tabs() {
    let tabs: Vec<Tab> = (0..3)
        .map(|t| {
            tab(Node::Row(
                (0..4).map(|i| leaf(&name(t * 10 + i, 300), i == 0)).collect(),
            ))
        })
        .collect();
    let bufs: Vec<Vec<u8>> = (0..10).map(|i| name(100 + i, 300)).collect();
    let mut full = record(tabs, bufs.clone());
    full.tab = Some(3);

    let mut r = full.clone();
    let max = r.to_json().len() - 1;
    let dropped = r.fit_budget(max);
    assert_eq!(r.bufs, bufs[..9].to_vec(), "the last listed buffer goes first");
    assert_eq!(r.tabs, full.tabs);
    assert_eq!(dropped.len(), 1, "{dropped:?}");
    assert!(r.to_json().len() <= max);
    assert_eq!(parse(&r.to_json(), &root()), Ok(r.clone()));

    let mut expected = full.clone();
    expected.bufs.clear();
    expected.tabs.truncate(2);
    expected.tab = None;
    let max = expected.to_json().len();
    let mut r = full.clone();
    let dropped = r.fit_budget(max);
    assert_eq!(
        r, expected,
        "every listed buffer, then the last tab, which was the current one"
    );
    assert_eq!(dropped.len(), 11, "{dropped:?}");
    assert!(r.to_json().len() <= max);
    assert_eq!(parse(&r.to_json(), &root()), Ok(r.clone()));

    // A current tab that stays keeps its number.
    let mut kept = expected.clone();
    kept.tab = Some(2);
    let mut r = full.clone();
    r.tab = Some(2);
    r.fit_budget(kept.to_json().len());
    assert_eq!(r, kept);

    // However small the budget, the first tab stays.
    let mut r = full.clone();
    r.fit_budget(10);
    assert_eq!(r.tabs, full.tabs[..1].to_vec());
    assert!(r.bufs.is_empty());
}

#[test]
fn the_handoff_parses_same_and_record_and_refuses_other_keys() {
    let r = record(vec![tab(leaf(b"/p/a.rs", true))], vec![b"/p/b.rs".to_vec()]);
    let json = r.to_json();
    assert_eq!(
        parse_handoff(&format!(r#"{{"same":true,"record":{json}}}"#), &root()),
        Ok((r.clone(), true))
    );
    assert_eq!(
        parse_handoff(&format!(r#"{{"record":{json},"same":false}}"#), &root()),
        Ok((r.clone(), false))
    );
    for bad in [
        format!(r#"{{"record":{json}}}"#),
        r#"{"same":true}"#.to_string(),
        format!(r#"{{"same":true,"record":{json},"token":"{TOKEN}"}}"#),
        format!(r#"{{"same":1,"record":{json}}}"#),
        r#"{"same":true,"record":null}"#.to_string(),
        format!(r#"{{"same":true,"record":"{json}"}}"#),
        format!("[true,{json}]"),
        json.clone(),
    ] {
        assert!(
            matches!(parse_handoff(&bad, &root()), Err(RecordError::Syntax(_))),
            "{bad}"
        );
    }
    assert_eq!(
        parse_handoff(&format!(r#"{{"same":true,"record":{json}}}"#), Path::new("/elsewhere")),
        Err(RecordError::OtherRoot)
    );
    let mut wide = r.clone();
    wide.tabs
        .push(tab(Node::Row(vec![leaf(b"/p/a.rs", true), leaf(b"/p/b.rs", true)])));
    assert_eq!(
        parse_handoff(&format!(r#"{{"same":true,"record":{}}}"#, wide.to_json()), &root()),
        Err(RecordError::Bound("cur"))
    );
}

#[test]
fn the_input_file_has_its_five_keys_and_is_0600() {
    let dir = ScratchDir::new("nv-editor-record", "input");
    let r = record(vec![tab(leaf(b"/p/a.rs", true))], vec![]);
    let prefixes = [
        PathBuf::from("/home/someone/.cache/x"),
        PathBuf::from(OsStr::from_bytes(b"/tmp/\xff")),
    ];
    write_input(&dir, TOKEN, &root(), &prefixes, Some(&r), false).unwrap();
    let path = dir.join(INPUT_FILE_NAME);
    assert_eq!(mode(&path), 0o600);
    let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let object = value.as_object().unwrap();
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["prefixes", "record", "root", "skip", "token"]);
    assert_eq!(object["token"], TOKEN);
    assert_eq!(object["root"], b64(root().as_os_str().as_bytes()));
    assert_eq!(
        object["prefixes"],
        serde_json::json!([b64(b"/home/someone/.cache/x"), b64(b"/tmp/\xff")])
    );
    assert_eq!(parse(&object["record"].to_string(), &root()), Ok(r.clone()));
    assert_eq!(object["skip"], false);

    write_input(&dir, TOKEN, &root(), &[], None, true).unwrap();
    let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(value["record"], serde_json::Value::Null);
    assert_eq!(value["skip"], true);
    assert_eq!(value["prefixes"], serde_json::json!([]));

    // The token goes into file names nvim makes; only 16 lowercase hex digits are one.
    for bad in [
        "0123456789ABCDEF",
        "0123456789abcde",
        "0123456789abcdef0",
        "../../../../x/ab",
    ] {
        assert!(write_input(&dir, bad, &root(), &[], None, false).is_err(), "{bad}");
    }
    let names: Vec<_> = std::fs::read_dir(&*dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        names,
        [std::ffi::OsString::from(INPUT_FILE_NAME)],
        "no temporary file is left"
    );
}

const UMASK_CHILD: &str = "EITRI_TEST_EDITOR_RECORD_UMASK";

/// The umask is process-wide and the other tests make files at the same time, so the body runs in a child copy of
/// the test binary that runs this one test alone, once for each umask.
#[test]
fn the_file_is_0600_in_0700_under_umask_0277_and_002() {
    const NAME: &str = "editor_record::tests::the_file_is_0600_in_0700_under_umask_0277_and_002";
    if let Ok(umask) = std::env::var(UMASK_CHILD) {
        let umask = u32::from_str_radix(&umask, 8).unwrap();
        let dir = ScratchDir::new("nv-editor-record", "umask");
        // SAFETY: this child runs this one test and nothing else.
        unsafe { libc::umask(umask as libc::mode_t) };
        let file = main_file(&dir);
        let r = record(vec![tab(leaf(b"/p/a.rs", true))], vec![]);
        file.write(&r).unwrap();
        file.set_marker(TOKEN).unwrap();
        assert_eq!(mode(&dir.join("eitri")), 0o700);
        assert_eq!(mode(&dir.join("eitri").join("editor")), 0o700);
        assert_eq!(mode(file.path()), 0o600);
        assert_eq!(mode(&file.marker_path()), 0o600);
        assert_eq!(file.read_start(&root()), StartRead::Record(r));
        assert_eq!(file.marker().as_deref(), Some(TOKEN));
        return;
    }
    for umask in ["277", "002"] {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([NAME, "--exact", "--nocapture", "--test-threads=1"])
            .env(UMASK_CHILD, umask)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "under umask {umask}:\n{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn a_bad_file_is_set_aside_whole_as_unusable() {
    let dir = ScratchDir::new("nv-editor-record", "aside");
    let file = main_file(&dir);
    assert_eq!(file.read_start(&root()), StartRead::Nothing, "no file");
    let aside = file.path().with_extension("json.unusable");

    let too_many = record((0..17).map(|i| tab(leaf(&name(i, 10), true))).collect(), vec![]).to_json();
    let mut over = vec![b' '; RECORD_MAX + 1];
    over[0] = b'{';
    for bytes in [
        b"{\"v\":1, nope".to_vec(),
        too_many.into_bytes(),
        over,
        b"{\"v\":1,\"root\":\"\xff\"}".to_vec(),
    ] {
        crate::state_paths::create_state_dir(file.path().parent().unwrap()).unwrap();
        agent::private_fs::write_private(file.path(), &bytes).unwrap();
        match file.read_start(&root()) {
            StartRead::SetAside(why) => assert!(!why.is_empty()),
            other => panic!("{other:?}"),
        }
        assert!(!file.path().exists());
        assert_eq!(std::fs::read(&aside).unwrap(), bytes, "moved whole");
        assert_eq!(file.read_start(&root()), StartRead::Nothing);
    }
}

#[test]
fn unpaired_uses_the_u_name_and_second_has_none() {
    let dir = ScratchDir::new("nv-editor-record", "roles");
    let key = crate::state_paths::project_key(&root());
    let editor = dir.join("eitri").join("editor");
    let main = main_file(&dir);
    assert_eq!(main.path(), editor.join(format!("{key}.json")));
    assert_eq!(main.marker_path(), editor.join(format!("{key}.json.restoring")));
    let unpaired = RecordFile::for_window(Some(state_home(&dir)), None, &root(), Role::Unpaired).unwrap();
    assert_eq!(unpaired.path(), editor.join(format!("{key}-u.json")));
    assert_eq!(unpaired.marker_path(), editor.join(format!("{key}-u.json.restoring")));
    assert_eq!(
        RecordFile::for_window(Some(state_home(&dir)), None, &root(), Role::Second),
        None
    );

    let home = RecordFile::for_window(None, Some(OsStr::new("/home/someone")), &root(), Role::Main).unwrap();
    assert_eq!(
        home.path(),
        Path::new("/home/someone/.local/state/eitri/editor").join(format!("{key}.json"))
    );
    assert_eq!(
        RecordFile::for_window(Some(OsStr::new("rel")), Some(OsStr::new("rel")), &root(), Role::Main),
        None
    );

    // Each window reads only its own file.
    let r = record(vec![tab(leaf(b"/p/a.rs", true))], vec![]);
    main.write(&r).unwrap();
    assert_eq!(unpaired.read_start(&root()), StartRead::Nothing);
    assert_eq!(main.read_start(&root()), StartRead::Record(r));
}

#[test]
fn the_marker_round_trips_and_clears() {
    let dir = ScratchDir::new("nv-editor-record", "marker");
    let file = main_file(&dir);
    assert_eq!(file.marker(), None);
    file.clear_marker().unwrap();
    file.set_marker(TOKEN).unwrap();
    assert_eq!(file.marker().as_deref(), Some(TOKEN));
    assert_eq!(mode(&file.marker_path()), 0o600);
    file.set_marker("fedcba9876543210").unwrap();
    assert_eq!(file.marker().as_deref(), Some("fedcba9876543210"));
    file.clear_marker().unwrap();
    assert_eq!(file.marker(), None);
    assert!(!file.marker_path().exists());

    // A marker that cannot be read still says the last restore did not finish.
    std::os::unix::fs::symlink("/nowhere", file.marker_path()).unwrap();
    assert!(file.marker().is_some(), "a link in the marker's place");
    file.clear_marker().unwrap();
    assert_eq!(file.marker(), None);
    let c_path = std::ffi::CString::new(file.marker_path().as_os_str().as_bytes()).unwrap();
    // SAFETY: the NUL-terminated path remains valid for the call.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    assert!(file.marker().is_some(), "a FIFO in the marker's place");
    file.clear_marker().unwrap();
    assert_eq!(file.marker(), None);
}
