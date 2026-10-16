//! Two processes appending to one plugin's log at once, with a small segment so both cross segment
//! boundaries many times: every item arrives exactly once, whole, in each writer's order. The children are
//! this test binary run again with one test selected.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use eitri_core::plugin::state::{Limits, PluginStore};
use serde_json::{json, Value};

const CHILD: &str = "EITRI_TEST_PLUGIN_LOG_CHILD";
const PER_CHILD: u64 = 400;
const SEGMENT: u64 = 64 * 1024;

fn store(root: &str) -> PluginStore {
    PluginStore::with_limits(
        root.into(),
        "demo",
        Limits {
            segment_bytes: SEGMENT,
            ..Limits::DEFAULT
        },
    )
}

#[test]
fn two_processes_appending_at_once_lose_nothing() {
    if let Ok(spec) = std::env::var(CHILD) {
        let (root, tag) = spec.split_once('|').expect("<root>|<tag>");
        let st = store(root);
        for i in 0..PER_CHILD {
            st.append("events", &[json!({"tag": tag, "i": i, "pad": "p".repeat(200)})])
                .unwrap();
        }
        return;
    }
    let root = std::env::temp_dir().join(format!("eitri-plugin-2proc-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&root).unwrap();
    // Each child's output goes to a file beside the store (not into it), and the wait is bounded: a
    // lock that never frees must fail the test, not hang it.
    let spawn = |tag: &str| {
        let out = std::fs::File::create(root.join(format!("{tag}.out"))).unwrap();
        Command::new(std::env::current_exe().unwrap())
            .args([
                "two_processes_appending_at_once_lose_nothing",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, format!("{}|{tag}", root.display()))
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .stdin(Stdio::null())
            .spawn()
            .unwrap()
    };
    let mut children = vec![("a", spawn("a")), ("b", spawn("b"))];
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut statuses = Vec::new();
    while !children.is_empty() {
        let mut still = Vec::new();
        for (tag, mut child) in children {
            match child.try_wait().unwrap() {
                Some(status) => statuses.push((tag, status)),
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("child {tag} did not finish in 60 s");
                }
                None => still.push((tag, child)),
            }
        }
        children = still;
        std::thread::sleep(Duration::from_millis(20));
    }
    for (tag, status) in statuses {
        let text = std::fs::read_to_string(root.join(format!("{tag}.out"))).unwrap();
        assert!(
            status.success() && text.contains("1 passed"),
            "{tag}: {status:?}\n{text}"
        );
    }
    let st = store(root.to_str().unwrap());
    let mut items: Vec<Value> = Vec::new();
    let mut from: Option<String> = None;
    loop {
        let (page, next) = st.read_log("events", from.as_deref()).unwrap();
        items.extend(page);
        match next {
            Some(c) => from = Some(c),
            None => break,
        }
    }
    assert_eq!(items.len() as u64, 2 * PER_CHILD);
    for tag in ["a", "b"] {
        let seq: Vec<u64> = items
            .iter()
            .filter(|v| v["tag"] == tag)
            .map(|v| v["i"].as_u64().unwrap())
            .collect();
        assert_eq!(
            seq,
            (0..PER_CHILD).collect::<Vec<_>>(),
            "{tag}'s items, in its own order"
        );
    }
    for e in std::fs::read_dir(root.join("plugins/demo/log")).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "jsonl") {
            assert!(
                std::fs::metadata(&p).unwrap().len() <= SEGMENT,
                "{} is over its cap",
                p.display()
            );
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}
