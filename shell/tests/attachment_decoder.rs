//! The product binary is the composer images' decoder: `shell attachment-decoder` serves one request in a child
//! started with an empty environment, before `main` reads anything else, and its result is checked as any
//! decoder child's is (`eitri_core::attachments::isolate`).
//!
//! A plain `main` (`harness = false`): `own_display_scan` requires a test that starts the product binary to isolate its
//! display first, so the binary runs against this test's own `Xvfb` and no session bus, although the decoder is given
//! no environment and opens neither. With `EITRI_MEASURE_DECODER=1` it also prints what re-executing the whole binary
//! costs per image: the median of 20 decodes of a 1x1 PNG, whose own decoding is negligible, so the figure is the
//! child's start and exit. Nothing is asserted about time.

#[path = "support/own_x_server.rs"]
mod own_x_server;

use eitri_core::attachments::{normalize_in_child, DecoderCommand, SUBCOMMAND};
use std::time::{Duration, Instant};

/// A 1x1 PNG with an alpha channel: kept byte for byte.
const PNG: [u8; 70] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49,
    0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0xf8, 0xcf, 0x50, 0x0f, 0x00, 0x03, 0x86, 0x01, 0x80, 0x5a, 0x34, 0x7d,
    0x6b, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

fn shell_decoder() -> DecoderCommand {
    DecoderCommand::new(env!("CARGO_BIN_EXE_shell"), [SUBCOMMAND])
}

fn the_shell_binary_serves_the_decoder() {
    let normalized = normalize_in_child(&PNG, 3_670_016, &shell_decoder()).expect("decoded by the shell binary");
    assert_eq!((normalized.width, normalized.height), (1, 1));
    assert_eq!(normalized.bytes, PNG, "a small PNG is kept as it came");
    assert!(!normalized.thumbnail_png.is_empty());
}

fn measure_the_per_image_cost_of_the_shell_binary() {
    let decoder = shell_decoder();
    let mut runs: Vec<Duration> = (0..20)
        .map(|_| {
            let started = Instant::now();
            normalize_in_child(&PNG, 3_670_016, &decoder).expect("decoded");
            started.elapsed()
        })
        .collect();
    runs.sort();
    println!(
        "shell attachment-decoder per image: median {:?}, min {:?}, max {:?} over {} runs",
        runs[runs.len() / 2],
        runs[0],
        runs[runs.len() - 1],
        runs.len()
    );
}

fn main() {
    let _server = own_x_server::isolate("attachment_decoder", "640x480x24");
    the_shell_binary_serves_the_decoder();
    println!("test the_shell_binary_serves_the_decoder ... ok");
    if std::env::var_os("EITRI_MEASURE_DECODER").is_some_and(|v| v == "1") {
        measure_the_per_image_cost_of_the_shell_binary();
    }
}
