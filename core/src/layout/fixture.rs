//! A test-only builder that writes a layout as text, so the verb tests build every layout one way and
//! assert rectangles, never the tree's own shape.
//!
//! `node := id attrs | ("row" | "column") "(" node ("," node)+ ")" attrs`, where `id` is anything
//! `ModuleId::parse` takes (`editor`, `lua:notes`). After a node, in any order:
//! - a number with a `.` (`0.6`): the ratio of the split between this child and the children after
//!   it, so with two children the first child's share. Not on a list's last child. A child without
//!   one takes `1 / (children left)` clamped into `MIN_RATIO..=MAX_RATIO`, the chain `even` builds.
//! - `pinned`, optionally followed by a whole number of pixels: the split beside this child keeps
//!   this child's length. On any child but the last it pins the `First` side of that child's own
//!   split; on the last child, the `Second` side of the list's last split.
//! - `hidden` (a module only): the module is hidden.
//! - `focus` (a module only): the module holds the keys. Without one, the first module in tree order
//!   that is not hidden does.

use std::collections::BTreeSet;

use super::module::ModuleId;
use super::tree::{Axis, Branch, Layout, Node, Pin, MAX_RATIO, MIN_RATIO};

struct Child {
    node: Node,
    ratio: Option<f32>,
    pin: Option<Option<i32>>,
}

struct Parser<'a> {
    spec: &'a str,
    tokens: Vec<&'a str>,
    at: usize,
    hidden: BTreeSet<ModuleId>,
    focus: Option<ModuleId>,
}

impl<'a> Parser<'a> {
    fn fail(&self, reason: &str) -> ! {
        panic!("fixture: {reason}: {:?}", self.spec)
    }

    fn peek(&self) -> Option<&'a str> {
        self.tokens.get(self.at).copied()
    }

    fn take(&mut self) -> &str {
        let token = self
            .tokens
            .get(self.at)
            .copied()
            .unwrap_or_else(|| self.fail("expected a token"));
        self.at += 1;
        token
    }

    fn node(&mut self) -> Child {
        let token = self.take().to_string();
        let node = if token == "row" || token == "column" {
            let axis = if token == "row" { Axis::Row } else { Axis::Column };
            if self.take() != "(" {
                self.fail("expected (");
            }
            let mut children = vec![self.node()];
            while self.peek() == Some(",") {
                self.take();
                children.push(self.node());
            }
            if self.take() != ")" {
                self.fail("expected )");
            }
            if children.len() < 2 {
                self.fail("a list needs two children");
            }
            let last = children.pop().unwrap();
            if last.ratio.is_some() {
                self.fail("ratio on last child");
            }
            let mut tail_pin = last.pin;
            let mut node = last.node;
            let count = children.len() + 1;
            for (i, child) in children.into_iter().enumerate().rev() {
                if child.pin.is_some() && tail_pin.is_some() {
                    self.fail("both sides pinned");
                }
                let pin = child
                    .pin
                    .map(|px| Pin {
                        side: Branch::First,
                        px,
                    })
                    .or_else(|| {
                        tail_pin.take().map(|px| Pin {
                            side: Branch::Second,
                            px,
                        })
                    });
                node = Node::Split {
                    axis,
                    ratio: child
                        .ratio
                        .unwrap_or_else(|| (1.0 / (count - i) as f32).clamp(MIN_RATIO, MAX_RATIO)),
                    pin,
                    first: Box::new(child.node),
                    second: Box::new(node),
                };
            }
            node
        } else {
            Node::Leaf(ModuleId::parse(&token).unwrap_or_else(|_| self.fail("unknown module")))
        };
        let mut child = Child {
            node,
            ratio: None,
            pin: None,
        };
        while let Some(token) = self.peek() {
            match token {
                "," | ")" => break,
                "hidden" | "focus" => {
                    self.at += 1;
                    let Node::Leaf(id) = &child.node else {
                        self.fail("hidden or focus on split")
                    };
                    if token == "hidden" {
                        self.hidden.insert(id.clone());
                    } else if self.focus.replace(id.clone()).is_some() {
                        self.fail("two focused modules");
                    }
                }
                "pinned" => {
                    self.at += 1;
                    if child.pin.is_some() {
                        self.fail("duplicate pin");
                    }
                    let px = self.peek().and_then(|t| t.parse::<i32>().ok());
                    if px.is_some() {
                        self.at += 1;
                    }
                    child.pin = Some(px);
                }
                _ if token.contains('.') => {
                    let ratio = token.parse::<f32>().unwrap_or_else(|_| self.fail("unknown token"));
                    if !(0.0 < ratio && ratio < 1.0) || child.ratio.is_some() {
                        self.fail("invalid ratio");
                    }
                    self.at += 1;
                    child.ratio = Some(ratio);
                }
                _ => self.fail("unknown token"),
            }
        }
        child
    }
}

pub(crate) fn fixture(spec: &str) -> Layout {
    let mut tokens = Vec::new();
    let mut start = 0;
    for (i, c) in spec.char_indices() {
        if c.is_whitespace() || matches!(c, '(' | ')' | ',') {
            if start < i {
                tokens.push(&spec[start..i]);
            }
            if !c.is_whitespace() {
                tokens.push(&spec[i..i + 1]);
            }
            start = i + c.len_utf8();
        }
    }
    if start < spec.len() {
        tokens.push(&spec[start..]);
    }
    let mut parser = Parser {
        spec,
        tokens,
        at: 0,
        hidden: BTreeSet::new(),
        focus: None,
    };
    let child = parser.node();
    if child.pin.is_some() || child.ratio.is_some() {
        parser.fail("pin or ratio on root");
    }
    if parser.peek().is_some() {
        parser.fail("trailing token");
    }
    let focus = parser
        .focus
        .clone()
        .or_else(|| child.node.leaves().into_iter().find(|id| !parser.hidden.contains(id)))
        .unwrap_or_else(|| parser.fail("nothing shown"));
    Layout::from_parts(child.node, parser.hidden, focus).unwrap_or_else(|error| panic!("fixture: {error}: {spec:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::tree::{Axis, Branch, Node, Pin};

    fn leaf(s: &str) -> Node {
        Node::Leaf(ModuleId::parse(s).unwrap())
    }

    #[test]
    fn a_two_child_row_is_one_split_at_the_given_ratio() {
        let layout = fixture("row(editor 0.6, agent)");
        assert_eq!(
            layout.root(),
            &Node::split(Axis::Row, 0.6, leaf("editor"), leaf("agent"))
        );
        assert_eq!(layout.focus(), &ModuleId::editor());
        assert!(layout.hidden().is_empty());
    }

    #[test]
    fn three_children_chain_to_the_right_and_default_to_even() {
        let layout = fixture("column(editor, agent, terminal)");
        assert_eq!(
            layout.root(),
            &Node::split(
                Axis::Column,
                1.0 / 3.0,
                leaf("editor"),
                Node::split(Axis::Column, 0.5, leaf("agent"), leaf("terminal"))
            )
        );
    }

    #[test]
    fn pinned_hidden_and_focus_attach_where_they_are_written() {
        let layout = fixture("row(column(editor 0.6666667, terminal hidden pinned) 0.6, agent focus)");
        let Node::Split { ratio, first, .. } = layout.root() else {
            panic!("a split")
        };
        assert_eq!(*ratio, 0.6);
        assert_eq!(
            **first,
            Node::pinned(
                Axis::Column,
                0.6666667,
                Branch::Second,
                leaf("editor"),
                leaf("terminal")
            )
        );
        assert!(layout.hidden().contains(&ModuleId::terminal()));
        assert_eq!(layout.focus(), &ModuleId::agent());
        let layout = fixture("column(terminal pinned 240, lua:notes)");
        assert_eq!(
            layout.root(),
            &Node::Split {
                axis: Axis::Column,
                ratio: 0.5,
                pin: Some(Pin {
                    side: Branch::First,
                    px: Some(240)
                }),
                first: Box::new(leaf("terminal")),
                second: Box::new(leaf("lua:notes")),
            }
        );
        assert_eq!(fixture("editor").root(), &leaf("editor"));
    }

    #[test]
    #[should_panic(expected = "fixture: ")]
    fn a_ratio_on_the_last_child_is_refused() {
        fixture("row(editor, agent 0.5)");
    }

    #[test]
    #[should_panic(expected = "fixture: ")]
    fn hidden_on_a_split_is_refused() {
        fixture("row(column(editor, terminal) hidden, agent)");
    }
}
