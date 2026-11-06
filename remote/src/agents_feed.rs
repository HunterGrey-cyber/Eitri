//! Forwards the agents reader's ordered changes as phone frames on its own thread.

use std::cell::Cell;
use std::io;
use std::path::Path;

use eitri_core::agents::{Change, Reader, ReaderEvent, SCHEMA_V};
use eitri_core::remote_wire::{agents_change_frame, agents_view_frame};

/// Owns the reader and stops it when dropped. Starting and stopping perform disk work or join a thread.
pub struct AgentsFeed {
    reader: Reader,
}

impl AgentsFeed {
    /// The Reader's first Reset becomes an agents_view frame; every later change an agents_change_frame.
    /// The callback runs on the reader thread and should hand frames to the transport without blocking.
    pub fn start(control_dir: &Path, tx: impl Fn(String) + Send + 'static) -> io::Result<AgentsFeed> {
        let first = Cell::new(true);
        let reader = Reader::start(control_dir, SCHEMA_V, move |event| match event {
            ReaderEvent::Change(change) => {
                let frame = match &change {
                    Change::Reset { view } if first.replace(false) => agents_view_frame(view),
                    _ => agents_change_frame(&change),
                };
                tx(frame);
            }
            ReaderEvent::Error(why) => eprintln!("[remote] agents feed ended: {why}"),
        })?;
        Ok(AgentsFeed { reader })
    }

    /// Ends the reader and waits for its thread.
    pub fn stop(self) {
        self.reader.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::agents::{
        new_instance, publish::PublisherHooks, ProcessFile, ProjectRef, Publisher, Row, WindowRef,
    };
    use serde_json::Value;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    #[test]
    fn feed_sends_the_view_first_then_changes() {
        let instance = new_instance();
        let scratch = std::path::PathBuf::from(
            option_env!("CARGO_TARGET_DIR").unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../target")),
        )
        .join("tmp/remote-tests")
        .join(&instance);
        let control = scratch.join("control");
        std::fs::create_dir_all(&scratch).unwrap();
        let project = ProjectRef {
            key: "9b2c4e7a10f3d856".into(),
            name: "project".into(),
        };
        let window = WindowRef {
            instance: instance.clone(),
            name: "1".into(),
            focused: true,
        };
        let (published_tx, published) = channel();
        let publisher = Publisher::start_with(
            &control,
            &instance,
            project.clone(),
            window.clone(),
            PublisherHooks {
                on_publish: Some(Box::new(move |file: &ProcessFile| {
                    published_tx.send(file.clone()).unwrap();
                })),
                ..PublisherHooks::default()
            },
        )
        .unwrap();
        let mut row = Row {
            id: format!("{instance}.1"),
            key: "k-0000000000000001".into(),
            agent: "claude".into(),
            project,
            window,
            number: 1,
            state: "idle".into(),
            ..Row::default()
        };
        publisher.offer_now(vec![row.clone()]);
        while published.recv_timeout(Duration::from_secs(10)).unwrap().rows.is_empty() {}
        let (tx, rx) = channel();
        let feed = AgentsFeed::start(&control, move |frame| {
            tx.send(frame).unwrap();
        })
        .unwrap();
        let first: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(10)).unwrap()).unwrap();
        assert_eq!(first["kind"], "agents_view");
        assert_eq!(first["view"]["rows"][0]["state"], "idle");
        row.state = "running".into();
        publisher.offer_now(vec![row]);
        let next: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(10)).unwrap()).unwrap();
        assert_eq!(next["kind"], "agents_row");
        assert_eq!(next["row"]["state"], "running");
        feed.stop();
        publisher.stop();
        std::fs::remove_dir_all(scratch).unwrap();
    }
}
