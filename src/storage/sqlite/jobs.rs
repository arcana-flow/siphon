use std::path::PathBuf;
use std::str::FromStr;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::domain::{CreatorId, Timestamp, UploadId};
use crate::error::StorageError;
use crate::jobs::{
    ActiveOperation, CreatorJobOutcome, DisplayUrl, DownloadJobOutcome, DownloadProgress,
    DownloadStage, ExecutionMode, InterruptedJobOutcome, JobCompletion, JobId, JobKind, JobLease,
    JobOutcome, JobProgress, JobRecord, JobState, JobStore, JobTarget, OperationReservation,
    OperationSlot, OwnershipToken, ProcessOwner, ReconcileJobOutcome, ReconcileReservationOutcome,
    RefreshJobOutcome, RefreshProgress, StartJobRequest, StartedJob,
    validate_outcome_for_state_and_kind,
};
use crate::storage::migrations::{AttemptError, retry_storage_on_busy};

use super::SqliteStorage;

impl JobStore for SqliteStorage {
    fn acquire_reservation(
        &self,
        slot: OperationSlot,
        owner: ProcessOwner,
        acquired_at: Timestamp,
    ) -> Result<OperationReservation, StorageError> {
        let owner = validate_process_owner(owner)?;
        self.with_immediate_tx("failed to acquire operation reservation", |tx| {
            if let Some(active) = fetch_active_operation(tx, slot)? {
                return Err(AttemptError::Storage(StorageError::OperationBusy {
                    active: Box::new(active),
                }));
            }

            let owner_token = allocate_ownership_token(tx)?;
            insert_reservation(
                tx,
                slot,
                ExecutionMode::Foreground,
                None,
                owner_token,
                &owner,
                acquired_at,
            )?;

            Ok(OperationReservation {
                slot,
                owner_token,
                owner: owner.clone(),
                acquired_at,
            })
        })
    }

    fn release_reservation(&self, reservation: &OperationReservation) -> Result<(), StorageError> {
        self.with_immediate_tx("failed to release operation reservation", |tx| {
            let slot_key = reservation.slot.slot_key();
            let deleted = tx
                .execute(
                    "DELETE FROM operation_reservations
                     WHERE slot_key = ?1
                       AND owner_token = ?2
                       AND execution_mode = 'foreground'
                       AND job_id IS NULL",
                    params![slot_key, reservation.owner_token.as_i64()],
                )
                .map_err(AttemptError::Sqlite)?;

            if deleted != 1 {
                return Err(AttemptError::Storage(StorageError::StaleJobOwnership {
                    job_id: None,
                    slot: Some(reservation.slot),
                }));
            }
            Ok(())
        })
    }

    fn start_background_job(&self, request: StartJobRequest) -> Result<StartedJob, StorageError> {
        let owner = validate_process_owner(request.owner)?;
        let kind = request.target.kind();
        let slot = request.target.occupancy_slot();

        if let Some(progress) = &request.initial_progress {
            progress.validate_for_kind(kind)?;
        }

        let progress_json = request
            .initial_progress
            .as_ref()
            .map(encode_progress)
            .transpose()?;
        let encoded_target = encode_target(&request.target)?;

        self.with_immediate_tx("failed to start background job", |tx| {
            if let Some(slot) = slot
                && let Some(active) = fetch_active_operation(tx, slot)?
            {
                return Err(AttemptError::Storage(StorageError::OperationBusy {
                    active: Box::new(active),
                }));
            }

            let owner_token = allocate_ownership_token(tx)?;
            let started_unix = request.started_at.as_unix_seconds();
            let owner_pid = i64::from(owner.pid);

            tx.execute(
                "INSERT INTO jobs (
                    kind,
                    target_creator_id,
                    target_upload_id,
                    target_url,
                    state,
                    started_at,
                    updated_at,
                    finished_at,
                    progress_json,
                    outcome_json,
                    owner_token,
                    owner_pid,
                    owner_process_started_at
                 )
                 VALUES (?1, ?2, ?3, ?4, 'running', ?5, ?5, NULL, ?6, NULL, ?7, ?8, ?9)",
                params![
                    kind.as_str(),
                    encoded_target.creator_id,
                    encoded_target.upload_id,
                    encoded_target.url,
                    started_unix,
                    progress_json,
                    owner_token.as_i64(),
                    owner_pid,
                    owner.process_started_at,
                ],
            )
            .map_err(AttemptError::Sqlite)?;

            let job_id = job_id_from_i64(tx.last_insert_rowid()).map_err(AttemptError::Storage)?;

            if let Some(slot) = slot {
                insert_reservation(
                    tx,
                    slot,
                    ExecutionMode::Background,
                    Some(job_id),
                    owner_token,
                    &owner,
                    request.started_at,
                )?;
            }

            let job = JobRecord {
                id: job_id,
                target: request.target.clone(),
                state: JobState::Running,
                started_at: request.started_at,
                updated_at: request.started_at,
                finished_at: None,
                progress: request.initial_progress.clone(),
                outcome: None,
                owner_token,
                owner: owner.clone(),
            };
            let lease = JobLease {
                job_id,
                owner_token,
                slot,
            };

            Ok(StartedJob { job, lease })
        })
    }

    fn update_job_progress(
        &self,
        lease: &JobLease,
        progress: JobProgress,
        updated_at: Timestamp,
    ) -> Result<JobRecord, StorageError> {
        self.with_immediate_tx("failed to update job progress", |tx| {
            let mut job = fetch_job(tx, lease.job_id)?;
            ensure_lease_matches(&job, lease)?;

            progress
                .validate_for_kind(job.target.kind())
                .map_err(AttemptError::Storage)?;
            let progress_json = encode_progress(&progress).map_err(AttemptError::Storage)?;

            let updated = tx
                .execute(
                    "UPDATE jobs
                     SET progress_json = ?1, updated_at = ?2
                     WHERE id = ?3 AND owner_token = ?4 AND state = 'running'",
                    params![
                        progress_json,
                        updated_at.as_unix_seconds(),
                        lease.job_id.as_i64(),
                        lease.owner_token.as_i64(),
                    ],
                )
                .map_err(AttemptError::Sqlite)?;
            if updated != 1 {
                return Err(AttemptError::Storage(StorageError::StaleJobOwnership {
                    job_id: Some(lease.job_id),
                    slot: job.target.occupancy_slot(),
                }));
            }

            job.progress = Some(progress.clone());
            job.updated_at = updated_at;
            Ok(job)
        })
    }

    fn finish_job(
        &self,
        lease: &JobLease,
        completion: JobCompletion,
    ) -> Result<JobRecord, StorageError> {
        self.with_immediate_tx("failed to finish job", |tx| {
            let mut job = fetch_job(tx, lease.job_id)?;
            ensure_lease_matches(&job, lease)?;

            let kind = job.target.kind();
            completion
                .validate_for_kind(kind)
                .map_err(AttemptError::Storage)?;

            let next_state = completion.target_state();
            let finished_at = completion.finished_at();
            let progress_json = completion
                .progress()
                .map(encode_progress)
                .transpose()
                .map_err(AttemptError::Storage)?;
            let outcome_json =
                encode_outcome(completion.outcome()).map_err(AttemptError::Storage)?;

            let updated = tx
                .execute(
                    "UPDATE jobs
                     SET state = ?1,
                         updated_at = ?2,
                         finished_at = ?2,
                         progress_json = COALESCE(?3, progress_json),
                         outcome_json = ?4
                     WHERE id = ?5 AND owner_token = ?6 AND state = 'running'",
                    params![
                        next_state.as_str(),
                        finished_at.as_unix_seconds(),
                        progress_json,
                        outcome_json,
                        lease.job_id.as_i64(),
                        lease.owner_token.as_i64(),
                    ],
                )
                .map_err(AttemptError::Sqlite)?;
            if updated != 1 {
                return Err(AttemptError::Storage(StorageError::StaleJobOwnership {
                    job_id: Some(lease.job_id),
                    slot: job.target.occupancy_slot(),
                }));
            }

            release_job_reservation_in_tx(
                tx,
                lease.job_id,
                lease.owner_token,
                job.target.occupancy_slot(),
            )?;

            job.state = next_state;
            job.updated_at = finished_at;
            job.finished_at = Some(finished_at);
            if let Some(progress) = completion.progress() {
                job.progress = Some(progress.clone());
            }
            job.outcome = Some(completion.outcome().clone());
            Ok(job)
        })
    }

    fn reconcile_dead_reservation(
        &self,
        slot: OperationSlot,
        expected_owner_token: OwnershipToken,
        reason: &str,
        finished_at: Timestamp,
    ) -> Result<ReconcileReservationOutcome, StorageError> {
        let reason = validate_non_empty_reason(reason)?;
        self.with_immediate_tx("failed to reconcile dead reservation", |tx| {
            let Some(active) = fetch_active_operation(tx, slot)? else {
                return Ok(ReconcileReservationOutcome::StaleOrReleased {
                    current_active: None,
                });
            };
            if active.owner_token != expected_owner_token {
                return Ok(ReconcileReservationOutcome::StaleOrReleased {
                    current_active: Some(active),
                });
            }

            let interrupted_job = match active.job_id {
                Some(job_id) => Some(mark_job_interrupted_in_tx(
                    tx,
                    job_id,
                    expected_owner_token,
                    &reason,
                    finished_at,
                )?),
                None => None,
            };

            let deleted = tx
                .execute(
                    "DELETE FROM operation_reservations WHERE slot_key = ?1 AND owner_token = ?2",
                    params![slot.slot_key(), expected_owner_token.as_i64()],
                )
                .map_err(AttemptError::Sqlite)?;
            if deleted != 1 {
                return Err(AttemptError::Storage(StorageError::CorruptedData(format!(
                    "expected 1 reservation row deleted for {slot}, deleted {deleted}"
                ))));
            }

            Ok(ReconcileReservationOutcome::Reconciled {
                released_slot: slot,
                interrupted_job,
            })
        })
    }

    fn reconcile_interrupted_job(
        &self,
        job_id: JobId,
        expected_owner_token: OwnershipToken,
        reason: &str,
        finished_at: Timestamp,
    ) -> Result<ReconcileJobOutcome, StorageError> {
        let reason = validate_non_empty_reason(reason)?;
        self.with_immediate_tx("failed to reconcile interrupted job", |tx| {
            let current = fetch_job(tx, job_id)?;
            if current.state != JobState::Running || current.owner_token != expected_owner_token {
                return Ok(ReconcileJobOutcome::NotApplied { current });
            }

            let updated_job =
                mark_job_interrupted_in_tx(tx, job_id, expected_owner_token, &reason, finished_at)?;
            release_job_reservation_in_tx(
                tx,
                job_id,
                expected_owner_token,
                current.target.occupancy_slot(),
            )?;

            Ok(ReconcileJobOutcome::Reconciled(updated_job))
        })
    }

    fn get_job(&self, job_id: JobId) -> Result<JobRecord, StorageError> {
        let conn = self.lock_conn()?;
        retry_storage_on_busy("failed to fetch job record", || fetch_job(&conn, job_id))
    }

    fn get_active_reservation(
        &self,
        slot: OperationSlot,
    ) -> Result<Option<ActiveOperation>, StorageError> {
        let conn = self.lock_conn()?;
        retry_storage_on_busy("failed to query operation reservation", || {
            fetch_active_operation(&conn, slot)
        })
    }
}

impl SqliteStorage {
    fn with_immediate_tx<T, F>(&self, context: &str, mut op: F) -> Result<T, StorageError>
    where
        F: FnMut(&Transaction<'_>) -> Result<T, AttemptError>,
    {
        let mut conn = self.lock_conn()?;
        retry_storage_on_busy(context, || {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(AttemptError::Sqlite)?;
            let value = op(&tx)?;
            tx.commit().map_err(AttemptError::Sqlite)?;
            Ok(value)
        })
    }
}

fn validate_process_owner(owner: ProcessOwner) -> Result<ProcessOwner, StorageError> {
    ProcessOwner::new(owner.pid, owner.process_started_at)
        .map_err(|err| StorageError::Backend(err.to_string()))
}

fn validate_non_empty_reason(reason: &str) -> Result<String, StorageError> {
    let trimmed = reason.trim();
    if trimmed.is_empty() {
        return Err(StorageError::Backend(
            "interruption reason cannot be empty".to_owned(),
        ));
    }
    Ok(trimmed.to_owned())
}

fn ensure_lease_matches(job: &JobRecord, lease: &JobLease) -> Result<(), AttemptError> {
    let expected_slot = job.target.occupancy_slot();
    if job.state != JobState::Running
        || job.owner_token != lease.owner_token
        || lease.slot != expected_slot
    {
        return Err(AttemptError::Storage(StorageError::StaleJobOwnership {
            job_id: Some(lease.job_id),
            slot: expected_slot,
        }));
    }
    Ok(())
}

fn allocate_ownership_token(tx: &Transaction<'_>) -> Result<OwnershipToken, AttemptError> {
    let token: i64 = tx
        .query_row(
            "SELECT next_token FROM ownership_sequence WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(AttemptError::Sqlite)?
        .ok_or_else(|| {
            AttemptError::Storage(StorageError::CorruptedData(
                "ownership_sequence singleton row is missing".to_owned(),
            ))
        })?;

    let next_token = token.checked_add(1).ok_or_else(|| {
        AttemptError::Storage(StorageError::Backend(
            "ownership_sequence counter overflowed i64".to_owned(),
        ))
    })?;

    let updated = tx
        .execute(
            "UPDATE ownership_sequence SET next_token = ?1 WHERE id = 1",
            [next_token],
        )
        .map_err(AttemptError::Sqlite)?;
    if updated != 1 {
        return Err(AttemptError::Storage(StorageError::CorruptedData(
            "expected 1 ownership_sequence row updated".to_owned(),
        )));
    }

    OwnershipToken::new(token)
        .map_err(|err| AttemptError::Storage(StorageError::CorruptedData(err.to_string())))
}

fn insert_reservation(
    tx: &Transaction<'_>,
    slot: OperationSlot,
    mode: ExecutionMode,
    job_id: Option<JobId>,
    owner_token: OwnershipToken,
    owner: &ProcessOwner,
    acquired_at: Timestamp,
) -> Result<(), AttemptError> {
    let (operation_kind, upload_id) = match slot {
        OperationSlot::Refresh => ("refresh", None),
        OperationSlot::UploadDownload(id) => ("upload_download", Some(id.as_i64())),
    };

    tx.execute(
        "INSERT INTO operation_reservations (
            slot_key,
            operation_kind,
            upload_id,
            execution_mode,
            job_id,
            owner_token,
            owner_pid,
            owner_process_started_at,
            acquired_at
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            slot.slot_key(),
            operation_kind,
            upload_id,
            mode.as_str(),
            job_id.map(JobId::as_i64),
            owner_token.as_i64(),
            i64::from(owner.pid),
            owner.process_started_at,
            acquired_at.as_unix_seconds(),
        ],
    )
    .map_err(AttemptError::Sqlite)?;

    Ok(())
}

fn release_job_reservation_in_tx(
    tx: &Transaction<'_>,
    job_id: JobId,
    owner_token: OwnershipToken,
    slot: Option<OperationSlot>,
) -> Result<(), AttemptError> {
    let deleted = tx
        .execute(
            "DELETE FROM operation_reservations WHERE job_id = ?1 AND owner_token = ?2",
            params![job_id.as_i64(), owner_token.as_i64()],
        )
        .map_err(AttemptError::Sqlite)?;

    match (slot, deleted) {
        (Some(_), 1) | (None, 0) => Ok(()),
        (Some(slot), actual) => Err(AttemptError::Storage(StorageError::CorruptedData(format!(
            "expected 1 reservation row for job {job_id} ({slot}), deleted {actual}"
        )))),
        (None, actual) => Err(AttemptError::Storage(StorageError::CorruptedData(format!(
            "unexpected {actual} reservation row(s) linked to direct download job {job_id}"
        )))),
    }
}

fn mark_job_interrupted_in_tx(
    tx: &Transaction<'_>,
    job_id: JobId,
    expected_owner_token: OwnershipToken,
    reason: &str,
    finished_at: Timestamp,
) -> Result<JobRecord, AttemptError> {
    let outcome = JobOutcome::Interrupted(InterruptedJobOutcome {
        reason: reason.to_owned(),
    });
    let outcome_json = encode_outcome(&outcome).map_err(AttemptError::Storage)?;

    let updated = tx
        .execute(
            "UPDATE jobs
             SET state = 'interrupted',
                 updated_at = ?1,
                 finished_at = ?1,
                 outcome_json = ?2
             WHERE id = ?3 AND owner_token = ?4 AND state = 'running'",
            params![
                finished_at.as_unix_seconds(),
                outcome_json,
                job_id.as_i64(),
                expected_owner_token.as_i64(),
            ],
        )
        .map_err(AttemptError::Sqlite)?;

    if updated != 1 {
        return Err(AttemptError::Storage(StorageError::CorruptedData(format!(
            "expected 1 running job row updated for interruption of job {job_id}, updated {updated}"
        ))));
    }

    fetch_job(tx, job_id)
}

fn fetch_active_operation(
    conn: &Connection,
    slot: OperationSlot,
) -> Result<Option<ActiveOperation>, AttemptError> {
    let row = conn
        .query_row(
            "SELECT operation_kind, upload_id, execution_mode, job_id,
                    owner_token, owner_pid, owner_process_started_at, acquired_at
             FROM operation_reservations
             WHERE slot_key = ?1",
            [slot.slot_key()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )
        .optional()
        .map_err(AttemptError::Sqlite)?;

    let Some((
        operation_kind,
        upload_id,
        execution_mode,
        job_id,
        owner_token,
        owner_pid,
        owner_process_started_at,
        acquired_at,
    )) = row
    else {
        return Ok(None);
    };

    let decoded_slot = match (operation_kind.as_str(), upload_id) {
        ("refresh", None) => OperationSlot::Refresh,
        ("upload_download", Some(raw_id)) => {
            let id = UploadId::new(raw_id).map_err(|err| {
                AttemptError::Storage(StorageError::CorruptedData(format!(
                    "invalid upload_id in operation_reservations: {err}"
                )))
            })?;
            OperationSlot::UploadDownload(id)
        }
        _ => {
            return Err(AttemptError::Storage(StorageError::CorruptedData(format!(
                "inconsistent operation_reservations slot: kind={operation_kind}, upload_id={upload_id:?}"
            ))));
        }
    };
    if decoded_slot != slot {
        return Err(AttemptError::Storage(StorageError::CorruptedData(format!(
            "operation_reservations slot mismatch: expected {slot}, found {decoded_slot}"
        ))));
    }

    let execution_mode = ExecutionMode::from_str(&execution_mode).map_err(|err| {
        AttemptError::Storage(StorageError::CorruptedData(format!(
            "invalid execution_mode in operation_reservations: {err}"
        )))
    })?;
    let job_id = job_id
        .map(job_id_from_i64)
        .transpose()
        .map_err(AttemptError::Storage)?;
    match (execution_mode, job_id) {
        (ExecutionMode::Foreground, None) | (ExecutionMode::Background, Some(_)) => {}
        _ => {
            return Err(AttemptError::Storage(StorageError::CorruptedData(format!(
                "inconsistent operation_reservations mode={execution_mode} and job_id={job_id:?}"
            ))));
        }
    }

    let owner_token = token_from_i64(owner_token).map_err(AttemptError::Storage)?;
    let owner =
        owner_from_parts(owner_pid, owner_process_started_at).map_err(AttemptError::Storage)?;

    Ok(Some(ActiveOperation {
        slot: decoded_slot,
        execution_mode,
        job_id,
        owner_token,
        owner,
        acquired_at: Timestamp::from_unix_seconds(acquired_at),
    }))
}

fn fetch_job(conn: &Connection, job_id: JobId) -> Result<JobRecord, AttemptError> {
    let row = conn
        .query_row(
            "SELECT id, kind, target_creator_id, target_upload_id, target_url,
                    state, started_at, updated_at, finished_at,
                    progress_json, outcome_json,
                    owner_token, owner_pid, owner_process_started_at
             FROM jobs
             WHERE id = ?1",
            [job_id.as_i64()],
            |row| {
                Ok(RawJobRow {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    target_creator_id: row.get(2)?,
                    target_upload_id: row.get(3)?,
                    target_url: row.get(4)?,
                    state: row.get(5)?,
                    started_at: row.get(6)?,
                    updated_at: row.get(7)?,
                    finished_at: row.get(8)?,
                    progress_json: row.get(9)?,
                    outcome_json: row.get(10)?,
                    owner_token: row.get(11)?,
                    owner_pid: row.get(12)?,
                    owner_process_started_at: row.get(13)?,
                })
            },
        )
        .optional()
        .map_err(AttemptError::Sqlite)?
        .ok_or(AttemptError::Storage(StorageError::JobNotFound { job_id }))?;

    decode_job_row(row).map_err(AttemptError::Storage)
}

struct RawJobRow {
    id: i64,
    kind: String,
    target_creator_id: Option<i64>,
    target_upload_id: Option<i64>,
    target_url: Option<String>,
    state: String,
    started_at: i64,
    updated_at: i64,
    finished_at: Option<i64>,
    progress_json: Option<String>,
    outcome_json: Option<String>,
    owner_token: i64,
    owner_pid: i64,
    owner_process_started_at: Option<String>,
}

fn decode_job_row(row: RawJobRow) -> Result<JobRecord, StorageError> {
    let id = job_id_from_i64(row.id)?;
    let kind = JobKind::from_str(&row.kind).map_err(|err| {
        StorageError::CorruptedData(format!("invalid job kind '{}' in SQLite: {err}", row.kind))
    })?;
    let target = decode_target(
        kind,
        row.target_creator_id,
        row.target_upload_id,
        row.target_url,
    )?;
    let state = JobState::from_str(&row.state).map_err(|err| {
        StorageError::CorruptedData(format!(
            "invalid job state '{}' in SQLite: {err}",
            row.state
        ))
    })?;
    let finished_at = row.finished_at.map(Timestamp::from_unix_seconds);

    let progress = row
        .progress_json
        .as_deref()
        .map(|json| decode_progress(kind, json))
        .transpose()?;
    let outcome = row
        .outcome_json
        .as_deref()
        .map(|json| decode_outcome(state, kind, json))
        .transpose()?;

    match (state, finished_at, &outcome) {
        (JobState::Running, None, None) => {}
        (JobState::Succeeded | JobState::Failed | JobState::Interrupted, Some(_), Some(_)) => {}
        _ => {
            return Err(StorageError::CorruptedData(format!(
                "job {id} has inconsistent state={state}, finished_at={finished_at:?}, outcome_present={}",
                outcome.is_some()
            )));
        }
    }

    let owner_token = token_from_i64(row.owner_token)?;
    let owner = owner_from_parts(row.owner_pid, row.owner_process_started_at)?;

    Ok(JobRecord {
        id,
        target,
        state,
        started_at: Timestamp::from_unix_seconds(row.started_at),
        updated_at: Timestamp::from_unix_seconds(row.updated_at),
        finished_at,
        progress,
        outcome,
        owner_token,
        owner,
    })
}

struct EncodedTarget {
    creator_id: Option<i64>,
    upload_id: Option<i64>,
    url: Option<String>,
}

fn encode_target(target: &JobTarget) -> Result<EncodedTarget, StorageError> {
    match target {
        JobTarget::Refresh { creator_id } => Ok(EncodedTarget {
            creator_id: creator_id.map(CreatorId::as_i64),
            upload_id: None,
            url: None,
        }),
        JobTarget::UploadDownload { upload_id, url } => {
            let url = url
                .as_ref()
                .map(|display| {
                    ensure_sanitized_stored_url(display.as_str()).map(|d| d.as_str().to_owned())
                })
                .transpose()?;
            Ok(EncodedTarget {
                creator_id: None,
                upload_id: Some(upload_id.as_i64()),
                url,
            })
        }
        JobTarget::DirectDownload { url } => {
            let safe = ensure_sanitized_stored_url(url.as_str())?;
            Ok(EncodedTarget {
                creator_id: None,
                upload_id: None,
                url: Some(safe.as_str().to_owned()),
            })
        }
    }
}

fn decode_target(
    kind: JobKind,
    target_creator_id: Option<i64>,
    target_upload_id: Option<i64>,
    target_url: Option<String>,
) -> Result<JobTarget, StorageError> {
    match (kind, target_creator_id, target_upload_id, target_url) {
        (JobKind::Refresh, creator_id, None, None) => {
            let creator_id = creator_id
                .map(|raw| {
                    CreatorId::new(raw).map_err(|err| {
                        StorageError::CorruptedData(format!(
                            "invalid target_creator_id in jobs: {err}"
                        ))
                    })
                })
                .transpose()?;
            Ok(JobTarget::Refresh { creator_id })
        }
        (JobKind::UploadDownload, None, Some(upload_id), url) => {
            let upload_id = UploadId::new(upload_id).map_err(|err| {
                StorageError::CorruptedData(format!("invalid target_upload_id in jobs: {err}"))
            })?;
            let url = url.map(|raw| decode_sanitized_url(&raw)).transpose()?;
            Ok(JobTarget::UploadDownload { upload_id, url })
        }
        (JobKind::DirectDownload, None, None, Some(url)) => {
            let url = decode_sanitized_url(&url)?;
            Ok(JobTarget::DirectDownload { url })
        }
        _ => Err(StorageError::CorruptedData(format!(
            "inconsistent job target columns for kind '{kind}'"
        ))),
    }
}

fn ensure_sanitized_stored_url(raw: &str) -> Result<DisplayUrl, StorageError> {
    DisplayUrl::sanitize(raw).map_err(|err| StorageError::Backend(err.to_string()))
}

fn decode_sanitized_url(raw: &str) -> Result<DisplayUrl, StorageError> {
    let sanitized = DisplayUrl::sanitize(raw)
        .map_err(|err| StorageError::CorruptedData(format!("invalid target_url in jobs: {err}")))?;
    if sanitized.as_str() != raw {
        return Err(StorageError::CorruptedData(
            "jobs.target_url contained unsanitized credentials, query, or fragment".to_owned(),
        ));
    }
    Ok(sanitized)
}

fn job_id_from_i64(id: i64) -> Result<JobId, StorageError> {
    JobId::new(id)
        .map_err(|err| StorageError::CorruptedData(format!("invalid job ID in SQLite: {err}")))
}

fn token_from_i64(token: i64) -> Result<OwnershipToken, StorageError> {
    OwnershipToken::new(token).map_err(|err| {
        StorageError::CorruptedData(format!("invalid ownership token in SQLite: {err}"))
    })
}

fn owner_from_parts(
    pid: i64,
    process_started_at: Option<String>,
) -> Result<ProcessOwner, StorageError> {
    let pid = u32::try_from(pid)
        .map_err(|_| StorageError::CorruptedData(format!("invalid owner_pid '{pid}' in SQLite")))?;
    ProcessOwner::new(pid, process_started_at)
        .map_err(|err| StorageError::CorruptedData(format!("invalid owner in SQLite: {err}")))
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredJobProgress {
    Refresh {
        total_creators: Option<u64>,
        processed_creators: u64,
        succeeded_creators: u64,
        failed_creators: u64,
        current_creator_id: Option<i64>,
        current_creator_name: Option<String>,
    },
    Download {
        stage: String,
        downloaded_bytes: Option<u64>,
        total_bytes: Option<u64>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum StoredCreatorOutcome {
    Succeeded {
        creator_id: i64,
        display_name: String,
        new_uploads_inserted: u64,
        existing_uploads_seen: u64,
        previous_checkpoint: i64,
        new_checkpoint: i64,
    },
    Failed {
        creator_id: i64,
        display_name: String,
        error: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredJobOutcome {
    Refresh {
        creator_outcomes: Vec<StoredCreatorOutcome>,
        error: Option<String>,
    },
    Download {
        confirmed_artifacts: Vec<String>,
        marked_downloaded: bool,
        error: Option<String>,
    },
    Interrupted {
        reason: String,
    },
}

fn encode_progress(progress: &JobProgress) -> Result<String, StorageError> {
    let dto = match progress {
        JobProgress::Refresh(refresh) => StoredJobProgress::Refresh {
            total_creators: refresh.total_creators,
            processed_creators: refresh.processed_creators,
            succeeded_creators: refresh.succeeded_creators,
            failed_creators: refresh.failed_creators,
            current_creator_id: refresh.current_creator_id.map(CreatorId::as_i64),
            current_creator_name: refresh.current_creator_name.clone(),
        },
        JobProgress::Download(download) => StoredJobProgress::Download {
            stage: download.stage.as_str().to_owned(),
            downloaded_bytes: download.downloaded_bytes,
            total_bytes: download.total_bytes,
        },
    };
    serde_json::to_string(&dto)
        .map_err(|err| StorageError::Backend(format!("failed to serialize job progress: {err}")))
}

fn decode_progress(kind: JobKind, raw_json: &str) -> Result<JobProgress, StorageError> {
    let dto: StoredJobProgress = serde_json::from_str(raw_json).map_err(|err| {
        StorageError::CorruptedData(format!("invalid progress_json in jobs: {err}"))
    })?;
    let progress = match dto {
        StoredJobProgress::Refresh {
            total_creators,
            processed_creators,
            succeeded_creators,
            failed_creators,
            current_creator_id,
            current_creator_name,
        } => {
            let current_creator_id = current_creator_id
                .map(|raw| {
                    CreatorId::new(raw).map_err(|err| {
                        StorageError::CorruptedData(format!(
                            "invalid current_creator_id in progress_json: {err}"
                        ))
                    })
                })
                .transpose()?;
            JobProgress::Refresh(RefreshProgress {
                total_creators,
                processed_creators,
                succeeded_creators,
                failed_creators,
                current_creator_id,
                current_creator_name,
            })
        }
        StoredJobProgress::Download {
            stage,
            downloaded_bytes,
            total_bytes,
        } => {
            let stage = DownloadStage::from_str(&stage).map_err(|err| {
                StorageError::CorruptedData(format!(
                    "invalid download stage '{stage}' in progress_json: {err}"
                ))
            })?;
            JobProgress::Download(DownloadProgress {
                stage,
                downloaded_bytes,
                total_bytes,
            })
        }
    };
    progress
        .validate_for_kind(kind)
        .map_err(|err| StorageError::CorruptedData(err.to_string()))?;
    Ok(progress)
}

fn encode_outcome(outcome: &JobOutcome) -> Result<String, StorageError> {
    let dto = match outcome {
        JobOutcome::Refresh(refresh) => StoredJobOutcome::Refresh {
            creator_outcomes: refresh
                .creator_outcomes
                .iter()
                .map(|item| match item {
                    CreatorJobOutcome::Succeeded {
                        creator_id,
                        display_name,
                        new_uploads_inserted,
                        existing_uploads_seen,
                        previous_checkpoint,
                        new_checkpoint,
                    } => StoredCreatorOutcome::Succeeded {
                        creator_id: creator_id.as_i64(),
                        display_name: display_name.clone(),
                        new_uploads_inserted: *new_uploads_inserted,
                        existing_uploads_seen: *existing_uploads_seen,
                        previous_checkpoint: previous_checkpoint.as_unix_seconds(),
                        new_checkpoint: new_checkpoint.as_unix_seconds(),
                    },
                    CreatorJobOutcome::Failed {
                        creator_id,
                        display_name,
                        error,
                    } => StoredCreatorOutcome::Failed {
                        creator_id: creator_id.as_i64(),
                        display_name: display_name.clone(),
                        error: error.clone(),
                    },
                })
                .collect(),
            error: refresh.error.clone(),
        },
        JobOutcome::Download(download) => {
            let mut confirmed_artifacts = Vec::with_capacity(download.confirmed_artifacts.len());
            for path in &download.confirmed_artifacts {
                let path_str = path.to_str().ok_or_else(|| {
                    StorageError::Backend(
                        "confirmed artifact path must be valid UTF-8 for SQLite storage".to_owned(),
                    )
                })?;
                confirmed_artifacts.push(path_str.to_owned());
            }
            StoredJobOutcome::Download {
                confirmed_artifacts,
                marked_downloaded: download.marked_downloaded,
                error: download.error.clone(),
            }
        }
        JobOutcome::Interrupted(interrupted) => StoredJobOutcome::Interrupted {
            reason: interrupted.reason.clone(),
        },
    };
    serde_json::to_string(&dto)
        .map_err(|err| StorageError::Backend(format!("failed to serialize job outcome: {err}")))
}

fn decode_outcome(
    state: JobState,
    kind: JobKind,
    raw_json: &str,
) -> Result<JobOutcome, StorageError> {
    let dto: StoredJobOutcome = serde_json::from_str(raw_json).map_err(|err| {
        StorageError::CorruptedData(format!("invalid outcome_json in jobs: {err}"))
    })?;
    let outcome = match dto {
        StoredJobOutcome::Refresh {
            creator_outcomes,
            error,
        } => {
            let mut decoded = Vec::with_capacity(creator_outcomes.len());
            for item in creator_outcomes {
                match item {
                    StoredCreatorOutcome::Succeeded {
                        creator_id,
                        display_name,
                        new_uploads_inserted,
                        existing_uploads_seen,
                        previous_checkpoint,
                        new_checkpoint,
                    } => {
                        let creator_id = CreatorId::new(creator_id).map_err(|err| {
                            StorageError::CorruptedData(format!(
                                "invalid creator_id in outcome_json: {err}"
                            ))
                        })?;
                        decoded.push(CreatorJobOutcome::Succeeded {
                            creator_id,
                            display_name,
                            new_uploads_inserted,
                            existing_uploads_seen,
                            previous_checkpoint: Timestamp::from_unix_seconds(previous_checkpoint),
                            new_checkpoint: Timestamp::from_unix_seconds(new_checkpoint),
                        });
                    }
                    StoredCreatorOutcome::Failed {
                        creator_id,
                        display_name,
                        error,
                    } => {
                        let creator_id = CreatorId::new(creator_id).map_err(|err| {
                            StorageError::CorruptedData(format!(
                                "invalid creator_id in outcome_json: {err}"
                            ))
                        })?;
                        decoded.push(CreatorJobOutcome::Failed {
                            creator_id,
                            display_name,
                            error,
                        });
                    }
                }
            }
            JobOutcome::Refresh(RefreshJobOutcome {
                creator_outcomes: decoded,
                error,
            })
        }
        StoredJobOutcome::Download {
            confirmed_artifacts,
            marked_downloaded,
            error,
        } => JobOutcome::Download(DownloadJobOutcome {
            confirmed_artifacts: confirmed_artifacts.into_iter().map(PathBuf::from).collect(),
            marked_downloaded,
            error,
        }),
        StoredJobOutcome::Interrupted { reason } => {
            JobOutcome::Interrupted(InterruptedJobOutcome { reason })
        }
    };

    validate_outcome_for_state_and_kind(state, kind, &outcome)
        .map_err(StorageError::CorruptedData)?;
    Ok(outcome)
}
