//! What byte-at-a-time driving costs.
//!
//! `SyncDriver::feed` hands the parser ONE BYTE at a time so that a publication can land at the
//! exact byte that dispatched an ESU (see `SyncDriver::feed`'s docs for why nothing coarser is
//! exact). This measures the price on a Claude-Code-shaped stream. The assertion is deliberately
//! loose -- it exists so a future regression of an order of magnitude cannot pass unnoticed --
//! and the real number is printed with `--nocapture`.

mod common;

use std::time::Instant;

use common::{new_term, BSU, ESU};
use terminal_sync::SyncDriver;

fn tui_like_stream(frames: usize) -> Vec<u8> {
    let mut v = Vec::new();
    for f in 0..frames {
        v.extend_from_slice(BSU);
        for line in 0..24 {
            v.extend_from_slice(format!("\x1b[{};1H\x1b[K", line + 1).as_bytes());
            v.extend_from_slice(b"\x1b[38;5;244m");
            v.extend_from_slice(format!("frame {f:05} line {line:02} ").as_bytes());
            v.extend_from_slice(b"\x1b[0m");
            v.extend_from_slice(&vec![b'x'; 40]);
        }
        v.extend_from_slice(ESU);
    }
    v
}

#[test]
fn byte_at_a_time_throughput() {
    let stream = tui_like_stream(400);
    let mut term = new_term();
    let mut driver = SyncDriver::new();
    let mut publications = 0usize;

    let start = Instant::now();
    for chunk in stream.chunks(4096) {
        driver.feed(&mut term, chunk, |_, _| publications += 1);
    }
    let elapsed = start.elapsed();

    let mib = stream.len() as f64 / (1024.0 * 1024.0);
    let rate = mib / elapsed.as_secs_f64();
    println!(
        "terminal-sync byte-at-a-time: {} bytes ({mib:.2} MiB) in {elapsed:?} => {rate:.1} MiB/s \
         ({publications} publications)",
        stream.len()
    );

    assert_eq!(publications, 400, "one publication per synchronized update");
    assert!(
        rate > 1.0,
        "byte-at-a-time driving collapsed to {rate:.2} MiB/s; if this is real, the driver needs \
         an escape-aware fast path"
    );
}
