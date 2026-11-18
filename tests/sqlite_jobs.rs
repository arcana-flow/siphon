use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use rusqlite::Connection;
use siphon::domain::{CreatorId, Timestamp, UploadId};
use siphon::error::StorageError;
use siphon::jobs::{
    CreatorJobOutcome, DisplayUrl, DownloadJobOutcome, DownloadProgress, DownloadStage,
    ExecutionMode, InterruptedJobOutcome, JobCompletion, JobId, JobOutcome, JobProgress, JobState,
    JobStore, JobTarget, OperationSlot, ProcessOwner, ReconcileJobOutcome,
    ReconcileReservationOutcome, RefreshJobOutcome, RefreshProgress, StartJobRequest,
};
use siphon::storage::SqliteStorage;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "siphon-sqlite-jobs-{}-{}-{id}",
            label,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self { path: dir }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn owner(pid: u32, started_at: Option<&str>) -> ProcessOwner {
    ProcessOwner::new(pid, started_at.map(str::to_owned)).unwrap()
}

#[test]
fn refresh_job_lifecycle_persists_progress_mixed_outcomes_and_releases_slot() {
    let temp = TempDir::new("refresh-lifecycle");
    let db_path = temp.path().join("jobs.db");
    let storage = SqliteStorage::open(&db_path).unwrap();

    let t0 = Timestamp::from_unix_seconds(1_760_000_000);
    let t1 = Timestamp::from_unix_seconds(1_760_000_010);
    let t2 = Timestamp::from_unix_seconds(1_760_000_020);
    let c1 = CreatorId::new(1).unwrap();
    let c2 = CreatorId::new(2).unwrap();

    let started = storage
        .start_background_job(StartJobRequest {
            target: JobTarget::Refresh { creator_id: None },
            owner: owner(1111, Some("boot-100")),
            started_at: t0,
            initial_progress: Some(JobProgress::Refresh(RefreshProgress {
                total_creators: Some(2),
                processed_creators: 0,
                succeeded_creators: 0,
                failed_creators: 0,
                current_creator_id: Some(c1),
                current_creator_name: Some("Alice".to_owned()),
            })),
        })
        .unwrap();

    assert_eq!(started.job.state, JobState::Running);
    assert_eq!(started.lease.slot, Some(OperationSlot::Refresh));

    let active = storage
        .get_active_reservation(OperationSlot::Refresh)
        .unwrap()
        .unwrap();
    assert_eq!(active.execution_mode, ExecutionMode::Background);
    assert_eq!(active.job_id, Some(started.job.id));
    assert_eq!(active.owner_token, started.lease.owner_token);

    // Concurrent refresh (even targeting a single creator) must be rejected while active.
    let conflict = storage
        .start_background_job(StartJobRequest {
            target: JobTarget::Refresh {
                creator_id: Some(c1),
            },
            owner: owner(2222, Some("boot-200")),
            started_at: t1,
            initial_progress: None,
        })
        .unwrap_err();
    assert_eq!(
        conflict,
        StorageError::OperationBusy {
            active: Box::new(active),
        }
    );

    let updated = storage
        .update_job_progress(
            &started.lease,
            JobProgress::Refresh(RefreshProgress {
                total_creators: Some(2),
                processed_creators: 1,
                succeeded_creators: 1,
                failed_creators: 0,
                current_creator_id: Some(c2),
                current_creator_name: Some("Bob".to_owned()),
            }),
            t1,
        )
        .unwrap();
    assert_eq!(updated.updated_at, t1);

    // Partial creator failure makes the refresh job failed while retaining all creator outcomes.
    let mixed_outcome = JobOutcome::Refresh(RefreshJobOutcome {
        creator_outcomes: vec![
            CreatorJobOutcome::Succeeded {
                creator_id: c1,
                display_name: "Alice".to_owned(),
                new_uploads_inserted: 3,
                existing_uploads_seen: 1,
                previous_checkpoint: Timestamp::from_unix_seconds(1_759_999_000),
                new_checkpoint: t1,
            },
            CreatorJobOutcome::Failed {
                creator_id: c2,
                display_name: "Bob".to_owned(),
                error: "rate limited".to_owned(),
            },
        ],
        error: Some("1 of 2 creators failed".to_owned()),
    });

    // Cannot finish as Succeeded when a creator failed.
    assert!(
        storage
            .finish_job(
                &started.lease,
                JobCompletion::Succeeded {
                    progress: None,
                    outcome: mixed_outcome.clone(),
                    finished_at: t2,
                },
            )
            .is_err()
    );

    let finished = storage
        .finish_job(
            &started.lease,
            JobCompletion::Failed {
                progress: Some(JobProgress::Refresh(RefreshProgress {
                    total_creators: Some(2),
                    processed_creators: 2,
                    succeeded_creators: 1,
                    failed_creators: 1,
                    current_creator_id: None,
                    current_creator_name: None,
                })),
                outcome: mixed_outcome.clone(),
                finished_at: t2,
            },
        )
        .unwrap();

    assert_eq!(finished.state, JobState::Failed);
    assert_eq!(finished.finished_at, Some(t2));
    assert_eq!(finished.outcome, Some(mixed_outcome));
    assert_eq!(
        storage
            .get_active_reservation(OperationSlot::Refresh)
            .unwrap(),
        None
    );

    // Terminal record stays queryable across reopen.
    drop(storage);
    let reopened = SqliteStorage::open(&db_path).unwrap();
    let queried = reopened.get_job(started.job.id).unwrap();
    assert_eq!(queried, finished);

    // New refresh can start after previous refresh finished.
    let second = reopened
        .start_background_job(StartJobRequest {
            target: JobTarget::Refresh {
                creator_id: Some(c1),
            },
            owner: owner(3333, None),
            started_at: Timestamp::from_unix_seconds(1_760_000_030),
            initial_progress: None,
        })
        .unwrap();
    assert!(second.job.id > started.job.id);
    assert!(second.lease.owner_token > started.lease.owner_token);
}

#[test]
fn download_lifecycle_and_occupancy_rules_isolate_uploads_and_allow_direct_downloads() {
    let storage = SqliteStorage::open_in_memory().unwrap();
    let t0 = Timestamp::from_unix_seconds(1_760_000_000);
    let t1 = Timestamp::from_unix_seconds(1_760_000_005);
    let t2 = Timestamp::from_unix_seconds(1_760_000_010);

    let u1 = UploadId::new(10).unwrap();
    let u2 = UploadId::new(20).unwrap();

    // Start refresh, upload 10 download, upload 20 download, and two direct downloads concurrently.
    let refresh = storage
        .start_background_job(StartJobRequest {
            target: JobTarget::Refresh { creator_id: None },
            owner: owner(100, None),
            started_at: t0,
            initial_progress: None,
        })
        .unwrap();

    let upload_10 = storage
        .start_background_job(StartJobRequest {
            target: JobTarget::UploadDownload {
                upload_id: u1,
                url: Some(
                    DisplayUrl::sanitize("https://user:pass@www.bilibili.com/video/BV10?t=1#p")
                        .unwrap(),
                ),
            },
            owner: owner(101, Some("s-101")),
            started_at: t0,
            initial_progress: Some(JobProgress::Download(DownloadProgress {
                stage: DownloadStage::Preparing,
                downloaded_bytes: None,
                total_bytes: None,
            })),
        })
        .unwrap();

    let upload_20 = storage
        .start_background_job(StartJobRequest {
            target: JobTarget::UploadDownload {
                upload_id: u2,
                url: None,
            },
            owner: owner(102, None),
            started_at: t0,
            initial_progress: None,
        })
        .unwrap();

    let direct_a = storage
        .start_background_job(StartJobRequest {
            target: JobTarget::DirectDownload {
                url: DisplayUrl::sanitize(
                    "https://alice:secret@example.com/playlist?auth=tok#frag",
                )
                .unwrap(),
            },
            owner: owner(103, None),
            started_at: t0,
            initial_progress: None,
        })
        .unwrap();

    let direct_b = storage
        .start_background_job(StartJobRequest {
            target: JobTarget::DirectDownload {
                url: DisplayUrl::sanitize("https://example.com/playlist?auth=other").unwrap(),
            },
            owner: owner(104, None),
            started_at: t0,
            initial_progress: None,
        })
        .unwrap();

    assert_eq!(direct_a.lease.slot, None);
    assert_eq!(direct_b.lease.slot, None);

    // Same upload 10 cannot run concurrently (foreground or background).
    let fg_conflict = storage
        .acquire_reservation(OperationSlot::UploadDownload(u1), owner(105, None), t1)
        .unwrap_err();
    assert!(matches!(fg_conflict, StorageError::OperationBusy { .. }));

    // Update download progress with optional transfer totals.
    let prog = storage
        .update_job_progress(
            &upload_10.lease,
            JobProgress::Download(DownloadProgress {
                stage: DownloadStage::Downloading,
                downloaded_bytes: Some(512),
                total_bytes: Some(1024),
            }),
            t1,
        )
        .unwrap();
    assert_eq!(
        prog.progress,
        Some(JobProgress::Download(DownloadProgress {
            stage: DownloadStage::Downloading,
            downloaded_bytes: Some(512),
            total_bytes: Some(1024),
        }))
    );

    // Finish upload 10 and direct_a (direct_a simulates with no confirmed artifacts).
    let done_u10 = storage
        .finish_job(
            &upload_10.lease,
            JobCompletion::Succeeded {
                progress: Some(JobProgress::Download(DownloadProgress {
                    stage: DownloadStage::Verifying,
                    downloaded_bytes: Some(1024),
                    total_bytes: Some(1024),
                })),
                outcome: JobOutcome::Download(DownloadJobOutcome {
                    confirmed_artifacts: vec![PathBuf::from("video.mp4")],
                    marked_downloaded: true,
                    error: None,
                }),
                finished_at: t2,
            },
        )
        .unwrap();
    assert_eq!(done_u10.state, JobState::Succeeded);
    assert_eq!(
        storage
            .get_active_reservation(OperationSlot::UploadDownload(u1))
            .unwrap(),
        None
    );

    let done_direct_a = storage
        .finish_job(
            &direct_a.lease,
            JobCompletion::Succeeded {
                progress: None,
                outcome: JobOutcome::Download(DownloadJobOutcome {
                    confirmed_artifacts: Vec::new(),
                    marked_downloaded: false,
                    error: None,
                }),
                finished_at: t2,
            },
        )
        .unwrap();
    assert_eq!(done_direct_a.state, JobState::Succeeded);

    // Clean up remaining active jobs cleanly.
    storage
        .finish_job(
            &upload_20.lease,
            JobCompletion::Failed {
                progress: None,
                outcome: JobOutcome::Download(DownloadJobOutcome {
                    confirmed_artifacts: Vec::new(),
                    marked_downloaded: false,
                    error: Some("yt-dlp exited with code 1".to_owned()),
                }),
                finished_at: t2,
            },
        )
        .unwrap();
    storage
        .finish_job(
            &refresh.lease,
            JobCompletion::Succeeded {
                progress: None,
                outcome: JobOutcome::Refresh(RefreshJobOutcome {
                    creator_outcomes: Vec::new(),
                    error: None,
                }),
                finished_at: t2,
            },
        )
        .unwrap();
    storage
        .finish_job(
            &direct_b.lease,
            JobCompletion::Failed {
                progress: None,
                outcome: JobOutcome::Download(DownloadJobOutcome {
                    confirmed_artifacts: Vec::new(),
                    marked_downloaded: false,
                    error: Some("network timeout".to_owned()),
                }),
                finished_at: t2,
            },
        )
        .unwrap();
}

#[test]
fn target_urls_stored_in_sqlite_redact_credentials_query_and_fragment() {
    let temp = TempDir::new("redacted-urls");
    let db_path = temp.path().join("redacted.db");
    let storage = SqliteStorage::open(&db_path).unwrap();

    let started = storage
        .start_background_job(StartJobRequest {
            target: JobTarget::DirectDownload {
                url: DisplayUrl::sanitize(
                    "https://myuser:supersecret@media.example.com/watch?sess=abc123#t=42",
                )
                .unwrap(),
            },
            owner: owner(400, None),
            started_at: Timestamp::from_unix_seconds(1_760_000_000),
            initial_progress: None,
        })
        .unwrap();

    assert_eq!(
        started.job.target,
        JobTarget::DirectDownload {
            url: DisplayUrl::sanitize("https://media.example.com/watch").unwrap(),
        }
    );
    drop(storage);

    let conn = Connection::open(&db_path).unwrap();
    let raw_stored: String = conn
        .query_row(
            "SELECT target_url FROM jobs WHERE id = ?1",
            [started.job.id.as_i64()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(raw_stored, "https://media.example.com/watch");
    assert!(!raw_stored.contains("myuser"));
    assert!(!raw_stored.contains("supersecret"));
    assert!(!raw_stored.contains("sess=abc123"));
    assert!(!raw_stored.contains("t=42"));
}

#[test]
fn foreground_reservations_do_not_create_public_job_history() {
    let temp = TempDir::new("foreground-reservations");
    let db_path = temp.path().join("fg.db");
    let storage = SqliteStorage::open(&db_path).unwrap();

    let t0 = Timestamp::from_unix_seconds(1_760_000_000);
    let t1 = Timestamp::from_unix_seconds(1_760_000_005);

    let fg = storage
        .acquire_reservation(OperationSlot::Refresh, owner(500, Some("fg-1")), t0)
        .unwrap();

    let active = storage
        .get_active_reservation(OperationSlot::Refresh)
        .unwrap()
        .unwrap();
    assert_eq!(active.execution_mode, ExecutionMode::Foreground);
    assert_eq!(active.job_id, None);
    assert_eq!(active.owner_token, fg.owner_token);

    // Background refresh sees the foreground reservation as busy.
    let conflict = storage
        .start_background_job(StartJobRequest {
            target: JobTarget::Refresh { creator_id: None },
            owner: owner(501, None),
            started_at: t1,
            initial_progress: None,
        })
        .unwrap_err();
    assert_eq!(
        conflict,
        StorageError::OperationBusy {
            active: Box::new(active),
        }
    );

    // Release foreground reservation; verify zero rows in `jobs`.
    storage.release_reservation(&fg).unwrap();
    assert_eq!(
        storage
            .get_active_reservation(OperationSlot::Refresh)
            .unwrap(),
        None
    );

    // Releasing the same foreground reservation twice is rejected as stale.
    let stale_release = storage.release_reservation(&fg).unwrap_err();
    assert!(matches!(
        stale_release,
        StorageError::StaleJobOwnership {
            job_id: None,
            slot: Some(OperationSlot::Refresh),
        }
    ));

    drop(storage);
    let conn = Connection::open(&db_path).unwrap();
    let job_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(job_count, 0);
}

#[test]
fn conditional_reconciliation_and_stale_writer_fencing_protect_newer_ownership() {
    let storage = SqliteStorage::open_in_memory().unwrap();
    let t0 = Timestamp::from_unix_seconds(1_760_000_000);
    let t1 = Timestamp::from_unix_seconds(1_760_000_010);
    let t2 = Timestamp::from_unix_seconds(1_760_000_020);
    let t3 = Timestamp::from_unix_seconds(1_760_000_030);
    let upload_id = UploadId::new(77).unwrap();

    // 1. Dead foreground reservation reconciled conditionally.
    let fg_dead = storage
        .acquire_reservation(OperationSlot::Refresh, owner(600, Some("dead-fg")), t0)
        .unwrap();
    let fg_reconciled = storage
        .reconcile_dead_reservation(
            OperationSlot::Refresh,
            fg_dead.owner_token,
            "foreground process exited",
            t1,
        )
        .unwrap();
    assert_eq!(
        fg_reconciled,
        ReconcileReservationOutcome::Reconciled {
            released_slot: OperationSlot::Refresh,
            interrupted_job: None,
        }
    );
    // Stale foreground owner cannot release after reconciliation.
    assert!(matches!(
        storage.release_reservation(&fg_dead),
        Err(StorageError::StaleJobOwnership { .. })
    ));

    // 2. Dead background exclusive job reconciled via reservation; newer owner takes over.
    let bg_dead = storage
        .start_background_job(StartJobRequest {
            target: JobTarget::UploadDownload {
                upload_id,
                url: None,
            },
            owner: owner(700, Some("dead-bg")),
            started_at: t0,
            initial_progress: Some(JobProgress::Download(DownloadProgress {
                stage: DownloadStage::Downloading,
                downloaded_bytes: Some(256),
                total_bytes: Some(1024),
            })),
        })
        .unwrap();

    let bg_reconciled = storage
        .reconcile_dead_reservation(
            OperationSlot::UploadDownload(upload_id),
            bg_dead.lease.owner_token,
            "worker process 700 vanished",
            t1,
        )
        .unwrap();
    let interrupted_record = match bg_reconciled {
        ReconcileReservationOutcome::Reconciled {
            released_slot,
            interrupted_job: Some(job),
        } => {
            assert_eq!(released_slot, OperationSlot::UploadDownload(upload_id));
            assert_eq!(job.state, JobState::Interrupted);
            assert_eq!(job.finished_at, Some(t1));
            assert_eq!(job.progress, bg_dead.job.progress);
            assert_eq!(
                job.outcome,
                Some(JobOutcome::Interrupted(InterruptedJobOutcome {
                    reason: "worker process 700 vanished".to_owned(),
                }))
            );
            job
        }
        other => panic!("unexpected reconciliation outcome: {other:?}"),
    };

    // Newer worker acquires the same upload slot.
    let bg_new = storage
        .start_background_job(StartJobRequest {
            target: JobTarget::UploadDownload {
                upload_id,
                url: None,
            },
            owner: owner(701, Some("live-bg")),
            started_at: t2,
            initial_progress: None,
        })
        .unwrap();
    assert!(bg_new.lease.owner_token > bg_dead.lease.owner_token);

    // Stale worker cannot update progress or finish the old job, and cannot touch newer slot.
    assert!(matches!(
        storage.update_job_progress(
            &bg_dead.lease,
            JobProgress::Download(DownloadProgress {
                stage: DownloadStage::Verifying,
                downloaded_bytes: Some(1024),
                total_bytes: Some(1024),
            }),
            t3,
        ),
        Err(StorageError::StaleJobOwnership { .. })
    ));
    assert!(matches!(
        storage.finish_job(
            &bg_dead.lease,
            JobCompletion::Succeeded {
                progress: None,
                outcome: JobOutcome::Download(DownloadJobOutcome {
                    confirmed_artifacts: vec![PathBuf::from("stale.mp4")],
                    marked_downloaded: true,
                    error: None,
                }),
                finished_at: t3,
            },
        ),
        Err(StorageError::StaleJobOwnership { .. })
    ));

    // Stale reconciler carrying old owner_token cannot evict newer owner `bg_new`!
    let stale_reconcile = storage
        .reconcile_dead_reservation(
            OperationSlot::UploadDownload(upload_id),
            bg_dead.lease.owner_token,
            "delayed stale reconcile",
            t3,
        )
        .unwrap();
    match stale_reconcile {
        ReconcileReservationOutcome::StaleOrReleased {
            current_active: Some(active),
        } => {
            assert_eq!(active.owner_token, bg_new.lease.owner_token);
            assert_eq!(active.job_id, Some(bg_new.job.id));
        }
        other => panic!("expected StaleOrReleased with new active owner, got {other:?}"),
    }
    assert_eq!(storage.get_job(bg_dead.job.id).unwrap(), interrupted_record);
    assert_eq!(storage.get_job(bg_new.job.id).unwrap(), bg_new.job);

    // 3. Direct download job (no occupancy slot) reconciled via reconcile_interrupted_job.
    let direct_dead = storage
        .start_background_job(StartJobRequest {
            target: JobTarget::DirectDownload {
                url: DisplayUrl::sanitize("https://example.com/direct.mp4").unwrap(),
            },
            owner: owner(800, Some("dead-direct")),
            started_at: t0,
            initial_progress: None,
        })
        .unwrap();

    let direct_reconciled = storage
        .reconcile_interrupted_job(
            direct_dead.job.id,
            direct_dead.lease.owner_token,
            "direct worker crashed",
            t1,
        )
        .unwrap();
    assert!(matches!(
        direct_reconciled,
        ReconcileJobOutcome::Reconciled(ref job) if job.state == JobState::Interrupted
    ));

    // Racing second reconcile on the same direct job does not apply.
    let second_reconcile = storage
        .reconcile_interrupted_job(
            direct_dead.job.id,
            direct_dead.lease.owner_token,
            "duplicate reconcile",
            t2,
        )
        .unwrap();
    assert!(matches!(
        second_reconcile,
        ReconcileJobOutcome::NotApplied { ref current } if current.state == JobState::Interrupted
    ));

    // Missing job returns JobNotFound.
    assert_eq!(
        storage.get_job(JobId::new(9999).unwrap()).unwrap_err(),
        StorageError::JobNotFound {
            job_id: JobId::new(9999).unwrap(),
        }
    );
}

#[test]
fn independent_connections_contend_atomically_on_exclusive_slots() {
    let temp = TempDir::new("connection-contention");
    let db_path = temp.path().join("contention.db");
    drop(SqliteStorage::open(&db_path).unwrap());

    let upload_id = UploadId::new(42).unwrap();
    let workers = 12usize;
    let barrier = Arc::new(Barrier::new(workers));
    let now = Timestamp::from_unix_seconds(1_760_000_000);

    // Race 12 independent SqliteStorage connections to acquire the UploadDownload(42) slot.
    let handles: Vec<_> = (0..workers)
        .map(|idx| {
            let path = db_path.clone();
            let start = Arc::clone(&barrier);
            thread::spawn(move || {
                let conn_storage = SqliteStorage::open(&path).unwrap();
                start.wait();
                conn_storage.start_background_job(StartJobRequest {
                    target: JobTarget::UploadDownload {
                        upload_id,
                        url: None,
                    },
                    owner: owner(1000 + idx as u32, Some(&format!("proc-{idx}"))),
                    started_at: now,
                    initial_progress: None,
                })
            })
        })
        .collect();

    let mut winners = Vec::new();
    let mut conflicts = Vec::new();
    for handle in handles {
        match handle.join().unwrap() {
            Ok(started) => winners.push(started),
            Err(StorageError::OperationBusy { active }) => conflicts.push(*active),
            Err(other) => panic!("unexpected contention error: {other:?}"),
        }
    }

    assert_eq!(winners.len(), 1);
    assert_eq!(conflicts.len(), workers - 1);
    let winner = &winners[0];
    for active in &conflicts {
        assert_eq!(active.slot, OperationSlot::UploadDownload(upload_id));
        assert_eq!(active.job_id, Some(winner.job.id));
        assert_eq!(active.owner_token, winner.lease.owner_token);
    }

    // Now race 12 independent connections on foreground refresh reservations + direct jobs.
    let fg_barrier = Arc::new(Barrier::new(workers));
    let fg_handles: Vec<_> = (0..workers)
        .map(|idx| {
            let path = db_path.clone();
            let start = Arc::clone(&fg_barrier);
            thread::spawn(move || {
                let conn_storage = SqliteStorage::open(&path).unwrap();
                start.wait();
                let direct = conn_storage
                    .start_background_job(StartJobRequest {
                        target: JobTarget::DirectDownload {
                            url: DisplayUrl::sanitize(&format!(
                                "https://example.com/item/{idx}?secret={idx}"
                            ))
                            .unwrap(),
                        },
                        owner: owner(2000 + idx as u32, None),
                        started_at: now,
                        initial_progress: None,
                    })
                    .unwrap();
                let res = conn_storage.acquire_reservation(
                    OperationSlot::Refresh,
                    owner(2000 + idx as u32, None),
                    now,
                );
                (direct, res)
            })
        })
        .collect();

    let mut fg_winners = Vec::new();
    let mut fg_conflicts = 0usize;
    for handle in fg_handles {
        let (direct, res) = handle.join().unwrap();
        assert_eq!(direct.job.state, JobState::Running);
        match res {
            Ok(reservation) => fg_winners.push(reservation),
            Err(StorageError::OperationBusy { .. }) => fg_conflicts += 1,
            Err(other) => panic!("unexpected foreground contention error: {other:?}"),
        }
    }

    assert_eq!(fg_winners.len(), 1);
    assert_eq!(fg_conflicts, workers - 1);
}
