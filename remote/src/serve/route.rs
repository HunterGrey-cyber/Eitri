//! Where a phone's frame goes: the agents feed, the Eitri process that owns the tab it names, or nowhere.
//!
//! The owner of a tab is the window its row names (`row.window.instance`), read from the agents directory by one
//! reader for the whole server ([`RowsIndex`]). A row that moves to another process keeps its key; the index then names
//! the new owner and a session following the key subscribes there.

use std::collections::{BTreeMap, BTreeSet};

use eitri_core::agents::{Change, View};
use eitri_core::remote_wire::PhoneInbound;

/// The process each row's key lives in now.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RowsIndex {
    /// Key to the row's id and its window's instance.
    rows: BTreeMap<String, (String, String)>,
}

impl RowsIndex {
    pub fn from_view(view: &View) -> RowsIndex {
        let mut index = RowsIndex::default();
        index.reset(view);
        index
    }

    fn reset(&mut self, view: &View) {
        self.rows = view
            .rows
            .iter()
            .filter(|row| !row.key.is_empty())
            .map(|row| (row.key.clone(), (row.id.clone(), row.window.instance.clone())))
            .collect();
    }

    /// Applies one change of the agents reader; `true` when an owner changed, appeared or went.
    pub fn apply(&mut self, change: &Change) -> bool {
        match change {
            Change::Reset { view } => {
                let before = self.rows.clone();
                self.reset(view);
                before != self.rows
            }
            Change::Row { row } => {
                if row.key.is_empty() {
                    return false;
                }
                let now = (row.id.clone(), row.window.instance.clone());
                self.rows.insert(row.key.clone(), now.clone()) != Some(now)
            }
            Change::Gone { id, key, .. } => {
                // A row that left one process may already have arrived in another under the same key: only the row
                // this id names goes.
                if self.rows.get(key).is_some_and(|(held, _)| held == id) {
                    self.rows.remove(key);
                    true
                } else {
                    false
                }
            }
            Change::Selection { .. } | Change::Round { .. } => false,
        }
    }

    /// The instance that owns `key`, if a row names it.
    pub fn owner(&self, key: &str) -> Option<&str> {
        self.rows.get(key).map(|(_, instance)| instance.as_str())
    }

    /// Every instance some row names.
    pub fn instances(&self) -> BTreeSet<String> {
        self.rows.values().map(|(_, instance)| instance.clone()).collect()
    }
}

/// Where a frame goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// `subscribe_rows`: an agents feed of the session's own.
    Rows,
    /// The `rv-` socket of the process that owns the tab the frame names.
    Owner { instance: String },
    /// The `rv-` socket of the process the frame names (`enroll`).
    Instance(String),
    /// The `rv-` socket of any Eitri process: the errand concerns the device store, which every window shares
    /// (`revoke_self`, `set_push_endpoint`).
    AnyInstance,
    /// Answered by `serve` itself (`ping`).
    Local,
    /// Turned down here, with the reason the phone is told.
    Refused(&'static str),
}

/// A tab no row names.
pub const NO_SUCH_TAB: &str = "no such tab";
/// An `enroll` naming something that is not an instance id.
pub const NO_SUCH_WINDOW: &str = "no such window";
/// A second `hello`.
pub const HELLO_ONCE: &str = "hello is the first frame only";

/// Where `frame` goes, given the rows now. Who may send it is the session's to judge, before or after.
pub fn route(frame: &PhoneInbound, rows: &RowsIndex) -> Route {
    let owner = |key: &str| match rows.owner(key) {
        Some(instance) => Route::Owner {
            instance: instance.to_owned(),
        },
        None => Route::Refused(NO_SUCH_TAB),
    };
    match frame {
        PhoneInbound::Hello { .. } => Route::Refused(HELLO_ONCE),
        PhoneInbound::SubscribeRows => Route::Rows,
        PhoneInbound::Subscribe { key }
        | PhoneInbound::Unsubscribe { key }
        | PhoneInbound::More { key, .. }
        | PhoneInbound::Act { key, .. } => owner(key),
        PhoneInbound::Enroll { instance, .. } => {
            if is_instance(instance) {
                Route::Instance(instance.clone())
            } else {
                Route::Refused(NO_SUCH_WINDOW)
            }
        }
        PhoneInbound::RevokeSelf | PhoneInbound::SetPushEndpoint { .. } => Route::AnyInstance,
        PhoneInbound::Ping => Route::Local,
    }
}

/// Whether `text` is an instance id: 16 lowercase hex digits, the only names an `rv-` socket has.
pub fn is_instance(text: &str) -> bool {
    text.len() == 16 && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::agents::{GoneWhy, Row, WindowRef};
    use eitri_core::remote_wire::parse_phone_frame;

    const A: &str = "a3f09c1d2e4b5f60";
    const B: &str = "b4e1c09a7d3f5e21";

    fn row(key: &str, instance: &str, n: u16) -> Row {
        Row {
            id: format!("{instance}.{n}"),
            key: key.into(),
            window: WindowRef {
                instance: instance.into(),
                ..WindowRef::default()
            },
            number: n,
            ..Row::default()
        }
    }

    fn frame(body: &str) -> PhoneInbound {
        let line = format!(r#"{{"v":1,"seq":1,"ack":0,{body}}}"#);
        parse_phone_frame(&line).unwrap().body
    }

    #[test]
    fn route_names_the_owner_of_a_key() {
        let rows = RowsIndex::from_view(&View {
            rows: vec![row("k-1", A, 1), row("k-2", B, 1)],
            ..View::default()
        });
        let to = |instance: &str| Route::Owner {
            instance: instance.into(),
        };
        assert_eq!(route(&frame(r#""kind":"subscribe","key":"k-1""#), &rows), to(A));
        assert_eq!(route(&frame(r#""kind":"unsubscribe","key":"k-2""#), &rows), to(B));
        assert_eq!(
            route(
                &frame(r#""kind":"more","key":"k-2","generation":1,"before_turn":3"#),
                &rows
            ),
            to(B)
        );
        assert_eq!(
            route(&frame(r#""kind":"act","key":"k-1","cmd":{"type":"interrupt"}"#), &rows),
            to(A)
        );
        assert_eq!(
            route(&frame(r#""kind":"subscribe","key":"k-9""#), &rows),
            Route::Refused(NO_SUCH_TAB)
        );
        assert_eq!(route(&frame(r#""kind":"subscribe_rows""#), &rows), Route::Rows);
        assert_eq!(route(&frame(r#""kind":"ping""#), &rows), Route::Local);
        assert_eq!(route(&frame(r#""kind":"revoke_self""#), &rows), Route::AnyInstance);
        assert_eq!(
            route(
                &frame(&format!(
                    r#""kind":"enroll","instance":"{B}","code":"c","device_name":"p","recipe":"ssh""#
                )),
                &rows
            ),
            Route::Instance(B.into())
        );
        assert_eq!(
            route(
                &frame(r#""kind":"enroll","instance":"../x","code":"c","device_name":"p","recipe":"ssh""#),
                &rows
            ),
            Route::Refused(NO_SUCH_WINDOW)
        );
        assert_eq!(
            route(
                &frame(r#""kind":"hello","device":"d","layout":"slab","protocol":1"#),
                &rows
            ),
            Route::Refused(HELLO_ONCE)
        );
    }

    #[test]
    fn a_moved_row_names_its_new_owner_whatever_order_the_changes_come_in() {
        let mut rows = RowsIndex::from_view(&View {
            rows: vec![row("k-1", A, 1)],
            ..View::default()
        });
        // The row arrives at B before A's departure is read.
        assert!(rows.apply(&Change::Row { row: row("k-1", B, 4) }));
        assert!(!rows.apply(&Change::Gone {
            id: format!("{A}.1"),
            key: "k-1".into(),
            why: GoneWhy::Moved,
        }));
        assert_eq!(rows.owner("k-1"), Some(B));
        // The same row again changes nothing.
        assert!(!rows.apply(&Change::Row { row: row("k-1", B, 4) }));
        assert!(rows.apply(&Change::Gone {
            id: format!("{B}.4"),
            key: "k-1".into(),
            why: GoneWhy::Closed,
        }));
        assert_eq!(rows.owner("k-1"), None);
        assert!(rows.apply(&Change::Reset {
            view: View {
                rows: vec![row("k-3", A, 2)],
                ..View::default()
            }
        }));
        assert_eq!(rows.instances(), BTreeSet::from([A.to_owned()]));
    }
}
