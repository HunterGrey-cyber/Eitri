mod headless_common;
use eitri_terminal::{Screen, TerminalColors};
use headless_common::*;

#[test]
fn fifty_snapshots_in_a_row_lose_nothing() {
    for bytes in corpus() {
        let mut original = Screen::new(size(80, 24), TerminalColors::default());
        original.feed(&bytes);
        let before = original.render(true);
        let mode = original.mode();
        let mut replay = canonical(&mut original);
        let mut stable = None;
        for _ in 0..50 {
            let mut rebuilt = Screen::replayed(&replay, size(80, 24), TerminalColors::default());
            assert_eq!(rebuilt.render(true), before);
            assert_eq!(rebuilt.mode(), mode);
            replay = canonical(&mut rebuilt);
            if let Some(previous) = stable {
                assert_eq!(replay, previous);
            }
            stable = Some(replay.clone());
        }
    }
}
