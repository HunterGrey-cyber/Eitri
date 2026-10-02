//! The Lua half of the scratch round trip, driven by a real `nvim --headless`. `#[ignore]`d: it needs
//! `nvim` on PATH; it spends no tokens and needs no display.
//!
//! Run: `cargo test -p eitri-core --test scratch_with_real_nvim -- --ignored`

use eitri_core::scratch::{EditDone, ScratchDir};

fn nvim(dir: &ScratchDir, commands: &[String]) {
    let xdg = dir.path().join("xdg");
    let mut command = std::process::Command::new("nvim");
    command.args(["--headless", "--clean", "-i", "NONE"]);
    // Every XDG base under the scratch directory: nothing reads or writes the user's own nvim state.
    for (key, sub) in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
    ] {
        command.env(key, xdg.join(sub));
    }
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

fn lua_bytes(text: &str) -> String {
    let codes: Vec<String> = text.bytes().map(|b| b.to_string()).collect();
    format!("string.char({})", codes.join(","))
}

/// A probe that writes the current buffer's name, byte for byte, to `out`.
fn write_buffer_name(out: &std::path::Path) -> String {
    format!(
        "lua local f = io.open('{}', 'w') f:write(vim.api.nvim_buf_get_name(0)) f:close()",
        out.display()
    )
}

/// A file name is data. A newline in it must not end the command that opens it and start another:
/// the line after it would run as Ex in the user's nvim. The payload has no space, quote, `|`,
/// brace or bracket, so escaping the name for Ex does not defuse it; only never parsing it does.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_newline_in_an_opened_path_is_part_of_the_name_and_runs_nothing() {
    let dir = ScratchDir::new().expect("a scratch directory");
    let marker = dir.path().join("injected");
    let name = format!(
        "notes\nlua(io.open)({},{})",
        lua_bytes(&marker.to_string_lossy()),
        lua_bytes("w")
    );
    let hostile = dir.path().join(&name);
    std::fs::write(&hostile, "hostile\n").unwrap();
    let probe = dir.path().join("opened");
    let request = eitri_core::scratch::open_request(&hostile, Some(1));
    nvim(&dir, &[call(request.hex()), write_buffer_name(&probe)]);
    assert!(!marker.exists(), "the text after the newline ran as a command");
    assert_eq!(
        std::fs::read_to_string(&probe).unwrap(),
        hostile.to_string_lossy(),
        "the file itself is the one open"
    );
    dir.cleanup();
}

/// `%`, `#`, a space and a backslash in a name name themselves, not the current file, the
/// alternate file or two arguments.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_opened_path_is_never_expanded() {
    let dir = ScratchDir::new().expect("a scratch directory");
    let odd = dir.path().join("a %b #c d\\e.txt");
    std::fs::write(&odd, "x\n").unwrap();
    let probe = dir.path().join("opened");
    let request = eitri_core::scratch::open_request(&odd, None);
    nvim(&dir, &[call(request.hex()), write_buffer_name(&probe)]);
    assert_eq!(std::fs::read_to_string(&probe).unwrap(), odd.to_string_lossy());
    dir.cleanup();
}

/// A view shows text the user never chose to treat as a file (a tool's output, a reply), so a
/// modeline in it sets nothing.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_view_reads_no_modeline() {
    let mut dir = ScratchDir::new().expect("a scratch directory");
    let probe = dir.path().join("probe.txt");
    let request = dir
        .prepare_view("Bash: cat", "first\nvim: set textwidth=77 :\nlast")
        .unwrap();
    nvim(
        &dir,
        &[
            call(request.hex()),
            format!(
                "call writefile([string(&l:modeline) . ' ' . string(&textwidth) . ' ' . string(line('$')) . ' ' . string(&modified)], '{}')",
                probe.display()
            ),
        ],
    );
    assert_eq!(std::fs::read_to_string(&probe).unwrap().trim(), "0 0 3 0");
    dir.cleanup();
}

/// A view decides line ends the way opening the file would have: CR-LF throughout reads as plain
/// lines, and one bare LF among them keeps every CR as text.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_view_reads_crlf_line_ends_the_way_split_would() {
    let mut dir = ScratchDir::new().expect("a scratch directory");
    for (text, want) in [
        ("one\r\ntwo\r\n", "2 3 3"),
        ("one\r\ntwo", "2 3 3"),
        ("one\r\ntwo\nthree\r\n", "3 4 3"),
    ] {
        let probe = dir.path().join("probe.txt");
        let request = dir.prepare_view("Bash: cat", text).unwrap();
        nvim(
            &dir,
            &[
                call(request.hex()),
                format!(
                    "call writefile([string(line('$')) . ' ' . string(strlen(getline(1))) . ' ' . string(strlen(getline(2)))], '{}')",
                    probe.display()
                ),
            ],
        );
        assert_eq!(std::fs::read_to_string(&probe).unwrap().trim(), want, "{text:?}");
    }
    dir.cleanup();
}
