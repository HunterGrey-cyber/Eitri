//! The scripted Messages API stand-in as a process, for runs where the CLI is a child of another program.
//!
//! `eitri-claude-scripted serve <dir> [--addr <host:port>] [--census] [--key <api key>]`
//!
//! Reads `<dir>/script.json` (see `testkit::claude_scripted::script::ScriptFile`) and again whenever that file
//! changes, writes the port it listens on to `<dir>/port`, and logs every request to `<dir>/requests.jsonl` and
//! `<dir>/endpoint.jsonl`. `--addr` defaults to `127.0.0.1:0`; `--census` turns on census mode. It runs until it is
//! killed.
//!
//! `eitri-claude-scripted scrub <run dir> <staging dir>` writes the scrubbed form of a run directory's tap
//! logs, request log with bodies and CLI transcript into the staging directory, with a `manifest.json` that
//! names every file there: for a run that no test binary recorded.

use agent::testkit::claude_scripted::{
    script::ScriptFile,
    scrub::{scrub_exec_run, Scrubber},
    Script, ScriptedModel,
};
use std::{path::PathBuf, time::Duration};

const DEFAULT_KEY: &str = "scripted-harness-no-credential";

fn fail(message: &str) -> ! {
    eprintln!("eitri-claude-scripted: {message}");
    std::process::exit(2);
}

fn scrub(mut args: impl Iterator<Item = String>) {
    let (Some(run), Some(staging), None) = (args.next(), args.next(), args.next()) else {
        fail("usage: eitri-claude-scripted scrub <run dir> <staging dir>");
    };
    let run = PathBuf::from(run);
    let staging = PathBuf::from(staging);
    let scrubber = Scrubber::for_run(&run);
    match scrub_exec_run(&scrubber, &run, &staging) {
        Ok(files) => println!("scrubbed {} file(s) into {}", files.len(), staging.display()),
        Err(e) => fail(&format!("cannot scrub {}: {e}", run.display())),
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("serve") => {}
        Some("scrub") => return scrub(args),
        _ => fail(
            "usage: eitri-claude-scripted serve <dir> [--addr <host:port>] [--census] [--key <key>]\n       \
             eitri-claude-scripted scrub <run dir> <staging dir>",
        ),
    }
    let Some(dir) = args.next().map(PathBuf::from) else {
        fail("serve needs a directory")
    };
    let mut addr = "127.0.0.1:0".to_owned();
    let mut census = false;
    let mut key = DEFAULT_KEY.to_owned();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" => addr = args.next().unwrap_or_else(|| fail("--addr needs a value")),
            "--key" => key = args.next().unwrap_or_else(|| fail("--key needs a value")),
            "--census" => census = true,
            other => fail(&format!("unknown argument `{other}`")),
        }
    }
    let script_path = dir.join("script.json");
    let text =
        std::fs::read_to_string(&script_path).unwrap_or_else(|e| fail(&format!("{}: {e}", script_path.display())));
    let file = ScriptFile::parse(&text).unwrap_or_else(|e| fail(&format!("{}: {e}", script_path.display())));
    let script = Script {
        key,
        log_dir: dir.clone(),
        census,
        ..file.into_script()
    };
    let model = ScriptedModel::start(addr.as_str(), script).unwrap_or_else(|e| fail(&format!("cannot start: {e}")));
    model.watch_script(&script_path);
    let port = format!("{}\n", model.addr().port());
    agent::private_fs::write_private(&dir.join("port"), port.as_bytes())
        .unwrap_or_else(|e| fail(&format!("port file: {e}")));
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
