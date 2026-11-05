#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};

use eitri_core::agents::cli::{run, write_view};
use eitri_core::agents::publish::PublisherHooks;
use eitri_core::agents::*;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "agents-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn control(&self) -> PathBuf {
        self.0.join("eitri")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture() -> View {
    serde_json::from_str(include_str!("../src/agents/fixtures/view.json")).unwrap()
}

fn writer(control: &Path, rows: Vec<Row>) -> (Publisher, Receiver<ProcessFile>) {
    let view = fixture();
    let (tx, rx) = channel();
    let publisher = Publisher::start_with(
        control,
        &view.rows[0].window.instance,
        view.projects[0].clone(),
        view.rows[0].window.clone(),
        PublisherHooks {
            on_publish: Some(Box::new(move |file| {
                tx.send(file.clone()).unwrap();
            })),
            ..Default::default()
        },
    )
    .unwrap();
    rx.recv().unwrap();
    publisher.offer_now(rows);
    rx.recv().unwrap();
    (publisher, rx)
}

fn invoke(args: &[&str], control: &Path) -> (i32, String) {
    let mut out = Vec::new();
    let code = run(&args.iter().map(|s| (*s).into()).collect::<Vec<_>>(), &mut out, control);
    (code, String::from_utf8(out).unwrap())
}

#[test]
fn plain_prints_the_table_in_row_order() {
    let scratch = Scratch::new();
    let mut view = fixture();
    view.rows[0].since = 0;
    view.rows[0].label = format!("\n{}", "界".repeat(LABEL_MAX));
    let mut second = view.rows[0].clone();
    second.id.push('2');
    second.key.push('2');
    second.number = 4;
    second.label = "second".into();
    let (_writer, _) = writer(&scratch.control(), vec![second, view.rows[0].clone()]);
    let (code, out) = invoke(&[], &scratch.control());
    assert_eq!(code, 0);
    let lines: Vec<_> = out.lines().collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(
        lines[0].split_whitespace().collect::<Vec<_>>(),
        ["PROJECT", "WIN", "#", "AGENT", "STATE", "AGE", "LABEL"]
    );
    assert!(lines[1].contains("eitri") && lines[1].contains("⚑ card"));
    assert!(lines[1].ends_with(&wire::cut(&view.rows[0].label, LABEL_MAX)));
    assert!(lines[2].ends_with("second"));
}

#[test]
fn plain_columns_line_up_when_a_name_is_wide() {
    let scratch = Scratch::new();
    let mut wide = fixture().rows[0].clone();
    wide.id.push('2');
    wide.key.push('2');
    wide.number = 5;
    wide.project.name = "项目".into();
    let mut narrow = wide.clone();
    narrow.id.push('3');
    narrow.key.push('3');
    narrow.number = 6;
    narrow.project.name = "ab".into();
    let (_writer, _) = writer(&scratch.control(), vec![wide, narrow]);
    let (code, out) = invoke(&[], &scratch.control());
    assert_eq!(code, 0);
    // The WIN column starts at the same terminal column in every line: "项目" is four columns wide.
    let starts: Vec<usize> = out
        .lines()
        .map(|line| {
            let name = line.split_whitespace().next().unwrap();
            let name_cols = if name == "项目" { 4 } else { name.chars().count() };
            let rest = &line[name.len()..];
            name_cols + (rest.len() - rest.trim_start().len())
        })
        .collect();
    assert!(starts.windows(2).all(|pair| pair[0] == pair[1]), "{out}");
}

#[test]
fn plain_says_so_when_no_window_shares_agents() {
    let scratch = Scratch::new();
    let (code, out) = invoke(&[], &scratch.control());
    assert_eq!(code, 0);
    assert!(out.lines().next().unwrap().starts_with("PROJECT"));
    assert!(out.contains("no window shares agents"), "{out}");
    let (code, json) = invoke(&["--json"], &scratch.control());
    assert_eq!(code, 0);
    assert!(!json.contains("no window shares agents"));
}

#[test]
fn json_prints_one_view_line() {
    let scratch = Scratch::new();
    let (_writer, _) = writer(&scratch.control(), fixture().rows);
    let (code, out) = invoke(&["--json"], &scratch.control());
    assert_eq!(code, 0);
    assert_eq!(out.lines().count(), 1);
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["v"], SCHEMA_V);
    assert_eq!(value["rows"][0]["label"], "close question");
    assert!(value.get("type").is_none());
}

fn child(control: &Path, mode: &str) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "cli_child", "--nocapture"])
        .env("EITRI_CLI_TEST_CONTROL", control)
        .env("EITRI_CLI_TEST_MODE", mode)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn next_json(reader: &mut impl BufRead) -> serde_json::Value {
    loop {
        let mut line = String::new();
        assert_ne!(
            reader.read_line(&mut line).unwrap(),
            0,
            "the stream ended before its next object"
        );
        if line.starts_with('{') {
            return serde_json::from_str(&line).unwrap();
        }
    }
}

#[test]
fn cli_child() {
    let Some(control) = std::env::var_os("EITRI_CLI_TEST_CONTROL") else {
        return;
    };
    let mode = std::env::var("EITRI_CLI_TEST_MODE").unwrap();
    let mut out = std::io::stdout();
    let code = run(&[mode], &mut out, Path::new(&control));
    eprintln!("cli returned {code}");
    std::process::exit(code);
}

#[test]
fn watch_prints_a_view_then_changes() {
    let scratch = Scratch::new();
    let (writer, published) = writer(&scratch.control(), fixture().rows);
    let mut child = child(&scratch.control(), "--watch");
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let first = next_json(&mut output);
    assert_eq!(first["rows"][0]["state"], "card");
    assert!(first.get("type").is_none());
    let mut row = fixture().rows.remove(0);
    row.state = "running".into();
    row.class = Class::Working;
    writer.offer_now(vec![row]);
    published.recv().unwrap();
    let change = next_json(&mut output);
    assert_eq!(change["type"], "row");
    assert_eq!(change["row"]["state"], "running");
    drop(output);
    assert!(child.wait().unwrap().success());
}

#[test]
fn watch_ends_when_the_pipe_reader_goes() {
    let scratch = Scratch::new();
    let mut child = child(&scratch.control(), "--watch");
    let mut output = BufReader::new(child.stdout.take().unwrap());
    assert_eq!(next_json(&mut output)["rows"], serde_json::json!([]));
    // No publish follows the initial view: closing the pipe alone must wake the reader's poll.
    drop(output);
    assert!(child.wait().unwrap().success());
    let mut events = String::new();
    child.stderr.take().unwrap().read_to_string(&mut events).unwrap();
    assert_eq!(events.lines().collect::<Vec<_>>(), ["cli returned 0"]);
}

#[test]
fn no_writer_gives_an_empty_view_and_exit_0() {
    let scratch = Scratch::new();
    let (code, out) = invoke(&["--json"], &scratch.control());
    assert_eq!(code, 0);
    let view: View = serde_json::from_str(&out).unwrap();
    assert!(view.rows.is_empty() && view.projects.is_empty());
    assert_eq!(view.enabled, eitri_core::agent_prefs::AGENTS_ENABLED_DEFAULT);
    assert!(!scratch.control().exists());
}

#[test]
fn schema_2_is_refused_with_exit_64() {
    let scratch = Scratch::new();
    assert_eq!(
        invoke(&["--schema", &(SCHEMA_V + 1).to_string()], &scratch.control()).0,
        64
    );
    for args in [
        vec!["--schema"],
        vec!["--schema", "0"],
        vec!["--schema", "abc"],
        vec!["--unknown"],
        vec!["project"],
    ] {
        assert_eq!(invoke(&args, &scratch.control()).0, 64);
    }
    assert!(!scratch.control().exists());
}

#[test]
fn schema_1_output_matches_the_view_fixture() {
    let mut out = Vec::new();
    write_view(&fixture(), &mut out, SCHEMA_V).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        format!("{}\n", include_str!("../src/agents/fixtures/view.json"))
    );
    let scratch = Scratch::new();
    let (_writer, _) = writer(&scratch.control(), fixture().rows);
    let (code, out) = invoke(&["--json", "--schema", &SCHEMA_V.to_string()], &scratch.control());
    assert_eq!(code, 0);
    let mut view: View = serde_json::from_str(&out).unwrap();
    view.at = fixture().at;
    assert_eq!(view, fixture());
}

#[test]
fn cli_and_host_bodies_are_byte_identical() {
    // The host's watch reply fixture pins the view body, including selection and round.
    let reply: serde_json::Value =
        serde_json::from_str(include_str!("../src/plugin/fixtures/rep-agents-watch.json")).unwrap();
    let view: View = serde_json::from_value(reply["result"].clone()).unwrap();
    let mut out = Vec::new();
    write_view(&view, &mut out, SCHEMA_V).unwrap();
    let host_body = wire::view_json(&view, SCHEMA_V);
    assert_eq!(out, format!("{host_body}\n").as_bytes());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&host_body).unwrap(),
        reply["result"]
    );
}

#[test]
fn runs_with_no_display_variables() {
    let scratch = Scratch::new();
    let mut child = child(&scratch.control(), "--json");
    let mut output = BufReader::new(child.stdout.take().unwrap());
    assert_eq!(next_json(&mut output)["v"], SCHEMA_V);
    assert!(child.wait().unwrap().success());
}
