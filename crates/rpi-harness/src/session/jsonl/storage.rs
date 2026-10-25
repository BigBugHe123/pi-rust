//! Mirrors `packages/agent/src/harness/session/jsonl/storage.ts` — the durable
//! file-backed `SessionStorage`.
//!
//! ## Write path (single `tokio::Mutex<()>` tail chain)
//!
//! Every mutating method is [`enqueue`]d onto a `tokio::Mutex<()>`-guarded tail
//! (the Rust analog of the TS `Promise` chain `this.tail`). Each enqueued op
//! stamps `seq` + `timestamp` from the live [`SessionState`], appends the JSONL
//! line, then applies the mutation in memory — so the in-memory state is always
//! consistent with the durable tail. Reads clone straight from the in-memory
//! state (defensive-copy contract), never blocking on the tail.
//!
//! ## Load path + torn-tail recovery (plan §5.14)
//!
//! On `load`, the file is split into physical lines. Only a **syntax** error on
//! the **last** physical line is a recoverable *torn tail* (an unacknowledged
//! partial append): we atomically publish the valid prefix (`<dest>.tmp` then
//! rename) and proceed with the prefix's mutations. Any other error (a `schema`
//! error, or a syntax error on a non-last line) is hard corruption →
//! `SessionError::invalid_entry`. The per-destination rename is serialized by a
//! `tokio::Mutex<()>` on each storage handle (intra-handle; the repo additionally
//! guards cross-handle create races).
//!
//! ### Duplicate-seq recovery (divergent writers)
//!
//! Two writers appending to one JSONL diverge their `sequence` counters, which
//! used to make the whole session unopenable. `load` now heals the two shapes:
//!
//! - **A stale seq** (`seq <= sequence`): a duplicate/late append. The line is
//!   dropped (its content already applied) instead of failing the load.
//! - **A forward gap** (`seq > sequence + 1`): the other writer's counter was
//!   ahead. The counter is resynced up to the gap, leaving raw seqs untouched so
//!   future appends stay consistent with the file.
//!
//! An entry that no longer chains to the lane leaf (its parent was the other
//! writer's leaf) is re-parented onto the current leaf. When any duplicate was
//! dropped the file is atomically republished without it. The host additionally
//! takes an OS advisory lock per session (see `pi-cli`'s `claim_session_lock`)
//! so a second process is refused rather than allowed to write concurrently.
//!
//! ## Adaptation
//!
//! TS `appendEntr`/`appendRecord` take provisioned/new shapes and stamp inline;
//! the Rust [`SessionStorage`] trait takes a fully-stamped [`Entry`] / record is
//! *not* used here — this backend follows the in-memory one's convention of
//! stamping `seq`+`timestamp` itself (it owns the `SessionState`), so its
//! `append_entry` receives a [`ProvisionedEntry`] and stamps via the state +
//! a [`Clock`], then encodes the resulting full `Entry`.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use rpi_tools::env::{FileContent, FileSystem};

use crate::error::{SessionError, SessionErrorCode, SessionResult};
use crate::session::jsonl::codec::{
    encode_header, encode_mutation, encode_mutation_into, metadata_from_header, parse_header,
    parse_mutation,
};
use crate::session::jsonl::errors::{file_result, invalid_file, JsonlDecodeErrorKind};
use crate::session::jsonl::types::{JsonlSessionMetadata, JsonlV4Header};
use crate::session::memory::{stamp_record, Clock};
use crate::session::state::{ApplyOutcome, SessionState};
use crate::session::types::{
    provisioned_into_entry, BranchBounds, Entry, EntryQuery, ForkOptions, IdGenerator, LanePointer,
    LaneRecord, LogItem, LogOptions, OperationStartedRecord, ProvisionedEntry, RecordQuery,
    SessionMetadata, SessionMutation, SessionStats, SessionStorage,
};

/// Durable file-backed `SessionStorage`. Mirrors TS `JsonlSessionStorage`.
///
/// Owns: the shared [`FileSystem`], the session [`JsonlSessionMetadata`] (with
/// its on-disk `path`), the in-memory [`SessionState`] mirror, a [`Clock`] for
/// timestamps, an [`IdGenerator`] reserved for auto-id (callers currently
/// supply ids), and a `tokio::Mutex<()>` tail serializing appends.
pub struct JsonlSessionStorage {
    fs: Arc<dyn FileSystem>,
    metadata: JsonlSessionMetadata,
    state: tokio::sync::Mutex<SessionState>,
    clock: Arc<dyn Clock>,
    /// Reserved for future auto-id generation (callers currently supply entry/
    /// record ids). Kept for parity with `InMemorySessionStorage`.
    #[allow(dead_code)]
    ids: Arc<dyn IdGenerator>,
    /// Serializes appends to this session's file (the TS `tail` Promise chain).
    /// Not `&self` across await points, so each mutating op holds it for its
    /// full duration.
    tail: Mutex<()>,
    append: Mutex<Option<Box<dyn rpi_tools::env::AppendFile>>>,
}

impl JsonlSessionStorage {
    /// Build a storage over an already-initialized file (header written).
    /// Mirrors TS `new JsonlSessionStorage(fs, metadata)`.
    pub fn new(
        fs: Arc<dyn FileSystem>,
        metadata: JsonlSessionMetadata,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> Self {
        Self {
            fs,
            metadata,
            state: tokio::sync::Mutex::new(SessionState::new()),
            clock,
            ids,
            tail: Mutex::new(()),
            append: Mutex::new(None),
        }
    }

    /// Create a new session file: write the header, then build a storage over
    /// it. Mirrors TS `JsonlSessionStorage.create`.
    pub async fn create(
        fs: Arc<dyn FileSystem>,
        path: &str,
        header: JsonlV4Header,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> SessionResult<Self> {
        file_result(
            fs.write_file(path, FileContent::Text(encode_header(&header)), None)
                .await,
            &format!("Failed to initialize session {path}"),
        )?;
        let info = file_result(
            fs.file_info(path, None).await,
            &format!("Failed to read session metadata {path}"),
        )?;
        let metadata = metadata_from_header(&header, path, info.mtime_ms);
        Ok(Self::new(fs, metadata, clock, ids))
    }

    /// Load + replay an existing session file, repairing a torn tail if the
    /// last physical line is a syntax error. Mirrors TS `JsonlSessionStorage.load`.
    pub async fn load(
        fs: Arc<dyn FileSystem>,
        path: &str,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> SessionResult<Self> {
        let content = file_result(
            fs.read_text_file(path, None).await,
            &format!("Failed to read session {path}"),
        )?;
        // TS: content.split("\n"); drop trailing "" from the final "\n".
        let mut physical_lines: Vec<&str> = content.split('\n').collect();
        if physical_lines.last() == Some(&"") {
            physical_lines.pop();
        }
        if physical_lines.is_empty() || physical_lines[0].is_empty() {
            return Err(invalid_file(path, 1, &"is missing a header".to_string()));
        }
        let header = parse_header(physical_lines[0]).map_err(|e| invalid_file(path, 1, &e))?;
        let info = file_result(
            fs.file_info(path, None).await,
            &format!("Failed to read session metadata {path}"),
        )?;
        let metadata = metadata_from_header(&header, path, info.mtime_ms);
        let storage = Self::new(fs.clone(), metadata, clock, ids);

        // Lines kept through the duplicate-seq recovery, so a healed file can
        // be republished without the dropped duplicates (see after the loop).
        let mut kept_lines: Vec<&str> = Vec::new();
        let mut dropped_duplicates = false;
        for (idx, line) in physical_lines.iter().enumerate().skip(1) {
            let line_no = (idx + 1) as u32;
            match parse_mutation(line) {
                Ok(mutation) => {
                    let mut state = storage.state.lock().await;
                    // Self-heal a duplicated/late append. Two writers on one
                    // JSONL (e.g. a session resumed while still open in another
                    // process) diverge their `sequence` counters, so a mutation
                    // lands with an already-consumed seq. Its content is a
                    // duplicate of what already applied, so drop it rather than
                    // refuse the whole session.
                    if mutation.seq() <= state.sequence() {
                        dropped_duplicates = true;
                        drop(state);
                        continue;
                    }
                    // A divergent writer's counter can leave a forward gap (its
                    // seqs were consumed elsewhere). Accept the gap by moving
                    // the counter up to it. Raw seqs stay untouched, so future
                    // appends stay consistent with the file.
                    if mutation.seq() > state.next_sequence() {
                        state.resync_sequence(mutation.seq() - 1);
                    }
                    let mutation = chain_onto_lane_leaf(mutation, &state);
                    if let Err(e) = state.apply_mutation(mutation) {
                        if e.code == SessionErrorCode::InvalidEntry {
                            drop(state);
                            return Err(invalid_file(path, line_no, &e));
                        }
                        drop(state);
                        return Err(e);
                    }
                    kept_lines.push(*line);
                }
                Err(err) => {
                    // Assistant frames are transient streaming progress, not
                    // session history. Native Pi treats an undecodable frame
                    // as lost progress rather than making the whole session
                    // unopenable. Keep all other schema errors strict.
                    if err.kind == JsonlDecodeErrorKind::Schema && is_assistant_frame_append(*line)
                    {
                        tracing::warn!(line = line_no, "skipping undecodable assistant frame");
                        continue;
                    }
                    let is_last = idx == physical_lines.len() - 1;
                    let is_torn_tail = is_last && err.kind == JsonlDecodeErrorKind::Syntax;
                    if is_torn_tail {
                        // Drop the unacknowledged partial append by atomically
                        // publishing the valid prefix.
                        let valid_prefix = format!("{}\n", physical_lines[..idx].join("\n"));
                        let fs = storage.fs.clone();
                        let path_owned = path.to_string();
                        publish_file_atomically(&fs, &path_owned, |fs, temp_path| {
                            let valid_prefix = valid_prefix.clone();
                            let path_owned = path_owned.clone();
                            Box::pin(async move {
                                file_result(
                                    fs.write_file(temp_path, FileContent::Text(valid_prefix), None)
                                        .await,
                                    &format!("Failed to stage torn-tail repair {path_owned}"),
                                )?;
                                Ok(())
                            })
                        })
                        .await?;
                        return Ok(storage);
                    }
                    return Err(invalid_file(path, line_no, &err));
                }
            }
        }
        // Persist the duplicate-seq recovery: atomically republish the file
        // without the dropped duplicates so the log no longer carries them.
        // Seq values are untouched (no renumbering), so subsequent appends
        // stay consistent with the file.
        if dropped_duplicates {
            let mut repaired = String::with_capacity(content.len());
            repaired.push_str(physical_lines[0]);
            repaired.push('\n');
            for line in &kept_lines {
                repaired.push_str(line);
                repaired.push('\n');
            }
            let fs = storage.fs.clone();
            let path_owned = path.to_string();
            publish_file_atomically(&fs, &path_owned, |fs, temp_path| {
                let repaired = repaired.clone();
                let path_owned = path_owned.clone();
                Box::pin(async move {
                    file_result(
                        fs.write_file(temp_path, FileContent::Text(repaired), None)
                            .await,
                        &format!("Failed to stage session repair {path_owned}"),
                    )?;
                    Ok(())
                })
            })
            .await?;
            return Ok(storage);
        }
        // Repair an unterminated tail (no trailing "\n") so future appends are
        // on their own line.
        if !content.ends_with('\n') {
            file_result(
                storage
                    .fs
                    .append_file(path, FileContent::Text("\n".into()), None)
                    .await,
                &format!("Failed to repair unterminated session tail {path}"),
            )?;
        }
        Ok(storage)
    }

    /// Fork into `path` by writing a fresh header then replaying the fork
    /// mutations. Mirrors TS `JsonlSessionStorage.fork`. The target file is
    /// built via [`publish_file_atomically`] so a crash leaves only a `.tmp`.
    pub async fn fork(
        &self,
        path: &str,
        header: JsonlV4Header,
        options: &ForkOptions,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> SessionResult<Self> {
        let mutations = {
            let state = self.state.lock().await;
            state.create_fork_mutations(options)?
        };
        publish_file_atomically(&self.fs, path, |fs, temp_path| {
            let header = header.clone();
            let mutations = mutations.clone();
            let clock = clock.clone();
            let ids = ids.clone();
            Box::pin(async move {
                let target =
                    JsonlSessionStorage::create(fs.clone(), temp_path, header, clock, ids).await?;
                for mutation in &mutations {
                    let line = encode_mutation(mutation);
                    file_result(
                        fs.append_file(temp_path, FileContent::Text(line), None)
                            .await,
                        &format!("Failed to append session {temp_path}"),
                    )?;
                    let mut state = target.state.lock().await;
                    state.apply_mutation(mutation.clone())?;
                }
                Ok(())
            })
        })
        .await?;
        JsonlSessionStorage::load(self.fs.clone(), path, clock, ids).await
    }

    /// Wait for all in-flight appends to settle. Mirrors TS `drain`.
    pub async fn drain(&self) {
        let _g = self.tail.lock().await;
    }

    /// The rich JSONL metadata (the trait exposes only the base subset).
    pub fn jsonl_metadata(&self) -> JsonlSessionMetadata {
        self.metadata.clone()
    }

    /// Enqueue a mutating operation onto the tail chain. Mirrors TS `enqueue`.
    #[allow(dead_code)]
    async fn enqueue<T>(
        &self,
        op: impl std::future::Future<Output = SessionResult<T>>,
    ) -> SessionResult<T> {
        let _g = self.tail.lock().await;
        op.await
    }

    async fn append_encoded_bytes(&self, lines: Vec<u8>) -> SessionResult<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let mut append = self.append.lock().await;
        if append.is_none() {
            *append = file_result(
                self.fs.open_append_file(&self.metadata.path).await,
                &format!("Failed to open session {}", self.metadata.path),
            )?;
        }
        if let Some(writer) = append.as_mut() {
            // The tail lock still orders every append, and the writer flushes
            // before acknowledgement. A failed handle is discarded for retry.
            let result = writer.append(&lines).await;
            if result.is_err() {
                *append = None;
            }
            return file_result(
                result,
                &format!("Failed to append session {}", self.metadata.path),
            );
        }
        drop(append);
        file_result(
            self.fs
                .append_file(&self.metadata.path, FileContent::Bytes(lines), None)
                .await,
            &format!("Failed to append session {}", self.metadata.path),
        )
    }

    /// Append a frame-only batch without cloning the complete session state.
    /// Frame records do not change lanes or open-operation state, so their
    /// sequence and id validation can use a small local batch index.
    async fn append_frame_record_batch(
        &self,
        records: Vec<LaneRecord>,
    ) -> SessionResult<Vec<LaneRecord>> {
        let _tail = self.tail.lock().await;
        let state = self.state.lock().await;
        let mut next_seq = state.next_sequence();
        let mut ids = HashSet::with_capacity(records.len());
        let mut mutations = Vec::with_capacity(records.len());
        let mut encoded_lines = Vec::with_capacity(records.len() * 256);

        for record in records {
            let lane = record.lane().to_string();
            state.require_lane(&lane)?;
            if !ids.insert(record.id().to_string()) {
                return Err(SessionError::storage(format!(
                    "Record id already used in batch: {}",
                    record.id()
                )));
            }
            state.validate_unused_id(record.id())?;
            let timestamp = self.clock.now_ms();
            let stamped = stamp_record(record, next_seq, lane, timestamp);
            next_seq += 1;
            let mutation = SessionMutation::Record { record: stamped };
            mutations.push(mutation);
        }
        drop(state);

        for mutation in &mutations {
            encode_mutation_into(mutation, &mut encoded_lines);
        }
        self.append_encoded_bytes(encoded_lines).await?;
        let mut state = self.state.lock().await;
        let mut stamped_records = Vec::with_capacity(mutations.len());
        for mutation in mutations {
            match state.apply_mutation(mutation)? {
                ApplyOutcome::Record(stamped) => stamped_records.push(stamped),
                _ => {
                    return Err(SessionError::storage(
                        "append_frame_record_batch did not persist a record",
                    ));
                }
            }
        }
        Ok(stamped_records)
    }

    /// Append one non-frame record without cloning the complete session state.
    ///
    /// Record validation is completed while the session tail is held. The
    /// state is only advanced after the durable append succeeds, so a failed
    /// write leaves the in-memory state unchanged while avoiding the O(history)
    /// candidate-state clone on the normal single-record path.
    async fn append_single_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        let _tail = self.tail.lock().await;
        let lane = record.lane().to_string();
        let state = self.state.lock().await;
        state.require_lane(&lane)?;
        state.validate_unused_id(record.id())?;
        if matches!(record, LaneRecord::OperationStarted(_)) {
            if let Some(current) = state.find_open_operations(&lane, Some(1))?.first() {
                return Err(SessionError::storage(format!(
                    "Lane {} already has an open operation {}",
                    lane, current.base.id
                )));
            }
        }
        let seq = state.next_sequence();
        let timestamp = self.clock.now_ms();
        let stamped = stamp_record(record, seq, lane, timestamp);
        let mutation = SessionMutation::Record { record: stamped };
        let mut encoded = Vec::with_capacity(256);
        encode_mutation_into(&mutation, &mut encoded);
        drop(state);

        self.append_encoded_bytes(encoded).await?;
        let mut state = self.state.lock().await;
        match state.apply_mutation(mutation)? {
            ApplyOutcome::Record(stamped) => Ok(stamped),
            _ => Err(SessionError::storage("append_record did not persist")),
        }
    }

    /// Append a batch of lane records atomically from the reducer's point of
    /// view. The candidate state is advanced before the file write, but the
    /// live state is replaced only after the whole encoded batch is visible.
    async fn append_record_batch(
        &self,
        records: Vec<LaneRecord>,
    ) -> SessionResult<Vec<LaneRecord>> {
        if records.is_empty() {
            return Ok(Vec::new());
        }
        if records
            .iter()
            .all(|record| matches!(record, LaneRecord::AssistantFrame(_)))
        {
            return self.append_frame_record_batch(records).await;
        }
        if records.len() == 1 {
            return Ok(vec![
                self.append_single_record(records.into_iter().next().expect("non-empty records"))
                    .await?,
            ]);
        }
        let _tail = self.tail.lock().await;
        let state = self.state.lock().await;
        let mut candidate = state.clone();
        let mut encoded_lines = Vec::with_capacity(records.len() * 256);
        let mut stamped_records = Vec::with_capacity(records.len());

        for record in records {
            let lane = record.lane().to_string();
            candidate.require_lane(&lane)?;
            candidate.validate_unused_id(record.id())?;
            if matches!(record, LaneRecord::OperationStarted(_)) {
                if let Some(current) = candidate.find_open_operations(&lane, Some(1))?.first() {
                    return Err(SessionError::storage(format!(
                        "Lane {} already has an open operation {}",
                        lane, current.base.id
                    )));
                }
            }
            let seq = candidate.next_sequence();
            let timestamp = self.clock.now_ms();
            let stamped = stamp_record(record, seq, lane, timestamp);
            let mutation = SessionMutation::Record { record: stamped };
            encode_mutation_into(&mutation, &mut encoded_lines);
            match candidate.apply_mutation(mutation)? {
                ApplyOutcome::Record(stamped) => stamped_records.push(stamped),
                _ => {
                    return Err(SessionError::storage(
                        "append_record_batch did not persist a record",
                    ));
                }
            }
        }
        drop(state);

        self.append_encoded_bytes(encoded_lines).await?;
        let mut state = self.state.lock().await;
        *state = candidate;
        Ok(stamped_records)
    }

    /// Append one mutation line to the file (no in-memory apply).
    async fn append_mutation_line(&self, mutation: &SessionMutation) -> SessionResult<()> {
        let mut encoded = Vec::with_capacity(256);
        encode_mutation_into(mutation, &mut encoded);
        self.append_encoded_bytes(encoded).await
    }
}

/// Normalize an entry's parent onto the lane's current leaf before applying.
///
/// A valid log always satisfies `entry.parent_id == lane.leaf` (the validator
/// enforces it), so this is a no-op for well-formed sessions. When two writers
/// diverge, an entry can land with a parent the local state never reached;
/// re-chaining onto the leaf recovers the session instead of refusing to open
/// it. Non-entry mutations and entries without a named lane pass through.
fn chain_onto_lane_leaf(mutation: SessionMutation, state: &SessionState) -> SessionMutation {
    if let SessionMutation::Entry {
        seq,
        timestamp,
        lane,
        mut entry,
    } = mutation
    {
        if let Some(lane_name) = lane.clone() {
            if let Ok(leaf) = state.require_lane(&lane_name) {
                if entry.parent_id().map(str::to_string) != leaf {
                    entry.base_mut().parent_id = leaf;
                }
            }
        }
        SessionMutation::Entry {
            seq,
            timestamp,
            lane,
            entry,
        }
    } else {
        mutation
    }
}

#[async_trait]
impl SessionStorage for JsonlSessionStorage {
    fn metadata(&self) -> SessionMetadata {
        self.metadata.to_base()
    }

    async fn get_metadata(&self) -> SessionMetadata {
        self.metadata.to_base()
    }

    async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
        let state = self.state.lock().await;
        Ok(state.get_lanes())
    }

    async fn create_lane(&self, lane: &str, at: Option<&str>) -> SessionResult<()> {
        let lane = lane.to_string();
        let at = at.map(|s| s.to_string());
        let _g = self.tail.lock().await;
        let state = self.state.lock().await;
        state.validate_new_lane(&lane)?;
        state.validate_target(at.as_deref())?;
        let seq = state.next_sequence();
        let mutation = SessionMutation::Lane {
            seq,
            lane: lane.clone(),
            leaf_id: at,
        };
        drop(state);
        self.append_mutation_line(&mutation).await?;
        let mut state = self.state.lock().await;
        state.apply_mutation(mutation)?;
        Ok(())
    }

    async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
        let lane = lane.to_string();
        let to = to.map(|s| s.to_string());
        let _g = self.tail.lock().await;
        let state = self.state.lock().await;
        state.require_lane(&lane)?;
        state.validate_target(to.as_deref())?;
        let seq = state.next_sequence();
        let mutation = SessionMutation::Lane {
            seq,
            lane,
            leaf_id: to,
        };
        drop(state);
        self.append_mutation_line(&mutation).await?;
        let mut state = self.state.lock().await;
        state.apply_mutation(mutation)?;
        Ok(())
    }

    async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
        let _g = self.tail.lock().await;
        let state = self.state.lock().await;
        let parent_id = state.require_lane(lane)?;
        state.validate_unused_id(&entry.id)?;
        let seq = state.next_sequence();
        let timestamp = self.clock.now_ms();
        let full = provisioned_into_entry(entry, seq, parent_id, timestamp);
        let mutation = SessionMutation::Entry {
            seq,
            timestamp,
            lane: Some(lane.to_string()),
            entry: full,
        };
        drop(state);
        self.append_mutation_line(&mutation).await?;
        let mut state = self.state.lock().await;
        match state.apply_mutation(mutation)? {
            ApplyOutcome::Entry(stamped) => Ok(stamped),
            _ => Err(SessionError::storage("append_entry did not persist")),
        }
    }

    async fn append_record_and_entry(
        &self,
        record: LaneRecord,
        entry: ProvisionedEntry,
        lane: &str,
    ) -> SessionResult<(LaneRecord, Entry)> {
        let _tail = self.tail.lock().await;
        let state = self.state.lock().await;
        let record_lane = record.lane().to_string();
        state.require_lane(&record_lane)?;
        state.validate_unused_id(record.id())?;
        if matches!(record, LaneRecord::OperationStarted(_)) {
            if let Some(current) = state.find_open_operations(&record_lane, Some(1))?.first() {
                return Err(SessionError::storage(format!(
                    "Lane {} already has an open operation {}",
                    record_lane, current.base.id
                )));
            }
        }
        let parent_id = state.require_lane(lane)?;
        state.validate_unused_id(&entry.id)?;
        if entry.id == record.id() {
            return Err(SessionError::already_exists(format!(
                "Session id already exists: {}",
                entry.id
            )));
        }
        let record_seq = state.next_sequence();
        let stamped_record = stamp_record(record, record_seq, record_lane, self.clock.now_ms());
        let record_mutation = SessionMutation::Record {
            record: stamped_record,
        };
        let entry_timestamp = self.clock.now_ms();
        let entry_mutation = SessionMutation::Entry {
            seq: record_seq + 1,
            timestamp: entry_timestamp,
            lane: Some(lane.to_string()),
            entry: provisioned_into_entry(entry, record_seq + 1, parent_id, entry_timestamp),
        };
        let mut encoded = Vec::with_capacity(512);
        encode_mutation_into(&record_mutation, &mut encoded);
        encode_mutation_into(&entry_mutation, &mut encoded);
        drop(state);
        self.append_encoded_bytes(encoded).await?;
        let mut state = self.state.lock().await;
        let committed_record = match state.apply_mutation(record_mutation)? {
            ApplyOutcome::Record(stamped) => stamped,
            _ => return Err(SessionError::storage("append_record did not persist")),
        };
        let committed_entry = match state.apply_mutation(entry_mutation)? {
            ApplyOutcome::Entry(stamped) => stamped,
            _ => return Err(SessionError::storage("append_entry did not persist")),
        };
        Ok((committed_record, committed_entry))
    }

    async fn append_record_entry_and_record(
        &self,
        before: LaneRecord,
        entry: ProvisionedEntry,
        lane: &str,
        after: LaneRecord,
    ) -> SessionResult<(LaneRecord, Entry, LaneRecord)> {
        let _tail = self.tail.lock().await;
        let state = self.state.lock().await;
        let before_lane = before.lane().to_string();
        let after_lane = after.lane().to_string();
        state.require_lane(&before_lane)?;
        state.require_lane(lane)?;
        state.require_lane(&after_lane)?;
        state.validate_unused_id(before.id())?;
        state.validate_unused_id(&entry.id)?;
        state.validate_unused_id(after.id())?;
        if before.id() == entry.id || before.id() == after.id() || entry.id == after.id() {
            return Err(SessionError::already_exists(
                "Session mutation ids must be distinct",
            ));
        }
        if matches!(before, LaneRecord::OperationStarted(_)) {
            if let Some(current) = state.find_open_operations(&before_lane, Some(1))?.first() {
                return Err(SessionError::storage(format!(
                    "Lane {} already has an open operation {}",
                    before_lane, current.base.id
                )));
            }
        }
        let parent_id = state.require_lane(lane)?;
        let first_seq = state.next_sequence();
        let before_mutation = SessionMutation::Record {
            record: stamp_record(before, first_seq, before_lane, self.clock.now_ms()),
        };
        let entry_timestamp = self.clock.now_ms();
        let entry_mutation = SessionMutation::Entry {
            seq: first_seq + 1,
            timestamp: entry_timestamp,
            lane: Some(lane.to_string()),
            entry: provisioned_into_entry(entry, first_seq + 1, parent_id, entry_timestamp),
        };
        let after_mutation = SessionMutation::Record {
            record: stamp_record(after, first_seq + 2, after_lane, self.clock.now_ms()),
        };
        let mut encoded = Vec::with_capacity(768);
        encode_mutation_into(&before_mutation, &mut encoded);
        encode_mutation_into(&entry_mutation, &mut encoded);
        encode_mutation_into(&after_mutation, &mut encoded);
        drop(state);

        self.append_encoded_bytes(encoded).await?;
        let mut state = self.state.lock().await;
        let committed_before = match state.apply_mutation(before_mutation)? {
            ApplyOutcome::Record(stamped) => stamped,
            _ => return Err(SessionError::storage("before record did not persist")),
        };
        let committed_entry = match state.apply_mutation(entry_mutation)? {
            ApplyOutcome::Entry(stamped) => stamped,
            _ => return Err(SessionError::storage("entry did not persist")),
        };
        let committed_after = match state.apply_mutation(after_mutation)? {
            ApplyOutcome::Record(stamped) => stamped,
            _ => return Err(SessionError::storage("after record did not persist")),
        };
        Ok((committed_before, committed_entry, committed_after))
    }

    async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        self.append_record_batch(vec![record])
            .await?
            .pop()
            .ok_or_else(|| SessionError::storage("append_record returned no record"))
    }

    async fn append_records(&self, records: Vec<LaneRecord>) -> SessionResult<Vec<LaneRecord>> {
        self.append_record_batch(records).await
    }

    async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
        let state = self.state.lock().await;
        Ok(state.get_entry(id).cloned())
    }

    async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
        let state = self.state.lock().await;
        state.find_entries(query)
    }

    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<Entry>> {
        let state = self.state.lock().await;
        state.find_entries_on_branch(query, bounds, start)
    }

    async fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        let state = self.state.lock().await;
        state.find_records(query)
    }

    async fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> SessionResult<Vec<OperationStartedRecord>> {
        let state = self.state.lock().await;
        state.find_open_operations(lane, limit)
    }

    async fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
        let state = self.state.lock().await;
        state.get_log(options)
    }

    async fn get_name(&self) -> SessionResult<Option<String>> {
        let state = self.state.lock().await;
        Ok(state.get_name().map(|s| s.to_string()))
    }

    async fn set_name(&self, name: Option<&str>) -> SessionResult<()> {
        let _g = self.tail.lock().await;
        let state = self.state.lock().await;
        let seq = state.next_sequence();
        let mutation = SessionMutation::FactName {
            seq,
            name: name.map(|s| s.to_string()),
        };
        drop(state);
        self.append_mutation_line(&mutation).await?;
        let mut state = self.state.lock().await;
        state.apply_mutation(mutation)?;
        Ok(())
    }

    async fn get_label(&self, id: &str) -> SessionResult<Option<String>> {
        let state = self.state.lock().await;
        Ok(state.get_label(id).map(|s| s.to_string()))
    }

    async fn set_label(&self, id: &str, label: Option<&str>) -> SessionResult<()> {
        let _g = self.tail.lock().await;
        let state = self.state.lock().await;
        state.validate_target(Some(id))?;
        let seq = state.next_sequence();
        let mutation = SessionMutation::FactLabel {
            seq,
            target_id: id.to_string(),
            label: label.map(|s| s.to_string()),
        };
        drop(state);
        self.append_mutation_line(&mutation).await?;
        let mut state = self.state.lock().await;
        state.apply_mutation(mutation)?;
        Ok(())
    }

    async fn get_stats(&self) -> SessionResult<SessionStats> {
        let state = self.state.lock().await;
        Ok(state.get_stats().clone())
    }
}

/// Whether a raw JSONL line is an append of transient assistant-frame progress.
///
/// This intentionally only identifies the record envelope. The nested frame is
/// allowed to fail schema decoding; ordinary records must remain strict.
fn is_assistant_frame_append(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    value.get("kind").and_then(serde_json::Value::as_str) == Some("record")
        && value.get("type").and_then(serde_json::Value::as_str) == Some("assistant_frame")
        && value.get("op").and_then(serde_json::Value::as_str) == Some("append")
}

// The `enqueue` closure approach above fights the borrow checker (closures
// can't capture `&self` across an await of a method on `self`). The trait impl
// therefore inlines the stamp→persist→apply sequence directly per method,
// holding the `tail` lock across the whole op (the same serialization the TS
// `tail` Promise chain provides). `enqueue` + `self_append_and_apply` are kept
// only as documentation of the intent and are unused.
#[allow(dead_code)]
async fn self_append_and_apply(
    _storage: &JsonlSessionStorage,
    _m: &SessionMutation,
) -> SessionResult<()> {
    Ok(())
}

#[allow(dead_code)]
fn self_fs_placeholder() -> Arc<dyn FileSystem> {
    // unreachable placeholder for the never-called enqueue path.
    unreachable!("enqueue path is not used; trait impls inline the op")
}

/// Build a complete sibling temporary file, then atomically rename it over the
/// destination. Mirrors TS `publishFileAtomically`.
///
/// The populate callback must create/overwrite `tempPath` with the complete
/// file; the destination is untouched until the rename commits, so a crash
/// while populating leaves only the ignored `.tmp`. Per-destination
/// serialization is the caller's responsibility (the storage holds its `tail`
/// mutex; the repo holds its create-destination set).
async fn publish_file_atomically<F>(
    fs: &Arc<dyn FileSystem>,
    destination_path: &str,
    populate: F,
) -> SessionResult<()>
where
    F: for<'a> FnOnce(
        &'a Arc<dyn FileSystem>,
        &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = SessionResult<()>> + Send + 'a>,
    >,
{
    let temp_path = format!("{destination_path}.tmp");
    // Mirror the TS `try { populate; rename } catch { remove(tempPath) }` shape:
    // the staged `.tmp` is best-effort removed on ANY failure (staging or
    // rename), so a crash/failure never leaves a partial `.tmp` behind. The
    // original destination is untouched until the rename commits.
    let result: SessionResult<()> = (async {
        populate(fs, &temp_path).await?;
        file_result(
            fs.rename_file(&temp_path, destination_path, None).await,
            &format!("Failed to publish staged file {destination_path}"),
        )?;
        Ok(())
    })
    .await;
    if result.is_err() {
        // Best-effort cleanup; preserve the original error.
        let _ = fs.remove(&temp_path, false, true, None).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::memory::{CounterIdGenerator, FakeClock};
    use crate::session::types::{
        OperationIntent, OperationStartedRecord, ProvisionedEntry, ProvisionedKind, RecordBase,
    };

    #[tokio::test]
    async fn append_returns_the_owned_payload_but_keeps_storage_defensive() {
        let fs: Arc<dyn FileSystem> =
            Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
        let jsonl = JsonlSessionStorage::create(
            fs,
            "/owned.jsonl",
            header(),
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .await
        .unwrap();
        let memory = crate::session::memory::InMemorySessionStorage::new(
            SessionMetadata {
                id: "memory".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        );
        let storages: Vec<Arc<dyn SessionStorage>> = vec![Arc::new(jsonl), Arc::new(memory)];
        for storage in storages {
            let text = "owned append".repeat(4096);
            let original_pointer = text.as_ptr() as usize;
            let expected = text.clone();
            let mut returned = storage
                .append_entry(
                    ProvisionedEntry {
                        id: "entry".into(),
                        kind: ProvisionedKind::Message {
                            message: rpi_ai::UserMessage::new(text, 0).into(),
                            terminate: None,
                        },
                    },
                    "main",
                )
                .await
                .unwrap();
            let Entry::Message(message) = &mut returned else {
                panic!("expected message")
            };
            let rpi_agent::AgentMessage::User(user) = &mut message.message else {
                panic!("expected user")
            };
            let rpi_ai::UserContent::Text(text) = &mut user.content else {
                panic!("expected text")
            };
            assert_eq!(text.as_ptr() as usize, original_pointer);
            text.clear();
            let Entry::Message(stored) = storage.get_entry("entry").await.unwrap().unwrap() else {
                panic!("expected stored message")
            };
            let rpi_agent::AgentMessage::User(user) = stored.message else {
                panic!("expected stored user")
            };
            assert_eq!(user.content, rpi_ai::UserContent::Text(expected));

            let payload = "owned frame".repeat(4096);
            let original_pointer = payload.as_ptr() as usize;
            let expected = payload.clone();
            let mut returned = storage
                .append_record(LaneRecord::AssistantFrame(
                    crate::session::types::AssistantFrameRecord {
                        base: RecordBase {
                            id: "frame".into(),
                            seq: 0,
                            lane: "main".into(),
                            timestamp: 0,
                        },
                        run_id: "run".into(),
                        stream_index: 0,
                        op: crate::session::types::AssistantFrameOp::Append,
                        frame: Some(serde_json::Value::String(payload)),
                    },
                ))
                .await
                .unwrap();
            let LaneRecord::AssistantFrame(frame) = &mut returned else {
                panic!("expected frame")
            };
            let Some(serde_json::Value::String(payload)) = &mut frame.frame else {
                panic!("expected frame payload")
            };
            assert_eq!(payload.as_ptr() as usize, original_pointer);
            payload.clear();
            let records = storage.find_records(&RecordQuery::default()).await.unwrap();
            let LaneRecord::AssistantFrame(frame) = &records[0] else {
                panic!("expected stored frame")
            };
            assert_eq!(frame.frame, Some(serde_json::Value::String(expected)));
        }
    }

    #[tokio::test]
    async fn append_records_batches_frame_records_with_contiguous_sequences() {
        let fs: Arc<dyn FileSystem> =
            Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
        let storage = JsonlSessionStorage::create(
            fs,
            "/batch.jsonl",
            header(),
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .await
        .unwrap();
        let frame = |id: &str| {
            LaneRecord::AssistantFrame(crate::session::types::AssistantFrameRecord {
                base: RecordBase {
                    id: id.into(),
                    seq: 0,
                    lane: "main".into(),
                    timestamp: 0,
                },
                run_id: "run".into(),
                stream_index: 0,
                op: crate::session::types::AssistantFrameOp::Append,
                frame: Some(serde_json::json!({"id": id})),
            })
        };
        let returned = storage
            .append_records(vec![frame("frame-1"), frame("frame-2")])
            .await
            .unwrap();
        assert_eq!(
            returned.iter().map(LaneRecord::seq).collect::<Vec<_>>(),
            [1, 2]
        );
        let stored = storage.find_records(&RecordQuery::default()).await.unwrap();
        assert_eq!(stored.len(), 2);
        let mut ids = stored.iter().map(|record| record.id()).collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids, ["frame-1", "frame-2"]);
    }

    fn user_msg(text: &str) -> rpi_agent::message::AgentMessage {
        rpi_agent::message::AgentMessage::User(rpi_ai::types::UserMessage::new(text, 1))
    }

    fn header() -> JsonlV4Header {
        JsonlV4Header::new(
            "sess-1".into(),
            1_700_000_000_000,
            "/cwd".into(),
            None,
            None,
            None,
        )
    }

    #[tokio::test]
    async fn reusable_os_append_keeps_each_acknowledged_entry_visible_to_reload() {
        let temp = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
            temp.path().to_path_buf(),
        ));
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());
        let storage = JsonlSessionStorage::create(
            fs.clone(),
            "session.jsonl",
            header(),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        for index in 0..3 {
            let id = format!("e{index}");
            storage
                .append_entry(
                    ProvisionedEntry {
                        id: id.clone(),
                        kind: ProvisionedKind::Message {
                            message: user_msg("visible 🐈"),
                            terminate: None,
                        },
                    },
                    "main",
                )
                .await
                .unwrap();
            let reader =
                JsonlSessionStorage::load(fs.clone(), "session.jsonl", clock.clone(), ids.clone())
                    .await
                    .unwrap();
            assert!(reader.get_entry(&id).await.unwrap().is_some());
            assert_eq!(
                reader
                    .find_entries(&EntryQuery::default())
                    .await
                    .unwrap()
                    .len(),
                index + 1
            );
        }
    }

    #[tokio::test]
    async fn cached_append_reopens_after_an_atomic_session_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
            temp.path().to_path_buf(),
        ));
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());
        let storage = JsonlSessionStorage::create(
            fs.clone(),
            "session.jsonl",
            header(),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        let provisioned = |id: &str| ProvisionedEntry {
            id: id.into(),
            kind: ProvisionedKind::Message {
                message: user_msg(id),
                terminate: None,
            },
        };
        storage
            .append_entry(provisioned("before"), "main")
            .await
            .unwrap();
        let prefix = fs.read_text_file("session.jsonl", None).await.unwrap();
        fs.write_file("replacement.tmp", FileContent::Text(prefix), None)
            .await
            .unwrap();
        fs.rename_file("replacement.tmp", "session.jsonl", None)
            .await
            .unwrap();
        storage
            .append_entry(provisioned("after"), "main")
            .await
            .unwrap();
        let reloaded = JsonlSessionStorage::load(fs, "session.jsonl", clock, ids)
            .await
            .unwrap();
        assert!(reloaded.get_entry("after").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn cached_append_reopens_after_a_native_file_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
            temp.path().to_path_buf(),
        ));
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());
        let storage = JsonlSessionStorage::create(
            fs.clone(),
            "session.jsonl",
            header(),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        let entry = |id: &str| ProvisionedEntry {
            id: id.into(),
            kind: ProvisionedKind::Message {
                message: user_msg(id),
                terminate: None,
            },
        };
        storage
            .append_entry(entry("before-native"), "main")
            .await
            .unwrap();
        let path = temp.path().join("session.jsonl");
        let replacement = temp.path().join("replacement.tmp");
        std::fs::write(&replacement, std::fs::read(&path).unwrap()).unwrap();
        // Deliberately bypass FileSystem: editors, plugins and subprocesses can
        // replace a path without updating any process-local generation counter.
        std::fs::rename(&replacement, &path).unwrap();
        storage
            .append_entry(entry("after-native"), "main")
            .await
            .unwrap();
        assert!(storage.get_entry("after-native").await.unwrap().is_some());
        let reloaded = JsonlSessionStorage::load(fs, "session.jsonl", clock, ids)
            .await
            .unwrap();
        assert!(
            reloaded.get_entry("after-native").await.unwrap().is_some(),
            "an acknowledged append must reach the current file"
        );
    }

    struct RefusingAppend(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for RefusingAppend {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    #[async_trait::async_trait]
    impl rpi_tools::env::AppendFile for RefusingAppend {
        async fn append(&mut self, _: &[u8]) -> Result<(), rpi_tools::FileError> {
            Err(rpi_tools::FileError::new(
                rpi_tools::FileErrorCode::Unknown,
                "injected append failure",
            ))
        }
    }

    #[tokio::test]
    async fn failed_append_does_not_commit_state_and_discards_the_failed_writer() {
        let fs: Arc<dyn FileSystem> =
            Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
        let storage = JsonlSessionStorage::create(
            fs,
            "/s.jsonl",
            header(),
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .await
        .unwrap();
        let freed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        *storage.append.lock().await = Some(Box::new(RefusingAppend(Arc::clone(&freed))));
        let entry = ProvisionedEntry {
            id: "retry".into(),
            kind: ProvisionedKind::Message {
                message: user_msg("retry"),
                terminate: None,
            },
        };
        assert!(storage.append_entry(entry.clone(), "main").await.is_err());
        assert!(storage.get_entry("retry").await.unwrap().is_none());
        assert_eq!(freed.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(storage.append_entry(entry, "main").await.unwrap().seq(), 1);
    }

    #[tokio::test]
    async fn create_append_reload_roundtrips() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());

        let storage = JsonlSessionStorage::create(
            fs.clone(),
            "/s.jsonl",
            header(),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        let entry = storage
            .append_entry(
                ProvisionedEntry {
                    id: "e1".into(),
                    kind: ProvisionedKind::Message {
                        message: user_msg("hi"),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        assert_eq!(entry.id(), "e1");

        let reloaded = JsonlSessionStorage::load(fs, "/s.jsonl", clock, ids)
            .await
            .unwrap();
        let got = reloaded
            .get_entry("e1")
            .await
            .unwrap()
            .expect("entry exists");
        assert_eq!(got.id(), "e1");
    }

    #[tokio::test]
    async fn torn_tail_last_line_is_repaired() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());

        let _ = JsonlSessionStorage::create(
            fs.clone(),
            "/s.jsonl",
            header(),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        // Manually append a valid entry line + a garbage (torn) last line.
        // The first mutation after the header is seq 1 (the header does not
        // consume a sequence number — see `create_append_reload_roundtrips`).
        let valid = encode_mutation(&SessionMutation::Lane {
            seq: 1,
            lane: "main".into(),
            leaf_id: None,
        });
        fs.append_file("/s.jsonl", FileContent::Text(valid), None)
            .await
            .unwrap();
        fs.append_file("/s.jsonl", FileContent::Text("{not json".into()), None)
            .await
            .unwrap();

        let reloaded = JsonlSessionStorage::load(fs.clone(), "/s.jsonl", clock, ids)
            .await
            .unwrap();
        // The torn tail was dropped; the file now ends after the valid lane line.
        let content = fs.read_text_file("/s.jsonl", None).await.unwrap();
        assert!(!content.contains("{not json"));
        // And the storage is usable for further appends.
        let _ = reloaded.get_name().await.unwrap();
    }

    #[tokio::test]
    async fn non_last_syntax_error_is_hard_corruption() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());

        let _ = JsonlSessionStorage::create(
            fs.clone(),
            "/s.jsonl",
            header(),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        // Garbage on line 2 (NOT last), then a valid line 3.
        fs.append_file("/s.jsonl", FileContent::Text("{not json\n".into()), None)
            .await
            .unwrap();
        fs.append_file(
            "/s.jsonl",
            FileContent::Text(encode_mutation(&SessionMutation::Lane {
                seq: 1,
                lane: "main".into(),
                leaf_id: None,
            })),
            None,
        )
        .await
        .unwrap();

        let err = JsonlSessionStorage::load(fs, "/s.jsonl", clock, ids)
            .await
            .err()
            .unwrap();
        assert_eq!(err.code, SessionErrorCode::InvalidEntry);
    }

    #[tokio::test]
    async fn undecodable_assistant_frame_does_not_block_load() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());

        JsonlSessionStorage::create(fs.clone(), "/s.jsonl", header(), clock.clone(), ids.clone())
            .await
            .unwrap();

        // The envelope is a valid assistant-frame append, but its nested frame
        // variant is not. This is recoverable progress loss, not history loss.
        fs.append_file(
            "/s.jsonl",
            FileContent::Text(
                r#"{"kind":"record","type":"assistant_frame","id":"bad","seq":1,"lane":"main","timestamp":1,"runId":"run","streamIndex":0,"op":"append","frame":{"type":"not_a_real_frame_variant"}}
"#.into(),
            ),
            None,
        )
        .await
        .unwrap();
        fs.append_file(
            "/s.jsonl",
            FileContent::Text(encode_mutation(&SessionMutation::Lane {
                seq: 2,
                lane: "main".into(),
                leaf_id: None,
            })),
            None,
        )
        .await
        .unwrap();

        let reloaded = JsonlSessionStorage::load(fs, "/s.jsonl", clock, ids)
            .await
            .expect("an undecodable assistant frame must not block load");
        assert_eq!(reloaded.get_name().await.unwrap(), None);
        reloaded
            .append_entry(
                ProvisionedEntry {
                    id: "e1".into(),
                    kind: ProvisionedKind::Message {
                        message: user_msg("after recovery"),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .expect("session remains writable after skipping frame");
    }

    #[tokio::test]
    async fn schema_error_on_last_line_is_hard_corruption() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());

        let _ = JsonlSessionStorage::create(
            fs.clone(),
            "/s.jsonl",
            header(),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        // A schema error (valid JSON, unknown kind) on the last line is NOT a
        // torn tail.
        fs.append_file(
            "/s.jsonl",
            FileContent::Text("{\"kind\":\"bogus\",\"seq\":2}\n".into()),
            None,
        )
        .await
        .unwrap();
        let err = JsonlSessionStorage::load(fs, "/s.jsonl", clock, ids)
            .await
            .err()
            .unwrap();
        assert_eq!(err.code, SessionErrorCode::InvalidEntry);
    }

    #[tokio::test]
    async fn append_record_rejects_second_open_operation() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());

        let storage = JsonlSessionStorage::create(fs, "/s.jsonl", header(), clock, ids)
            .await
            .unwrap();
        let started = LaneRecord::OperationStarted(OperationStartedRecord {
            base: RecordBase {
                id: "op-1".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 0,
            },
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        });
        storage.append_record(started).await.unwrap();
        let second = LaneRecord::OperationStarted(OperationStartedRecord {
            base: RecordBase {
                id: "op-2".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 0,
            },
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        });
        let err = storage.append_record(second).await.unwrap_err();
        assert!(err.to_string().contains("already has an open operation"));
    }
}
