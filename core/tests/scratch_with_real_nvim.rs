//! The Lua half of the scratch round trip, driven by a real `nvim --headless`. `#[ignore]`d: it needs
//! `nvim` on PATH; it spends no tokens and needs no display.
//!
//! Run: `cargo test -p eitri-core --test scratch_with_real_nvim -- --ignored`

use eitri_core::scratch::{EditDone, ScratchDir};

fn nvim(dir: &ScratchDir, commands: &[String]) {
    let mut command = std::process::Command::new("nvim");
    command.arg("--headless").arg("--clean");
    for arg in dir.nvim_args() {
        command.arg(arg);
    }
    for (key, value) in dir.child_env() {
        command.env(key, value);
    }
    for c in commands {
        command.args(["-c", c]);
    }
    let status = command
        .args(["-c", "qa!"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("nvim must be on PATH for this test");
    assert!(status.success(), "nvim exited with {status}");
}

fn call(hex: &str) -> String {
    format!("lua EitriScratch.call('{hex}')")
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn wq_returns_the_edited_draft_and_q_bang_changes_nothing() {
    let mut dir = ScratchDir::new().expect("a scratch directory");
    let (request, edit) = dir.prepare_edit("first draft").unwrap();
    nvim(
        &dir,
        &[
            call(request.hex()),
            "call setline(1, 'edited in nvim')".into(),
            "wq".into(),
        ],
    );
    assert_eq!(edit.poll(), Some(EditDone::Written("edited in nvim".into())));

    let (request, edit) = dir.prepare_edit("keep me").unwrap();
    nvim(
        &dir,
        &[
            call(request.hex()),
            "call setline(1, 'thrown away')".into(),
            "q!".into(),
        ],
    );
    assert_eq!(edit.poll(), Some(EditDone::Discarded));
    assert_eq!(
        std::fs::read_to_string(&edit.body).unwrap(),
        "keep me",
        ":q! wrote nothing"
    );
    dir.cleanup();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_view_is_read_only_and_a_bad_request_says_why() {
    let mut dir = ScratchDir::new().expect("a scratch directory");
    let probe = dir.path().join("probe.txt");
    let request = dir.prepare_view("Bash: ls", "a\nb").unwrap();
    nvim(
        &dir,
        &[
            call(request.hex()),
            format!(
                "call writefile([string(&modifiable) . ' ' . string(&readonly) . ' ' . string(line('$'))], '{}')",
                probe.display()
            ),
        ],
    );
    assert_eq!(std::fs::read_to_string(&probe).unwrap().trim(), "0 1 2");

    // An unknown op fails inside the snippet's pcall and writes the error marker.
    let (_, edit) = dir.prepare_edit("x").unwrap();
    let bad = serde_json::json!({ "op": "explode", "done": edit.done.to_string_lossy() }).to_string();
    let hex: String = bad.bytes().map(|b| format!("{b:02x}")).collect();
    nvim(&dir, &[call(&hex)]);
    assert!(matches!(edit.poll(), Some(EditDone::Failed(why)) if why.contains("unknown scratch op")));
    dir.cleanup();
}
