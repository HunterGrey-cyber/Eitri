//! `ReadPath`: the `Lsp` service end to end over a socket (the contract: 64 KiB data chunks, the hash on the last, a
//! refusal in place of it), and `lsp::read_path` itself where a test has to act between two reads.
#![allow(dead_code)]

#[path = "../src/audit.rs"]
mod audit;
#[path = "../src/daemon.rs"]
mod daemon;
#[path = "../src/peer.rs"]
mod peer;
#[path = "../src/run_dir.rs"]
mod run_dir;

use std::ffi::{CString, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use daemon::login_env::{Captured, Environment};
use daemon::lsp::read_path::{self, CHUNK_BYTES};
use daemon::lsp::Servers;
use eitri_server_proto::lsp_client::LspClient;
use eitri_server_proto::*;
use sha2::{Digest, Sha256};
use tonic::transport::Channel;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REVISION: &str = env!("EITRI_BUILD_COMMIT");

fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

/// A scratch directory and the `Lsp` service served from it over a socket. Paths stay short: it binds one.
struct Fixture {
    base: PathBuf,
    client: LspClient<Channel>,
    server: tokio::task::JoinHandle<()>,
}

static NEXT: AtomicU32 = AtomicU32::new(0);

impl Fixture {
    async fn new() -> Self {
        let base = PathBuf::from(format!(
            "/tmp/rp-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        agent::private_fs::create_private_dir_all(&base, &base).unwrap();
        let base = base.canonicalize().unwrap();
        for dir in ["home", "d", "s", "files"] {
            std::fs::create_dir(base.join(dir)).unwrap();
        }
        let paths = run_dir::Paths::new(VERSION, &base.join("d"), &base.join("s"), &base).unwrap();
        paths.prepare().unwrap();
        let sidecars = Arc::new(daemon::sidecars::Sidecars::new(paths.clone()).unwrap());
        sidecars.set_environment(Ok(Captured {
            env: Environment::from([("PATH".into(), OsString::from("/usr/bin:/bin"))]),
            fallback: None,
        }));
        let servers = Arc::new(Servers::new(paths, sidecars));
        let socket = base.join("l.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let incoming = futures_util::stream::unfold(listener, |listener| async move {
            Some((listener.accept().await.map(|(stream, _)| stream), listener))
        });
        let service = daemon::serve::lsp_server(servers);
        let server = tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(service)
                .serve_with_incoming(incoming)
                .await;
        });
        let channel = tonic::transport::Endpoint::from_static("http://localhost")
            .connect_with_connector(tower::service_fn(move |_| {
                let socket = socket.clone();
                async move {
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                        tokio::net::UnixStream::connect(socket).await?,
                    ))
                }
            }))
            .await
            .unwrap();
        Fixture {
            base,
            client: LspClient::new(channel),
            server,
        }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.base.join("files").join(name)
    }

    fn write(&self, name: &str, bytes: &[u8]) -> String {
        let path = self.file(name);
        std::fs::write(&path, bytes).unwrap();
        path.to_str().unwrap().to_owned()
    }

    /// Every chunk of the answer for `path` and `limit`.
    async fn ask(&mut self, path: &str, limit: u64) -> Vec<ReadPathChunk> {
        let mut stream = self
            .client
            .read_path(ReadPathRequest {
                path: path.to_owned(),
                limit,
            })
            .await
            .unwrap()
            .into_inner();
        let mut chunks = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        while let Some(chunk) = tokio::time::timeout(deadline - Instant::now(), stream.message())
            .await
            .expect("the answer stopped")
            .unwrap()
        {
            chunks.push(chunk);
        }
        chunks
    }

    /// The one-line refusal that is the whole answer for `path`.
    async fn refusal(&mut self, path: &str, limit: u64) -> String {
        let chunks = self.ask(path, limit).await;
        assert_eq!(chunks.len(), 1, "{path}: {chunks:?}");
        assert!(
            chunks[0].data.is_empty() && chunks[0].sha256.is_empty(),
            "{path}: {chunks:?}"
        );
        let text = chunks[0].refusal.clone();
        assert!(!text.is_empty() && !text.contains('\n'), "{path}: {text:?}");
        text
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|at| (at * 31 % 251) as u8).collect()
}

#[tokio::test]
async fn a_file_comes_in_64_kib_chunks_and_the_last_carries_its_hash() {
    let mut fixture = Fixture::new().await;
    let bytes = pattern(150 * 1024 + 17);
    let path = fixture.write("big.rs", &bytes);
    let chunks = fixture.ask(&path, 0).await;
    let (last, data) = chunks.split_last().unwrap();
    assert!(
        !data.is_empty()
            && data
                .iter()
                .all(|chunk| !chunk.data.is_empty() && chunk.data.len() <= CHUNK_BYTES)
    );
    assert!(data
        .iter()
        .all(|chunk| chunk.sha256.is_empty() && chunk.refusal.is_empty()));
    assert_eq!(data.concat_len(), bytes.len());
    assert_eq!(
        data.iter().flat_map(|chunk| chunk.data.clone()).collect::<Vec<u8>>(),
        bytes
    );
    assert!(last.data.is_empty() && last.refusal.is_empty());
    assert_eq!(last.sha256, sha(&bytes));
    // An empty file is its hash alone.
    let path = fixture.write("empty", b"");
    let chunks = fixture.ask(&path, 0).await;
    assert_eq!(chunks.len(), 1, "{chunks:?}");
    assert_eq!(chunks[0].sha256, sha(b""));
}

trait ConcatLen {
    fn concat_len(&self) -> usize;
}

impl ConcatLen for [ReadPathChunk] {
    fn concat_len(&self) -> usize {
        self.iter().map(|chunk| chunk.data.len()).sum()
    }
}

#[tokio::test]
async fn links_on_the_way_are_followed_as_cat_would() {
    let mut fixture = Fixture::new().await;
    std::fs::create_dir(fixture.file("real")).unwrap();
    fixture.write("real/lib.rs", b"pub fn a() {}\n");
    std::os::unix::fs::symlink(fixture.file("real"), fixture.file("via")).unwrap();
    let through = fixture.file("via").join("lib.rs");
    let chunks = fixture.ask(through.to_str().unwrap(), 0).await;
    assert_eq!(chunks[0].data, b"pub fn a() {}\n");
    assert_eq!(chunks.last().unwrap().sha256, sha(b"pub fn a() {}\n"));
}

#[tokio::test]
async fn read_path_refuses_a_directory_a_fifo_and_a_final_link() {
    let mut fixture = Fixture::new().await;
    let target = fixture.write("target.rs", b"fn t() {}\n");
    std::os::unix::fs::symlink(&target, fixture.file("link.rs")).unwrap();
    std::os::unix::fs::symlink("nowhere", fixture.file("dangling.rs")).unwrap();
    let fifo = fixture.file("pipe");
    let c_fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: mkfifo of a path in the test's own scratch directory.
    assert_eq!(unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) }, 0);

    let directory = fixture
        .refusal(fixture.file("").to_str().unwrap().trim_end_matches('/'), 0)
        .await;
    assert!(directory.contains("is a directory"), "{directory}");
    // A FIFO with no writer would hold a plain open for ever; it is refused at once.
    let pipe = tokio::time::timeout(Duration::from_secs(30), fixture.refusal(fifo.to_str().unwrap(), 0))
        .await
        .expect("a FIFO held the read");
    assert!(pipe.contains("not a regular file"), "{pipe}");
    for link in ["link.rs", "dangling.rs"] {
        let refused = fixture.refusal(fixture.file(link).to_str().unwrap(), 0).await;
        assert!(refused.contains("is a symbolic link"), "{refused}");
    }
    // A device, and what does not exist.
    let device = fixture.refusal("/dev/null", 0).await;
    assert!(device.contains("not a regular file"), "{device}");
    let missing = fixture.refusal(fixture.file("absent.rs").to_str().unwrap(), 0).await;
    assert!(missing.contains("does not exist"), "{missing}");
}

/// A FIFO is judged on a descriptor that does not open it: a writer waiting for a reader is never woken by the read.
#[test]
fn a_fifo_is_refused_without_being_opened() {
    let dir = std::env::temp_dir().join(format!("read-path-fifo-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let fifo = dir.join("pipe");
    let c_fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: mkfifo of a path in the test's own scratch directory.
    assert_eq!(unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) }, 0);
    let woken = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (fifo, woken) = (fifo.clone(), woken.clone());
        // A blocking open for writing returns once any reader has opened the FIFO, even one that closed it again.
        std::thread::spawn(move || {
            let file = std::fs::OpenOptions::new().write(true).open(&fifo);
            woken.store(true, Ordering::SeqCst);
            drop(file);
        })
    };
    // Long enough for the writer to be waiting in its open.
    std::thread::sleep(Duration::from_millis(200));
    let last = read_path::read_path(
        fifo.to_str().unwrap(),
        0,
        Instant::now() + Duration::from_secs(60),
        &mut |_| true,
    )
    .unwrap();
    assert!(last.refusal.contains("not a regular file"), "{}", last.refusal);
    std::thread::sleep(Duration::from_millis(300));
    let woken_by_the_read = woken.load(Ordering::SeqCst);
    // The test's own reader lets the writer go.
    let reader = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .unwrap();
    writer.join().unwrap();
    drop(reader);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!woken_by_the_read, "the read opened the FIFO");
}

#[tokio::test]
async fn a_path_that_is_not_an_absolute_normal_one_is_refused_before_any_file_is_touched() {
    let mut fixture = Fixture::new().await;
    let path = fixture.write("a.rs", b"x");
    let dir = fixture.file("").to_str().unwrap().trim_end_matches('/').to_owned();
    for bad in [
        "relative/a.rs".to_owned(),
        String::new(),
        format!("{dir}/../files/a.rs"),
        format!("{dir}/./a.rs"),
        format!("{dir}//a.rs"),
        format!("{path}/"),
        format!("{path}\0"),
        format!("/{}", "a/".repeat(3000)),
    ] {
        let refused = fixture.refusal(&bad, 0).await;
        assert!(
            refused.contains("must be absolute") || refused.contains("longer than") || refused.contains("NUL"),
            "{bad:?}: {refused}"
        );
    }
}

#[tokio::test]
async fn read_path_stops_at_its_limit_with_a_refusal_not_a_partial_file() {
    let mut fixture = Fixture::new().await;
    let path = fixture.write("ten.txt", b"0123456789");
    // Exactly the limit is a file; one byte more is refused whole, with no data before it.
    let chunks = fixture.ask(&path, 10).await;
    assert_eq!(chunks[0].data, b"0123456789");
    assert_eq!(chunks.last().unwrap().sha256, sha(b"0123456789"));
    let refused = fixture.refusal(&path, 9).await;
    assert!(
        refused.contains("is 10 bytes") && refused.contains("9-byte limit"),
        "{refused}"
    );
    // 0 is the server's maximum, and no limit goes past it.
    let most = READ_PATH_MAX as u64;
    let huge = fixture.file("huge");
    let file = std::fs::File::create(&huge).unwrap();
    file.set_len(most + 1).unwrap();
    for limit in [0, most, u64::MAX] {
        let refused = fixture.refusal(huge.to_str().unwrap(), limit).await;
        assert!(
            refused.contains(&format!("{}-byte limit", READ_PATH_MAX)),
            "{limit}: {refused}"
        );
    }
    file.set_len(most).unwrap();
    let chunks = fixture.ask(huge.to_str().unwrap(), 0).await;
    assert_eq!(chunks.concat_len(), READ_PATH_MAX);
    assert!(chunks.last().unwrap().refusal.is_empty() && !chunks.last().unwrap().sha256.is_empty());
}

/// A file that grew past the cap while it was read: the data already sent is followed by a refusal, which voids it.
#[test]
fn a_file_that_grows_past_the_limit_ends_in_a_refusal_after_its_data() {
    let base = PathBuf::from(format!("/tmp/rp-grow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let path = base.join("growing");
    let limit = (CHUNK_BYTES * 2) as u64;
    std::fs::write(&path, pattern(CHUNK_BYTES * 2)).unwrap();
    let mut sent = Vec::new();
    let appended = path.clone();
    let last = read_path::read_path(
        path.to_str().unwrap(),
        limit,
        Instant::now() + Duration::from_secs(60),
        &mut |chunk| {
            if sent.is_empty() {
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&appended)
                    .unwrap()
                    .write_all(&pattern(CHUNK_BYTES))
                    .unwrap();
            }
            sent.push(chunk);
            true
        },
    )
    .unwrap();
    assert!(!sent.is_empty(), "data went out before the refusal");
    assert!(last.sha256.is_empty() && last.data.is_empty());
    assert!(last.refusal.contains("limit of this read"), "{}", last.refusal);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_read_nobody_waits_for_ends_without_an_answer() {
    let base = PathBuf::from(format!("/tmp/rp-gone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let path = base.join("f");
    std::fs::write(&path, pattern(CHUNK_BYTES * 3)).unwrap();
    let mut chunks = 0;
    let last = read_path::read_path(
        path.to_str().unwrap(),
        0,
        Instant::now() + Duration::from_secs(60),
        &mut |_| {
            chunks += 1;
            false
        },
    );
    assert!(last.is_none() && chunks == 1);
    let late = read_path::read_path(path.to_str().unwrap(), 0, Instant::now(), &mut |_| true);
    assert!(late.is_none());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn the_cap_is_the_limit_up_to_the_maximum() {
    assert_eq!(read_path::cap_of(0), READ_PATH_MAX as u64);
    assert_eq!(read_path::cap_of(1), 1);
    assert_eq!(read_path::cap_of(READ_PATH_MAX as u64 + 1), READ_PATH_MAX as u64);
    assert_eq!(read_path::cap_of(u64::MAX), READ_PATH_MAX as u64);
    let _ = Path::new("/");
}
