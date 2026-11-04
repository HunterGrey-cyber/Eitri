//! What a terminal needs to move to another process with its screen and parser intact: where the parser stands,
//! shadows of the state alacritty keeps without a getter, handler wrappers that count or swallow calls, and the
//! bytes that draw a grid's cells again in a fresh `Term`.

pub mod boundary;
pub mod cells;
pub mod gate;
pub mod screen_replay;
pub mod shadow;
