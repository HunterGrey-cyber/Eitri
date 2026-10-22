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

/// A name that starts with `+` is an Ex command to `:edit` even as a structured argument. Every producer
/// hands over an absolute path; a relative one must be refused by the snippet, not opened.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_relative_path_is_refused_and_runs_nothing() {
    let dir = ScratchDir::new().expect("a scratch directory");
    let pwned = dir.path().join("pwned");
    let lua = format!(
        "lua if vim.g.eitri_pwned then io.open({},{}):close() end",
        lua_bytes(&pwned.to_string_lossy()),
        lua_bytes("w")
    );
    for op in ["open", "view"] {
        let request = serde_json::json!({ "op": op, "path": "+let\\ g:eitri_pwned=1 x", "line": null }).to_string();
        let hex: String = request.bytes().map(|b| format!("{b:02x}")).collect();
        nvim(&dir, &[call(&hex), lua.clone()]);
        assert!(!pwned.exists(), "{op}: the +command in a relative path ran");
    }
    dir.cleanup();
}

/// The `edit` op opens its draft with `:split`, which reads a leading `+` as a command just as `:edit` does, so it
/// goes through the same absolute-path check. Unlike `open` and `view` it carries a `done` marker, so the refusal
/// also has to come back through it as a failure the panel can show.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_relative_path_for_an_edit_is_refused_and_reported_through_done() {
    let mut dir = ScratchDir::new().expect("a scratch directory");
    let pwned = dir.path().join("pwned");
    let lua = format!(
        "lua if vim.g.eitri_pwned then io.open({},{}):close() end",
        lua_bytes(&pwned.to_string_lossy()),
        lua_bytes("w")
    );
    let (_, edit) = dir.prepare_edit("draft").unwrap();
    let request = serde_json::json!({
        "op": "edit",
        "path": "+let\\ g:eitri_pwned=1 x",
        "done": edit.done.to_string_lossy(),
    })
    .to_string();
    let hex: String = request.bytes().map(|b| format!("{b:02x}")).collect();
    nvim(&dir, &[call(&hex), lua]);
    assert!(!pwned.exists(), "the +command in a relative edit path ran");
    assert!(
        matches!(edit.poll(), Some(EditDone::Failed(why)) if why.contains("not absolute")),
        "the refusal did not reach the done marker"
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

/// Writes, one line per window of the current tab page, the window's buffer name and its `modifiable`,
/// `readonly`, `diff`, `modeline`, `filetype` and `textwidth`, then the tab page count, to `out`.
fn write_tab_state(out: &std::path::Path) -> String {
    format!(
        "lua local t = {{}} \
         for _, w in ipairs(vim.api.nvim_tabpage_list_wins(0)) do \
           local b = vim.api.nvim_win_get_buf(w) \
           t[#t + 1] = table.concat({{ vim.api.nvim_buf_get_name(b), tostring(vim.bo[b].modifiable), \
             tostring(vim.bo[b].readonly), tostring(vim.wo[w].diff), tostring(vim.bo[b].modeline), \
             vim.bo[b].filetype, tostring(vim.bo[b].textwidth) }}, ' ') \
         end \
         t[#t + 1] = 'tabs ' .. #vim.api.nvim_list_tabpages() \
         local f = io.open('{}', 'w') f:write(table.concat(t, '\\n')) f:close()",
        out.display()
    )
}

/// The proposal's view is a tab page of its own holding the file on disk and the file as proposed, both
/// read-only and in diff mode, typed by the real file's name; `q` closes it, and the buffer the user had
/// open is left as it was.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_diff_op_opens_two_read_only_buffers_in_a_new_tab() {
    let mut dir = ScratchDir::new().expect("a scratch directory");
    let mine = dir.path().join("mine.txt");
    std::fs::write(&mine, "the user's own text\n").unwrap();
    let request = dir
        .prepare_proposal("src/main.rs", "fn main() {}\n", "fn main() {\n    run();\n}\n")
        .unwrap();
    let opened = dir.path().join("opened");
    let closed = dir.path().join("closed");
    nvim(
        &dir,
        &[
            format!("edit {}", mine.display()),
            "call setline(1, 'typed, not saved')".into(),
            call(request.hex()),
            write_tab_state(&opened),
            "normal q".into(),
            format!(
                "lua local f = io.open('{}', 'w') f:write(vim.api.nvim_buf_get_name(0), ' ', \
                 vim.fn.getline(1), ' ', tostring(vim.bo.modified), ' tabs ', #vim.api.nvim_list_tabpages()) f:close()",
                closed.display()
            ),
        ],
    );
    let opened = std::fs::read_to_string(&opened).expect("the diff tab was probed");
    let lines: Vec<&str> = opened.lines().collect();
    assert_eq!(lines.len(), 3, "two windows and the tab count: {opened}");
    assert_eq!(lines[2], "tabs 2", "a new tab page: {opened}");
    for line in &lines[..2] {
        let fields: Vec<&str> = line.split(' ').collect();
        assert!(
            std::path::Path::new(fields[0]).starts_with(dir.path()),
            "a scratch buffer, not the real file: {line}"
        );
        // The textwidth is left out: the filetype's own plugin may set one.
        assert_eq!(
            &fields[1..6],
            &["false", "true", "true", "false", "rust"],
            "read-only, in diff mode, no modeline, typed by the real name: {line}"
        );
    }
    assert!(
        lines[0].contains("-on-disk-main.rs") && lines[1].contains("-proposed-main.rs"),
        "{opened}"
    );
    assert_eq!(
        std::fs::read_to_string(&closed).unwrap(),
        format!("{} typed, not saved true tabs 1", mine.display()),
        "q closed the tab and the user's buffer is untouched"
    );
    assert_eq!(std::fs::read_to_string(&mine).unwrap(), "the user's own text\n");
    dir.cleanup();
}

/// A proposal is the agent's text: a modeline in it (`vim:` or `ex:`) sets nothing in either buffer.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_modeline_in_a_proposal_never_runs() {
    let mut dir = ScratchDir::new().expect("a scratch directory");
    let request = dir
        .prepare_proposal(
            "notes.txt",
            "first\n# vim: set ft=sh textwidth=66:\n",
            "first\n# ex: set textwidth=77:\nlast\n# vim: set ft=sh:\n",
        )
        .unwrap();
    let probe = dir.path().join("probe");
    nvim(&dir, &[call(request.hex()), write_tab_state(&probe)]);
    let probe = std::fs::read_to_string(&probe).expect("the diff tab was probed");
    let lines: Vec<&str> = probe.lines().collect();
    assert_eq!(lines.len(), 3, "{probe}");
    for line in &lines[..2] {
        let fields: Vec<&str> = line.split(' ').collect();
        assert_eq!(
            &fields[1..],
            &["false", "true", "true", "false", "text", "0"],
            "the options the op set, not the modeline's: {line}"
        );
    }
    dir.cleanup();
}

/// Writes each window's buffer lines of the current tab page to `out`, one window per line, the lines
/// joined by `|` and a CR shown as `^M`.
fn write_tab_lines(out: &std::path::Path) -> String {
    format!(
        "lua local t = {{}} \
         for _, w in ipairs(vim.api.nvim_tabpage_list_wins(0)) do \
           local lines = vim.api.nvim_buf_get_lines(vim.api.nvim_win_get_buf(w), 0, -1, false) \
           t[#t + 1] = (table.concat(lines, '|'):gsub('\\r', '^M')) \
         end \
         local f = io.open('{}', 'w') f:write(table.concat(t, '\\n')) f:close()",
        out.display()
    )
}

/// A proposal that changes only line ends must not look like no change: each side keeps the ends the
/// two sides do not share, so the difference shows in the diff.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_change_of_line_ends_alone_still_shows_in_the_diff() {
    let mut dir = ScratchDir::new().expect("a scratch directory");
    for (base, proposed, want) in [
        // CR-LF made LF: the CRs stay on the left.
        ("one\r\ntwo\r\n", "one\ntwo\n", "one^M|two^M\none|two"),
        // The final line end removed: an empty last line on the side that has it.
        ("one\ntwo\n", "one\ntwo", "one|two|\none|two"),
        // Both sides CR-LF, and both with a final end: read away on both, as `:split` would.
        ("one\r\ntwo\r\n", "one\r\nTWO\r\n", "one|two\none|TWO"),
        // A new file has nothing to disagree with.
        ("", "one\r\n", "\none"),
    ] {
        let request = dir.prepare_proposal("notes.txt", base, proposed).unwrap();
        let probe = dir.path().join("lines");
        nvim(&dir, &[call(request.hex()), write_tab_lines(&probe)]);
        assert_eq!(
            std::fs::read_to_string(&probe).expect("the diff tab was probed"),
            want,
            "{base:?} -> {proposed:?}"
        );
    }
    dir.cleanup();
}

/// When the proposal's tab is the last one, `q` cannot close it without quitting nvim; the window that
/// stays leaves diff mode, so the buffer it shows next is not diffed against nothing.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn q_in_the_last_tab_leaves_no_window_in_diff_mode() {
    let mut dir = ScratchDir::new().expect("a scratch directory");
    let request = dir.prepare_proposal("main.rs", "a\n", "b\n").unwrap();
    let probe = dir.path().join("after");
    nvim(
        &dir,
        &[
            call(request.hex()),
            "tabonly".into(),
            "normal q".into(),
            format!(
                "lua local t = {{}} \
                 for _, w in ipairs(vim.api.nvim_list_wins()) do \
                   t[#t + 1] = table.concat({{ tostring(vim.wo[w].diff), tostring(vim.wo[w].scrollbind), \
                     tostring(vim.wo[w].cursorbind), vim.wo[w].foldmethod }}, ' ') \
                 end \
                 t[#t + 1] = 'tabs ' .. #vim.api.nvim_list_tabpages() \
                 local f = io.open('{}', 'w') f:write(table.concat(t, '\\n')) f:close()",
                probe.display()
            ),
        ],
    );
    assert_eq!(
        std::fs::read_to_string(&probe).expect("the windows were probed"),
        "false false false manual\ntabs 1"
    );
    dir.cleanup();
}
