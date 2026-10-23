use crate::layout::{Layout, ModuleId, ModuleKind};

/// Choose the lowest instance number absent from every layout and every held or reserved id.
pub fn plan_new_instance(
    kind: ModuleKind,
    here: &[&Layout],
    reserved: &[ModuleId],
    other_held: &[ModuleId],
) -> Result<ModuleId, String> {
    let taken: Vec<_> = here
        .iter()
        .flat_map(|layout| layout.modules())
        .chain(reserved.iter().cloned())
        .chain(other_held.iter().cloned())
        .collect();
    ModuleId::lowest_free(kind, &taken).ok_or_else(|| match kind {
        ModuleKind::Terminal => "at most 9 terminals".to_string(),
        ModuleKind::Agent => "at most 9 chat views".to_string(),
        _ => format!("no new instance of {kind:?} can be made"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_new_instance_skips_a_reserved_number() {
        use crate::layout::{Layout, ModuleDecl, Placement};
        let id = |text: &str| ModuleId::parse(text).unwrap();
        let below = |ids: &[&str]| -> Layout {
            let decls: Vec<ModuleDecl> = ids
                .iter()
                .map(|text| ModuleDecl {
                    id: id(text),
                    placement: Placement::BelowEditor,
                })
                .collect();
            Layout::initial(&decls).unwrap()
        };
        let layout = below(&["terminal", "terminal#2"]);
        assert_eq!(
            plan_new_instance(ModuleKind::Terminal, &[&layout], &[], &[]).ok(),
            Some(id("terminal#3"))
        );
        assert_eq!(
            plan_new_instance(ModuleKind::Terminal, &[&layout], &[id("terminal#3")], &[]).ok(),
            Some(id("terminal#4")),
            "a reserved number is passed over"
        );
        let other = below(&["terminal", "terminal#4"]);
        assert_eq!(
            plan_new_instance(ModuleKind::Terminal, &[&layout, &other], &[id("terminal#3")], &[]).ok(),
            Some(id("terminal#5")),
            "a number another workspace holds is taken too"
        );
        let full: Vec<String> = (2..=9).map(|n| format!("terminal#{n}")).collect();
        let full: Vec<&str> = std::iter::once("terminal")
            .chain(full.iter().map(String::as_str))
            .collect();
        assert_eq!(
            plan_new_instance(ModuleKind::Terminal, &[&below(&full)], &[], &[]).ok(),
            None,
            "at most 9 terminals"
        );
    }

    #[test]
    fn a_new_chat_view_takes_the_lowest_free_number_not_reserved() {
        use crate::layout::{Layout, ModuleDecl, Placement};
        let id = |text: &str| ModuleId::parse(text).unwrap();
        let below = |ids: &[&str]| -> Layout {
            let decls: Vec<ModuleDecl> = ids
                .iter()
                .map(|text| ModuleDecl {
                    id: id(text),
                    placement: Placement::BelowEditor,
                })
                .collect();
            Layout::initial(&decls).unwrap()
        };
        let current = below(&["terminal"]);
        let other = below(&["terminal", "agent#2"]);
        assert_eq!(
            plan_new_instance(ModuleKind::Agent, &[&current, &other], &[id("agent#3")], &[]).ok(),
            Some(id("agent#4")),
            "agent#2 is held in another workspace and agent#3 is reserved"
        );
        assert_eq!(
            plan_new_instance(ModuleKind::Agent, &[&current], &[], &[]).ok(),
            Some(id("agent#2")),
            "the base view is not a number a new view can take"
        );
        assert_eq!(
            plan_new_instance(ModuleKind::Agent, &[&current, &other], &[id("terminal#3")], &[]).ok(),
            Some(id("agent#3")),
            "a terminal's number reserves no chat view"
        );
        let full: Vec<String> = (2..=9).map(|n| format!("agent#{n}")).collect();
        let full: Vec<&str> = std::iter::once("terminal")
            .chain(full.iter().map(String::as_str))
            .collect();
        assert_eq!(
            plan_new_instance(ModuleKind::Agent, &[&below(&full)], &[], &[]).ok(),
            None,
            "at most 9 chat views"
        );
    }

    #[test]
    fn plan_new_instance_skips_reserved_and_other_held_ids() {
        let id = |kind, n| ModuleId::instance(kind, n).unwrap();
        for kind in [ModuleKind::Terminal, ModuleKind::Agent] {
            let here = Layout::initial(&[]).unwrap();
            assert_eq!(
                plan_new_instance(kind, &[&here], &[id(kind, 2)], &[id(kind, 3)]),
                Ok(id(kind, 4))
            );
        }
    }

    #[test]
    fn the_tenth_instance_is_refused_with_todays_text() {
        for (kind, text) in [
            (ModuleKind::Terminal, "at most 9 terminals"),
            (ModuleKind::Agent, "at most 9 chat views"),
        ] {
            let held: Vec<_> = (2..=9).map(|n| ModuleId::instance(kind, n).unwrap()).collect();
            assert_eq!(plan_new_instance(kind, &[], &[], &held), Err(text.to_string()));
        }
        assert_eq!(
            plan_new_instance(ModuleKind::Editor, &[], &[], &[]),
            Err("no new instance of Editor can be made".to_string())
        );
    }
}
