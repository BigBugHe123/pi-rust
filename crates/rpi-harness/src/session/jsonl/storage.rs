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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use rpi_tools::env::{FileContent, FileSystem};
use rpi_tools::FileError;

use crate::error::{SessionError, SessionErrorCode, SessionResult};
use crate::session::jsonl::codec::{
    encode_header, encode_mutation, metadata_from_header, parse_header, parse_mutation,
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
    /// Set when a failed append could not be cut back, so the file may end in
    /// bytes this session cannot account for. Writes are refused until reopen.
    uncertain: AtomicBool,
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
            uncertain: AtomicBool::new(false),
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

    /// Append a mutation line to the file (no in-memory apply). Mirrors TS
    /// `appendMutation`.
    ///
    /// A reported failure can leave part of the line on disk, so the tail is cut
    /// back before the caller may retry. See [`Self::settle_failed_append`].
    async fn append_mutation_line(&self, mutation: &SessionMutation) -> SessionResult<()> {
        if self.uncertain.load(Ordering::SeqCst) {
            return Err(SessionError::storage(format!(
                "Session {} has an unsettled append; reopen it before writing",
                self.metadata.path
            )));
        }
        let line = encode_mutation(mutation);
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
            let result = writer.append(line.as_bytes()).await;
            if let Err(error) = result {
                *append = None;
                drop(append);
                return self.settle_failed_append(&line, error).await;
            }
            return Ok(());
        }
        drop(append);
        match self
            .fs
            .append_file(&self.metadata.path, FileContent::Text(line.clone()), None)
            .await
        {
            Ok(()) => Ok(()),
            Err(error) => self.settle_failed_append(&line, error).await,
        }
    }

    /// Report a failed append, first making the file safe for the next one.
    ///
    /// The append did not commit, so its state is not applied and the caller
    /// may retry with the same sequence number. The bytes it left behind must
    /// not be built on, so the tail is cut back to where `line` began. If the
    /// tail cannot be proven to be this line's own bytes, or the cut fails, the
    /// session becomes uncertain and refuses writes until it is reopened, which
    /// repairs the tail through [`Self::load`].
    async fn settle_failed_append(&self, line: &str, error: FileError) -> SessionResult<()> {
        let failure = file_result::<()>(
            Err(error),
            &format!("Failed to append session {}", self.metadata.path),
        );
        if self.cut_back_failed_append(line).await.is_err() {
            self.uncertain.store(true, Ordering::SeqCst);
        }
        failure
    }

    /// Truncate the file to where the failed `line` began. Fails when the
    /// trailing bytes cannot be proven to be this line's own.
    async fn cut_back_failed_append(&self, line: &str) -> Result<(), ()> {
        let path = &self.metadata.path;
        let bytes = self.fs.read_binary_file(path, None).await.map_err(|_| ())?;
        let start = failed_append_start(&bytes, line.as_bytes()).ok_or(())?;
        if start == bytes.len() {
            return Ok(());
        }
        self.fs
            .truncate_file(path, start as u64, None)
            .await
            .map_err(|_| ())
    }
}

/// Where a failed append of `line` began in `bytes`, or `None` when the bytes
/// after the last newline cannot be proven to be a fragment of `line`.
///
/// Every acknowledged line ends in a newline, so the bytes after the last one
/// are never acknowledged content. A complete copy of `line` at the end is also
/// this append's own, because its state was never applied.
fn failed_append_start(bytes: &[u8], line: &[u8]) -> Option<usize> {
    if bytes.ends_with(line) {
        return Some(bytes.len() - line.len());
    }
    let start = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    line.starts_with(&bytes[start..]).then_some(start)
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

    async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        let lane = record.lane().to_string();
        let is_operation_started = matches!(record, LaneRecord::OperationStarted(_));
        let _g = self.tail.lock().await;
        let state = self.state.lock().await;
        state.require_lane(&lane)?;
        state.validate_unused_id(record.id())?;
        if is_operation_started {
            let open = state.find_open_operations(&lane, Some(1))?;
            if let Some(current) = open.first() {
                return Err(SessionError::storage(format!(
                    "Lane {} already has an open operation {}",
                    lane, current.base.id
                )));
            }
        }
        let seq = state.next_sequence();
        let timestamp = self.clock.now_ms();
        let stamped = stamp_record(record, seq, lane.clone(), timestamp);
        let mutation = SessionMutation::Record { record: stamped };
        drop(state);
        self.append_mutation_line(&mutation).await?;
        let mut state = self.state.lock().await;
        match state.apply_mutation(mutation)? {
            ApplyOutcome::Record(stamped) => Ok(stamped),
            _ => Err(SessionError::storage("append_record did not persist")),
        }
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

    /// How a reusable append handle left the file when it reported failure.
    #[derive(Clone, Copy)]
    enum Tear {
        /// The first half of the line landed, then the write failed.
        Half,
        /// The whole line landed, but the write still reported failure.
        Whole,
        /// Bytes that are not a prefix of the line.
        Foreign,
    }

    /// A reusable append handle that writes part of a line and then fails, the
    /// way `write_all` does when the disk fills or an I/O error interrupts it.
    struct TearingAppend {
        fs: Arc<dyn FileSystem>,
        path: String,
        tear: Tear,
    }

    #[async_trait::async_trait]
    impl rpi_tools::env::AppendFile for TearingAppend {
        async fn append(&mut self, bytes: &[u8]) -> Result<(), rpi_tools::FileError> {
            let torn: &[u8] = match self.tear {
                Tear::Half => &bytes[..bytes.len() / 2],
                Tear::Whole => bytes,
                Tear::Foreign => b"XYZ",
            };
            self.fs
                .append_file(
                    &self.path,
                    FileContent::Text(String::from_utf8_lossy(torn).into_owned()),
                    None,
                )
                .await?;
            Err(rpi_tools::FileError::new(
                rpi_tools::FileErrorCode::Unknown,
                "injected partial write",
            ))
        }
    }

    fn provisioned_message(id: &str) -> ProvisionedEntry {
        ProvisionedEntry {
            id: id.into(),
            kind: ProvisionedKind::Message {
                message: user_msg(id),
                terminate: None,
            },
        }
    }

    /// A session on a real directory, so the retry uses the production reusable
    /// append handle rather than the one-shot fallback.
    async fn session_on_disk(
        temp: &tempfile::TempDir,
    ) -> (
        Arc<dyn FileSystem>,
        JsonlSessionStorage,
        Arc<dyn Clock>,
        Arc<dyn IdGenerator>,
    ) {
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
        (fs, storage, clock, ids)
    }

    async fn install_tearing_writer(
        storage: &JsonlSessionStorage,
        fs: &Arc<dyn FileSystem>,
        tear: Tear,
    ) {
        *storage.append.lock().await = Some(Box::new(TearingAppend {
            fs: fs.clone(),
            path: "session.jsonl".into(),
            tear,
        }));
    }

    #[tokio::test]
    async fn failed_append_that_tore_a_prefix_is_cut_back_before_the_retry() {
        let temp = tempfile::tempdir().unwrap();
        let (fs, storage, clock, ids) = session_on_disk(&temp).await;
        storage
            .append_entry(provisioned_message("kept"), "main")
            .await
            .unwrap();
        install_tearing_writer(&storage, &fs, Tear::Half).await;
        assert!(storage
            .append_entry(provisioned_message("retry"), "main")
            .await
            .is_err());
        storage
            .append_entry(provisioned_message("retry"), "main")
            .await
            .expect("the retry is acknowledged");
        // A later line stops the retry from being the physical last line, so a
        // fragment left in front of it would make the file unopenable.
        storage
            .append_entry(provisioned_message("later"), "main")
            .await
            .unwrap();
        let reloaded = JsonlSessionStorage::load(fs, "session.jsonl", clock, ids)
            .await
            .expect("the session reloads after a torn append and a retry");
        for id in ["kept", "retry", "later"] {
            assert!(
                reloaded.get_entry(id).await.unwrap().is_some(),
                "acknowledged entry {id} must survive reload"
            );
        }
    }

    #[test]
    fn failed_append_start_proves_only_the_fragment_of_its_own_line() {
        let line: &[u8] = b"{\"seq\":2}\n";
        let head: &[u8] = b"h\n{\"seq\":1}\n";
        // Nothing was written: the tail is clean and nothing needs cutting.
        assert_eq!(failed_append_start(head, line), Some(head.len()));
        // A fragment of this line after the last newline is this append's own.
        let torn = [head, b"{\"se"].concat();
        assert_eq!(failed_append_start(&torn, line), Some(head.len()));
        // A fragment that differs from the line is not provably this append's.
        let foreign = [head, b"{\"seq\":3"].concat();
        assert_eq!(failed_append_start(&foreign, line), None);
        let unknown = [head, b"XYZ"].concat();
        assert_eq!(failed_append_start(&unknown, line), None);
        // A whole copy of the line at the end is removed from its start.
        let whole = [head, line].concat();
        assert_eq!(failed_append_start(&whole, line), Some(head.len()));
        // Without a trailing newline, a complete line is still this append's.
        let unterminated = [head, &line[..line.len() - 1]].concat();
        assert_eq!(failed_append_start(&unterminated, line), Some(head.len()));
        assert_eq!(failed_append_start(b"", line), Some(0));
    }

    #[tokio::test]
    async fn acknowledged_retry_after_a_torn_prefix_survives_reload_when_nothing_follows() {
        let temp = tempfile::tempdir().unwrap();
        let (fs, storage, clock, ids) = session_on_disk(&temp).await;
        install_tearing_writer(&storage, &fs, Tear::Half).await;
        assert!(storage
            .append_entry(provisioned_message("retry"), "main")
            .await
            .is_err());
        storage
            .append_entry(provisioned_message("retry"), "main")
            .await
            .expect("the retry is acknowledged");
        // The retry is the last physical line, so load's torn-tail repair would
        // drop the whole line, fragment and acknowledged retry together.
        let reloaded = JsonlSessionStorage::load(fs, "session.jsonl", clock, ids)
            .await
            .expect("the session reloads");
        assert!(
            reloaded.get_entry("retry").await.unwrap().is_some(),
            "an acknowledged append must survive reload"
        );
    }

    #[tokio::test]
    async fn failed_append_that_landed_whole_is_removed_before_the_retry() {
        let temp = tempfile::tempdir().unwrap();
        let (fs, storage, clock, ids) = session_on_disk(&temp).await;
        storage
            .append_entry(provisioned_message("kept"), "main")
            .await
            .unwrap();
        install_tearing_writer(&storage, &fs, Tear::Whole).await;
        assert!(storage
            .append_entry(provisioned_message("rejected"), "main")
            .await
            .is_err());
        // The caller may retry with different content. The rejected line must not
        // reappear on reload and shadow the acknowledged retry.
        storage
            .append_entry(provisioned_message("retry"), "main")
            .await
            .expect("the retry is acknowledged");
        let reloaded = JsonlSessionStorage::load(fs, "session.jsonl", clock, ids)
            .await
            .expect("the session reloads");
        assert!(reloaded.get_entry("retry").await.unwrap().is_some());
        assert!(
            reloaded.get_entry("rejected").await.unwrap().is_none(),
            "a rejected append must not be committed on reload"
        );
    }

    #[tokio::test]
    async fn failed_append_with_an_unknown_tail_stays_uncertain_until_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let (fs, storage, clock, ids) = session_on_disk(&temp).await;
        storage
            .append_entry(provisioned_message("kept"), "main")
            .await
            .unwrap();
        install_tearing_writer(&storage, &fs, Tear::Foreign).await;
        assert!(storage
            .append_entry(provisioned_message("rejected"), "main")
            .await
            .is_err());
        assert!(
            storage
                .append_entry(provisioned_message("retry"), "main")
                .await
                .is_err(),
            "writes must be refused while the tail cannot be proven to be this append's"
        );
        let reopened =
            JsonlSessionStorage::load(fs.clone(), "session.jsonl", clock.clone(), ids.clone())
                .await
                .expect("reopen repairs the unknown tail");
        reopened
            .append_entry(provisioned_message("retry"), "main")
            .await
            .expect("writes are accepted after reopen");
        let reloaded = JsonlSessionStorage::load(fs, "session.jsonl", clock, ids)
            .await
            .expect("the session reloads");
        assert!(reloaded.get_entry("kept").await.unwrap().is_some());
        assert!(reloaded.get_entry("retry").await.unwrap().is_some());
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
