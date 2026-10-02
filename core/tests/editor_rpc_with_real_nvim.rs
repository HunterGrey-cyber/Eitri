//! `EditorRpc` over an `NvimLink` to a real `nvim --embed --listen`: values sent as arguments come
//! back as the same bytes and are never run, and an editor that dies under a call answers `Closed`.
//!
//! Every case is `#[ignore]`d: it needs `nvim` on `PATH`, spends no tokens and opens no window.

#[path = "support/embed_client.rs"]
mod embed_client;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eitri_core::editor_rpc::EditorRpc;
use eitri_core::nvim_rpc::{NvimLink, RpcError};
use embed_client::Embed;
use rmpv::Value;

/// Under `std::env::temp_dir()` and short: the socket inside it is capped at 103 bytes.
fn scratch_dir(case: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("erpc-{}-{case}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// An nvim listening on a socket in `dir`, and a link to it. nvim binds the socket a moment before
/// it listens, so the connect is retried until it answers.
fn start(dir: &Path) -> (Embed, NvimLink) {
    std::fs::create_dir_all(dir).unwrap();
    let sock = agent::socket_path::in_dir(dir, "n.sock").unwrap();
    let embed = Embed::start(dir, &["--listen", sock.to_str().unwrap()], &[]);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match NvimLink::connect(&sock, Duration::from_secs(1)) {
            Ok((link, _events)) => return (embed, link),
            Err(e) => {
                assert!(Instant::now() < deadline, "could not connect: {e}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// The bytes of a Lua string nvim sent back: msgpack str for text, bin for anything else.
fn bytes(value: &Value) -> Vec<u8> {
    match value {
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Binary(b) => b.clone(),
        other => panic!("not a string: {other:?}"),
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn args_are_values_not_source() {
    let dir = scratch_dir("args");
    let (mut embed, link) = start(&dir);
    let rpc: &dyn EditorRpc = &link;
    let wait = Duration::from_secs(5);

    let hostile = b"'); vim.cmd('qa!') --".to_vec();
    let echoed = rpc
        .exec_lua("return ...", vec![Value::Binary(hostile.clone())])
        .wait(wait)
        .expect("an answer");
    assert_eq!(bytes(&echoed.expect("a value")), hostile);

    let raw = vec![0xff, b'\r', b'\n', 0];
    let echoed = rpc
        .exec_lua("return ...", vec![Value::Binary(raw.clone())])
        .wait(wait)
        .expect("an answer");
    assert_eq!(bytes(&echoed.expect("a value")), raw, "binary comes back byte for byte");

    assert_eq!(
        rpc.exec_lua("return 1 + 1", vec![]).wait(wait),
        Some(Ok(Value::from(2)))
    );
    assert!(link.is_alive(), "nvim did not quit");
    assert_eq!(embed.eval("1"), Value::from(1));
    assert_eq!(rpc.target(), Some(link.channel_id()));

    drop(embed);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_dead_editor_answers_closed() {
    let dir = scratch_dir("dead");
    let (embed, link) = start(&dir);
    let rpc: &dyn EditorRpc = &link;

    let stuck = rpc.exec_lua("vim.uv.sleep(10000)", vec![]);
    // Only this test's own nvim, through the handle that started it.
    drop(embed);
    assert_eq!(stuck.wait(Duration::from_secs(2)), Some(Err(RpcError::Closed)));

    let deadline = Instant::now() + Duration::from_secs(2);
    while link.is_alive() {
        assert!(Instant::now() < deadline, "the link never saw nvim go");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(rpc.exec_lua("return 1", vec![]).try_take(), Some(Err(RpcError::Closed)));
    assert_eq!(rpc.target(), None, "a dead link is no editor");

    let _ = std::fs::remove_dir_all(&dir);
}
