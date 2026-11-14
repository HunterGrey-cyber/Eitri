// Fault-injection regressions share the journal's private test fixtures.

#[test]
fn review_enlarged_final_length_and_non_dense_torn_header_are_refused() {
    for change_sequence in [false, true] {
        let root = Scratch::new();
        let fs = Arc::new(Faults::default());
        let mut journal =
            Journal::open_with(root.path(), &config(), fs.clone(), Arc::new(ManualClock::default())).unwrap();
        journal.append(1, b"one").unwrap();
        journal.append(2, b"two").unwrap();
        drop(journal);
        let path = segment_paths(&*fs, root.path()).unwrap()[0].1.clone();
        let mut bytes = fs.read(&path).unwrap();
        let offset = 32;
        if change_sequence {
            bytes[offset + 1..offset + 9].copy_from_slice(&9u64.to_le_bytes());
            bytes[offset + 9..offset + 17].copy_from_slice(&9u64.to_le_bytes());
            bytes.truncate(offset + 22);
        } else {
            bytes[offset + 17..offset + 21].copy_from_slice(&4096u32.to_le_bytes());
        }
        fs.replace(&path, &bytes).unwrap();
        let error = Journal::open_with(root.path(), &config(), fs.clone(), Arc::new(ManualClock::default()))
            .err()
            .expect("corruption was cut");
        assert!(error.to_string().contains("offset 32"));
        assert!(error.to_string().contains(&path.display().to_string()));
        assert_eq!(fs.read(&path).unwrap(), bytes);
    }
}

#[test]
fn review_zero_tails_are_cut_but_unreadable_nonzero_tails_are_refused() {
    let root = Scratch::new();
    let fs = Arc::new(Faults::default());
    let path = root.path().join("seg-00000000000000000001.log");
    let first = encode(&Record::Event {
        sequence: 1,
        bytes: b"one".to_vec(),
    })
    .unwrap();
    for tail in [vec![0; 100], vec![0; 10]] {
        let mut bytes = first.clone();
        bytes.extend(tail);
        fs.replace(&path, &bytes).unwrap();
        let journal = Journal::open_with(root.path(), &config(), fs.clone(), Arc::new(ManualClock::default())).unwrap();
        assert_eq!(journal.head(), 1);
        assert_eq!(fs.read(&path).unwrap(), first);
    }
    let mut bytes = first;
    bytes.extend([0, 9, 0]);
    fs.replace(&path, &bytes).unwrap();
    assert!(Journal::open_with(root.path(), &config(), fs, Arc::new(ManualClock::default())).is_err());
}

#[test]
fn review_fsync_failure_is_sticky_and_covers_unsynced_events() {
    let root = Scratch::new();
    let fs = Arc::new(Faults::default());
    let mut journal = Journal::open_with(root.path(), &config(), fs.clone(), Arc::new(ManualClock::default())).unwrap();
    journal.append(1, b"durable").unwrap();
    journal.turn_ended().unwrap();
    journal.append(2, b"uncertain").unwrap();
    journal.append(3, b"uncertain").unwrap();
    fs.fail_sync.store(true, Ordering::SeqCst);
    assert_eq!(journal.turn_ended().unwrap_err().raw_os_error(), Some(libc::EIO));
    fs.fail_sync.store(false, Ordering::SeqCst);
    for sequence in 4..=6 {
        assert_eq!(
            journal.append(sequence, b"later").unwrap_err().raw_os_error(),
            Some(libc::EIO)
        );
        assert_eq!(journal.sync_due().unwrap_err().raw_os_error(), Some(libc::EIO));
    }
    assert_eq!(journal.read_after(0, 100).unwrap().len(), 1);
    gap(journal.read_after(1, 100), 2, 3, GapWhy::WriteFailed);
}

#[test]
fn review_recovered_records_are_scheduled_for_sync() {
    let root = Scratch::new();
    let path = root.path().join("seg-00000000000000000001.log");
    Disk.append(
        &path,
        &encode(&Record::Event {
            sequence: 1,
            bytes: vec![7],
        })
        .unwrap(),
    )
    .unwrap();
    let fs = Arc::new(Faults::default());
    let clock = Arc::new(ManualClock::default());
    let journal = Journal::open_with(root.path(), &config(), fs.clone(), clock.clone()).unwrap();
    clock.advance(200);
    journal.sync_due().unwrap();
    assert!(fs.syncs.load(Ordering::SeqCst) > 0);
}

#[derive(Default)]
struct DirectoryFaults {
    synced: Mutex<Vec<PathBuf>>,
    fail: AtomicBool,
}
impl FileSystem for DirectoryFaults {
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.synced.lock().unwrap().push(path.to_path_buf());
        if self.fail.load(Ordering::SeqCst) {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        Disk.sync_dir(path)
    }
}

#[test]
fn review_new_directory_parents_are_synced() {
    let root = Scratch::new();
    let fs = Arc::new(DirectoryFaults::default());
    let dir = root.path().join("state/journal/session");
    let _journal = Journal::open_with(&dir, &config(), fs.clone(), Arc::new(ManualClock::default())).unwrap();
    let _pending = Pending::open_with(&root.path().join("state/pending/session.bin"), fs.clone()).unwrap();
    let synced = fs.synced.lock().unwrap();
    for parent in [
        root.path().to_path_buf(),
        root.path().join("state"),
        root.path().join("state/journal"),
    ] {
        assert!(synced.contains(&parent), "missing parent sync: {}", parent.display());
    }
}

#[test]
fn review_pending_matches_renamed_clear_after_directory_sync_failure() {
    let root = Scratch::new();
    let path = root.path().join("pending/session.bin");
    let fs = Arc::new(DirectoryFaults::default());
    let mut pending = Pending::open_with(&path, fs.clone()).unwrap();
    pending.request(1, b"one", "one").unwrap();
    fs.fail.store(true, Ordering::SeqCst);
    assert!(pending.clear(2).is_err());
    assert!(pending.list().is_empty());
    fs.fail.store(false, Ordering::SeqCst);
    pending.request(3, b"two", "two").unwrap();
    assert_eq!(Pending::open(&path).unwrap().list(), vec![(3, b"two".to_vec())]);
}

#[test]
fn review_pending_enospc_before_request_keeps_memory_and_retries() {
    let root = Scratch::new();
    let path = root.path().join("pending/session.bin");
    let fs = Arc::new(Faults::default());
    let mut pending = Pending::open_with(&path, fs.clone()).unwrap();
    fs.full.store(true, Ordering::SeqCst);
    assert!(pending.request(1, &[255; 4096], "one").is_err());
    assert_eq!(pending.list(), vec![(1, vec![255; 4096])]);
    fs.full.store(false, Ordering::SeqCst);
    pending.request(1, &[255; 4096], "one").unwrap();
    assert_eq!(Pending::open(&path).unwrap().list(), pending.list());
    assert!(Disk.read(&path).unwrap().len() < 4200);
}

#[test]
fn review_replay_does_not_read_a_whole_segment() {
    #[derive(Default)]
    struct Reads(AtomicUsize);
    impl FileSystem for Reads {
        fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Disk.read(path)
        }
    }
    let root = Scratch::new();
    let fs = Arc::new(Reads::default());
    let mut journal = Journal::open_with(root.path(), &config(), fs.clone(), Arc::new(ManualClock::default())).unwrap();
    for sequence in 1..=10 {
        journal.append(sequence, &[7; 20]).unwrap();
    }
    fs.0.store(0, Ordering::SeqCst);
    assert_eq!(journal.read_after(7, 1).unwrap().len(), 1);
    assert_eq!(fs.0.load(Ordering::SeqCst), 0);
}

#[test]
fn review_eviction_works_without_temp_allocations_and_expires_stubs() {
    let root = Scratch::new();
    let clock = ManualClock::default();
    let closed = create_session(root.path(), "closed", Some(clock.now()));
    let fs = Faults::default();
    fs.fail_replace.store(true, Ordering::SeqCst);
    let cfg = JournalConfig {
        cap_bytes: 1,
        ..config()
    };
    assert!(matches!(
        enforce_budget_with(root.path(), &[closed], &cfg, &plentiful, &fs, &clock),
        BudgetOutcome::Evicted(_)
    ));
    let dir = root.path().join("journal/closed");
    let journal = Journal::open(&dir, &cfg).unwrap();
    gap(journal.read_after(0, 100), 1, 8, GapWhy::Evicted);
    drop(journal);
    let expired = SessionUsage {
        session: "closed".into(),
        bytes: 0,
        closed_at: Some(clock.now() - cfg.closed_keep),
    };
    enforce_budget_with(root.path(), &[expired], &cfg, &plentiful, &fs, &clock);
    assert!(!dir.exists());
}

#[test]
fn review_open_and_budget_remove_crash_temporaries() {
    let root = Scratch::new();
    let fs = Arc::new(Faults::default());
    let temp = root.path().join(".journal-2147483647-0.tmp");
    fs.replace(&temp, b"orphan").unwrap();
    let journal = Journal::open_with(root.path(), &config(), fs.clone(), Arc::new(ManualClock::default())).unwrap();
    assert!(!temp.exists());
    drop(journal);
    let temp = root.path().join("pending/.journal-2147483647-1.tmp");
    fs.replace(&temp, b"orphan").unwrap();
    enforce_budget_with(root.path(), &[], &config(), &plentiful, &*fs, &ManualClock::default());
    assert!(!temp.exists());
}

#[test]
fn review_budget_accepts_a_linked_state_root() {
    let root = Scratch::new();
    let link = root.path().join("linked");
    let state = root.path().join("state");
    Disk.create_dir(&state).unwrap();
    std::os::unix::fs::symlink(&state, &link).unwrap();
    let closed = create_session(&link, "closed", Some(ManualClock::default().now()));
    let cfg = JournalConfig {
        cap_bytes: 1,
        ..config()
    };
    assert!(matches!(
        enforce_budget_with(
            &link,
            &[closed],
            &cfg,
            &plentiful,
            &Faults::default(),
            &ManualClock::default()
        ),
        BudgetOutcome::Evicted(_)
    ));
}

#[test]
fn review_budget_continues_after_bad_session_and_nonprivate_subdirectory() {
    let root = Scratch::new();
    let clock = ManualClock::default();
    let bad = create_session(root.path(), "bad", Some(clock.now() - Duration::from_secs(2)));
    let good = create_session(root.path(), "good", Some(clock.now()));
    let path = segment_paths(&Disk, &root.path().join("journal/bad")).unwrap()[0]
        .1
        .clone();
    let mut bytes = Disk.read(&path).unwrap();
    bytes[21] ^= 1;
    Disk.replace(&path, &bytes).unwrap();
    let other = root.path().join("other");
    Disk.create_dir(&other).unwrap();
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = JournalConfig {
        cap_bytes: 1,
        ..config()
    };
    let outcome = enforce_budget_with(root.path(), &[bad, good], &cfg, &plentiful, &Faults::default(), &clock);
    let journal = Journal::open(&root.path().join("journal/good"), &cfg).unwrap();
    gap(journal.read_after(0, 100), 1, 8, GapWhy::Evicted);
    let description = format!("{outcome:?}");
    assert!(
        description.contains("good") && description.contains("bad") && description.contains("other"),
        "{description}"
    );
}

#[test]
fn review_closed_session_torn_final_record_ends_its_range() {
    let root = Scratch::new();
    let clock = ManualClock::default();
    let closed = create_session(root.path(), "closed", Some(clock.now()));
    let path = segment_paths(&Disk, &root.path().join("journal/closed")).unwrap()[0]
        .1
        .clone();
    Disk.truncate(&path, 7 * 129 + 22).unwrap();
    let cfg = JournalConfig {
        cap_bytes: 1,
        segment_bytes: 4096,
        ..config()
    };
    assert!(matches!(
        enforce_budget_with(root.path(), &[closed], &cfg, &plentiful, &Faults::default(), &clock),
        BudgetOutcome::Evicted(_)
    ));
    let journal = Journal::open(&root.path().join("journal/closed"), &cfg).unwrap();
    gap(journal.read_after(0, 100), 1, 7, GapWhy::Evicted);
}

#[test]
fn review_sync_io_does_not_hold_the_state_lock() {
    #[derive(Default)]
    struct CheckLock(Mutex<Option<Arc<(Mutex<SyncState>, Condvar)>>>);
    impl FileSystem for CheckLock {
        fn sync(&self, path: &Path) -> io::Result<()> {
            let state = self.0.lock().unwrap();
            if state.as_ref().unwrap().0.try_lock().is_err() {
                return Err(io::Error::other("state lock held during sync"));
            }
            Disk.sync(path)
        }
        fn sync_dir(&self, path: &Path) -> io::Result<()> {
            if let Some(state) = self.0.lock().unwrap().as_ref() {
                if state.0.try_lock().is_err() {
                    return Err(io::Error::other("state lock held during directory sync"));
                }
            }
            Disk.sync_dir(path)
        }
    }
    let root = Scratch::new();
    let fs = Arc::new(CheckLock::default());
    let mut journal = Journal::open_with(root.path(), &config(), fs.clone(), Arc::new(ManualClock::default())).unwrap();
    // Park the worker so another thread's brief bookkeeping cannot make try_lock a false failure.
    journal.syncer.state.0.lock().unwrap().stop = true;
    journal.syncer.state.1.notify_one();
    journal.syncer.worker.take().unwrap().join().unwrap();
    *fs.0.lock().unwrap() = Some(journal.syncer.state.clone());
    journal.append(1, b"one").unwrap();
    journal.turn_ended().unwrap();
}

#[test]
fn review_appends_during_sync_keep_their_dirty_generation() {
    use std::sync::mpsc::{channel, Receiver, Sender};
    struct BlockSync {
        entered: Sender<()>,
        release: Mutex<Receiver<()>>,
        block: AtomicBool,
        fail: bool,
        syncs: AtomicUsize,
    }
    impl FileSystem for BlockSync {
        fn sync(&self, path: &Path) -> io::Result<()> {
            self.syncs.fetch_add(1, Ordering::SeqCst);
            Disk.sync(path)?;
            if self.block.swap(false, Ordering::SeqCst) {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .map_err(io::Error::other)?;
                if self.fail {
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
            }
            Ok(())
        }
    }
    for fail in [false, true] {
        let root = Scratch::new();
        let (entered_tx, entered_rx) = channel();
        let (release_tx, release_rx) = channel();
        let fs = Arc::new(BlockSync {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            block: AtomicBool::new(true),
            fail,
            syncs: AtomicUsize::new(0),
        });
        let clock = Arc::new(ManualClock::default());
        let mut journal = Journal::open_with(root.path(), &config(), fs.clone(), clock.clone()).unwrap();
        journal.append(1, b"one").unwrap();
        clock.advance(200);
        journal.syncer.state.1.notify_one();
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let (written_tx, written_rx) = channel();
        let writer = std::thread::spawn(move || {
            let result = journal.append(2, b"two");
            written_tx.send(result).unwrap();
            journal
        });
        let result = written_rx.recv_timeout(Duration::from_secs(10));
        release_tx.send(()).unwrap();
        let mut journal = writer.join().unwrap();
        result.expect("append waited for fsync").unwrap();
        // flush serializes with the blocked generation, giving a deterministic completion boundary.
        if fail {
            assert_eq!(journal.sync_due().unwrap_err().raw_os_error(), Some(libc::EIO));
            gap(journal.read_after(0, 100), 1, 2, GapWhy::WriteFailed);
            for sequence in 3..=4 {
                assert_eq!(
                    journal.append(sequence, b"later").unwrap_err().raw_os_error(),
                    Some(libc::EIO)
                );
            }
        } else {
            journal.sync_due().unwrap();
            assert_eq!(fs.syncs.load(Ordering::SeqCst), 1);
            clock.advance(200);
            journal.sync_due().unwrap();
            assert_eq!(fs.syncs.load(Ordering::SeqCst), 2);
        }
    }
}

#[test]
fn review_budget_keeps_evictions_before_a_later_failure_and_pressure() {
    let root = Scratch::new();
    let clock = ManualClock::default();
    let first = create_session(root.path(), "first", Some(clock.now() - Duration::from_secs(1)));
    let bad = create_session(root.path(), "bad", Some(clock.now()));
    let path = segment_paths(&Disk, &root.path().join("journal/bad")).unwrap()[0]
        .1
        .clone();
    let mut bytes = Disk.read(&path).unwrap();
    bytes[21] ^= 1;
    Disk.replace(&path, &bytes).unwrap();
    let cfg = JournalConfig {
        cap_bytes: 1,
        ..config()
    };
    let outcome = enforce_budget_with(root.path(), &[first, bad], &cfg, &plentiful, &Faults::default(), &clock);
    assert!(
        matches!(outcome, BudgetOutcome::Partial { evicted, errors, pressure: Some(_) } if evicted.len() == 1 && evicted[0].session == "first" && errors.len() == 1)
    );
}

#[test]
fn review_eviction_post_rename_failure_still_reports_the_gap() {
    let root = Scratch::new();
    let clock = ManualClock::default();
    let closed = create_session(root.path(), "closed", Some(clock.now()));
    let fs = DirectoryFaults::default();
    fs.fail.store(true, Ordering::SeqCst);
    let cfg = JournalConfig {
        cap_bytes: 1,
        ..config()
    };
    let outcome = enforce_budget_with(root.path(), &[closed], &cfg, &plentiful, &fs, &clock);
    assert!(
        matches!(outcome, BudgetOutcome::Partial { evicted, errors, .. } if evicted.len() == 1 && errors.len() == 1)
    );
    let journal = Journal::open(&root.path().join("journal/closed"), &cfg).unwrap();
    gap(journal.read_after(0, 100), 1, 8, GapWhy::Evicted);
}

#[test]
fn review_pending_resolve_and_request_follow_post_rename_failures() {
    let root = Scratch::new();
    let path = root.path().join("pending/session.bin");
    let fs = Arc::new(DirectoryFaults::default());
    let mut pending = Pending::open_with(&path, fs.clone()).unwrap();
    pending.request(1, b"one", "one").unwrap();
    pending.request(2, b"two", "two").unwrap();
    fs.fail.store(true, Ordering::SeqCst);
    assert!(pending.resolved(3, "one").is_err());
    assert_eq!(pending.list(), vec![(2, b"two".to_vec())]);
    assert!(pending.request(4, b"three", "three").is_err());
    assert_eq!(Pending::open(&path).unwrap().list(), pending.list());
    fs.fail.store(false, Ordering::SeqCst);
    pending.request(5, b"four", "four").unwrap();
    assert_eq!(
        Pending::open(&path).unwrap().list(),
        vec![(2, b"two".to_vec()), (4, b"three".to_vec()), (5, b"four".to_vec())]
    );
}

#[test]
fn review_appending_after_an_eviction_marker_starts_a_new_segment() {
    let root = Scratch::new();
    let fs = Arc::new(Faults::default());
    let path = root.path().join("seg-00000000000000000001.log");
    fs.replace(
        &path,
        &encode(&Record::Event {
            sequence: 1,
            bytes: vec![1],
        })
        .unwrap(),
    )
    .unwrap();
    let marker = segment::evicted_path(root.path(), 2, 2);
    fs.replace(&marker, &[]).unwrap();
    let mut journal = Journal::open_with(root.path(), &config(), fs.clone(), Arc::new(ManualClock::default())).unwrap();
    journal.append(3, b"three").unwrap();
    journal.turn_ended().unwrap();
    drop(journal);
    let journal = Journal::open_with(root.path(), &config(), fs, Arc::new(ManualClock::default())).unwrap();
    gap(journal.read_after(1, 100), 2, 2, GapWhy::Evicted);
    assert_eq!(journal.read_after(2, 100).unwrap().len(), 1);
}

#[test]
fn review_failed_rollback_poison_keeps_the_original_error_and_unsynced_range() {
    struct RollbackFault(AtomicBool);
    impl FileSystem for RollbackFault {
        fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            if self.0.load(Ordering::SeqCst) {
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
            Disk.append(path, bytes)
        }
        fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
            if self.0.load(Ordering::SeqCst) {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            Disk.truncate(path, len)
        }
    }
    let root = Scratch::new();
    let fs = Arc::new(RollbackFault(AtomicBool::new(false)));
    let mut journal = Journal::open_with(root.path(), &config(), fs.clone(), Arc::new(ManualClock::default())).unwrap();
    journal.append(1, b"durable").unwrap();
    journal.turn_ended().unwrap();
    journal.append(2, b"uncertain").unwrap();
    fs.0.store(true, Ordering::SeqCst);
    assert_eq!(
        journal.append(3, b"lost").unwrap_err().raw_os_error(),
        Some(libc::ENOSPC)
    );
    fs.0.store(false, Ordering::SeqCst);
    assert_eq!(
        journal.append(4, b"later").unwrap_err().raw_os_error(),
        Some(libc::ENOSPC)
    );
    gap(journal.read_after(1, 100), 2, 3, GapWhy::WriteFailed);
}

#[test]
fn review_pending_binary_damage_and_unknown_formats_are_refused() {
    let root = Scratch::new();
    let path = root.path().join("pending/session.bin");
    let fs = Arc::new(Faults::default());
    fs.replace(&path, br#"{"one":[1,[0,128,255]]}"#).unwrap();
    assert!(Pending::open_with(&path, fs.clone()).is_err());
    fs.remove_file(&path).unwrap();
    let mut pending = Pending::open_with(&path, fs.clone()).unwrap();
    pending.request(1, &[0, 128, 255], "one").unwrap();
    pending.request(2, &[0, 255], "two").unwrap();
    let bytes = fs.read(&path).unwrap();
    assert!(bytes.starts_with(b"EPN2"));
    for cut in 1..bytes.len() {
        fs.replace(&path, &bytes[..cut]).unwrap();
        assert!(Pending::open_with(&path, fs.clone()).is_err(), "accepted cut {cut}");
    }
    // The first format has no watermark; its newest request stands for it.
    let mut first = b"EPN1".to_vec();
    first.extend(1u32.to_le_bytes());
    first.extend(3u32.to_le_bytes());
    first.extend(2u32.to_le_bytes());
    first.extend(5u64.to_le_bytes());
    first.extend(b"one");
    first.extend([0, 255]);
    fs.replace(&path, &first).unwrap();
    let pending = Pending::open_with(&path, fs.clone()).unwrap();
    assert_eq!(pending.list(), vec![(5, vec![0, 255])]);
    assert_eq!(pending.applied(), 5);
    // A request newer than the watermark beside it is damage.
    let mut ahead = b"EPN2".to_vec();
    ahead.extend(4u64.to_le_bytes());
    ahead.extend_from_slice(&first[4..]);
    fs.replace(&path, &ahead).unwrap();
    assert!(Pending::open_with(&path, fs.clone()).is_err());
}

#[test]
fn review_cleanup_does_not_remove_a_live_writers_temporary() {
    let root = Scratch::new();
    let fs = Arc::new(Faults::default());
    let temp = root.path().join(format!(".journal-{}-999999.tmp", std::process::id()));
    fs.replace(&temp, b"in flight").unwrap();
    let journal = Journal::open_with(root.path(), &config(), fs.clone(), Arc::new(ManualClock::default())).unwrap();
    assert!(temp.exists());
    drop(journal);
    enforce_budget_with(root.path(), &[], &config(), &plentiful, &*fs, &ManualClock::default());
    assert!(temp.exists());
}

#[test]
fn review_budget_does_not_bypass_an_unsafe_journal_parent() {
    for linked in [false, true] {
        let root = Scratch::new();
        let clock = ManualClock::default();
        let cfg = config();
        let closed = create_session(root.path(), "closed", Some(clock.now() - cfg.closed_keep));
        let journal_dir = root.path().join("journal");
        let kept = root.path().join("kept");
        let actual = if linked {
            std::fs::rename(&journal_dir, &kept).unwrap();
            std::os::unix::fs::symlink(&kept, &journal_dir).unwrap();
            kept
        } else {
            std::fs::set_permissions(&journal_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            journal_dir
        };
        let segment = segment_paths(&Disk, &actual.join("closed")).unwrap()[0].1.clone();
        let bytes = Disk.read(&segment).unwrap();
        let outcome = enforce_budget_with(root.path(), &[closed], &cfg, &plentiful, &Faults::default(), &clock);
        assert_eq!(Disk.read(&segment).unwrap(), bytes);
        assert!(matches!(outcome, BudgetOutcome::Partial { errors, .. } if !errors.is_empty()));
    }
}

#[test]
fn matching_partial_headers_are_cut_and_contradictions_are_refused() {
    let root = Scratch::new();
    let fs = Arc::new(Faults::default());
    let path = root.path().join("seg-00000000000000000001.log");
    let first = encode(&Record::Event { sequence: 1, bytes: vec![7] }).unwrap();
    for record in [
        Record::Event { sequence: 2, bytes: vec![8] },
        Record::Gap { first: 2, last: 900, why: GapWhy::NotObserved },
    ] {
        let frame = encode(&record).unwrap();
        for cut in 1..21 {
            let mut bytes = first.clone();
            bytes.extend(&frame[..cut]);
            fs.replace(&path, &bytes).unwrap();
            let journal = Journal::open_with(root.path(), &config(), fs.clone(), Arc::new(ManualClock::default())).unwrap();
            assert_eq!(journal.head(), 1);
            assert_eq!(fs.read(&path).unwrap(), first);
        }
    }
    let event = encode(&Record::Event { sequence: 2, bytes: vec![] }).unwrap();
    let mut bad_last = event[..10].to_vec();
    bad_last[9] = 3;
    let mut backwards_gap = encode(&Record::Gap { first: 2, last: 2, why: GapWhy::NotObserved }).unwrap()[..17].to_vec();
    backwards_gap[9] = 1;
    let mut gap_payload = encode(&Record::Gap { first: 2, last: 2, why: GapWhy::Evicted }).unwrap()[..20].to_vec();
    gap_payload[17] = 1;
    for tail in [vec![4], vec![0, 3], bad_last, backwards_gap, gap_payload] {
        let mut bytes = first.clone();
        bytes.extend(tail);
        fs.replace(&path, &bytes).unwrap();
        assert!(Journal::open_with(root.path(), &config(), fs.clone(), Arc::new(ManualClock::default())).is_err());
        assert_eq!(fs.read(&path).unwrap(), bytes);
    }
}

#[test]
fn failed_pending_removals_never_return_on_a_later_write() {
    for clear in [false, true] {
        for initially_persisted in [false, true] {
            let root = Scratch::new();
            let fs = Arc::new(Faults::default());
            let path = root.path().join("pending/session.bin");
            let mut pending = Pending::open_with(&path, fs.clone()).unwrap();
            fs.full.store(!initially_persisted, Ordering::SeqCst);
            assert_eq!(pending.request(7, b"answered", "a").is_ok(), initially_persisted);
            fs.full.store(true, Ordering::SeqCst);
            let removed = if clear { pending.clear(8) } else { pending.resolved(8, "a") };
            assert_eq!(removed.unwrap_err().raw_os_error(), Some(libc::ENOSPC));
            assert!(pending.list().is_empty());
            fs.full.store(false, Ordering::SeqCst);
            pending.request(9, b"new", "b").unwrap();
            assert_eq!(Pending::open_with(&path, fs).unwrap().list(), vec![(9, b"new".to_vec())]);
        }
    }
}

#[test]
fn existing_directory_parents_are_synced_at_every_open() {
    let root = Scratch::new();
    let dir = root.path().join("state/journal/session");
    let pending = root.path().join("state/pending/session.bin");
    Disk.create_dir(&dir).unwrap();
    Disk.create_dir(pending.parent().unwrap()).unwrap();
    let fs = Arc::new(DirectoryFaults::default());
    for _ in 0..2 {
        fs.synced.lock().unwrap().clear();
        drop(Journal::open_with(&dir, &config(), fs.clone(), Arc::new(ManualClock::default())).unwrap());
        drop(Pending::open_with(&pending, fs.clone()).unwrap());
        let synced = fs.synced.lock().unwrap();
        for parent in [root.path().join("state"), root.path().join("state/journal")] {
            assert!(synced.contains(&parent));
        }
    }
    fs.fail.store(true, Ordering::SeqCst);
    assert!(Journal::open_with(&dir, &config(), fs.clone(), Arc::new(ManualClock::default())).is_err());
    assert!(Pending::open_with(&pending, fs).is_err());
}

struct BootFaults {
    boot: Mutex<String>,
    faults: Faults,
}
impl BootFaults {
    fn new() -> Self {
        Self { boot: Mutex::new("boot-a".into()), faults: Faults::default() }
    }
}
impl FileSystem for BootFaults {
    fn boot_id(&self) -> io::Result<String> { Ok(self.boot.lock().unwrap().clone()) }
    fn sync(&self, path: &Path) -> io::Result<()> { self.faults.sync(path) }
    fn replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> { self.faults.replace(path, bytes) }
}

fn forget_process_poison(dir: &Path) {
    poison::POISONED.get().unwrap().lock().unwrap().remove(&std::fs::canonicalize(dir).unwrap());
}

#[test]
fn poison_survives_a_fresh_helper_and_reboot_recovers_only_verified_frames() {
    for corrupt in [false, true] {
        let root = Scratch::new();
        let fs = Arc::new(BootFaults::new());
        let clock = Arc::new(ManualClock::default());
        let mut journal = Journal::open_with(root.path(), &config(), fs.clone(), clock.clone()).unwrap();
        journal.append(1, b"durable").unwrap();
        journal.turn_ended().unwrap();
        journal.append(2, b"uncertain").unwrap();
        journal.append(3, b"uncertain").unwrap();
        fs.faults.fail_sync.store(true, Ordering::SeqCst);
        assert_eq!(journal.turn_ended().unwrap_err().raw_os_error(), Some(libc::EIO));
        fs.faults.fail_sync.store(false, Ordering::SeqCst);
        for sequence in 4..=100 {
            assert!(journal.append(sequence, b"refused").is_err());
        }
        assert_eq!(journal.entries.len(), 4);
        assert_eq!(journal.unwritten.len(), 1);
        gap(journal.read_after(3, 100), 4, 100, GapWhy::WriteFailed);
        let marker = fs.read(&root.path().join("poisoned")).unwrap();
        assert_eq!(marker, b"boot-a\n2 100\n");
        drop(journal);
        if corrupt {
            let path = segment_paths(&*fs, root.path()).unwrap()[0].1.clone();
            let mut bytes = fs.read(&path).unwrap();
            bytes[29 + b"durable".len() + 21] ^= 1;
            fs.replace(&path, &bytes).unwrap();
        }
        forget_process_poison(root.path());
        let mut journal = Journal::open_with(root.path(), &config(), fs.clone(), clock.clone()).unwrap();
        assert_eq!(journal.head(), 100);
        assert_eq!(journal.read_after(0, 100).unwrap().len(), 1);
        gap(journal.read_after(1, 100), 2, 100, GapWhy::WriteFailed);
        assert!(journal.append(101, b"refused").is_err());
        assert!(journal.turn_ended().is_err());
        drop(journal);
        forget_process_poison(root.path());
        *fs.boot.lock().unwrap() = "boot-b".into();
        let reopened = Journal::open_with(root.path(), &config(), fs.clone(), clock);
        if corrupt {
            let mut journal = reopened.unwrap();
            assert_eq!(journal.head(), 101);
            assert_eq!(journal.read_after(0, 1000).unwrap().len(), 1);
            gap(journal.read_after(1, 1000), 2, 101, GapWhy::WriteFailed);
            assert!(!root.path().join("poisoned").exists());
            journal.append(102, b"after reboot").unwrap();
            journal.turn_ended().unwrap();
            drop(journal);
            let journal =
                Journal::open_with(root.path(), &config(), fs.clone(), Arc::new(ManualClock::default())).unwrap();
            gap(journal.read_after(1, 1000), 2, 101, GapWhy::WriteFailed);
            assert_eq!(journal.read_after(101, 1000).unwrap().len(), 1);
        } else {
            let mut journal = reopened.unwrap();
            assert_eq!(journal.head(), 3);
            assert_eq!(journal.read_after(0, 1000).unwrap().len(), 3);
            assert!(!root.path().join("poisoned").exists());
            journal.append(4, b"after reboot").unwrap();
            journal.turn_ended().unwrap();
        }
    }
}

#[test]
fn poison_marker_failure_still_protects_in_process_reopens() {
    let root = Scratch::new();
    let fs = Arc::new(BootFaults::new());
    let clock = Arc::new(ManualClock::default());
    let mut journal = Journal::open_with(root.path(), &config(), fs.clone(), clock.clone()).unwrap();
    journal.append(1, b"uncertain").unwrap();
    fs.faults.fail_replace.store(true, Ordering::SeqCst);
    fs.faults.fail_sync.store(true, Ordering::SeqCst);
    assert!(journal.turn_ended().is_err());
    drop(journal);
    assert!(!root.path().join("poisoned").exists());
    let journal = Journal::open_with(root.path(), &config(), fs, clock).unwrap();
    gap(journal.read_after(0, 100), 1, 1, GapWhy::WriteFailed);
}

#[test]
fn replay_opens_each_segment_once_per_page() {
    #[derive(Default)]
    struct Opens(AtomicUsize);
    impl FileSystem for Opens {
        fn open_segment(&self, path: &Path) -> io::Result<Box<dyn SegmentReader>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Disk.open_segment(path)
        }
    }
    let root = Scratch::new();
    let fs = Arc::new(Opens::default());
    let mut journal = Journal::open_with(root.path(), &config(), fs.clone(), Arc::new(ManualClock::default())).unwrap();
    for sequence in 1..=100 { journal.append(sequence, &[9; 10]).unwrap(); }
    journal.turn_ended().unwrap();
    assert!(journal.segments.len() > 1);
    fs.0.store(0, Ordering::SeqCst);
    assert_eq!(journal.read_after(0, usize::MAX).unwrap().len(), 100);
    assert_eq!(fs.0.load(Ordering::SeqCst), journal.segments.len());
}

#[test]
fn expired_sessions_remove_damaged_segments_and_report_unknown_ranges() {
    for damage in [0, 1, 2] {
        let root = Scratch::new();
        let fs = Arc::new(Faults::default());
        let clock = Arc::new(ManualClock::default());
        let cfg = JournalConfig { segment_bytes: 32, ..config() };
        let dir = root.path().join("journal/expired");
        let mut journal = Journal::open_with(&dir, &cfg, fs.clone(), clock.clone()).unwrap();
        for sequence in 1..=3 { journal.append(sequence, b"one").unwrap(); }
        journal.turn_ended().unwrap();
        drop(journal);
        let paths = segment_paths(&*fs, &dir).unwrap();
        for (_, path) in &paths[..2] {
            let mut bytes = fs.read(path).unwrap();
            match damage {
                0 => bytes[21] ^= 1,
                1 => bytes = vec![0, 99],
                _ => bytes = encode(&Record::Event { sequence: 9, bytes: b"one".to_vec() }).unwrap(),
            }
            fs.replace(path, &bytes).unwrap();
        }
        fs.replace(&dir.join("poisoned"), b"boot-a\n1 3\n").unwrap();
        let usage = SessionUsage { session: "expired".into(), bytes: 0, closed_at: Some(clock.now() - cfg.closed_keep) };
        let outcome = enforce_budget_with(root.path(), &[usage], &cfg, &plentiful, &*fs, &*clock);
        assert!(!dir.exists(), "{outcome:?}");
        assert!(matches!(outcome, BudgetOutcome::Partial { errors, evicted, pressure: None }
            if errors.len() == 2 && evicted.len() == 1 && evicted[0].first == 3 && evicted[0].last == 3));
    }
}

#[test]
fn completed_eviction_markers_do_no_io_or_recount_and_expire_without_new_ranges() {
    #[derive(Default)]
    struct Counts {
        root_walks: AtomicUsize,
        truncates: AtomicUsize,
        syncs: AtomicUsize,
    }
    impl FileSystem for Counts {
        fn entries(&self, dir: &Path) -> io::Result<Vec<FileEntry>> {
            if dir.file_name().is_some_and(|name| name.to_string_lossy().starts_with("eitri-journal-")) {
                self.root_walks.fetch_add(1, Ordering::SeqCst);
            }
            Disk.entries(dir)
        }
        fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
            self.truncates.fetch_add(1, Ordering::SeqCst);
            Disk.truncate(path, len)
        }
        fn sync(&self, path: &Path) -> io::Result<()> {
            self.syncs.fetch_add(1, Ordering::SeqCst);
            Disk.sync(path)
        }
    }
    let root = Scratch::new();
    let dir = root.path().join("journal/closed");
    Disk.create_dir(&dir).unwrap();
    for sequence in 1..=20 {
        Disk.replace(&segment::evicted_path(&dir, sequence, sequence), &[]).unwrap();
    }
    let fs = Counts::default();
    let clock = ManualClock::default();
    let calls = AtomicUsize::new(0);
    let free = |_: &Path| { calls.fetch_add(1, Ordering::SeqCst); Ok(0) };
    let cfg = JournalConfig { free_floor_bytes: 1, ..config() };
    let mut usage = SessionUsage { session: "closed".into(), bytes: 0, closed_at: Some(clock.now()) };
    for _ in 0..2 {
        fs.root_walks.store(0, Ordering::SeqCst);
        calls.store(0, Ordering::SeqCst);
        let outcome = enforce_budget_with(root.path(), std::slice::from_ref(&usage), &cfg, &free, &fs, &clock);
        assert!(matches!(outcome, BudgetOutcome::BelowFloor { free: 0 }), "{outcome:?}");
        assert_eq!(fs.root_walks.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    usage.closed_at = Some(clock.now() - cfg.closed_keep);
    let outcome = enforce_budget_with(root.path(), &[usage], &cfg, &plentiful, &fs, &clock);
    assert!(matches!(outcome, BudgetOutcome::Within), "{outcome:?}");
    assert!(!dir.exists());
    assert_eq!(fs.truncates.load(Ordering::SeqCst), 0);
    assert_eq!(fs.syncs.load(Ordering::SeqCst), 0);
}

#[test]
fn a_nonempty_crash_marker_is_reclaimed_once() {
    let root = Scratch::new();
    let dir = root.path().join("journal/closed");
    let marker = segment::evicted_path(&dir, 1, 8);
    let fs = Faults::default();
    fs.replace(&marker, &[7; 100]).unwrap();
    let clock = ManualClock::default();
    let usage = SessionUsage { session: "closed".into(), bytes: 0, closed_at: Some(clock.now()) };
    let cfg = JournalConfig { cap_bytes: 1, ..config() };
    assert!(matches!(enforce_budget_with(root.path(), &[usage], &cfg, &plentiful, &fs, &clock), BudgetOutcome::Within));
    assert!(fs.read(&marker).unwrap().is_empty());
    assert_eq!(fs.syncs.load(Ordering::SeqCst), 1);
}

#[test]
fn unsafe_subtrees_are_counted_without_following_links_or_cleaning_files() {
    #[derive(Default)]
    struct Accounting(Mutex<Vec<PathBuf>>);
    impl FileSystem for Accounting {
        fn accounting_entries(&self, dir: &Path) -> io::Result<Vec<FileEntry>> {
            self.0.lock().unwrap().push(dir.into());
            Disk.accounting_entries(dir)
        }
    }
    let root = Scratch::new();
    let outside = Scratch::new();
    Disk.replace(&outside.path().join("large"), &[0; 20_000]).unwrap();
    let unsafe_dir = root.path().join("logs");
    Disk.replace(&unsafe_dir.join("nested/data"), &[1; 2000]).unwrap();
    let temp = unsafe_dir.join(".journal-2147483647-0.tmp");
    Disk.replace(&temp, &[2; 1000]).unwrap();
    std::os::unix::fs::symlink(outside.path(), unsafe_dir.join("linked")).unwrap();
    std::fs::set_permissions(&unsafe_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let fs = Accounting::default();
    let cfg = JournalConfig { cap_bytes: 2500, ..config() };
    let outcome = enforce_budget_with(root.path(), &[], &cfg, &plentiful, &fs, &ManualClock::default());
    assert!(matches!(outcome, BudgetOutcome::Partial { errors, pressure: Some(BudgetPressure::LiveOverCap { .. }), .. } if errors.len() == 1));
    assert!(temp.exists());
    assert_eq!(*fs.0.lock().unwrap(), vec![unsafe_dir.clone(), unsafe_dir.join("nested")]);
    let cfg = JournalConfig { cap_bytes: 4000, ..cfg };
    let outcome = enforce_budget_with(root.path(), &[], &cfg, &plentiful, &fs, &ManualClock::default());
    assert!(matches!(outcome, BudgetOutcome::Partial { pressure: None, .. }));
}

#[test]
fn eviction_rename_enospc_unlinks_all_closed_segments_and_keeps_live_data() {
    #[derive(Default)]
    struct FullRename(AtomicUsize);
    impl FileSystem for FullRename {
        fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::from_raw_os_error(libc::ENOSPC))
        }
        fn replace(&self, _: &Path, _: &[u8]) -> io::Result<()> {
            panic!("eviction allocated a temporary file");
        }
    }
    let root = Scratch::new();
    let clock = ManualClock::default();
    let cfg = JournalConfig { cap_bytes: 1, segment_bytes: 32, ..config() };
    let dir = root.path().join("journal/closed");
    let mut journal = Journal::open(&dir, &cfg).unwrap();
    for sequence in 1..=3 { journal.append(sequence, b"one").unwrap(); }
    journal.turn_ended().unwrap();
    drop(journal);
    let live = create_session(root.path(), "live", None);
    let live_path = segment_paths(&Disk, &root.path().join("journal/live")).unwrap()[0].1.clone();
    let live_bytes = Disk.read(&live_path).unwrap();
    let closed = SessionUsage { session: "closed".into(), bytes: 0, closed_at: Some(clock.now()) };
    let fs = FullRename::default();
    let outcome = enforce_budget_with(root.path(), &[closed, live], &cfg, &plentiful, &fs, &clock);
    assert!(!dir.exists());
    assert_eq!(Disk.read(&live_path).unwrap(), live_bytes);
    assert_eq!(fs.0.load(Ordering::SeqCst), 1);
    match outcome {
        BudgetOutcome::Partial { evicted, errors, pressure: Some(_) } => {
            assert!(errors.is_empty());
            assert_eq!(evicted.iter().map(|range| (range.first, range.last)).collect::<Vec<_>>(), vec![(1, 1), (2, 2), (3, 3)]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn poisoned_closed_sessions_reopen_after_boundary_eviction() {
    for durable_prefix in [false, true] {
        for extend in [false, true] {
            for fresh_helper in [false, true] {
                let root = Scratch::new();
                let dir = root.path().join("journal/closed");
                let fs = Arc::new(BootFaults::new());
                let clock = Arc::new(ManualClock::default());
                let cfg = JournalConfig {
                    cap_bytes: 1,
                    ..config()
                };
                let mut journal = Journal::open_with(&dir, &cfg, fs.clone(), clock.clone()).unwrap();
                journal.append(1, b"one").unwrap();
                if durable_prefix {
                    journal.turn_ended().unwrap();
                }
                journal.append(2, b"two").unwrap();
                journal.append(3, b"three").unwrap();
                fs.faults.fail_sync.store(true, Ordering::SeqCst);
                assert!(journal.turn_ended().is_err());
                fs.faults.fail_sync.store(false, Ordering::SeqCst);
                if extend {
                    assert!(journal.append(5, b"refused").is_err());
                }
                drop(journal);
                let usage = SessionUsage {
                    session: "closed".into(),
                    bytes: 0,
                    closed_at: Some(clock.now()),
                };
                enforce_budget_with(root.path(), &[usage], &cfg, &plentiful, &*fs, &*clock);
                assert!(segment::evicted_path(&dir, 1, 3).exists());
                if fresh_helper {
                    forget_process_poison(&dir);
                }
                let mut journal = Journal::open_with(&dir, &cfg, fs.clone(), clock.clone()).unwrap();
                gap(journal.read_after(0, 1000), 1, 3, GapWhy::Evicted);
                if extend {
                    assert_eq!(journal.syncer.state.0.lock().unwrap().failed, vec![(4, 5)]);
                    assert_eq!(journal.syncer.state.0.lock().unwrap().poison_last, 5);
                    gap(journal.read_after(3, 1000), 4, 5, GapWhy::WriteFailed);
                    assert!(journal.append(6, b"refused").is_err());
                    assert!(journal.turn_ended().is_err());
                } else {
                    assert!(journal.syncer.state.0.lock().unwrap().failed.is_empty());
                    journal.append(4, b"after eviction").unwrap();
                    journal.turn_ended().unwrap();
                    drop(journal);
                    let journal = Journal::open_with(&dir, &cfg, fs, clock).unwrap();
                    gap(journal.read_after(0, 1000), 1, 3, GapWhy::Evicted);
                    assert_eq!(journal.read_after(3, 1000).unwrap().len(), 1);
                }
            }
        }
    }
}

#[test]
fn failure_readers_and_gap_writers_wait_for_the_durable_marker() {
    use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
    struct HeldMarker {
        entered: Sender<()>,
        release: Mutex<Receiver<()>>,
        hold: AtomicBool,
    }
    impl FileSystem for HeldMarker {
        fn boot_id(&self) -> io::Result<String> {
            Ok("boot-a".into())
        }
        fn sync(&self, _: &Path) -> io::Result<()> {
            Err(io::Error::from_raw_os_error(libc::EIO))
        }
        fn replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            if path.file_name().unwrap() == "poisoned" && self.hold.swap(false, Ordering::SeqCst) {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(60))
                    .unwrap();
            }
            Disk.replace(path, bytes)
        }
    }
    for why in [None, Some(GapWhy::NotObserved), Some(GapWhy::Evicted)] {
        let root = Scratch::new();
        let (entered_tx, entered_rx) = channel();
        let (release_tx, release_rx) = channel();
        let (event_tx, event_rx) = channel();
        let fs = Arc::new(HeldMarker {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            hold: AtomicBool::new(true),
        });
        let clock = Arc::new(ManualClock::default());
        let mut journal = Journal::open_with(root.path(), &config(), fs, clock.clone()).unwrap();
        *journal.syncer.failure_wait.lock().unwrap() = Some(event_tx.clone());
        journal.append(1, b"uncertain").unwrap();
        clock.advance(200);
        journal.syncer.state.1.notify_one();
        entered_rx.recv_timeout(Duration::from_secs(60)).unwrap();
        let caller = std::thread::spawn(move || {
            if let Some(why) = why {
                assert_eq!(
                    journal.record_gap(2, 4, why).unwrap_err().raw_os_error(),
                    Some(libc::EIO)
                );
            } else {
                gap(journal.read_after(0, 100), 1, 1, GapWhy::WriteFailed);
            }
            event_tx.send(FailureIoEvent::Returned).unwrap();
            journal
        });
        // This notification means the caller encountered the still-held marker I/O lock.
        let waiting = event_rx.recv_timeout(Duration::from_secs(60));
        let premature = event_rx.try_recv();
        let marker_existed = root.path().join("poisoned").exists();
        release_tx.send(()).unwrap();
        let journal = caller.join().unwrap();
        assert!(
            matches!(waiting, Ok(FailureIoEvent::Waiting)),
            "failure escaped without waiting for marker persistence"
        );
        assert!(matches!(premature, Err(TryRecvError::Empty)));
        assert!(!marker_existed);
        let last = if why.is_some() { 4 } else { 1 };
        assert_eq!(
            Disk.read(&root.path().join("poisoned")).unwrap(),
            format!("boot-a\n1 {last}\n").as_bytes()
        );
        drop(journal);
        forget_process_poison(root.path());
        let fs = Arc::new(BootFaults::new());
        let journal = Journal::open_with(root.path(), &config(), fs, clock).unwrap();
        gap(journal.read_after(0, 100), 1, last, GapWhy::WriteFailed);
    }
}

#[test]
fn reboot_damage_is_recovered_only_inside_the_poison_range() {
    for segment_bytes in [32, 1024] {
        for damaged in 1..=4 {
            for damage in 0..5 {
                let root = Scratch::new();
                let fs = Arc::new(BootFaults::new());
                *fs.boot.lock().unwrap() = "boot-b".into();
                let cfg = JournalConfig {
                    segment_bytes,
                    ..config()
                };
                let mut journal =
                    Journal::open_with(root.path(), &cfg, fs.clone(), Arc::new(ManualClock::default())).unwrap();
                let written_last = if damaged == 4 { 4 } else { 3 };
                for sequence in 1..=written_last {
                    journal.append(sequence, b"one").unwrap();
                }
                journal.turn_ended().unwrap();
                let location = journal.entries[damaged - 1].location.as_ref().unwrap();
                let (path, offset) = (location.path.clone(), location.offset);
                drop(journal);
                let mut bytes = fs.read(&path).unwrap();
                match damage {
                    0 => bytes[offset + 21] ^= 1,
                    1 => bytes[offset..offset + 32].fill(0),
                    2 => bytes.truncate(offset + 7),
                    3 => bytes[offset] = 9,
                    _ => bytes[offset..offset + 32].copy_from_slice(
                        &encode(&Record::Event {
                            sequence: 99,
                            bytes: b"one".to_vec(),
                        })
                        .unwrap(),
                    ),
                }
                fs.replace(&path, &bytes).unwrap();
                fs.replace(&root.path().join("poisoned"), b"boot-a\n2 3\n").unwrap();
                let opened = Journal::open_with(root.path(), &cfg, fs.clone(), Arc::new(ManualClock::default()));
                if damaged == 1 || damaged == 4 {
                    // An ordinary plausible torn final frame still uses the usual tail recovery.
                    if damaged == 4 && (damage == 1 || damage == 2) {
                        assert_eq!(opened.unwrap().head(), 3);
                    } else if damaged == 1 && damage == 2 && segment_bytes == 1024 {
                        assert_eq!(opened.unwrap().head(), 0);
                    } else {
                        assert!(
                            opened.is_err(),
                            "segment_bytes={segment_bytes}, damaged={damaged}, damage={damage}"
                        );
                        assert_eq!(fs.read(&path).unwrap(), bytes);
                        assert!(root.path().join("poisoned").exists());
                    }
                    continue;
                }
                let mut journal = opened.unwrap();
                assert!(!root.path().join("poisoned").exists());
                let prefix = journal.read_after(0, 1000).unwrap();
                assert_eq!(prefix.len(), damaged - 1);
                gap(
                    journal.read_after(damaged as u64 - 1, 1000),
                    damaged as u64,
                    3,
                    GapWhy::WriteFailed,
                );
                assert_eq!(journal.head(), 3);
                journal.append(4, b"new").unwrap();
                journal.turn_ended().unwrap();
                drop(journal);
                let journal = Journal::open_with(root.path(), &cfg, fs, Arc::new(ManualClock::default())).unwrap();
                assert_eq!(journal.head(), 4);
                gap(
                    journal.read_after(damaged as u64 - 1, 1000),
                    damaged as u64,
                    3,
                    GapWhy::WriteFailed,
                );
            }
        }
    }
}

#[test]
fn interrupted_reboot_repair_keeps_a_durable_gap_and_retries_cleanup() {
    struct RepairFaults(AtomicUsize);
    impl FileSystem for RepairFaults {
        fn boot_id(&self) -> io::Result<String> {
            Ok("boot-b".into())
        }
        fn replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            if path.file_name().unwrap().to_string_lossy().starts_with("seg-") {
                if self
                    .0
                    .compare_exchange(1, 0, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
                if self
                    .0
                    .compare_exchange(2, 0, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    Disk.replace(path, bytes)?;
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
                Disk.replace(path, bytes)?;
                let _ = self.0.compare_exchange(5, 6, Ordering::SeqCst, Ordering::SeqCst);
                return Ok(());
            }
            Disk.replace(path, bytes)
        }
        fn remove_file(&self, path: &Path) -> io::Result<()> {
            let stage = if path.file_name().unwrap() == "poisoned" { 4 } else { 3 };
            if self
                .0
                .compare_exchange(stage, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            Disk.remove_file(path)
        }
        fn sync_dir(&self, path: &Path) -> io::Result<()> {
            if self
                .0
                .compare_exchange(6, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            Disk.sync_dir(path)
        }
    }
    for stage in 1..=5 {
        let root = Scratch::new();
        let fs = Arc::new(RepairFaults(AtomicUsize::new(0)));
        let cfg = JournalConfig {
            segment_bytes: 32,
            ..config()
        };
        let mut journal = Journal::open_with(root.path(), &cfg, fs.clone(), Arc::new(ManualClock::default())).unwrap();
        for sequence in 1..=3 {
            journal.append(sequence, b"one").unwrap();
        }
        journal.turn_ended().unwrap();
        drop(journal);
        let paths = segment_paths(&*fs, root.path()).unwrap();
        let prefix = fs.read(&paths[0].1).unwrap();
        let mut damaged = fs.read(&paths[1].1).unwrap();
        damaged[21] ^= 1;
        fs.replace(&paths[1].1, &damaged).unwrap();
        fs.replace(&root.path().join("poisoned"), b"boot-a\n2 8\n").unwrap();
        fs.0.store(stage, Ordering::SeqCst);
        assert!(Journal::open_with(root.path(), &cfg, fs.clone(), Arc::new(ManualClock::default())).is_err());
        assert!(root.path().join("poisoned").exists());
        assert_eq!(fs.read(&paths[0].1).unwrap(), prefix);
        if stage == 1 {
            assert_eq!(fs.read(&paths[1].1).unwrap(), damaged);
        }
        let mut journal = Journal::open_with(root.path(), &cfg, fs.clone(), Arc::new(ManualClock::default())).unwrap();
        assert_eq!(journal.read_after(0, 1000).unwrap().len(), 1);
        gap(journal.read_after(1, 1000), 2, 8, GapWhy::WriteFailed);
        assert_eq!(journal.head(), 8);
        assert!(!root.path().join("poisoned").exists());
        assert!(!paths[2].1.exists());
        journal.append(9, b"new").unwrap();
        journal.turn_ended().unwrap();
        drop(journal);
        let journal = Journal::open_with(root.path(), &cfg, fs, Arc::new(ManualClock::default())).unwrap();
        assert_eq!(journal.head(), 9);
        gap(journal.read_after(1, 1000), 2, 8, GapWhy::WriteFailed);
    }
}
