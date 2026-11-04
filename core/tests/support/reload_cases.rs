//! Cases for the reload check's re-read of a buffer opened on a missing file, shared by the test of the
//! timer (`buffer_reload_with_real_nvim.rs`) and the test of the agent slot's part
//! (`reload_part_with_real_nvim.rs`). Each takes an nvim with a UI attached and the check loaded, the
//! scratch directory it may write in, and `tick`, which runs one check and gives it time to finish.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::embed_client::Embed;

pub type Tick<'a> = &'a mut dyn FnMut(&mut Embed);

fn ui_mode(nvim: &mut Embed) -> (String, bool) {
    let m = nvim.request("nvim_get_mode", vec![]);
    let get = |key: &str| {
        m.as_map()
            .and_then(|entries| entries.iter().find(|(k, _)| k.as_str() == Some(key)))
            .map(|(_, v)| v.clone())
    };
    (
        get("mode")
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default(),
        get("blocking").and_then(|v| v.as_bool()).unwrap_or(false),
    )
}

/// No dialog is open and the next key typed reaches nvim: `i` makes it Insert mode.
pub fn the_next_key_reaches_nvim(nvim: &mut Embed) {
    assert_eq!(ui_mode(nvim), ("n".to_string(), false), "a dialog met the buffer");
    nvim.input("i");
    nvim.wait_until("insert mode", |n| ui_mode(n).0 == "i");
    nvim.input("<Esc>");
    nvim.wait_until("normal mode", |n| ui_mode(n).0 == "n");
}

fn line_of(nvim: &mut Embed, file: &Path) -> Option<String> {
    nvim.lua(
        "return vim.api.nvim_buf_get_lines(vim.fn.bufnr(...), 0, 1, false)[1]",
        vec![rmpv::Value::from(file.to_str().unwrap())],
    )
    .as_str()
    .map(str::to_string)
}

fn open_missing_then_create(nvim: &mut Embed, made: &Path, tick: Tick<'_>) {
    tick(nvim);
    std::fs::write(made, "made by the agent\n").unwrap();
    tick(nvim);
}

/// The file is in a split that is not the current window.
pub fn a_split_that_is_not_the_current_window(nvim: &mut Embed, scratch: &Path, tick: Tick<'_>) {
    let made = scratch.join("split.rs");
    nvim.command(&format!("split {}", made.display()));
    nvim.command("wincmd w");
    open_missing_then_create(nvim, &made, tick);
    assert_eq!(line_of(nvim, &made).as_deref(), Some("made by the agent"));
    the_next_key_reaches_nvim(nvim);
}

/// The file is shown only in a second tab page, so the re-read runs in the autocommand window.
pub fn a_second_tab_page(nvim: &mut Embed, scratch: &Path, tick: Tick<'_>) {
    let made = scratch.join("tab.rs");
    nvim.command(&format!("tabnew {}", made.display()));
    nvim.command("tabprevious");
    open_missing_then_create(nvim, &made, tick);
    assert_eq!(line_of(nvim, &made).as_deref(), Some("made by the agent"));
    the_next_key_reaches_nvim(nvim);
}

/// The command-line window is open when the file appears: `:edit` is refused there. The failed read is
/// not the buffer's first look at its file, so after the window closes the buffer is read, not checked.
pub fn the_command_line_window_open_when_the_file_appears(nvim: &mut Embed, scratch: &Path, tick: Tick<'_>) {
    let made = scratch.join("cmdwin.rs");
    nvim.command(&format!("edit {}", made.display()));
    tick(nvim);
    nvim.input("q:");
    nvim.wait_until("the command-line window", |n| {
        n.eval("getcmdwintype()").as_str() == Some(":")
    });
    std::fs::write(&made, "made by the agent\n").unwrap();
    tick(nvim);
    nvim.input(":q<CR>");
    nvim.wait_until("the window closed", |n| n.eval("getcmdwintype()").as_str() == Some(""));
    tick(nvim);
    tick(nvim);
    assert_eq!(line_of(nvim, &made).as_deref(), Some("made by the agent"));
    the_next_key_reaches_nvim(nvim);
}

/// Another nvim left a swap file with changes for the path. `:edit` looks for swap files and would open
/// the ATTENTION dialog; the check does not ask, and leaves the swap file as it was. `nvim` must have been
/// started with `EMBED_KEEP_SWAP`.
pub fn a_swap_file_left_by_another_nvim(nvim: &mut Embed, scratch: &Path, tick: Tick<'_>) {
    let made = scratch.join("swapped.rs");
    let swap = scratch.join(".swapped.rs.swp");
    let mut other = std::process::Command::new("nvim")
        .args(["--clean", "--headless", "-u", "NONE", "-i", "NONE"])
        .args(["-c", "set directory=.", "-c", &format!("edit {}", made.display())])
        .args(["-c", "normal! ihello", "-c", "preserve", "-c", "sleep 60"])
        .current_dir(scratch)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("nvim must be on PATH for this test");
    let until = Instant::now() + Duration::from_secs(10);
    while !swap.exists() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
    // A crash: the swap file stays, with changes in it.
    other.kill().unwrap();
    other.wait().unwrap();
    assert!(swap.exists(), "the other nvim left a swap file");
    let before = std::fs::read(&swap).unwrap();

    nvim.command("set directory=.");
    nvim.command("autocmd SwapExists * let v:swapchoice = 'e'");
    nvim.command(&format!("edit {}", made.display()));
    nvim.command("autocmd! SwapExists");
    open_missing_then_create(nvim, &made, tick);
    assert_eq!(line_of(nvim, &made).as_deref(), Some("made by the agent"));
    the_next_key_reaches_nvim(nvim);
    assert_eq!(
        std::fs::read(&swap).unwrap(),
        before,
        "the other nvim's swap file was left as it was"
    );
}

/// A buffer named for a FIFO: opening it would block. `tick` and one answer from nvim take about as long
/// as they do on an ordinary file.
pub fn a_fifo(nvim: &mut Embed, scratch: &Path, tick: Tick<'_>) {
    let began = Instant::now();
    tick(nvim);
    nvim.eval("1");
    let ordinary = began.elapsed();

    let fifo = scratch.join("pipe");
    let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: `c_path` is a valid NUL-terminated path that outlives the call.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    nvim.command("enew");
    nvim.command(&format!("file {}", fifo.display()));
    let began = Instant::now();
    tick(nvim);
    nvim.eval("1");
    let with_fifo = began.elapsed();
    assert!(
        with_fifo < 2 * ordinary,
        "a buffer over a FIFO took {with_fifo:?}, an ordinary tick {ordinary:?}"
    );
    nvim.command("bwipeout!");
    the_next_key_reaches_nvim(nvim);
}

/// The re-read can be undone: `u` brings back the empty buffer.
pub fn undo_after_a_reread(nvim: &mut Embed, scratch: &Path, tick: Tick<'_>) {
    let made = scratch.join("undo-after.rs");
    nvim.command(&format!("edit {}", made.display()));
    open_missing_then_create(nvim, &made, tick);
    assert_eq!(line_of(nvim, &made).as_deref(), Some("made by the agent"));
    nvim.command("silent normal! u");
    assert_eq!(
        line_of(nvim, &made).as_deref(),
        Some(""),
        "u brings back the empty buffer"
    );
    the_next_key_reaches_nvim(nvim);
}

/// What the re-read leaves as the person had it: the working directory (with 'autochdir' on and a BufEnter
/// autocmd that changes directory),
/// 'readonly', 'buflisted', the options the call sets, and no enter or leave events or option-set events
/// from it (the read events still fire).
pub fn the_persons_directory_options_and_events(nvim: &mut Embed, scratch: &Path, tick: Tick<'_>) {
    let sub = scratch.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let made = sub.join("background.rs");
    nvim.command(&format!("cd {}", scratch.display()));
    nvim.command(&format!("split {}", made.display()));
    nvim.command("setlocal readonly nobuflisted");
    nvim.command("wincmd w");
    nvim.command("set autochdir");
    nvim.command(&format!("cd {}", scratch.display()));
    nvim.command("let g:enters = 0 | let g:optsets = 0 | let g:reads = 0 | let g:log = []");
    for event in ["BufEnter", "BufLeave", "WinEnter", "WinLeave"] {
        nvim.command(&format!(
            "autocmd {event} * let g:enters += 1 | let g:log += ['{event} ' . expand('<abuf>') . ' ' . win_getid()]"
        ));
    }
    nvim.command("autocmd BufEnter * silent! cd %:p:h");
    nvim.command(
        "autocmd OptionSet shortmess,autochdir let g:optsets += 1 | let g:log += ['opt ' . expand('<amatch>')]",
    );
    nvim.command("autocmd BufReadPost * let g:reads += 1");
    tick(nvim);
    std::fs::write(&made, "made by the agent\n").unwrap();
    let before = nvim.eval("[getcwd(), &shortmess, &eventignore, &autochdir, g:enters, g:optsets, g:reads]");
    tick(nvim);
    let after = nvim.eval("[getcwd(), &shortmess, &eventignore, &autochdir, g:enters, g:optsets, g:reads]");
    let (before, after) = (before.as_array().unwrap().clone(), after.as_array().unwrap().clone());
    let log = nvim.eval("g:log");
    assert_eq!(
        after[..6],
        before[..6],
        "directory, options, enter/leave or option-set events moved: {log:?}"
    );
    assert!(
        after[6].as_i64() > before[6].as_i64(),
        "the file was not read: {after:?}"
    );
    assert_eq!(line_of(nvim, &made).as_deref(), Some("made by the agent"));
    let info = nvim.lua(
        "local b = vim.fn.bufnr(...); return { vim.bo[b].readonly, vim.bo[b].buflisted }",
        vec![rmpv::Value::from(made.to_str().unwrap())],
    );
    assert_eq!(info.as_array().unwrap()[0].as_bool(), Some(true), "'readonly' was lost");
    assert_eq!(
        info.as_array().unwrap()[1].as_bool(),
        Some(false),
        "the buffer was listed"
    );
    the_next_key_reaches_nvim(nvim);
}
