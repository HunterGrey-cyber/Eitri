use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use eitri_core::companion::{install_args, parse_install_report, InstallKind, Sockets, INSTALL_LUA, REPLACED_METHOD};
use eitri_core::nvim_rpc::{LinkEvent, NvimLink};
use rmpv::Value;

struct Nvim {
    child: Child,
    dir: PathBuf,
    socket: PathBuf,
}

impl Nvim {
    fn start() -> Self {
        let dir = std::env::temp_dir().join(format!("pn-{}", uuid::Uuid::new_v4().simple()));
        agent::private_fs::create_private_dir_all(&dir, &dir).unwrap();
        let socket = agent::socket_path::in_dir(&dir, "n.sock").unwrap();
        let mut command = Command::new("nvim");
        command
            .args(["--clean", "--headless", "-n", "-i", "NONE", "--listen"])
            .arg(&socket)
            .current_dir(&dir)
            .env("HOME", &dir)
            .env("NVIM_LOG_FILE", dir.join("nvim.log"))
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("NVIM")
            .env_remove("NVIM_LISTEN_ADDRESS")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (name, _) in std::env::vars_os() {
            if name.to_str().is_some_and(|s| s.starts_with("EITRI_")) {
                command.env_remove(name);
            }
        }
        let child = command.spawn().expect("nvim must be on PATH");
        Self { child, dir, socket }
    }

    fn connect(&self) -> (NvimLink, Receiver<LinkEvent>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match NvimLink::connect(&self.socket, Duration::from_secs(1)) {
                Ok(link) => return link,
                Err(error) => {
                    assert!(Instant::now() < deadline, "nvim must listen: {error}");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }
}

impl Drop for Nvim {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn lua(link: &NvimLink, code: &str, args: Vec<Value>) -> Value {
    link.exec_lua(code, args)
        .wait(Duration::from_secs(5))
        .expect("nvim must answer")
        .expect("valid Lua")
}

fn install(link: &NvimLink, kind: InstallKind) {
    let report = lua(
        link,
        INSTALL_LUA,
        install_args(link.channel_id(), &Sockets::default(), &kind),
    );
    assert!(parse_install_report(&report).unwrap().failed.is_empty());
}

#[test]
#[ignore = "needs nvim on PATH and Unix sockets; uses no display"]
fn the_replaced_page_channel_receives_eitri_replaced() {
    let nvim = Nvim::start();
    let (agent, agent_events) = nvim.connect();
    let (first, first_events) = nvim.connect();
    let (other, other_events) = nvim.connect();
    let (second, second_events) = nvim.connect();
    install(&agent, InstallKind::Agent);
    install(&first, InstallKind::Page { id: "markdown".into() });
    install(&other, InstallKind::Page { id: "other".into() });
    lua(&agent, "_G.saved_agent = __eitri_companion; _G.saved_other = __eitri_pages.other; _G.saved_first = __eitri_pages.markdown", vec![]);
    install(&second, InstallKind::Page { id: "markdown".into() });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match first_events
            .recv_timeout(remaining)
            .expect("the replaced channel must hear its replacement")
        {
            LinkEvent::Notification { method, params } if method == REPLACED_METHOD => {
                assert_eq!(params, [Value::from(second.channel_id())]);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(
        lua(
            &second,
            "return saved_agent == __eitri_companion and saved_other == __eitri_pages.other and saved_first.torn",
            vec![]
        )
        .as_bool(),
        Some(true)
    );
    install(&second, InstallKind::Page { id: "markdown".into() });
    for events in [agent_events, other_events, second_events] {
        for event in events.try_iter() {
            if let LinkEvent::Notification { method, .. } = event {
                assert_ne!(
                    method, REPLACED_METHOD,
                    "only another holder of the same page is replaced"
                );
            }
        }
    }
}
