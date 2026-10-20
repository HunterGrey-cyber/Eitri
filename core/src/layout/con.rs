//! A container lays out an ordered list of children, each a module or another container.
//! Equality compares layout and children, ignoring container ids. Build containers only through
//! `Con::new`. Operations move or mutate surviving containers and preserve their ids; only a
//! container that did not exist before receives a fresh id.

use super::{
    module::ModuleId,
    tree::{Axis, Branch, MIN_RATIO},
};
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConId(u32);
impl ConId {
    pub fn fresh() -> ConId {
        static NEXT: AtomicU32 = AtomicU32::new(1);
        ConId(NEXT.fetch_add(1, Ordering::Relaxed))
    }
    pub fn get(self) -> u32 {
        self.0
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConLayout {
    SplitH,
    SplitV,
    Tabbed,
    Stacked,
}
impl ConLayout {
    pub fn axis(self) -> Option<Axis> {
        match self {
            Self::SplitH => Some(Axis::Row),
            Self::SplitV => Some(Axis::Column),
            Self::Tabbed | Self::Stacked => None,
        }
    }
    pub fn of_axis(axis: Axis) -> ConLayout {
        match axis {
            Axis::Row => Self::SplitH,
            Axis::Column => Self::SplitV,
        }
    }
}
/// At the minimum share, a split can hold at most twenty children.
pub const MAX_CHILDREN: usize = 20;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pin {
    pub px: Option<i32>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Child {
    pub node: Node,
    pub share: f32,
    pub pin: Option<Pin>,
}
impl Child {
    pub fn new(node: Node, share: f32) -> Child {
        Child { node, share, pin: None }
    }
}
#[derive(Debug, Clone)]
pub struct Con {
    pub id: ConId,
    pub layout: ConLayout,
    pub last_split: Axis,
    pub children: Vec<Child>,
    pub focus: usize,
}
impl PartialEq for Con {
    fn eq(&self, other: &Self) -> bool {
        self.layout == other.layout
            && self.children == other.children
            && (self.layout.axis().is_some() || (self.focus == other.focus && self.last_split == other.last_split))
    }
}
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Leaf(ModuleId),
    Con(Con),
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConFault {
    Share(f32),
    ShareSum(f32),
    Empty,
    OneChild,
    Focus(usize),
    Pin(i32),
    TooMany(usize),
}
impl Node {
    pub fn split(axis: Axis, ratio: f32, first: Node, second: Node) -> Node {
        Node::Con(Con::new(
            ConLayout::of_axis(axis),
            vec![Child::new(first, ratio), Child::new(second, 1.0 - ratio)],
        ))
    }
    pub fn pinned(axis: Axis, ratio: f32, side: Branch, first: Node, second: Node) -> Node {
        let Node::Con(mut c) = Self::split(axis, ratio, first, second) else {
            unreachable!()
        };
        c.children[if side == Branch::First { 0 } else { 1 }].pin = Some(Pin { px: None });
        Node::Con(c)
    }
    pub fn leaves(&self) -> Vec<ModuleId> {
        match self {
            Self::Leaf(id) => vec![id.clone()],
            Self::Con(c) => c.leaves(),
        }
    }
    pub fn as_con(&self) -> Option<&Con> {
        match self {
            Self::Con(c) => Some(c),
            _ => None,
        }
    }
    pub(crate) fn focused_leaf(&self, shown: &dyn Fn(&ModuleId) -> bool) -> Option<ModuleId> {
        match self {
            Node::Leaf(id) => shown(id).then(|| id.clone()),
            Node::Con(c) => c.focused_leaf(shown),
        }
    }
    pub(crate) fn without(self, id: &ModuleId) -> Option<Node> {
        match self {
            Self::Leaf(leaf) => (leaf != *id).then_some(Self::Leaf(leaf)),
            Self::Con(mut c) => {
                if !c.leaves().contains(id) {
                    return Some(Self::Con(c));
                }
                let old_len = c.children.len();
                let focus = c.focus;
                let mut before = 0;
                c.children = c
                    .children
                    .into_iter()
                    .enumerate()
                    .filter_map(|(i, mut ch)| {
                        ch.node = ch.node.without(id)?;
                        if i < focus {
                            before += 1;
                        }
                        Some(ch)
                    })
                    .collect();
                match c.children.len() {
                    0 => None,
                    1 if c.layout.axis().is_some() => Some(c.children.pop().unwrap().node),
                    _ => {
                        if c.children.len() != old_len {
                            c.rescale();
                        }
                        c.focus = before % c.children.len();
                        Some(Self::Con(c))
                    }
                }
            }
        }
    }
    pub(crate) fn swap_leaves(&mut self, a: &ModuleId, b: &ModuleId) {
        self.map_leaves(&|id| {
            if id == a {
                b.clone()
            } else if id == b {
                a.clone()
            } else {
                id.clone()
            }
        });
    }
    pub(crate) fn map_leaves(&mut self, f: &dyn Fn(&ModuleId) -> ModuleId) {
        match self {
            Self::Leaf(id) => *id = f(id),
            Self::Con(c) => {
                for ch in &mut c.children {
                    ch.node.map_leaves(f);
                }
            }
        }
    }
    pub(crate) fn replace_leaf(&mut self, id: &ModuleId, with: impl FnOnce(Node) -> Node) -> bool {
        match self {
            Self::Leaf(leaf) if leaf == id => {
                let old = std::mem::replace(self, Self::Leaf(id.clone()));
                *self = with(old);
                true
            }
            Self::Leaf(_) => false,
            Self::Con(c) => {
                if let Some(ch) = c.children.iter_mut().find(|ch| ch.node.leaves().contains(id)) {
                    ch.node.replace_leaf(id, with)
                } else {
                    false
                }
            }
        }
    }
}
impl Con {
    pub fn new(layout: ConLayout, children: Vec<Child>) -> Con {
        Con {
            id: ConId::fresh(),
            layout,
            last_split: layout.axis().unwrap_or(Axis::Row),
            children,
            focus: 0,
        }
    }
    /// Prefer the focused child, falling forward with wrapping when it has nothing shown.
    pub fn drawn(&self, shown: &dyn Fn(&ModuleId) -> bool) -> Option<usize> {
        (0..self.children.len())
            .map(|offset| (self.focus + offset) % self.children.len())
            .find(|&i| self.children[i].node.leaves().iter().any(shown))
    }

    /// Follow the remembered child to a shown leaf, so a strip names the module that receives focus.
    pub fn focused_leaf(&self, shown: &dyn Fn(&ModuleId) -> bool) -> Option<ModuleId> {
        self.children.get(self.drawn(shown)?)?.node.focused_leaf(shown)
    }

    pub(crate) fn focus_leaf(&mut self, id: &ModuleId) {
        if let Some(i) = self.children.iter().position(|ch| ch.node.leaves().contains(id)) {
            self.focus = i;
            if let Node::Con(c) = &mut self.children[i].node {
                c.focus_leaf(id);
            }
        }
    }

    pub fn leaves(&self) -> Vec<ModuleId> {
        self.children.iter().flat_map(|ch| ch.node.leaves()).collect()
    }
    pub fn find(&self, id: ConId) -> Option<&Con> {
        if self.id == id {
            return Some(self);
        }
        self.children
            .iter()
            .filter_map(|ch| ch.node.as_con())
            .find_map(|c| c.find(id))
    }
    pub fn find_mut(&mut self, id: ConId) -> Option<&mut Con> {
        if self.id == id {
            return Some(self);
        }
        self.children.iter_mut().find_map(|ch| match &mut ch.node {
            Node::Con(c) => c.find_mut(id),
            _ => None,
        })
    }
    pub fn ids(&self) -> Vec<ConId> {
        let mut out = vec![self.id];
        for ch in &self.children {
            if let Node::Con(c) = &ch.node {
                out.extend(c.ids());
            }
        }
        out
    }
    pub fn check(&self, is_root: bool) -> Result<(), ConFault> {
        let n = self.children.len();
        if n == 0 {
            return Err(ConFault::Empty);
        }
        if self.focus >= n {
            return Err(ConFault::Focus(self.focus));
        }
        if n == 1 && !is_root && self.layout.axis().is_some() {
            return Err(ConFault::OneChild);
        }
        if n > MAX_CHILDREN {
            return Err(ConFault::TooMany(n));
        }
        if n >= 2 {
            for ch in &self.children {
                if !(MIN_RATIO - 1e-6..=1.0).contains(&ch.share) {
                    return Err(ConFault::Share(ch.share));
                }
            }
        }
        let sum: f32 = self.children.iter().map(|ch| ch.share).sum();
        if !sum.is_finite() || (sum - 1.0).abs() > 1e-4 {
            return Err(ConFault::ShareSum(sum));
        }
        for ch in &self.children {
            if let Some(Pin { px: Some(px) }) = ch.pin {
                if px < 0 {
                    return Err(ConFault::Pin(px));
                }
            }
        }
        for ch in &self.children {
            if let Node::Con(c) = &ch.node {
                c.check(false)?;
            }
        }
        Ok(())
    }
    fn rescale(&mut self) {
        let sum: f32 = self.children.iter().map(|ch| ch.share).sum();
        for ch in &mut self.children {
            ch.share /= sum;
        }
    }
    pub(crate) fn normalise(&mut self) {
        let old_len = self.children.len();
        let focus = self.focus;
        let mut before = 0;
        self.children = std::mem::take(&mut self.children)
            .into_iter()
            .enumerate()
            .filter_map(|(i, mut ch)| {
                if let Node::Con(c) = &mut ch.node {
                    c.normalise();
                    match c.children.len() {
                        0 => return None,
                        1 if c.layout.axis().is_some() => ch.node = c.children.pop().unwrap().node,
                        _ => {}
                    }
                }
                if i < focus {
                    before += 1;
                }
                Some(ch)
            })
            .collect();
        self.focus = if self.children.is_empty() {
            0
        } else {
            before % self.children.len()
        };
        if self.children.len() != old_len && !self.children.is_empty() {
            self.rescale();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(s: &str) -> Node {
        Node::Leaf(ModuleId::parse(s).unwrap())
    }
    fn shares(c: &Con) -> Vec<f32> {
        c.children.iter().map(|ch| ch.share).collect()
    }

    #[test]
    fn split_and_pinned_build_two_child_containers() {
        let Node::Con(c) = Node::split(Axis::Row, 0.6, leaf("editor"), leaf("agent")) else {
            panic!("a con")
        };
        assert_eq!(c.layout, ConLayout::SplitH);
        assert_eq!(shares(&c), vec![0.6, 1.0 - 0.6]);
        assert!(c.children.iter().all(|ch| ch.pin.is_none()));
        let Node::Con(c) = Node::pinned(
            Axis::Column,
            2.0 / 3.0,
            Branch::Second,
            leaf("editor"),
            leaf("terminal"),
        ) else {
            panic!("a con")
        };
        assert_eq!(c.layout, ConLayout::SplitV);
        assert_eq!((c.children[0].pin, c.children[1].pin), (None, Some(Pin { px: None })));
        assert_eq!(c.leaves(), vec![ModuleId::editor(), ModuleId::terminal()]);
    }

    #[test]
    fn equality_ignores_container_ids_and_each_container_gets_a_fresh_one() {
        let x = Node::split(Axis::Row, 0.5, leaf("editor"), leaf("agent"));
        let y = Node::split(Axis::Row, 0.5, leaf("editor"), leaf("agent"));
        assert_eq!(x, y);
        assert_ne!(x.as_con().unwrap().id, y.as_con().unwrap().id);
    }

    #[test]
    fn structural_operations_keep_the_ids_of_kept_containers() {
        let inner = Node::split(Axis::Column, 0.5, leaf("editor"), leaf("terminal"));
        let root = Con::new(
            ConLayout::SplitH,
            vec![
                Child::new(inner, 0.5),
                Child::new(leaf("agent"), 0.25),
                Child::new(leaf("lua:a"), 0.25),
            ],
        );
        let ids = root.ids();
        let removed = Node::Con(root.clone()).without(&ModuleId::terminal()).unwrap();
        assert_eq!(removed.as_con().unwrap().ids(), vec![ids[0]]);
        let mut kept = Node::Con(root.clone()).without(&ModuleId::lua("a")).unwrap();
        assert_eq!(kept.as_con().unwrap().ids(), ids);
        kept.swap_leaves(&ModuleId::editor(), &ModuleId::agent());
        assert_eq!(kept.as_con().unwrap().ids(), ids);
        kept.map_leaves(&|id| {
            if *id == ModuleId::terminal() {
                ModuleId::lua("b")
            } else {
                id.clone()
            }
        });
        assert_eq!(kept.as_con().unwrap().ids(), ids);
        assert!(kept.replace_leaf(&ModuleId::lua("b"), |old| Node::split(
            Axis::Row,
            0.5,
            old,
            leaf("lua:c")
        )));
        let new_ids = kept.as_con().unwrap().ids();
        assert_eq!(&new_ids[..2], &ids);
        assert!(!ids.contains(&new_ids[2]));
        let Node::Con(mut kept) = kept else { unreachable!() };
        kept.normalise();
        assert_eq!(kept.ids(), new_ids);
        assert_eq!(
            Node::Con(root.clone()).without(&ModuleId::lua("absent")),
            Some(Node::Con(root))
        );
    }

    #[test]
    fn without_gives_the_survivor_the_containers_slot() {
        let inner = Node::pinned(Axis::Column, 0.6, Branch::Second, leaf("editor"), leaf("terminal"));
        let root = Node::pinned(Axis::Row, 0.7, Branch::First, inner, leaf("agent"));
        assert_eq!(
            root.without(&ModuleId::terminal()).unwrap(),
            Node::pinned(Axis::Row, 0.7, Branch::First, leaf("editor"), leaf("agent"))
        );
        assert_eq!(leaf("editor").without(&ModuleId::editor()), None);
    }

    #[test]
    fn without_in_a_wider_container_rescales_the_rest() {
        let c = Con::new(
            ConLayout::SplitH,
            vec![
                Child::new(leaf("editor"), 0.5),
                Child::new(leaf("agent"), 0.25),
                Child::new(leaf("terminal"), 0.25),
            ],
        );
        let Node::Con(c) = Node::Con(c).without(&ModuleId::agent()).unwrap() else {
            panic!("a con")
        };
        let s = shares(&c);
        assert!(
            (s[0] - 2.0 / 3.0).abs() < 1e-6 && (s[1] - 1.0 / 3.0).abs() < 1e-6,
            "{s:?}"
        );
    }

    #[test]
    fn normalise_dissolves_one_child_containers_keeps_the_root_and_never_merges() {
        let lonely = Con::new(ConLayout::SplitV, vec![Child::new(leaf("agent"), 1.0)]);
        let same_axis = Con::new(
            ConLayout::SplitH,
            vec![Child::new(leaf("lua:a"), 0.5), Child::new(leaf("lua:b"), 0.5)],
        );
        let mut root = Con::new(
            ConLayout::SplitH,
            vec![
                Child::new(leaf("editor"), 0.4),
                Child::new(Node::Con(lonely), 0.3),
                Child::new(Node::Con(same_axis), 0.3),
            ],
        );
        root.normalise();
        assert_eq!(root.children[1].node, leaf("agent"));
        assert_eq!(root.children[1].share, 0.3, "the survivor keeps the slot");
        assert!(
            matches!(root.children[2].node, Node::Con(_)),
            "same-orientation nesting is kept"
        );
        let mut alone = Con::new(ConLayout::SplitH, vec![Child::new(leaf("editor"), 1.0)]);
        alone.normalise();
        assert_eq!(alone.children.len(), 1, "a root keeps its one child");
    }

    #[test]
    fn check_refuses_what_breaks_an_invariant() {
        let two = |a: f32, b: f32| {
            Con::new(
                ConLayout::SplitH,
                vec![Child::new(leaf("editor"), a), Child::new(leaf("agent"), b)],
            )
        };
        assert_eq!(two(0.6, 0.4).check(true), Ok(()));
        assert!(matches!(two(0.6, 0.3).check(true), Err(ConFault::ShareSum(_))));
        assert_eq!(two(0.96, 0.04).check(true), Err(ConFault::Share(0.04)));
        let mut pinned = two(0.5, 0.5);
        pinned.children[1].pin = Some(Pin { px: Some(-1) });
        assert_eq!(pinned.check(true), Err(ConFault::Pin(-1)));
        assert_eq!(Con::new(ConLayout::SplitV, vec![]).check(true), Err(ConFault::Empty));
        let one = Con::new(ConLayout::SplitV, vec![Child::new(leaf("agent"), 1.0)]);
        assert_eq!(one.check(true), Ok(()));
        let nested = Con::new(
            ConLayout::SplitH,
            vec![Child::new(leaf("editor"), 0.5), Child::new(Node::Con(one), 0.5)],
        );
        assert_eq!(nested.check(true), Err(ConFault::OneChild));
        let many = Con::new(
            ConLayout::SplitH,
            (0..21)
                .map(|i| Child::new(leaf(&format!("lua:m{i}")), 1.0 / 21.0))
                .collect(),
        );
        assert_eq!(many.check(true), Err(ConFault::TooMany(21)));
    }
}
