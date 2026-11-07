//! Pure notification decisions with caller-supplied time and device context.

pub mod rules;

pub use rules::{Ctx, Target, Wake, WakeRules, DESK_ACTIVE_HOLD, FINISHED_COALESCE, NEEDS_YOU_COALESCE};
