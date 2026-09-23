//! Vendored Alacritty encoder core. See `build_sequence.rs` for provenance and
//! the Apache-2.0 section 4(b) notice of modification.

#[rustfmt::skip]
pub mod build_sequence;

pub use build_sequence::{
    build_sequence_pub as build_sequence, is_modifier_key_pub as is_modifier_key,
    should_build_sequence_pub as should_build_sequence,
};
