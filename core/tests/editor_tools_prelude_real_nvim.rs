//! `prelude.lua` in a real `nvim`: the deadline and sequence check that makes a chunk run late harmless.
//!
//! Every case is `#[ignore]`d: it needs `nvim` on `PATH`, spends no tokens and opens no window.

#[path = "support/embed_client.rs"]
mod embed_client;

use std::path::PathBuf;

use eitri_core::editor_tools::stamp::{wall_ms, Stamp};
use eitri_core::editor_tools::tools::CHUNKS;
use embed_client::Embed;
use rmpv::Value;

/// The check, then a body that says it ran.
const STAMPED: &str = concat!(
    include_str!("../src/editor_tools/prelude.lua"),
    "\nreturn { ran = true }"
);

const SCOPES: &str = "
local n = 0
for _ in pairs(_G.__eitri_agent.seq) do n = n + 1 end
return { held = n, counted = _G.__eitri_agent.scopes }";

const FAR: u64 = 60_000;

struct Nvim {
    embed: Embed,
    dir: PathBuf,
}

impl Drop for Nvim {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start(case: &str) -> Nvim {
    let dir = std::env::temp_dir().join(format!("etp-{}-{case}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let embed = Embed::start(&dir, &[], &[]);
    Nvim { embed, dir }
}

fn answer(nvim: &mut Nvim, code: &str, scope: &str, seq: u64, deadline_ms: u64) -> Value {
    let stamp = Stamp {
        deadline_ms,
        scope: scope.to_owned(),
        seq,
    };
    nvim.embed.lua(code, vec![stamp.to_value(), Value::Map(vec![])])
}

fn says(answer: &Value, key: &str) -> bool {
    let map = answer.as_map().expect("one table");
    map.iter()
        .any(|(name, value)| name.as_str() == Some(key) && *value == Value::from(true))
}

/// Whether a chunk stamped so ran its body.
fn runs(nvim: &mut Nvim, scope: &str, seq: u64) -> bool {
    let answer = answer(nvim, STAMPED, scope, seq, wall_ms() + FAR);
    assert_ne!(says(&answer, "ran"), says(&answer, "expired"), "{answer:?}");
    says(&answer, "ran")
}

#[test]
#[ignore = "needs nvim on PATH"]
fn the_scope_table_keeps_64_and_drops_the_oldest() {
    let mut nvim = start("scopes");
    for n in 0..64 {
        assert!(runs(&mut nvim, &format!("scope-{n}"), 5));
    }
    assert!(!runs(&mut nvim, "scope-0", 5), "the first scope is still held");
    assert!(runs(&mut nvim, "scope-64", 5));

    let held = nvim.embed.lua(SCOPES, vec![]);
    let number = |key: &str| {
        let map = held.as_map().unwrap();
        let found = map.iter().find(|(name, _)| name.as_str() == Some(key)).unwrap();
        found.1.as_u64().unwrap()
    };
    assert_eq!((number("held"), number("counted")), (64, 64));

    assert!(
        runs(&mut nvim, "scope-0", 5),
        "the oldest scope went, so its numbers start again"
    );
    assert!(!runs(&mut nvim, "scope-2", 5), "only the oldest went");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_later_scope_does_not_reset_an_earlier_one() {
    let mut nvim = start("later");
    assert!(runs(&mut nvim, "a", 5));
    assert!(runs(&mut nvim, "b", 1));
    assert!(!runs(&mut nvim, "a", 3), "older in its own scope");
    assert!(!runs(&mut nvim, "a", 5), "the same number runs once");
    assert!(runs(&mut nvim, "a", 6));
    assert!(runs(&mut nvim, "b", 2), "the other scope counts on its own");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn the_prelude_answers_expired_past_its_deadline() {
    let mut nvim = start("deadline");
    let past = answer(&mut nvim, STAMPED, "a", 1, wall_ms() - 1);
    assert!(says(&past, "expired"), "{past:?}");
    assert!(!says(&past, "ran"), "{past:?}");
    assert!(runs(&mut nvim, "a", 1), "a call past its deadline used no number");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn every_chunk_answers_expired_ahead_of_its_body() {
    let mut nvim = start("chunks");
    for (n, chunk) in CHUNKS.iter().enumerate() {
        let past = answer(&mut nvim, chunk, "a", n as u64 + 1, wall_ms() - 1);
        assert!(says(&past, "expired"), "{past:?}");
    }
}
