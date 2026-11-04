//! Files opened as nvim arguments after `--embed`, against real nvims.
//!
//! The pane spells `--embed` itself when it has something to open and puts the targets after it
//! (`neovide_editor::NeovideEditorPaneOptions::open`); the fork appends its own `--embed` only when none
//! is present, and nvim loads a positional file only after it has seen `--embed`. This starts real
//! `nvim --embed` children through `LiveHarness` with exactly that argument list (`--clean`, the
//! host's `--cmd`s, `--embed`, then the targets) and asks them over RPC what they loaded: the argument
//! list, the buffer, the cursor, what `VimEnter` saw. A pin bump that changes how the fork places
//! `--embed` fails here. It also leaves a swap file with unsaved changes behind a file and checks that
//! the prompt nvim raises for it is visible to the host and can be answered.
//!
//! The scenario runs in a child copy of this binary on its own `Xvfb` (winit and the clipboard connect
//! to a display even though no window is made); the parent connects to nothing. Ignored unless asked
//! for, because it spawns real nvims. Every case prints `open_at_start: ok <case>`; the parent fails if
//! one is missing:
//!
//!     cargo test -p neovide-editor --test open_at_start -- --ignored
use std::future::Future;
use std::path::Path;
use std::pin::pin;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use neovide::{
    bridge::NeovimWriter,
    live_harness::{LiveHarness, LiveHarnessOptions},
    units::{GridSize, PixelRect},
};
use nvim_rs::Neovim;
use skia_safe::{surfaces, Surface};

#[path = "../../shell/tests/support/own_x_server.rs"]
mod own_x_server;

const CANVAS: (i32, i32) = (800, 480);
const FRAME_DT: f32 = 1.0 / 60.0;
const CASE_ENV: &str = "EITRI_OPEN_AT_START_CASE";

/// Every case the scenario prints, in order; the parent requires each line.
const CASES: &[&str] = &[
    "line and cursor",
    "VimEnter sees the file",
    "restore stand-in stays quiet with a target",
    "restore stand-in runs without a target",
    "name with a space",
    "name starting with a plus",
    "name with a newline",
    "name with percent and hash",
    "missing file",
    "column on multibyte text",
    "a_dirty_swap_file_prompts_and_the_prompt_can_be_answered",
];

type Nvim = Neovim<NeovimWriter>;

struct Editor {
    harness: LiveHarness,
    surface: Surface,
}

/// Parks the calling thread between polls; nvim-rs's request future needs only a waker.
struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        std::thread::park();
    }
}

fn region() -> PixelRect<f32> {
    PixelRect::from_min_max((0.0, 0.0), (CANVAS.0 as f32, CANVAS.1 as f32))
}

/// The argument list the pane hands the fork for `clean`, the host's `extra` and `open`; mirrors
/// `nvim_args` in the library, whose own unit test pins the same order.
fn pane_args(extra: &[&str], open: &[String]) -> Vec<String> {
    let mut args = vec!["--clean".to_string()];
    args.extend(extra.iter().map(|s| s.to_string()));
    if !open.is_empty() {
        args.push("--embed".to_string());
        args.extend(open.iter().cloned());
    }
    args
}

fn launch(extra: &[&str], open: &[String]) -> Editor {
    launch_sized(extra, open, (80, 24))
}

fn launch_sized(extra: &[&str], open: &[String], (cols, rows): (u32, u32)) -> Editor {
    let harness = LiveHarness::with_options(LiveHarnessOptions {
        grid_size: Some(GridSize::new(cols, rows)),
        extra_nvim_args: pane_args(extra, open),
        // As the pane starts it.
        startup_message_capture: false,
        minimum_nvim_version: Some(neovide_editor::NVIM_FLOOR),
        ..Default::default()
    })
    .unwrap_or_else(|e| panic!("LiveHarness::with_options failed: {e:#}"));
    let surface = surfaces::raster_n32_premul(CANVAS).expect("raster surface");
    Editor { harness, surface }
}

fn frame(editor: &mut Editor) {
    let region = region();
    editor
        .harness
        .render_frame(editor.surface.canvas(), Some(&region), FRAME_DT);
}

/// Renders until `done`, failing after `limit`.
fn wait_for(editor: &mut Editor, what: &str, limit: Duration, mut done: impl FnMut(&Editor) -> bool) {
    let deadline = Instant::now() + limit;
    loop {
        frame(editor);
        if done(editor) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn nvim_of(editor: &Editor) -> Nvim {
    editor
        .harness
        .neovim_handler()
        .clone_current_neovim()
        .expect("an nvim connection")
}

/// Runs `call` against the editor's nvim on a thread of its own while this thread keeps rendering, so
/// the redraws nvim sends meanwhile are consumed. Fails after 20 s.
fn on_nvim<T, Fut>(editor: &mut Editor, call: impl FnOnce(Nvim) -> Fut + Send + 'static) -> T
where
    T: Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
{
    let nvim = nvim_of(editor);
    let worker: JoinHandle<T> = std::thread::spawn(move || block_on(call(nvim)));
    let deadline = Instant::now() + Duration::from_secs(20);
    while !worker.is_finished() {
        frame(editor);
        assert!(Instant::now() < deadline, "nvim did not answer within 20 s");
        std::thread::sleep(Duration::from_millis(5));
    }
    worker.join().expect("the request thread")
}

/// The answer of `return vim.json.encode(<expr>)` run in nvim.
fn ask(editor: &mut Editor, expr: &str) -> serde_json::Value {
    let code = format!("return vim.json.encode({expr})");
    let answer = on_nvim(editor, move |nvim| async move {
        nvim.exec_lua(&code, vec![])
            .await
            .map(|v| v.as_str().map(str::to_owned))
    });
    let text = answer
        .unwrap_or_else(|e| panic!("exec_lua({expr}) failed: {e}"))
        .unwrap_or_else(|| panic!("exec_lua({expr}) did not return a string"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{expr}: not JSON ({e}): {text}"))
}

/// Renders for `span`.
fn pump(editor: &mut Editor, span: Duration) {
    let end = Instant::now() + span;
    while Instant::now() < end {
        frame(editor);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn ready(editor: &mut Editor) {
    wait_for(editor, "the editor ready", Duration::from_secs(20), |e| {
        e.harness.is_ready()
    });
}

fn ok(case: &str) {
    println!("open_at_start: ok {case}");
}

fn shut(mut editor: Editor) {
    assert!(editor.harness.shutdown(), "shutdown did not observe nvim's exit");
}

fn path_str(path: &Path) -> String {
    path.to_str().expect("a UTF-8 scratch path").to_owned()
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap_or_else(|e| panic!("writing {path:?}: {e}"));
}

/// Opens `path` (after `prefix`, if any) and returns the editor once it is ready.
fn open(extra: &[&str], prefix: &[&str], path: &Path) -> Editor {
    let mut targets: Vec<String> = prefix.iter().map(|s| s.to_string()).collect();
    targets.push(path_str(path));
    let mut editor = launch(extra, &targets);
    ready(&mut editor);
    editor
}

fn line_and_cursor(dir: &Path) {
    let file = dir.join("fifty.txt");
    let text: String = (1..=50).map(|n| format!("line {n}\n")).collect();
    write(&file, &text);
    let mut editor = open(&[], &["+42"], &file);
    let got = ask(
        &mut editor,
        "{argc = vim.fn.argc(), name = vim.fn.bufname(''), line = vim.fn.line('.'), text = vim.fn.getline('.')}",
    );
    assert_eq!(got["argc"], 1, "{got}");
    assert!(got["name"].as_str().unwrap().ends_with("fifty.txt"), "{got}");
    assert_eq!(got["line"], 42, "{got}");
    assert_eq!(got["text"], "line 42", "{got}");
    shut(editor);
    ok("line and cursor");
}

fn vim_enter_and_restore(dir: &Path) {
    let file = dir.join("enter.txt");
    write(&file, "one\n");
    // A stand-in for a session-restore plugin: it acts only for a launch with no arguments.
    let enter = "autocmd VimEnter * let g:enter_argc = argc()";
    let restore = "autocmd VimEnter * if argc() == 0 | let g:restored = 1 | endif";
    let mut editor = open(&["--cmd", enter, "--cmd", restore], &[], &file);
    let got = ask(&mut editor, "{enter = vim.g.enter_argc, restored = vim.g.restored}");
    assert_eq!(got["enter"], 1, "{got}");
    ok("VimEnter sees the file");
    assert!(got["restored"].is_null(), "{got}");
    ok("restore stand-in stays quiet with a target");
    shut(editor);

    // The same stand-in with nothing to open does run, so the quiet above is the target's doing.
    let mut editor = launch(&["--cmd", enter, "--cmd", restore], &[]);
    ready(&mut editor);
    let got = ask(&mut editor, "{enter = vim.g.enter_argc, restored = vim.g.restored}");
    assert_eq!(got["enter"], 0, "{got}");
    assert_eq!(got["restored"], 1, "{got}");
    shut(editor);
    ok("restore stand-in runs without a target");
}

/// A file named `name` with one line of text opens as the only argument, whole and loaded.
fn odd_name(dir: &Path, name: &str, case: &str) {
    let file = dir.join(name);
    write(&file, "hello\n");
    let mut editor = open(&[], &[], &file);
    let got = ask(
        &mut editor,
        "{argc = vim.fn.argc(), full = vim.api.nvim_buf_get_name(0), text = vim.fn.getline(1)}",
    );
    assert_eq!(got["argc"], 1, "{case}: {got}");
    assert_eq!(got["full"], path_str(&file), "{case}: {got}");
    assert_eq!(got["text"], "hello", "{case}: {got}");
    shut(editor);
    ok(case);
}

fn missing_file(dir: &Path) {
    let file = dir.join("not-yet.txt");
    let mut editor = open(&[], &[], &file);
    let got = ask(
        &mut editor,
        "{argc = vim.fn.argc(), full = vim.api.nvim_buf_get_name(0), lines = vim.fn.line('$'), \
         text = vim.fn.getline(1), modified = vim.bo.modified, readable = vim.fn.filereadable(vim.api.nvim_buf_get_name(0))}",
    );
    assert_eq!(got["argc"], 1, "{got}");
    assert_eq!(got["full"], path_str(&file), "{got}");
    assert_eq!(
        (got["lines"].as_i64(), got["text"].as_str()),
        (Some(1), Some("")),
        "{got}"
    );
    assert_eq!(got["modified"], false, "{got}");
    assert_eq!(got["readable"], 0, "{got}");
    assert!(!file.exists(), "opening a missing file created it");
    shut(editor);
    ok("missing file");
}

fn multibyte_column(dir: &Path) {
    let file = dir.join("wide.txt");
    write(&file, "ab\n日本語テキスト\n");
    let mut editor = open(&[], &["+call cursor(2,4)"], &file);
    let got = ask(
        &mut editor,
        "{line = vim.fn.line('.'), col = vim.fn.col('.'), char = vim.fn.strcharpart(vim.fn.getline('.'), vim.fn.charcol('.') - 1, 1)}",
    );
    assert_eq!((got["line"].as_i64(), got["col"].as_i64()), (Some(2), Some(4)), "{got}");
    assert_eq!(got["char"], "本", "byte column 4 is the second character: {got}");
    shut(editor);
    ok("column on multibyte text");
}

/// Leaves `file` with a swap file in `swap_dir` that holds unsaved changes and belongs to no running
/// process: a headless nvim edits it, changes the buffer, preserves, and is killed by its pid.
fn leave_dirty_swap(file: &Path, swap_dir: &Path) {
    std::fs::create_dir_all(swap_dir).expect("swap dir");
    let mut child = Command::new("nvim")
        .args(["--headless", "--clean", "--cmd"])
        .arg(format!("set directory={}", swap_dir.display()))
        .args([
            "-c",
            "normal! Ounsaved-change-marker",
            "-c",
            "preserve",
            "-c",
            "sleep 600",
        ])
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("a headless nvim");
    let pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(15);
    let swap = loop {
        let found = std::fs::read_dir(swap_dir)
            .expect("swap dir listing")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| {
                std::fs::read(p)
                    .map(|bytes| bytes.windows(21).any(|w| w == b"unsaved-change-marker"))
                    .unwrap_or(false)
            });
        if let Some(swap) = found {
            break swap;
        }
        assert!(
            Instant::now() < deadline,
            "no swap file with the change appeared (nvim pid {pid})"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    child.kill().expect("kill the headless nvim by its handle");
    child.wait().expect("reap the headless nvim");
    println!("open_at_start: swap file {swap:?} left by pid {pid}");
}

/// At two grid sizes, because the question's message is several rows tall: on a short grid it waits
/// behind a "more" prompt first.
fn a_dirty_swap_file_prompts_and_the_prompt_can_be_answered(dir: &Path) {
    for (n, size) in [(80u32, 24u32), (120, 40)].into_iter().enumerate() {
        println!("open_at_start: swap case at grid {size:?}");
        dirty_swap_case(&dir.join(format!("g{n}")), size);
    }
    ok("a_dirty_swap_file_prompts_and_the_prompt_can_be_answered");
}

fn dirty_swap_case(dir: &Path, size: (u32, u32)) {
    let case = "a_dirty_swap_file_prompts_and_the_prompt_can_be_answered";
    std::fs::create_dir_all(dir).expect("case dir");
    let file = dir.join("swapped.txt");
    write(&file, "original\n");
    let swap_dir = dir.join("swaps");
    leave_dirty_swap(&file, &swap_dir);

    let directory = format!("set directory={}", swap_dir.display());
    let started = Instant::now();
    let mut editor = launch_sized(&["--cmd", &directory], &[path_str(&file)], size);
    println!("open_at_start: harness constructed after {:?}", started.elapsed());

    // The prompt must be up and visible to the host: nvim is in a prompt mode (the question's, `r?` from
    // nvim 0.12 and `c` on 0.11, or `rm` for a "more" page before it). Only the "more" page reports `blocking`.
    let question = question_mode();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut mode = loop {
        let mode = nvim_mode(&mut editor).0;
        if mode == question || mode == "rm" {
            break mode;
        }
        assert!(
            Instant::now() < deadline,
            "{case}: nvim never showed a prompt (last mode {mode:?})"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    println!("open_at_start: prompt up after {:?}, mode {mode:?}", started.elapsed());

    // The cover over the pane lifts on `is_ready`; it has to arrive while the prompt is up.
    wait_for(
        &mut editor,
        "is_ready with the prompt up",
        Duration::from_secs(10),
        |e| e.harness.is_ready(),
    );
    println!(
        "open_at_start: is_ready with the prompt up, {:?} after the start",
        started.elapsed()
    );
    let still = nvim_mode(&mut editor).0;
    assert!(
        still == question || still == "rm",
        "{case}: the prompt went away by itself (mode {still:?})"
    );

    // A space goes on to the next "more" page until the question itself is up.
    for _ in 0..8 {
        if mode != "rm" {
            break;
        }
        editor.harness.send_text_input(" ");
        pump(&mut editor, Duration::from_millis(150));
        mode = nvim_mode(&mut editor).0;
    }
    assert_eq!(mode, question, "{case}: the question itself is up");
    // `e` is "Edit anyway": the prompt goes and the file's buffer is the current one.
    editor.harness.send_text_input("e");
    let got = ask(
        &mut editor,
        "{mode = vim.api.nvim_get_mode().mode, argc = vim.fn.argc(), full = vim.api.nvim_buf_get_name(0), text = vim.fn.getline(1)}",
    );
    assert_eq!(got["mode"], "n", "{case}: {got}");
    assert_eq!(got["argc"], 1, "{case}: {got}");
    assert_eq!(got["full"], path_str(&file), "{case}: {got}");
    assert_eq!(got["text"], "original", "{case}: {got}");
    shut(editor);
}

/// The swap file question's `nvim_get_mode` mode on the `nvim` on `PATH`: `r?` from 0.12, `c` on 0.11.
fn question_mode() -> &'static str {
    let out = Command::new("nvim").arg("--version").output().expect("nvim on PATH");
    let first = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    let numbers = first
        .strip_prefix("NVIM v")
        .unwrap_or_else(|| panic!("unexpected nvim --version line: {first:?}"));
    let mut parts = numbers
        .split(|c: char| !c.is_ascii_digit())
        .map(|p| p.parse::<u64>().expect("a number"));
    if (parts.next().unwrap(), parts.next().unwrap()) >= (0, 12) {
        "r?"
    } else {
        "c"
    }
}

/// nvim's `(mode, blocking)` from `nvim_get_mode`, which it answers even while it waits for a key.
fn nvim_mode(editor: &mut Editor) -> (String, bool) {
    let pairs = on_nvim(editor, |nvim| async move { nvim.get_mode().await });
    let pairs = pairs.unwrap_or_else(|e| panic!("nvim_get_mode failed: {e}"));
    let field = |name: &str| {
        pairs
            .iter()
            .find(|(k, _)| k.as_str() == Some(name))
            .map(|(_, v)| v.clone())
    };
    (
        field("mode")
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
        field("blocking").and_then(|v| v.as_bool()).unwrap_or(false),
    )
}

fn scenario() {
    let dir = std::env::temp_dir().join(format!("open-at-start-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    line_and_cursor(&dir);
    vim_enter_and_restore(&dir);
    odd_name(&dir, "a b.txt", "name with a space");
    odd_name(&dir, "+x.txt", "name starting with a plus");
    odd_name(&dir, "new\nline.txt", "name with a newline");
    odd_name(&dir, "100%#.txt", "name with percent and hash");
    missing_file(&dir);
    multibyte_column(&dir);
    a_dirty_swap_file_prompts_and_the_prompt_can_be_answered(&dir);
    let _ = std::fs::remove_dir_all(&dir);
}

fn main() {
    if std::env::var_os(CASE_ENV).is_some() {
        if let Ok(Some(kind)) = neovide_editor::detach_stdin_from_nvim() {
            println!("[stdin] a {kind} on stdin would reach nvim as a buffer; stdin is /dev/null now");
        }
        let _server = own_x_server::isolate("open_at_start", "800x600x24");
        scenario();
        println!("RESULT pass");
        return;
    }
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("open_at_start: ignored (spawns real nvims); run with `-- --ignored`");
        return;
    }
    let out = Command::new(std::env::current_exe().unwrap())
        .env(CASE_ENV, "1")
        .output()
        .expect("re-exec of the test binary failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    println!("{stdout}");
    let missing: Vec<&&str> = CASES
        .iter()
        .filter(|case| !stdout.lines().any(|l| l == format!("open_at_start: ok {case}")))
        .collect();
    if !stdout.lines().any(|l| l == "RESULT pass") || !missing.is_empty() {
        panic!(
            "open_at_start failed ({}); cases that did not report ok: {missing:?}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
