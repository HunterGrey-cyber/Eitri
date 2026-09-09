//! `neovibe-supervisor`: a persistent, single-process GTK4 app showing live agent status
//! (blocked/working/done/idle/no_session) for every currently-running `shell` window on this
//! machine. See docs/superpowers/specs/2026-09-08-supervisor-cross-window-agent-status-design.md.
//!
//! Deliberately single-threaded: every accept/read/write below happens inside one
//! `glib::timeout_add_local` poll on the GTK main thread. There is no separate accept thread --
//! unlike `agent::process`'s hook listener (which genuinely needs a background thread so it never
//! stalls a live conversation), this dashboard's own update cadence has no real-time requirement,
//! and GTK widgets are not `Send`, so keeping the registry and the `ListBox` on the same thread
//! avoids an entire class of handoff complexity for no real benefit here.

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, Label, ListBox, ListBoxRow, Orientation};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::rc::Rc;

use supervisor::registry::Registry;
use supervisor::{AgentStatus, ShellMessage};

const APP_ID: &str = "cn.huntergrey.neovibe.supervisor";
const POLL_INTERVAL_MS: u64 = 200;

/// One accepted connection: its raw stream (kept for writing `Activate` back) and a buffered
/// reader over a `try_clone()` of it (kept separately so a partial line read doesn't block a
/// write attempt on the same underlying fd -- mirrors `agent::process`'s own
/// `stream.try_clone()` + separate `BufReader` pattern for its hook-relay connections).
struct Connection {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

struct AppState {
    registry: Registry,
    connections: HashMap<u64, Connection>,
    next_connection_id: u64,
}

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run_with_args::<&str>(&[])
}

fn build_ui(app: &Application) {
    let socket_path = supervisor::socket_path();
    // A stale socket file from a previous ungraceful exit would make `bind` fail with
    // `AddrInUse` even though nothing is actually listening -- remove it first, matching
    // `agent::process::spawn_with_binary`'s own pre-bind cleanup of its per-conversation socket.
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).unwrap_or_else(|e| {
        panic!("neovibe-supervisor: failed to bind {socket_path:?}: {e}");
    });
    // Both the listener AND every accepted stream must be non-blocking -- an accepted
    // `UnixStream` does NOT inherit its listener's non-blocking flag on Linux. This exact gap
    // caused a real ~6-hour deadlock in `agent::process`'s own hook listener during its
    // development; get it right here from the start rather than rediscovering it.
    listener.set_nonblocking(true).expect("set listener non-blocking");

    let state = Rc::new(RefCell::new(AppState { registry: Registry::new(), connections: HashMap::new(), next_connection_id: 0 }));

    let window = ApplicationWindow::builder()
        .application(app)
        .title("neovibe supervisor")
        .default_width(320)
        .default_height(400)
        .build();

    let list_box = ListBox::new();
    window.set_child(Some(&list_box));

    {
        let state = state.clone();
        let list_box = list_box.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(POLL_INTERVAL_MS), move || {
            poll_once(&listener, &state);
            rebuild_list(&list_box, &state.borrow().registry);
            glib::ControlFlow::Continue
        });
    }

    {
        let state = state.clone();
        list_box.connect_row_activated(move |_list_box, row| {
            let instance_id = row.widget_name().to_string();
            activate_instance(&state, &instance_id);
        });
    }

    window.present();
}

/// One poll tick: accept any newly-connected clients, drain any complete lines already buffered
/// on existing connections, and drop any connection whose read/write just failed. All of this is
/// non-blocking -- a client with nothing new to say is skipped instantly, never waited on.
fn poll_once(listener: &UnixListener, state: &Rc<RefCell<AppState>>) {
    let mut state_ref = state.borrow_mut();

    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                stream.set_nonblocking(true).expect("set accepted stream non-blocking");
                let reader = BufReader::new(stream.try_clone().expect("clone accepted stream"));
                let id = state_ref.next_connection_id;
                state_ref.next_connection_id += 1;
                state_ref.connections.insert(id, Connection { stream, reader });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break, // listener itself is gone -- nothing more to accept this tick
        }
    }

    let mut disconnected = Vec::new();
    let ids: Vec<u64> = state_ref.connections.keys().copied().collect();
    for id in ids {
        loop {
            let mut line = String::new();
            let read_result = state_ref.connections.get_mut(&id).unwrap().reader.read_line(&mut line);
            match read_result {
                Ok(0) => {
                    disconnected.push(id);
                    break;
                }
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<ShellMessage>(trimmed) {
                        Ok(ShellMessage::Register { instance_id, project_name, project_dir, pid }) => {
                            state_ref.registry.handle_register(id, instance_id, project_name, project_dir, pid);
                        }
                        Ok(ShellMessage::Status { instance_id, status }) => {
                            state_ref.registry.handle_status(&instance_id, status);
                        }
                        Err(e) => {
                            eprintln!("neovibe-supervisor: unparseable message on connection {id}: {e} -- raw: {trimmed}");
                        }
                    }
                    // Keep draining -- a burst of buffered lines shouldn't wait for the next poll tick.
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    disconnected.push(id);
                    break;
                }
            }
        }
    }

    for id in disconnected {
        state_ref.connections.remove(&id);
        state_ref.registry.handle_disconnect(id);
    }
}

fn rebuild_list(list_box: &ListBox, registry: &Registry) {
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }
    for row in registry.rows() {
        let hbox = gtk4::Box::new(Orientation::Horizontal, 8);
        hbox.set_margin_top(4);
        hbox.set_margin_bottom(4);
        hbox.set_margin_start(8);
        hbox.set_margin_end(8);

        let dot = Label::new(Some(status_dot(row.status)));
        let name = Label::new(Some(&row.project_name));
        name.set_hexpand(true);
        name.set_xalign(0.0);

        hbox.append(&dot);
        hbox.append(&name);

        let list_row = ListBoxRow::new();
        list_row.set_child(Some(&hbox));
        // The instance_id travels with the row via its own widget name -- read back in the
        // row-activated handler below, avoiding a separate parallel index to keep in sync with
        // `ListBox`'s own child order.
        list_row.set_widget_name(&row.instance_id);
        list_box.append(&list_row);
    }
}

fn status_dot(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Blocked => "\u{1F534}",   // red circle
        AgentStatus::Working => "\u{1F7E1}",   // yellow circle
        AgentStatus::Done => "\u{1F7E2}",      // green circle
        AgentStatus::Idle | AgentStatus::NoSession => "\u{26AA}", // white circle
    }
}

/// Sends `SupervisorMessage::Activate` down whichever connection currently owns `instance_id`
/// (via `Registry::connection_id_for`, added in Step 1 above). A row for an instance that has
/// since disconnected has no matching connection any more -- a harmless no-op, not an error.
fn activate_instance(state: &Rc<RefCell<AppState>>, instance_id: &str) {
    let mut state_ref = state.borrow_mut();
    let Some(connection_id) = state_ref.registry.connection_id_for(instance_id) else {
        return;
    };
    let payload = serde_json::to_string(&supervisor::SupervisorMessage::Activate).unwrap();
    if let Some(connection) = state_ref.connections.get_mut(&connection_id) {
        if let Err(e) = writeln!(connection.stream, "{payload}") {
            eprintln!("neovibe-supervisor: failed to send activate to connection {connection_id}: {e}");
        }
    }
}
