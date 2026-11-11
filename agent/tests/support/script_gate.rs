#![allow(dead_code)]

use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// A child publishes its position, then stays there until the test releases it.
pub struct ScriptGate(PathBuf);

impl ScriptGate {
    pub fn new() -> Self {
        let path = agent::state_dirs::test_workspace_dir("script-gate");
        Self(path)
    }
    pub fn step(&self) -> Value {
        json!({"gate": self.0})
    }
    pub fn command(&self) -> String {
        format!("gate {}", self.0.display())
    }
    pub fn is_ready(&self) -> bool {
        self.0.join("ready").exists()
    }

    pub fn wait(&self) {
        until(|| self.is_ready());
    }
    pub fn release(&self) {
        if let Err(error) = std::fs::write(self.0.join("release"), b"") {
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        }
    }
}

impl Drop for ScriptGate {
    fn drop(&mut self) {
        // The child removes the directory after observing release, so cleanup cannot hide it.
        self.release();
    }
}

pub fn until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready() {
        assert!(Instant::now() < deadline, "event never arrived");
        std::thread::sleep(Duration::from_millis(2));
    }
}
