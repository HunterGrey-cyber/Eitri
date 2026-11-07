//! `serve`'s wake-ups: which are due ([`rules`], pure, with caller-supplied time and device context), and the
//! sandboxed process that posts them ([`helper`], through [`post`]).

#[cfg(target_os = "linux")]
pub mod helper;
#[cfg(target_os = "linux")]
pub mod post;
pub mod rules;

pub use rules::{Ctx, Target, Wake, WakeRules, DESK_ACTIVE_HOLD, FINISHED_COALESCE, NEEDS_YOU_COALESCE};
