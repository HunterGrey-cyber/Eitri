//! Diagnostics read through the shared chunk in a private headless embedded nvim.
#![cfg(target_os = "linux")]

#[path = "support/embed_client.rs"]
mod embed_client;

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use eitri_core::editor_rpc::EditorRpc;
use eitri_core::editor_tools::stamp::{Sequencer, EDITOR_BUDGET};
use eitri_core::editor_tools::tools::diagnostics::{self, DiagnosticsArgs, Severity};
use eitri_core::ide_server::diagnostics::shape;
use eitri_core::nvim_rpc::Pending;
use embed_client::Embed;
use rmpv::Value;

struct Rpc(RefCell<Embed>);

impl EditorRpc for Rpc {
    fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
        let (answer, pending) = Pending::pair();
        answer.send(Ok(self.0.borrow_mut().lua(code, args)));
        pending
    }

    fn target(&self) -> Option<u64> {
        Some(1)
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "ide-diagnostics-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir.canonicalize().unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const PREPARE: &str = r#"
local path, loaded = ...
local b = vim.fn.bufadd(path)
vim.fn.bufload(b)
vim.api.nvim_buf_set_lines(b, 0, -1, false, { 'a😀bc' })
vim.diagnostic.set(vim.api.nvim_create_namespace('test'), b, {
  { lnum = 0, col = 5, end_lnum = 0, end_col = 6, severity = vim.diagnostic.severity.ERROR, message = 'message' }
})
if not loaded then
  vim.api.nvim_buf_delete(b, { unload = true, force = true })
end
"#;

fn answer(loaded: bool) -> serde_json::Value {
    let scratch = Scratch::new();
    let path = scratch.0.join("a b%20c.py");
    let rpc = Rpc(RefCell::new(Embed::start(&scratch.0, &[], &[])));
    rpc.0
        .borrow_mut()
        .lua(PREPARE, vec![Value::from(path.to_str().unwrap()), Value::from(loaded)]);
    let stamp = Sequencer::new("ide-1".into()).stamp(EDITOR_BUDGET);
    let pending = diagnostics::send(
        &rpc,
        &stamp,
        &DiagnosticsArgs {
            root: scratch.0.clone(),
            path: Some(path.clone()),
            min_severity: Severity::Hint,
            utf16: true,
            limit: 500,
        },
    );
    let parsed = diagnostics::parse(&pending.try_take().unwrap().unwrap()).unwrap();
    let uri = format!("file://{}", path.display());
    let entries = parsed
        .entries
        .into_iter()
        .map(|entry| (uri.clone(), entry))
        .collect::<Vec<_>>();
    let text = shape(&entries);
    serde_json::from_str(&text).unwrap()
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn byte_columns_become_utf16_units() {
    let answer = answer(true);
    assert_eq!(
        answer[0]["diagnostics"][0]["range"],
        serde_json::json!({
            "start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 4}
        })
    );
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn an_unloaded_buffer_answers_an_empty_list() {
    assert_eq!(answer(false), serde_json::json!([]));
}
