//! Instance numbers across the two windows of a pair. `terminal#2`..`#9` and `agent#2`..`#9` are one number space
//! for the pair: a window allocates only numbers the other window does not hold, so a moved module keeps its id
//! unless both windows allocated that number in the same moment.

use std::fmt;

use crate::layout::{ModuleId, ModuleKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoFreeNumber {
    pub kind: ModuleKind,
}

impl fmt::Display for NoFreeNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ModuleKind::Agent => write!(f, "no free chat view number: the two windows share agent#2 to agent#9"),
            _ => write!(
                f,
                "no free terminal number: the two windows share terminal#2 to terminal#9"
            ),
        }
    }
}

impl std::error::Error for NoFreeNumber {}

/// The lowest number of `kind` neither window uses.
pub fn new_instance(kind: ModuleKind, here: &[ModuleId], other_held: &[ModuleId]) -> Result<ModuleId, NoFreeNumber> {
    let taken: Vec<ModuleId> = here.iter().chain(other_held).cloned().collect();
    ModuleId::lowest_free(kind, &taken).ok_or(NoFreeNumber { kind })
}

/// The id a module arriving from the other window takes here: its own when no live module here and nothing else the
/// other window holds uses it, else the lowest free number of its kind. A module without instances (a plugin page,
/// a Lua panel) keeps its id: its placeholder here is its place.
pub fn arrival_id(
    moving: &ModuleId,
    here_live: &[ModuleId],
    other_held: &[ModuleId],
) -> Result<ModuleId, NoFreeNumber> {
    let kind = moving.kind();
    if !kind.has_instances() {
        return Ok(moving.clone());
    }
    let others: Vec<ModuleId> = other_held.iter().filter(|id| *id != moving).cloned().collect();
    if !here_live.contains(moving) && !others.contains(moving) {
        return Ok(moving.clone());
    }
    new_instance(kind, here_live, &others)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> ModuleId {
        ModuleId::parse(s).unwrap()
    }

    #[test]
    fn a_new_instance_skips_the_numbers_the_other_window_holds() {
        let here = [id("terminal"), id("terminal#2")];
        let other = [id("terminal#3"), id("agent#2")];
        assert_eq!(
            new_instance(ModuleKind::Terminal, &here, &other).unwrap(),
            id("terminal#4")
        );
        assert_eq!(new_instance(ModuleKind::Agent, &here, &other).unwrap(), id("agent#3"));
    }

    #[test]
    fn the_pair_has_eight_extra_numbers_of_each_kind_together() {
        let here: Vec<ModuleId> = (2..=5)
            .map(|n| ModuleId::instance(ModuleKind::Terminal, n).unwrap())
            .collect();
        let other: Vec<ModuleId> = (6..=9)
            .map(|n| ModuleId::instance(ModuleKind::Terminal, n).unwrap())
            .collect();
        let err = new_instance(ModuleKind::Terminal, &here, &other).unwrap_err();
        assert_eq!(
            err.to_string(),
            "no free terminal number: the two windows share terminal#2 to terminal#9"
        );
        let err = NoFreeNumber {
            kind: ModuleKind::Agent,
        };
        assert_eq!(
            err.to_string(),
            "no free chat view number: the two windows share agent#2 to agent#9"
        );
    }

    #[test]
    fn an_arriving_module_keeps_its_id_and_its_own_number_is_never_reserved_against_it() {
        let moving = id("terminal#3");
        assert_eq!(
            arrival_id(&moving, &[id("terminal")], &[moving.clone()]).unwrap(),
            moving
        );
        assert_eq!(
            arrival_id(
                &moving,
                &[id("terminal"), id("terminal#3")],
                &[moving.clone(), id("terminal#2")]
            )
            .unwrap(),
            id("terminal#4"),
            "a live terminal#3 here takes the number: the lowest free that neither window holds"
        );
        assert_eq!(
            arrival_id(&id("terminal"), &[id("terminal")], &[]).unwrap(),
            id("terminal#2")
        );
        let page = id("lua:snotra");
        assert_eq!(arrival_id(&page, &[page.clone()], &[page.clone()]).unwrap(), page);
    }
}
