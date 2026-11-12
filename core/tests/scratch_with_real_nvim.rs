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
    agent::state_dirs::redirect_state_to_a_test_root();
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
    agent::state_dirs::redirect_state_to_a_test_root();
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

/// Views do not pile up as splits: the next one takes the window of the one before while it still shows it (vim's
/// preview window), and a window the person turned to something else is left alone.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_new_view_takes_the_window_of_the_view_before_it() {
    agent::state_dirs::redirect_state_to_a_test_root();
    let mut dir = ScratchDir::new().expect("a scratch directory");
    let probe = dir.path().join("probe.txt");
    let other = dir.path().join("other.txt");
    std::fs::write(&other, "elsewhere\n").unwrap();
    let views: Vec<_> = ["one", "two", "three"]
        .iter()
        .map(|title| {
            dir.prepare_view(&format!("Bash: {title}"), &format!("text of {title}"))
                .unwrap()
        })
        .collect();
    let report = |out: &std::path::Path| {
        format!(
            "lua local f = io.open('{}', 'a') f:write(#vim.api.nvim_tabpage_list_wins(0) .. ' ' .. vim.fn.getline(1) .. '\\n') f:close()",
            out.display()
        )
    };
    let four = dir.prepare_view("Bash: four", "text of four").unwrap();
    nvim(
        &dir,
        &[
            call(views[0].hex()),
            report(&probe),
            call(views[1].hex()),
            report(&probe),
            call(views[2].hex()),
            report(&probe),
            // The person opens another file in the view's window: the next view must not overwrite it.
            format!("edit {}", other.display()),
            call(four.hex()),
            report(&probe),
        ],
    );
    assert_eq!(
        std::fs::read_to_string(&probe).unwrap(),
        "2 text of one\n2 text of two\n2 text of three\n3 text of four\n"
    );
    dir.cleanup();
}

/// A view is reused only in the tab page the person is in: one opened in tab page 1 stays as it is when a second view
/// is asked for from a `:tabnew`, which gets a split of its own.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_view_in_another_tab_page_is_not_taken_over() {
    agent::state_dirs::redirect_state_to_a_test_root();
    let mut dir = ScratchDir::new().expect("a scratch directory");
    let probe = dir.path().join("probe.txt");
    let first = dir.prepare_view("Bash: one", "text of one").unwrap();
    let second = dir.prepare_view("Bash: two", "text of two").unwrap();
    let report = format!(
        "lua local f = io.open('{}', 'w') for _, tab in ipairs(vim.api.nvim_list_tabpages()) do local seen = {{}} for _, win in ipairs(vim.api.nvim_tabpage_list_wins(tab)) do seen[#seen + 1] = vim.api.nvim_buf_get_lines(vim.api.nvim_win_get_buf(win), 0, 1, false)[1] or '' end f:write(table.concat(seen, '|') .. '\\n') end f:close()",
        probe.display()
    );
    nvim(&dir, &[call(first.hex()), "tabnew".into(), call(second.hex()), report]);
    assert_eq!(
        std::fs::read_to_string(&probe).unwrap(),
        "|text of one\n|text of two\n",
        "tab page 1 keeps the first view; tab page 2 holds an empty window and the second view"
    );
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

/// A probe that writes the cursor as `<line> <byte column>` (one-based line, zero-based byte) to `out`.
fn write_cursor(out: &std::path::Path) -> String {
    format!(
        "lua local c = vim.api.nvim_win_get_cursor(0) local f = io.open('{}', 'w') f:write(c[1] .. ' ' .. c[2]) f:close()",
        out.display()
    )
}

fn read_cursor(probe: &std::path::Path) -> (u32, u32) {
    let text = std::fs::read_to_string(probe).unwrap();
    let (line, col) = text.trim().split_once(' ').unwrap();
    (line.parse().unwrap(), col.parse().unwrap())
}

fn hex_of(request: &serde_json::Value) -> String {
    request.to_string().bytes().map(|b| format!("{b:02x}")).collect()
}

/// A `gd` open's column counts in a language server's units. nvim turns it into the byte column of the character it
/// names on the opened file's own line: past CJK, a surrogate pair and a combining mark, in each of the three units;
/// one inside a character lands on its first byte, one past the end on the last character; without a column the
/// cursor is at the line's start as before, and a line past the end moves nothing.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_open_column_lands_on_its_character_in_each_unit() {
    use eitri_core::editor_tools::text_edits::Encoding::{self, Utf16, Utf32, Utf8};
    use eitri_core::scratch::OpenColumn;
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = ScratchDir::new().expect("a scratch directory");
    // Line 2, in bytes: two three-byte CJK characters 0..3 and 3..6, an emoji (two UTF-16 units) 6..10, e 10, a
    // combining acute 11..13, a space 13, x 14. In code points x is the seventh, in UTF-16 units the eighth.
    // Line 3 is empty.
    let file = dir.path().join("wide.txt");
    std::fs::write(&file, "first\n\u{4e2d}\u{6587}\u{1F600}e\u{301} x\n\nlast\n").unwrap();
    let probe = dir.path().join("cursor");
    let cursor = |line: u32, col: Option<(u32, Encoding)>| {
        let col = col.map(|(col, unit)| OpenColumn { col, unit });
        let request = eitri_core::scratch::open_request_at(&file, Some(line), col);
        let _ = std::fs::remove_file(&probe);
        nvim(&dir, &[call(request.hex()), write_cursor(&probe)]);
        read_cursor(&probe)
    };
    for (col, unit, byte, why) in [
        (7, Utf32, 14, "x is the seventh code point"),
        (8, Utf16, 14, "the emoji is two UTF-16 units"),
        (15, Utf8, 14, "x starts at the fifteenth byte"),
        (
            7,
            Utf16,
            13,
            "the combining mark is a unit of its own; the space follows it",
        ),
        (3, Utf16, 6, "the emoji's first unit"),
        (4, Utf16, 6, "inside the surrogate pair: the emoji's first byte"),
        (8, Utf8, 6, "inside the emoji's bytes: its first byte"),
        (2, Utf8, 0, "inside the first character: its first byte"),
        (1000, Utf16, 14, "past the end: the last character"),
        (1000, Utf8, 14, "past the end in bytes: the last character"),
        (1000, Utf32, 14, "past the end in code points: the last character"),
    ] {
        assert_eq!(cursor(2, Some((col, unit))), (2, byte), "{col} {unit:?}: {why}");
    }
    assert_eq!(cursor(2, None), (2, 0), "no column: the line's start, as before");
    assert_eq!(cursor(3, Some((5, Utf16))), (3, 0), "an empty line");
    assert_eq!(
        cursor(99, Some((3, Utf16))),
        (1, 0),
        "a line past the buffer moves nothing, so no column is placed"
    );
    dir.cleanup();
}

/// The column and its unit are data in the JSON request: a value of the wrong shape sent straight to the snippet
/// raises no error, is never read as Ex or Lua, and leaves the cursor at the line's start.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_hostile_open_column_runs_nothing_and_places_nothing() {
    use serde_json::json;
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = ScratchDir::new().expect("a scratch directory");
    let file = dir.path().join("plain.txt");
    std::fs::write(&file, "first\nsecond line\n").unwrap();
    let probe = dir.path().join("cursor");
    let pwned = dir.path().join("pwned");
    let check = format!(
        "lua if vim.g.eitri_pwned then io.open({},{}):close() end",
        lua_bytes(&pwned.to_string_lossy()),
        lua_bytes("w")
    );
    let shapes = [
        (json!("1|let g:eitri_pwned=1"), json!("utf-16")),
        (json!("3"), json!("utf-16")),
        (json!(-1), json!("utf-16")),
        (json!(0), json!("utf-8")),
        (json!(1.5), json!("utf-16")),
        (json!(1e12), json!("utf-32")),
        (json!(null), json!("utf-16")),
        (json!(3), json!("utf-7")),
        (json!(3), json!("utf-16') vim.g.eitri_pwned = 1 --")),
        (json!(3), json!(16)),
        (json!(3), json!(null)),
        (json!({"col": 3}), json!(["utf-16"])),
    ];
    for (col, unit) in shapes {
        let request = json!({
            "op": "open", "path": file.to_string_lossy(), "line": 2, "col": col, "unit": unit,
        });
        let _ = std::fs::remove_file(&probe);
        nvim(&dir, &[call(&hex_of(&request)), write_cursor(&probe), check.clone()]);
        assert_eq!(read_cursor(&probe), (2, 0), "{request}");
        assert!(!pwned.exists(), "{request}: a column or unit ran as code");
    }
    dir.cleanup();
}

/// A file name is data. A newline in it must not end the command that opens it and start another:
/// the line after it would run as Ex in the user's nvim. The payload has no space, quote, `|`,
/// brace or bracket, so escaping the name for Ex does not defuse it; only never parsing it does.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_newline_in_an_opened_path_is_part_of_the_name_and_runs_nothing() {
    agent::state_dirs::redirect_state_to_a_test_root();
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
    agent::state_dirs::redirect_state_to_a_test_root();
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
    agent::state_dirs::redirect_state_to_a_test_root();
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
    agent::state_dirs::redirect_state_to_a_test_root();
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
    agent::state_dirs::redirect_state_to_a_test_root();
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

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_edit_reads_no_modeline_and_restores_the_previous_option() {
    let root = agent::state_dirs::test_workspace_dir("scratch-edit-modeline");
    let mut dir = ScratchDir::in_dir(&root).expect("a scratch directory");
    for enabled in [true, false] {
        let probe = dir.path().join("probe.txt");
        let (request, edit) = dir.prepare_edit("first\nvim: set textwidth=77 :\nlast").unwrap();
        nvim(
            &dir,
            &[
                format!("set {}modeline textwidth=0", if enabled { "" } else { "no" }),
                "let g:previous = bufnr()".into(),
                call(request.hex()),
                format!(
                    "call writefile([string(&l:modeline), string(&textwidth), string(&g:modeline), \
                     string(getbufvar(g:previous, '&modeline'))], '{}')",
                    probe.display()
                ),
                "wq".into(),
            ],
        );
        let option = i32::from(enabled);
        assert_eq!(
            std::fs::read_to_string(&probe).unwrap(),
            format!("0\n0\n{option}\n{option}\n")
        );
        assert_eq!(
            edit.poll(),
            Some(EditDone::Written("first\nvim: set textwidth=77 :\nlast".into()))
        );
    }
    dir.cleanup();
}

/// A view decides line ends the way opening the file would have: CR-LF throughout reads as plain
/// lines, and one bare LF among them keeps every CR as text.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_view_reads_crlf_line_ends_the_way_split_would() {
    agent::state_dirs::redirect_state_to_a_test_root();
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
    agent::state_dirs::redirect_state_to_a_test_root();
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
    agent::state_dirs::redirect_state_to_a_test_root();
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
    agent::state_dirs::redirect_state_to_a_test_root();
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
    agent::state_dirs::redirect_state_to_a_test_root();
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

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_read_only_marker_directory_reports_the_saved_draft() {
    agent::state_dirs::redirect_state_to_a_test_root();
    let mut dir = ScratchDir::new().unwrap();
    let (request, edit) = dir.prepare_edit("saved draft").unwrap();
    let probe = dir.path().join("notification.json");
    let marker_dir = dir.path().join("markers");
    std::fs::create_dir(&marker_dir).unwrap();
    nvim(&dir, &[
        format!("lua _G.notices = {{}}; vim.notify = function(text, level) table.insert(_G.notices, {{text, level}}) end; _G.req = vim.json.decode((('{}'):gsub('..', function(c) return string.char(tonumber(c, 16)) end)))", request.hex()),
        format!("lua _G.req.done = {}; vim.fn.setfperm({}, 'r-x------')", serde_json::to_string(&marker_dir.join("done")).unwrap(), serde_json::to_string(&marker_dir).unwrap()),
        "lua EitriScratch.call((vim.json.encode(_G.req):gsub('.', function(c) return string.format('%02x', string.byte(c)) end)))".into(),
        "call setline(1, 'edited draft')".into(),
        "wq".into(),
        format!("lua vim.fn.writefile({{vim.json.encode(_G.notices)}}, {})", serde_json::to_string(&probe).unwrap()),
    ]);
    std::fs::set_permissions(&marker_dir, {
        use std::os::unix::fs::PermissionsExt;
        std::fs::Permissions::from_mode(0o700)
    })
    .unwrap();
    let notices: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(probe).unwrap()).unwrap();
    assert_eq!(notices.as_array().unwrap().len(), 1, "{notices}");
    assert!(
        notices[0][0]
            .as_str()
            .unwrap()
            .contains("draft could not be handed back"),
        "{notices}"
    );
    assert!(
        notices[0][0].as_str().unwrap().contains(edit.body.to_str().unwrap()),
        "{notices}"
    );
    assert_eq!(notices[0][1], 4, "ERROR level");
    assert_eq!(std::fs::read_to_string(&edit.body).unwrap(), "edited draft\n");
    assert!(!marker_dir.join("done.tmp").exists());
    dir.cleanup();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn marker_write_close_and_rename_failures_remove_the_partial_marker() {
    agent::state_dirs::redirect_state_to_a_test_root();
    for step in ["write", "close", "rename"] {
        let mut dir = ScratchDir::new().unwrap();
        let (request, edit) = dir.prepare_edit("saved draft").unwrap();
        let probe = dir.path().join("notification.json");
        let setup = format!(
            r#"lua _G.notices = {{}}; vim.notify = function(text, level) table.insert(_G.notices, {{text, level}}) end;
            local done, step = {}, {};
            if step == 'rename' then
              local rename = os.rename;
              os.rename = function(from, to) if to == done then return nil, 'marker rename refused' end; return rename(from, to) end
            else
              local open = io.open;
              io.open = function(path, mode)
                local f, err = open(path, mode);
                if path ~= done .. '.tmp' or not f then return f, err end;
                return {{
                  write = function(_, text) f:write(text); if step == 'write' then return nil, 'marker write refused' end; return true end,
                  close = function() f:close(); if step == 'close' then return nil, 'marker close refused' end; return true end
                }}
              end
            end"#,
            serde_json::to_string(&edit.done).unwrap(),
            serde_json::to_string(step).unwrap(),
        );
        nvim(
            &dir,
            &[
                setup,
                call(request.hex()),
                "wq".into(),
                format!(
                    "lua vim.fn.writefile({{vim.json.encode(_G.notices)}}, {})",
                    serde_json::to_string(&probe).unwrap()
                ),
            ],
        );
        let notices: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(probe).unwrap()).unwrap();
        assert_eq!(notices.as_array().unwrap().len(), 1, "{step}: {notices}");
        assert!(
            notices[0][0]
                .as_str()
                .unwrap()
                .contains(&format!("marker {step} refused")),
            "{notices}"
        );
        assert_eq!(std::fs::read_to_string(&edit.body).unwrap(), "saved draft\n");
        assert!(!edit.done.exists());
        assert!(!edit.done.with_extension("done.tmp").exists());
        dir.cleanup();
    }
}
