//! The pure, I/O-free bookkeeping half of `neovibe-supervisor`: which `shell` instances are
//! currently connected, what their last-reported status is, and in what order to render them.
//! `connection_id` is an opaque key the caller (Task 3's accept loop) assigns per accepted
//! `UnixStream` -- this module never touches a socket itself, so it's fully unit-testable.

use crate::AgentStatus;

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub instance_id: String,
    pub project_name: String,
    pub status: AgentStatus,
}

struct Entry {
    connection_id: u64,
    instance_id: String,
    project_name: String,
    status: AgentStatus,
}

#[derive(Default)]
pub struct Registry {
    // Insertion order is the render order (spec doesn't require sorting, "add live when
    // registered" -- a Vec preserves this trivially; lookups are by connection_id or instance_id,
    // both cheap enough at the expected scale (a handful of shell windows, not thousands).
    entries: Vec<Entry>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle_register(
        &mut self,
        connection_id: u64,
        instance_id: String,
        project_name: String,
        _project_dir: String,
        _pid: u32,
    ) {
        // A re-register on the same connection_id (shouldn't happen in practice -- `shell` only
        // ever sends one Register per connection -- but never trust the wire) replaces the entry
        // rather than duplicating it.
        self.entries.retain(|e| e.connection_id != connection_id);
        self.entries.push(Entry {
            connection_id,
            instance_id,
            project_name,
            status: AgentStatus::NoSession,
        });
    }

    /// `connection_id` must match the connection that originally registered `instance_id` --
    /// without this check, any connected client could overwrite any other instance's row by
    /// sending a `Status` message claiming someone else's `instance_id` on the wire (the wire
    /// message itself carries no other identity). Not a real threat model on a single-user local
    /// socket, but free to close.
    pub fn handle_status(&mut self, connection_id: u64, instance_id: &str, status: AgentStatus) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|e| e.connection_id == connection_id && e.instance_id == instance_id)
        {
            entry.status = status;
        }
        // A status for a (connection_id, instance_id) pair that doesn't match any registered
        // entry (never registered, or a connection_id/instance_id mismatch) is a harmless no-op,
        // matching this project's established "unknown id -> no-op, not a panic" convention.
    }

    pub fn handle_disconnect(&mut self, connection_id: u64) {
        self.entries.retain(|e| e.connection_id != connection_id);
    }

    /// The connection_id currently associated with `instance_id`, if any -- used by the binary's
    /// `activate_instance` to know which live connection to write `SupervisorMessage::Activate`
    /// to. `None` for an instance that has already disconnected (a real, benign race: the user
    /// could click a row in the same poll tick its connection drops) -- a harmless no-op case,
    /// not an error.
    pub fn connection_id_for(&self, instance_id: &str) -> Option<u64> {
        self.entries
            .iter()
            .find(|e| e.instance_id == instance_id)
            .map(|e| e.connection_id)
    }

    pub fn rows(&self) -> Vec<Row> {
        self.entries
            .iter()
            .map(|e| Row {
                instance_id: e.instance_id.clone(),
                project_name: e.project_name.clone(),
                status: e.status,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_freshly_registered_instance_starts_as_no_session() {
        let mut reg = Registry::new();
        reg.handle_register(1, "abc".into(), "neovibe".into(), "/tmp/neovibe".into(), 100);
        assert_eq!(
            reg.rows(),
            vec![Row {
                instance_id: "abc".into(),
                project_name: "neovibe".into(),
                status: AgentStatus::NoSession
            }]
        );
    }

    #[test]
    fn status_updates_the_matching_instance_only() {
        let mut reg = Registry::new();
        reg.handle_register(1, "abc".into(), "proj-a".into(), "/tmp/a".into(), 100);
        reg.handle_register(2, "def".into(), "proj-b".into(), "/tmp/b".into(), 200);
        reg.handle_status(2, "def", AgentStatus::Blocked);
        let rows = reg.rows();
        assert_eq!(rows[0].status, AgentStatus::NoSession);
        assert_eq!(rows[1].status, AgentStatus::Blocked);
    }

    #[test]
    fn status_for_an_unregistered_instance_id_is_a_harmless_no_op() {
        let mut reg = Registry::new();
        reg.handle_status(1, "never-registered", AgentStatus::Working);
        assert_eq!(reg.rows(), vec![]);
    }

    #[test]
    fn status_is_ignored_when_the_connection_id_does_not_match_the_registered_one() {
        let mut reg = Registry::new();
        reg.handle_register(1, "abc".into(), "proj-a".into(), "/tmp/a".into(), 100);
        // A different connection claiming the same instance_id on the wire must not be able to
        // overwrite the row the real owning connection registered.
        reg.handle_status(999, "abc", AgentStatus::Blocked);
        assert_eq!(reg.rows()[0].status, AgentStatus::NoSession);
    }

    #[test]
    fn disconnect_removes_only_the_matching_connection() {
        let mut reg = Registry::new();
        reg.handle_register(1, "abc".into(), "proj-a".into(), "/tmp/a".into(), 100);
        reg.handle_register(2, "def".into(), "proj-b".into(), "/tmp/b".into(), 200);
        reg.handle_disconnect(1);
        let rows = reg.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].instance_id, "def");
    }

    #[test]
    fn connection_id_for_finds_the_right_connection_and_none_for_unknown_ids() {
        let mut reg = Registry::new();
        reg.handle_register(7, "abc".into(), "proj".into(), "/tmp".into(), 100);
        assert_eq!(reg.connection_id_for("abc"), Some(7));
        assert_eq!(reg.connection_id_for("does-not-exist"), None);
    }

    #[test]
    fn insertion_order_is_preserved_across_rows_calls() {
        let mut reg = Registry::new();
        reg.handle_register(1, "third".into(), "c".into(), "/tmp/c".into(), 300);
        reg.handle_register(2, "first".into(), "a".into(), "/tmp/a".into(), 100);
        reg.handle_status(2, "first", AgentStatus::Working);
        let rows = reg.rows();
        let ids: Vec<&str> = rows.iter().map(|r| r.instance_id.as_str()).collect();
        assert_eq!(ids, vec!["third", "first"]);
    }
}
