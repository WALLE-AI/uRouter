//! `DecisionRecord` and feedback persistence.
//!
//! Records are written by background tasks so that a slow or full disk never
//! blocks a request. Retention is enforced in three places — at startup, on read
//! and by a periodic sweep — because a record that outlives its tenant TTL is a
//! governance failure, not merely stale data.

use super::{
    AppState, Arc, AsyncWriteExt, AtomicU64, DecisionRecord, Duration, FeedbackCommand,
    FeedbackEvent, FsPath, GatewayError, Ordering, PathBuf, RecordCommand, RecordStore,
    RecordingMode, SystemTime, UNIX_EPOCH, VecDeque, VectorSideStore, evaluate_override,
    feedback_for_turn, mpsc, record_repository_error, records_for_tenant, sleep, tenant_key,
};

pub(crate) async fn tombstone_record_vectors(
    store: Option<&VectorSideStore>,
    records: &[DecisionRecord],
) -> Result<(), GatewayError> {
    let Some(store) = store else {
        return Ok(());
    };
    for reference in records
        .iter()
        .filter_map(|record| record.vector_ref.as_ref())
    {
        store
            .tombstone(reference)
            .await
            .map_err(|_| GatewayError::internal("semantic vector deletion failed"))?;
    }
    Ok(())
}

/// One retention pass: collect what has expired, prune it, and tombstone the
/// semantic vectors it referenced.
///
/// Separated from the loop so the sweep can be asserted directly. Leaving it
/// inline would mean the only way to exercise TTL enforcement is to wait out the
/// sweep interval, so in practice it would never be tested at all.
///
/// Vectors are tombstoned only after the prune succeeds: a failed prune leaves
/// the records readable, and their vectors must stay readable with them.
pub(crate) async fn sweep_expired_records(
    records: &RecordStore,
    vectors: Option<&VectorSideStore>,
) -> usize {
    let expired = records
        .records
        .read()
        .await
        .iter()
        .filter(|record| record_expired(record))
        .cloned()
        .collect::<Vec<_>>();
    if records.prune_expired().await.is_ok() {
        let _ = tombstone_record_vectors(vectors, &expired).await;
        return expired.len();
    }
    0
}

pub(crate) fn spawn_retention_sweeper(records: RecordStore, vectors: Option<Arc<VectorSideStore>>) {
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(60)).await;
            sweep_expired_records(&records, vectors.as_deref()).await;
        }
    });
}

pub(crate) async fn record_writer(
    path: PathBuf,
    max_bytes: u64,
    mut receiver: mpsc::Receiver<RecordCommand>,
    write_errors: Arc<AtomicU64>,
) {
    while let Some(command) = receiver.recv().await {
        let RecordCommand::Append(record) = command else {
            let RecordCommand::Rewrite { records, completed } = command else {
                unreachable!();
            };
            let success = rewrite_records(&path, &records).await.is_ok();
            if !success {
                write_errors.fetch_add(1, Ordering::Relaxed);
            }
            let _ = completed.send(success);
            continue;
        };
        let Ok(mut line) = serde_json::to_vec(&record) else {
            write_errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        line.push(b'\n');
        rotate_if_needed(&path, max_bytes, line.len(), &write_errors).await;
        let result = async {
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await?;
            file.write_all(&line).await
        }
        .await;
        if result.is_err() {
            write_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub(crate) async fn rewrite_records(
    path: &PathBuf,
    records: &[DecisionRecord],
) -> std::io::Result<()> {
    let temporary = PathBuf::from(format!("{}.rewrite.tmp", path.display()));
    let mut file = tokio::fs::File::create(&temporary).await?;
    for record in records {
        let mut line = serde_json::to_vec(record).map_err(std::io::Error::other)?;
        line.push(b'\n');
        file.write_all(&line).await?;
    }
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(temporary, path).await?;
    remove_rotated_copy(path).await
}

pub(crate) fn normalize_replayed_record(record: &mut DecisionRecord) {
    if record.tenant_key.is_empty() {
        record.tenant_key = tenant_key("local");
    }
    if record.expires_at_unix_s == 0 {
        record.expires_at_unix_s = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_add(7 * 24 * 60 * 60);
    }
}

pub(crate) fn record_expired(record: &DecisionRecord) -> bool {
    record.expires_at_unix_s
        <= SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
}

pub(crate) async fn feedback_writer(
    path: PathBuf,
    max_bytes: u64,
    mut receiver: mpsc::Receiver<FeedbackCommand>,
    write_errors: Arc<AtomicU64>,
) {
    while let Some(command) = receiver.recv().await {
        let FeedbackCommand::Append(event) = command else {
            let FeedbackCommand::Rewrite { events, completed } = command else {
                unreachable!();
            };
            let success = rewrite_feedback(&path, &events).await.is_ok();
            if !success {
                write_errors.fetch_add(1, Ordering::Relaxed);
            }
            let _ = completed.send(success);
            continue;
        };
        let Ok(mut line) = serde_json::to_vec(&event) else {
            write_errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        line.push(b'\n');
        rotate_if_needed(&path, max_bytes, line.len(), &write_errors).await;
        let result = async {
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await?;
            file.write_all(&line).await
        }
        .await;
        if result.is_err() {
            write_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub(crate) async fn rewrite_feedback(
    path: &PathBuf,
    events: &[FeedbackEvent],
) -> std::io::Result<()> {
    let temporary = PathBuf::from(format!("{}.rewrite.tmp", path.display()));
    let mut file = tokio::fs::File::create(&temporary).await?;
    for event in events {
        let mut line = serde_json::to_vec(event).map_err(std::io::Error::other)?;
        line.push(b'\n');
        file.write_all(&line).await?;
    }
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(temporary, path).await?;
    remove_rotated_copy(path).await
}

pub(crate) async fn remove_rotated_copy(path: &FsPath) -> std::io::Result<()> {
    let rotated = PathBuf::from(format!("{}.1", path.display()));
    match tokio::fs::remove_file(rotated).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) async fn rotate_if_needed(
    path: &PathBuf,
    max_bytes: u64,
    incoming_bytes: usize,
    write_errors: &AtomicU64,
) {
    if max_bytes > 0
        && tokio::fs::metadata(path)
            .await
            .is_ok_and(|metadata| metadata.len().saturating_add(incoming_bytes as u64) > max_bytes)
    {
        let rotated = PathBuf::from(format!("{}.1", path.display()));
        let _ = tokio::fs::remove_file(&rotated).await;
        if tokio::fs::rename(path, rotated).await.is_err() {
            write_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub(crate) async fn store_record(
    state: &AppState,
    mut record: DecisionRecord,
) -> Result<(), GatewayError> {
    if record.recording == RecordingMode::None {
        return Ok(());
    }
    if record.reason == "preference_pin" && record.parent_turn.is_some() {
        let records = records_for_tenant(state, &record.tenant_key)
            .await?
            .into_iter()
            .collect::<VecDeque<_>>();
        record.override_record = Some(evaluate_override(&state.route, &record, &records));
        if record
            .override_record
            .as_ref()
            .is_some_and(|override_record| override_record.paired)
        {
            state.metrics.paired.fetch_add(1, Ordering::Relaxed);
        } else {
            state
                .metrics
                .paired_rejected
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    if let Some(turn) = &record.trace_turn
        && let Some(signals) = feedback_for_turn(state, &record.tenant_key, turn).await?
    {
        record.outcome_signals = signals;
    }
    state
        .record_repository
        .put(record)
        .await
        .map_err(record_repository_error)
}
