//! Transfer engine: planning, conflict handling, bounded streaming pipeline,
//! atomic commit protocol, and basic integrity verification.
//! Depends only on `common` (never `ssh` or `tui`).

use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use common::{
    DEFAULT_IO_CHUNK_SIZE, Endpoint, FileKind, FileSystem, FsError, FsPath, FsResolver, FsResult,
    Location, MAX_IO_CHUNK_SIZE, Metadata, MetadataPatch, RenameMode, SyncOutcome, WriteOptions,
    bytes_from_os_str, contains_replacement_bytes, is_valid_entry_name, validate_windows_name,
};
use thiserror::Error;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

/// Default bounded channel depth between reader and writer tasks (`K` chunks).
pub const DEFAULT_CHANNEL_CHUNKS: usize = 4;

#[derive(Clone, Debug)]
pub struct TransferConfig {
    pub chunk_size: u32,
    pub channel_chunks: usize,
    pub progress_interval: Duration,
    pub enforce_windows_names: bool,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            chunk_size: DEFAULT_IO_CHUNK_SIZE,
            channel_chunks: DEFAULT_CHANNEL_CHUNKS,
            progress_interval: Duration::from_millis(100),
            enforce_windows_names: cfg!(windows),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TransferRequest {
    pub id: String,
    pub source: Endpoint,
    pub destination: Endpoint,
}

impl TransferRequest {
    pub fn new(id: impl Into<String>, source: Endpoint, destination: Endpoint) -> Self {
        Self {
            id: id.into(),
            source,
            destination,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ConflictPolicy {
    /// Stop after planning if any destination target already exists.
    #[default]
    Ask,
    Overwrite,
    Skip,
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrityLevel {
    /// Writer completed + byte count matches + source unchanged + target size matches.
    Basic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobState {
    Queued,
    Planning,
    AwaitingDecision,
    Running,
    Completed,
    CompletedWithIssues,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ItemState {
    Pending,
    Transferring,
    Verifying,
    Committing,
    Done,
    Skipped,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlannedOp {
    CreateDir {
        mode: u32,
        already_exists: bool,
    },
    CopyFile {
        initial_meta: Metadata,
        destination_exists: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedItem {
    pub source: FsPath,
    pub destination: FsPath,
    pub op: PlannedOp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkippedEntry {
    pub path: FsPath,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct TransferPlan {
    pub request: TransferRequest,
    pub items: Vec<PlannedItem>,
    pub skipped: Vec<SkippedEntry>,
    pub conflicts: Vec<FsPath>,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferProgress {
    pub job_id: String,
    pub state: JobState,
    pub current_item: Option<FsPath>,
    pub items_done: usize,
    pub items_total: usize,
    pub bytes_transferred: u64,
    pub bytes_total: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ItemReport {
    pub source: FsPath,
    pub destination: FsPath,
    pub state: ItemState,
    pub bytes_written: u64,
    pub integrity: Option<IntegrityLevel>,
    pub sync_outcome: Option<SyncOutcome>,
    pub warning: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TransferReport {
    pub job_id: String,
    pub state: JobState,
    pub items: Vec<ItemReport>,
    pub skipped: Vec<SkippedEntry>,
    pub residual_temp_files: Vec<FsPath>,
    pub bytes_transferred: u64,
    pub bytes_total: u64,
    pub error: Option<String>,
}

#[derive(Debug, Error)]
pub enum TransferError {
    #[error("invalid transfer configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("source and destination refer to the same path")]
    SameEndpoint,
    #[error("cannot copy a directory into itself or its descendant")]
    SubdirectoryTarget,
    #[error("target is already locked by an active transfer: {0}")]
    TargetLocked(String),
    #[error("conflicts require user decision ({} existing target(s))", .paths.len())]
    Conflict { paths: Vec<FsPath> },
    #[error("source file changed during transfer: {}", .path.display_name())]
    SourceModified { path: FsPath },
    #[error("transferred byte count ({actual}) does not match expected size ({expected})")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("transfer cancelled")]
    Cancelled,
    #[error("transfer interrupted by connection loss: {0}")]
    Interrupted(String),
    #[error("transfer reader task failed: {0}")]
    ReaderTask(#[source] tokio::task::JoinError),
    #[error(transparent)]
    Fs(#[from] FsError),
}

/// Cooperative cancellation token independent of external utility crates.
#[derive(Clone, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::SeqCst) {
            self.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

/// Tracks exclusive destination targets across active transfer jobs (ARCHITECTURE §7.2).
#[derive(Clone, Default)]
pub struct TargetLocks {
    active: Arc<Mutex<HashSet<(Location, FsPath)>>>,
}

pub struct TargetLockGuard {
    locks: TargetLocks,
    keys: Vec<(Location, FsPath)>,
}

impl Drop for TargetLockGuard {
    fn drop(&mut self) {
        let mut guard = self.locks.active.lock().expect("TargetLocks poisoned");
        for key in &self.keys {
            guard.remove(key);
        }
    }
}

impl TargetLocks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn acquire(
        &self,
        location: &Location,
        paths: &[FsPath],
    ) -> Result<TargetLockGuard, TransferError> {
        let mut guard = self.active.lock().expect("TargetLocks poisoned");
        for path in paths {
            let key = (location.clone(), path.clone());
            if guard.contains(&key) {
                return Err(TransferError::TargetLocked(path.display_name()));
            }
        }
        let mut keys = Vec::with_capacity(paths.len());
        for path in paths {
            let key = (location.clone(), path.clone());
            guard.insert(key.clone());
            keys.push(key);
        }
        Ok(TargetLockGuard {
            locks: self.clone(),
            keys,
        })
    }
}

fn entry_raw_bytes(path: &FsPath) -> FsResult<Vec<u8>> {
    match path {
        FsPath::Local(p) => {
            let name = p.file_name().ok_or(FsError::UnsafePath)?;
            Ok(bytes_from_os_str(name))
        }
        FsPath::Remote(p) => {
            let name = p.file_name().ok_or(FsError::UnsafePath)?;
            Ok(name.to_vec())
        }
    }
}

fn validate_target_path(path: &FsPath, location: &Location, enforce_windows: bool) -> FsResult<()> {
    let raw = entry_raw_bytes(path)?;
    if !is_valid_entry_name(&raw) || contains_replacement_bytes(&raw) {
        return Err(FsError::UnsafeName);
    }
    if enforce_windows && location.is_local() {
        validate_windows_name(&raw)?;
    }
    Ok(())
}

fn is_same_or_descendant(parent: &FsPath, candidate: &FsPath) -> (bool, bool) {
    match (parent, candidate) {
        (FsPath::Local(a), FsPath::Local(b)) => {
            let same = a == b;
            let desc = !same && b.starts_with(a);
            (same, desc)
        }
        (FsPath::Remote(a), FsPath::Remote(b)) => {
            let a_bytes = a.as_bytes();
            let b_bytes = b.as_bytes();
            if a_bytes == b_bytes {
                return (true, false);
            }
            if b_bytes.starts_with(a_bytes)
                && (a_bytes.ends_with(b"/") || b_bytes.get(a_bytes.len()) == Some(&b'/'))
            {
                return (false, true);
            }
            (false, false)
        }
        _ => (false, false),
    }
}

pub fn temp_part_path(target: &FsPath, job_id: &str) -> FsResult<FsPath> {
    let parent = target.parent().ok_or(FsError::UnsafePath)?;
    let name = entry_raw_bytes(target)?;
    if !is_valid_entry_name(&name) || contains_replacement_bytes(&name) {
        return Err(FsError::UnsafeName);
    }
    let short_id: String = job_id
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .take(12)
        .collect();
    let short_id = if short_id.is_empty() {
        "job"
    } else {
        &short_id
    };
    let mut temp_name = Vec::with_capacity(name.len() + short_id.len() + 14);
    temp_name.push(b'.');
    temp_name.extend_from_slice(&name);
    temp_name.extend_from_slice(b".relay-");
    temp_name.extend_from_slice(short_id.as_bytes());
    temp_name.extend_from_slice(b".part");
    parent.join_entry(&temp_name)
}

/// Phase 1 (Planning): traverses the source (following top-level file symlinks,
/// skipping nested symlinks and special files), checks destination conflicts
/// and `statvfs` free space, and produces an immutable [`TransferPlan`].
pub async fn plan_transfer(
    resolver: &FsResolver,
    config: &TransferConfig,
    request: TransferRequest,
    cancel: &CancelToken,
) -> Result<TransferPlan, TransferError> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(TransferError::Cancelled),
        result = plan_transfer_inner(resolver, config, request, cancel) => result,
    }
}

async fn plan_transfer_inner(
    resolver: &FsResolver,
    config: &TransferConfig,
    request: TransferRequest,
    cancel: &CancelToken,
) -> Result<TransferPlan, TransferError> {
    if config.chunk_size == 0 || config.channel_chunks == 0 {
        return Err(TransferError::InvalidConfig(
            "chunk size and channel depth must be nonzero",
        ));
    }
    if cancel.is_cancelled() {
        return Err(TransferError::Cancelled);
    }
    let src_fs = resolver.resolve(request.source.location())?;
    let dst_fs = resolver.resolve(request.destination.location())?;

    let src_root = request.source.path();
    let dst_root = request.destination.path();
    validate_target_path(
        dst_root,
        request.destination.location(),
        config.enforce_windows_names,
    )?;

    if request.source.location() == request.destination.location() {
        let src_canon = src_fs
            .canonicalize(src_root)
            .await
            .unwrap_or_else(|_| src_root.clone());
        let dst_canon = match dst_fs.canonicalize(dst_root).await {
            Ok(c) => c,
            Err(_) => {
                if let Some(parent) = dst_root.parent()
                    && let Ok(canon_parent) = dst_fs.canonicalize(&parent).await
                    && let Ok(name) = entry_raw_bytes(dst_root)
                {
                    canon_parent
                        .join_entry(&name)
                        .unwrap_or_else(|_| dst_root.clone())
                } else {
                    dst_root.clone()
                }
            }
        };
        let (same, descendant) = is_same_or_descendant(&src_canon, &dst_canon);
        if same {
            return Err(TransferError::SameEndpoint);
        }
        if descendant {
            return Err(TransferError::SubdirectoryTarget);
        }
    }

    // Directly selected source uses `stat` so a directly selected file symlink copies its target.
    let root_meta = src_fs.stat(src_root).await?;
    let mut items = Vec::new();
    let mut skipped = Vec::new();
    let mut conflicts = Vec::new();
    let mut total_bytes = 0u64;

    match root_meta.kind {
        FileKind::File => {
            let dst_exists = match dst_fs.lstat(dst_root).await {
                Ok(existing) => {
                    if existing.kind == FileKind::Directory {
                        return Err(TransferError::Fs(FsError::AlreadyExists));
                    }
                    conflicts.push(dst_root.clone());
                    true
                }
                Err(FsError::NotFound) => false,
                Err(err) => return Err(TransferError::Fs(err)),
            };
            total_bytes = total_bytes.saturating_add(root_meta.size);
            items.push(PlannedItem {
                source: src_root.clone(),
                destination: dst_root.clone(),
                op: PlannedOp::CopyFile {
                    initial_meta: root_meta,
                    destination_exists: dst_exists,
                },
            });
        }
        FileKind::Directory => {
            let mut stack = vec![(src_root.clone(), dst_root.clone(), root_meta)];
            while let Some((cur_src, cur_dst, cur_meta)) = stack.pop() {
                if cancel.is_cancelled() {
                    return Err(TransferError::Cancelled);
                }
                let dir_exists = match dst_fs.lstat(&cur_dst).await {
                    Ok(existing) => {
                        if existing.kind != FileKind::Directory {
                            // Never traverse a destination symlink or merge a directory into a file.
                            return Err(FsError::AlreadyExists.into());
                        }
                        true
                    }
                    Err(FsError::NotFound) => false,
                    Err(err) => return Err(TransferError::Fs(err)),
                };
                items.push(PlannedItem {
                    source: cur_src.clone(),
                    destination: cur_dst.clone(),
                    op: PlannedOp::CreateDir {
                        mode: cur_meta.sanitized_permissions().unwrap_or(0o755),
                        already_exists: dir_exists,
                    },
                });

                let mut entries = src_fs.read_dir(&cur_src).await?;
                if config.enforce_windows_names && request.destination.location().is_local() {
                    let names: Vec<String> = entries
                        .iter()
                        .map(|e| String::from_utf8_lossy(&e.raw_name).into_owned())
                        .collect();
                    if common::find_case_insensitive_conflict(&names).is_some() {
                        return Err(TransferError::Fs(FsError::UnsafeName));
                    }
                }
                // Push in reverse so stack pops in sorted order.
                entries.reverse();
                for entry in entries {
                    if cancel.is_cancelled() {
                        return Err(TransferError::Cancelled);
                    }
                    entry.ensure_mutable()?;
                    let child_dst = cur_dst.join_entry(&entry.raw_name)?;
                    validate_target_path(
                        &child_dst,
                        request.destination.location(),
                        config.enforce_windows_names,
                    )?;
                    match entry.kind {
                        FileKind::Directory => {
                            stack.push((entry.path, child_dst, entry.metadata));
                        }
                        FileKind::File => {
                            let dst_exists = match dst_fs.lstat(&child_dst).await {
                                Ok(existing) => {
                                    if existing.kind == FileKind::Directory {
                                        return Err(TransferError::Fs(FsError::AlreadyExists));
                                    }
                                    conflicts.push(child_dst.clone());
                                    true
                                }
                                Err(FsError::NotFound) => false,
                                Err(err) => return Err(TransferError::Fs(err)),
                            };
                            total_bytes = total_bytes.saturating_add(entry.metadata.size);
                            items.push(PlannedItem {
                                source: entry.path,
                                destination: child_dst,
                                op: PlannedOp::CopyFile {
                                    initial_meta: entry.metadata,
                                    destination_exists: dst_exists,
                                },
                            });
                        }
                        FileKind::Symlink => {
                            skipped.push(SkippedEntry {
                                path: entry.path,
                                reason: "nested symlink skipped by default policy".into(),
                            });
                        }
                        FileKind::Other => {
                            skipped.push(SkippedEntry {
                                path: entry.path,
                                reason: "special file (FIFO/device/socket) skipped".into(),
                            });
                        }
                    }
                }
            }
        }
        FileKind::Symlink | FileKind::Other => {
            skipped.push(SkippedEntry {
                path: src_root.clone(),
                reason: "unsupported special file".into(),
            });
        }
    }

    // Pre-check destination free space via statvfs if supported.
    if total_bytes > 0
        && let Some(parent) = dst_root.parent()
        && let Ok(Some(avail)) = dst_fs.free_space(&parent).await
        && avail < total_bytes
    {
        return Err(TransferError::Fs(FsError::NoSpace));
    }

    Ok(TransferPlan {
        request,
        items,
        skipped,
        conflicts,
        total_bytes,
    })
}

/// Phase 2 (Execution): executes a [`TransferPlan`] according to the chosen
/// [`ConflictPolicy`], streaming file chunks through a bounded channel of `K`
/// buffers and committing via temporary `.part` files + `rename`.
pub async fn execute_plan<F>(
    resolver: &FsResolver,
    config: &TransferConfig,
    locks: &TargetLocks,
    plan: TransferPlan,
    policy: ConflictPolicy,
    cancel: &CancelToken,
    mut on_progress: F,
) -> Result<TransferReport, TransferError>
where
    F: FnMut(&TransferProgress) + Send,
{
    let job_id = plan.request.id.clone();
    if cancel.is_cancelled() || policy == ConflictPolicy::Cancel {
        return Ok(TransferReport {
            job_id,
            state: JobState::Cancelled,
            items: Vec::new(),
            skipped: plan.skipped,
            residual_temp_files: Vec::new(),
            bytes_transferred: 0,
            bytes_total: plan.total_bytes,
            error: None,
        });
    }
    if !plan.conflicts.is_empty() {
        match policy {
            ConflictPolicy::Ask => {
                return Err(TransferError::Conflict {
                    paths: plan.conflicts,
                });
            }
            ConflictPolicy::Overwrite | ConflictPolicy::Skip | ConflictPolicy::Cancel => {}
        }
    }

    let src_fs = resolver.resolve(plan.request.source.location())?;
    let dst_fs = resolver.resolve(plan.request.destination.location())?;

    // ADR-014: If overwriting any existing file and destination lacks atomic posix-rename,
    // refuse before mutating anything on the destination.
    if policy == ConflictPolicy::Overwrite
        && !plan.conflicts.is_empty()
        && !dst_fs.capabilities().posix_rename
    {
        return Err(TransferError::Fs(FsError::Unsupported));
    }

    let mut target_paths = Vec::with_capacity(plan.items.len());
    for item in &plan.items {
        target_paths.push(canonical_target(&dst_fs, &item.destination).await?);
    }
    let _lock_guard = locks.acquire(plan.request.destination.location(), &target_paths)?;

    let effective_chunk = config
        .chunk_size
        .min(src_fs.capabilities().max_read_size)
        .min(dst_fs.capabilities().max_write_size)
        .min(MAX_IO_CHUNK_SIZE) as usize;
    if effective_chunk == 0 || config.channel_chunks == 0 {
        return Err(TransferError::InvalidConfig(
            "chunk size and channel depth must be nonzero",
        ));
    }
    let channel_depth = config.channel_chunks.clamp(1, 32);

    let items_total = plan.items.len();
    let mut item_reports = Vec::with_capacity(items_total);
    let mut residual_temp_files = Vec::new();
    let mut bytes_transferred = 0u64;
    let mut has_issues = !plan.skipped.is_empty();
    let mut last_emit = Instant::now()
        .checked_sub(config.progress_interval)
        .unwrap_or_else(Instant::now);

    for (idx, item) in plan.items.into_iter().enumerate() {
        if cancel.is_cancelled() {
            return Ok(TransferReport {
                job_id,
                state: JobState::Cancelled,
                items: item_reports,
                skipped: plan.skipped,
                residual_temp_files,
                bytes_transferred,
                bytes_total: plan.total_bytes,
                error: Some("transfer cancelled".into()),
            });
        }

        match item.op {
            PlannedOp::CreateDir {
                mode,
                already_exists,
            } => {
                if !already_exists {
                    match dst_fs.create_dir(&item.destination, mode).await {
                        Ok(()) => {}
                        Err(FsError::AlreadyExists)
                            if dst_fs.lstat(&item.destination).await?.kind
                                == FileKind::Directory => {}
                        Err(FsError::ConnectionLost) => {
                            return Ok(TransferReport {
                                job_id,
                                state: JobState::Interrupted,
                                items: item_reports,
                                skipped: plan.skipped,
                                residual_temp_files,
                                bytes_transferred,
                                bytes_total: plan.total_bytes,
                                error: Some(FsError::ConnectionLost.to_string()),
                            });
                        }
                        Err(err) => {
                            item_reports.push(ItemReport {
                                source: item.source,
                                destination: item.destination,
                                state: ItemState::Failed,
                                bytes_written: 0,
                                integrity: None,
                                sync_outcome: None,
                                warning: None,
                                error: Some(err.to_string()),
                            });
                            return Ok(TransferReport {
                                job_id,
                                state: JobState::Failed,
                                items: item_reports,
                                skipped: plan.skipped,
                                residual_temp_files,
                                bytes_transferred,
                                bytes_total: plan.total_bytes,
                                error: Some(err.to_string()),
                            });
                        }
                    }
                }
                item_reports.push(ItemReport {
                    source: item.source,
                    destination: item.destination,
                    state: ItemState::Done,
                    bytes_written: 0,
                    integrity: None,
                    sync_outcome: None,
                    warning: None,
                    error: None,
                });
            }
            PlannedOp::CopyFile {
                initial_meta,
                destination_exists,
            } => {
                if destination_exists && policy == ConflictPolicy::Skip {
                    has_issues = true;
                    item_reports.push(ItemReport {
                        source: item.source,
                        destination: item.destination,
                        state: ItemState::Skipped,
                        bytes_written: 0,
                        integrity: None,
                        sync_outcome: None,
                        warning: None,
                        error: None,
                    });
                    continue;
                }

                let temp_path = temp_part_path(&item.destination, &job_id)?;
                let mut temp_created = false;
                let bytes_before = bytes_transferred;
                let copy_res = copy_single_file(
                    &job_id,
                    &src_fs,
                    &dst_fs,
                    &item.source,
                    &item.destination,
                    &temp_path,
                    &mut temp_created,
                    &initial_meta,
                    destination_exists,
                    effective_chunk,
                    channel_depth,
                    config.progress_interval,
                    idx,
                    items_total,
                    &mut bytes_transferred,
                    plan.total_bytes,
                    &mut last_emit,
                    cancel,
                    &mut on_progress,
                )
                .await;

                match copy_res {
                    Ok((written, sync_outcome, warning)) => {
                        has_issues |= warning.is_some();
                        item_reports.push(ItemReport {
                            source: item.source,
                            destination: item.destination,
                            state: ItemState::Done,
                            bytes_written: written,
                            integrity: Some(IntegrityLevel::Basic),
                            sync_outcome: Some(sync_outcome),
                            warning,
                            error: None,
                        });
                    }
                    Err(err) => {
                        // Best-effort cleanup of the uncommitted temporary `.part` file.
                        if temp_created {
                            match dst_fs.remove_file(&temp_path).await {
                                Ok(()) | Err(FsError::NotFound) => {}
                                Err(_) => residual_temp_files.push(temp_path),
                            }
                        }
                        let (job_state, item_state) = match &err {
                            TransferError::Cancelled | TransferError::Fs(FsError::Cancelled) => {
                                (JobState::Cancelled, ItemState::Cancelled)
                            }
                            TransferError::Interrupted(_)
                            | TransferError::Fs(FsError::ConnectionLost) => {
                                (JobState::Interrupted, ItemState::Failed)
                            }
                            _ => (JobState::Failed, ItemState::Failed),
                        };
                        let msg = err.to_string();
                        item_reports.push(ItemReport {
                            source: item.source,
                            destination: item.destination,
                            state: item_state,
                            bytes_written: bytes_transferred - bytes_before,
                            integrity: None,
                            sync_outcome: None,
                            warning: None,
                            error: Some(msg.clone()),
                        });
                        return Ok(TransferReport {
                            job_id,
                            state: job_state,
                            items: item_reports,
                            skipped: plan.skipped,
                            residual_temp_files,
                            bytes_transferred,
                            bytes_total: plan.total_bytes,
                            error: Some(msg),
                        });
                    }
                }
            }
        }
    }

    let final_state = if has_issues {
        JobState::CompletedWithIssues
    } else {
        JobState::Completed
    };
    on_progress(&TransferProgress {
        job_id: job_id.clone(),
        state: final_state,
        current_item: None,
        items_done: items_total,
        items_total,
        bytes_transferred,
        bytes_total: plan.total_bytes,
    });

    Ok(TransferReport {
        job_id,
        state: final_state,
        items: item_reports,
        skipped: plan.skipped,
        residual_temp_files,
        bytes_transferred,
        bytes_total: plan.total_bytes,
        error: None,
    })
}

/// Canonicalize the nearest existing parent, including destinations under new directories.
async fn canonical_target(fs: &Arc<dyn FileSystem>, path: &FsPath) -> FsResult<FsPath> {
    let mut names = vec![entry_raw_bytes(path)?];
    let mut parent = path.parent().ok_or(FsError::UnsafePath)?;
    loop {
        match fs.canonicalize(&parent).await {
            Ok(mut canonical) => {
                for name in names.iter().rev() {
                    canonical = canonical.join_entry(name)?;
                }
                return Ok(canonical);
            }
            Err(FsError::NotFound) => {
                names.push(entry_raw_bytes(&parent)?);
                parent = parent.parent().ok_or(FsError::UnsafePath)?;
            }
            Err(error) => return Err(error),
        }
    }
}

struct ReaderTask(JoinHandle<FsResult<u64>>);

impl Drop for ReaderTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Convenience helper that plans and executes a transfer in one call.
pub async fn run_transfer<F>(
    resolver: &FsResolver,
    config: &TransferConfig,
    locks: &TargetLocks,
    request: TransferRequest,
    policy: ConflictPolicy,
    cancel: &CancelToken,
    on_progress: F,
) -> Result<TransferReport, TransferError>
where
    F: FnMut(&TransferProgress) + Send,
{
    let plan = plan_transfer(resolver, config, request, cancel).await?;
    execute_plan(resolver, config, locks, plan, policy, cancel, on_progress).await
}

#[allow(clippy::too_many_arguments)]
async fn copy_single_file<F>(
    job_id: &str,
    src_fs: &Arc<dyn FileSystem>,
    dst_fs: &Arc<dyn FileSystem>,
    src_path: &FsPath,
    dst_path: &FsPath,
    temp_path: &FsPath,
    temp_created: &mut bool,
    initial_meta: &Metadata,
    overwrite: bool,
    chunk_size: usize,
    channel_depth: usize,
    progress_interval: Duration,
    item_idx: usize,
    items_total: usize,
    bytes_transferred: &mut u64,
    bytes_total: u64,
    last_emit: &mut Instant,
    cancel: &CancelToken,
    on_progress: &mut F,
) -> Result<(u64, SyncOutcome, Option<String>), TransferError>
where
    F: FnMut(&TransferProgress) + Send,
{
    // Step 1: Open source reader and create temporary `.part` writer with 0600 permissions.
    if cancel.is_cancelled() {
        return Err(TransferError::Cancelled);
    }
    let mut reader = src_fs.open_read(src_path).await?;
    let mut writer = dst_fs
        .open_write(temp_path, WriteOptions::create_new(0o600))
        .await?;
    *temp_created = true;

    // Step 2: Bounded streaming relay (FileReader -> mpsc::channel(K) -> FileWriter).
    let (tx, mut rx) = mpsc::channel::<FsResult<Vec<u8>>>(channel_depth);
    let reader_cancel = cancel.clone();
    let mut reader_task = ReaderTask(tokio::spawn(async move {
        let mut total_read = 0u64;
        loop {
            let chunk_res = tokio::select! {
                biased;
                () = reader_cancel.cancelled() => Err(FsError::Cancelled),
                res = reader.read_chunk(chunk_size) => res,
            };
            match chunk_res {
                Ok(chunk) if chunk.is_empty() => {
                    reader.close().await?;
                    return Ok(total_read);
                }
                Ok(chunk) => {
                    total_read = total_read.saturating_add(chunk.len() as u64);
                    if tx.send(Ok(chunk)).await.is_err() {
                        return Err(FsError::Cancelled);
                    }
                }
                Err(err) => {
                    let _ = tx.send(Err(err)).await;
                    return Err(FsError::Cancelled);
                }
            }
        }
    }));

    let mut written = 0u64;
    while let Some(msg) = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            return Err(TransferError::Cancelled);
        }
        msg = rx.recv() => msg,
    } {
        let chunk = match msg {
            Ok(chunk) => chunk,
            Err(err) => {
                return Err(TransferError::Fs(err));
            }
        };
        if let Err(err) = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(FsError::Cancelled),
            res = writer.write_all(&chunk) => res,
        } {
            return Err(match err {
                FsError::Cancelled => TransferError::Cancelled,
                other => TransferError::Fs(other),
            });
        }
        let len = chunk.len() as u64;
        written = written.saturating_add(len);
        *bytes_transferred = bytes_transferred.saturating_add(len);

        if last_emit.elapsed() >= progress_interval {
            on_progress(&TransferProgress {
                job_id: job_id.to_owned(),
                state: JobState::Running,
                current_item: Some(dst_path.clone()),
                items_done: item_idx,
                items_total,
                bytes_transferred: *bytes_transferred,
                bytes_total,
            });
            *last_emit = Instant::now();
        }
    }

    let total_read = match (&mut reader_task.0).await {
        Ok(Ok(read_bytes)) => read_bytes,
        Ok(Err(FsError::Cancelled)) if cancel.is_cancelled() => {
            return Err(TransferError::Cancelled);
        }
        Ok(Err(err)) => return Err(TransferError::Fs(err)),
        Err(error) => return Err(TransferError::ReaderTask(error)),
    };

    // Step 3: Sync & close temporary file.
    let sync_outcome = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(TransferError::Cancelled),
        result = writer.sync() => result?,
    };
    writer.close().await?;
    if cancel.is_cancelled() {
        return Err(TransferError::Cancelled);
    }

    // Step 4: Basic verification (ARCHITECTURE §7.3 & §7.5).
    if written != total_read || written != initial_meta.size {
        return Err(TransferError::SizeMismatch {
            expected: initial_meta.size,
            actual: written,
        });
    }
    let after_src = src_fs.stat(src_path).await?;
    if after_src.size != initial_meta.size || after_src.mtime != initial_meta.mtime {
        return Err(TransferError::SourceModified {
            path: src_path.clone(),
        });
    }
    let temp_meta = dst_fs.lstat(temp_path).await?;
    if temp_meta.size != initial_meta.size {
        return Err(TransferError::SizeMismatch {
            expected: initial_meta.size,
            actual: temp_meta.size,
        });
    }

    // Step 5: Preserve sanitized permissions (stripping setuid/setgid/sticky) and mtime on `.part`.
    let patch = MetadataPatch {
        permissions: if dst_fs.location().is_local() && !cfg!(unix) {
            None
        } else {
            initial_meta.sanitized_permissions()
        },
        mtime: initial_meta.mtime,
        atime: initial_meta.atime,
    };
    let warning = if patch.permissions.is_some() || patch.mtime.is_some() {
        dst_fs
            .set_metadata(temp_path, &patch)
            .await
            .err()
            .map(|e| format!("metadata preservation warning: {e}"))
    } else {
        None
    };

    // Step 6: Atomic commit via rename.
    if cancel.is_cancelled() {
        return Err(TransferError::Cancelled);
    }
    let rename_mode = if overwrite {
        RenameMode::AtomicReplace
    } else {
        RenameMode::NoReplace
    };
    dst_fs.rename(temp_path, dst_path, rename_mode).await?;

    Ok((written, sync_outcome, warning))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use async_trait::async_trait;
    use common::{Capabilities, DirEntry, FileReader, FileWriter, HostId, RemotePath};

    use super::*;

    #[derive(Clone, Default)]
    struct MemNode {
        kind: FileKind,
        data: Vec<u8>,
        permissions: u32,
        mtime: u64,
    }

    struct MemFs {
        location: Location,
        capabilities: Capabilities,
        nodes: Arc<Mutex<BTreeMap<Vec<u8>, MemNode>>>,
        mutate_source_on_eof: Option<Vec<u8>>,
        disconnect_after_bytes: Option<usize>,
        fail_rename: bool,
        cancel_on_sync: Option<CancelToken>,
        block_stat: bool,
        block_read: bool,
        reader_started: Arc<Notify>,
        reader_dropped: Arc<AtomicBool>,
    }

    impl MemFs {
        fn new_remote(host: &str, posix_rename: bool) -> Self {
            let mut nodes = BTreeMap::new();
            nodes.insert(
                b"/".to_vec(),
                MemNode {
                    kind: FileKind::Directory,
                    data: Vec::new(),
                    permissions: 0o040755,
                    mtime: 100,
                },
            );
            Self {
                location: Location::Remote(HostId(host.into())),
                capabilities: Capabilities {
                    posix_rename,
                    fsync: true,
                    statvfs: true,
                    max_read_size: 16 * 1024,
                    max_write_size: 16 * 1024,
                    max_open_handles: Some(64),
                },
                nodes: Arc::new(Mutex::new(nodes)),
                mutate_source_on_eof: None,
                disconnect_after_bytes: None,
                fail_rename: false,
                cancel_on_sync: None,
                block_stat: false,
                block_read: false,
                reader_started: Arc::new(Notify::new()),
                reader_dropped: Arc::new(AtomicBool::new(false)),
            }
        }

        fn put_file(&self, path: &[u8], data: Vec<u8>, mode: u32, mtime: u64) {
            self.nodes.lock().unwrap().insert(
                path.to_vec(),
                MemNode {
                    kind: FileKind::File,
                    data,
                    permissions: 0o100000 | mode,
                    mtime,
                },
            );
        }

        fn put_dir(&self, path: &[u8], mode: u32) {
            self.nodes.lock().unwrap().insert(
                path.to_vec(),
                MemNode {
                    kind: FileKind::Directory,
                    data: Vec::new(),
                    permissions: 0o040000 | mode,
                    mtime: 100,
                },
            );
        }

        fn get_file(&self, path: &[u8]) -> Option<Vec<u8>> {
            self.nodes.lock().unwrap().get(path).map(|n| n.data.clone())
        }
    }

    fn key(path: &FsPath) -> FsResult<Vec<u8>> {
        match path {
            FsPath::Remote(r) => Ok(r.as_bytes().to_vec()),
            FsPath::Local(_) => Err(FsError::PathLocationMismatch),
        }
    }

    #[async_trait]
    impl FileSystem for MemFs {
        fn location(&self) -> &Location {
            &self.location
        }

        fn capabilities(&self) -> &Capabilities {
            &self.capabilities
        }

        async fn read_dir(&self, path: &FsPath) -> FsResult<Vec<DirEntry>> {
            let dir_key = key(path)?;
            let prefix = if dir_key == b"/" {
                b"/".to_vec()
            } else {
                let mut p = dir_key.clone();
                p.push(b'/');
                p
            };
            let guard = self.nodes.lock().unwrap();
            if !guard.contains_key(&dir_key) {
                return Err(FsError::NotFound);
            }
            let mut entries = Vec::new();
            for (k, node) in guard.iter() {
                if k == &dir_key || !k.starts_with(&prefix) {
                    continue;
                }
                let rest = &k[prefix.len()..];
                if rest.is_empty() || rest.contains(&b'/') {
                    continue;
                }
                entries.push(DirEntry::new_remote(
                    rest.to_vec(),
                    RemotePath::new(k.clone())?,
                    Metadata {
                        kind: node.kind,
                        size: node.data.len() as u64,
                        permissions: Some(node.permissions),
                        mtime: Some(node.mtime),
                        atime: Some(node.mtime),
                        uid: Some(1000),
                        gid: Some(1000),
                    },
                ));
            }
            Ok(entries)
        }

        async fn lstat(&self, path: &FsPath) -> FsResult<Metadata> {
            let k = key(path)?;
            let guard = self.nodes.lock().unwrap();
            let node = guard.get(&k).ok_or(FsError::NotFound)?;
            Ok(Metadata {
                kind: node.kind,
                size: node.data.len() as u64,
                permissions: Some(node.permissions),
                mtime: Some(node.mtime),
                atime: Some(node.mtime),
                uid: Some(1000),
                gid: Some(1000),
            })
        }

        async fn stat(&self, path: &FsPath) -> FsResult<Metadata> {
            if self.block_stat {
                std::future::pending::<()>().await;
            }
            self.lstat(path).await
        }

        async fn canonicalize(&self, path: &FsPath) -> FsResult<FsPath> {
            Ok(path.clone())
        }

        async fn create_dir(&self, path: &FsPath, mode: u32) -> FsResult<()> {
            let k = key(path)?;
            let mut guard = self.nodes.lock().unwrap();
            if guard.contains_key(&k) {
                return Err(FsError::AlreadyExists);
            }
            guard.insert(
                k,
                MemNode {
                    kind: FileKind::Directory,
                    data: Vec::new(),
                    permissions: 0o040000 | (mode & 0o777),
                    mtime: 100,
                },
            );
            Ok(())
        }

        async fn remove_file(&self, path: &FsPath) -> FsResult<()> {
            let k = key(path)?;
            if self.disconnect_after_bytes.is_some() {
                return Err(FsError::ConnectionLost);
            }
            self.nodes
                .lock()
                .unwrap()
                .remove(&k)
                .map(|_| ())
                .ok_or(FsError::NotFound)
        }

        async fn remove_dir(&self, path: &FsPath) -> FsResult<()> {
            self.remove_file(path).await
        }

        async fn rename(&self, from: &FsPath, to: &FsPath, mode: RenameMode) -> FsResult<()> {
            if self.fail_rename {
                return Err(FsError::PermissionDenied);
            }
            let src = key(from)?;
            let dst = key(to)?;
            let mut guard = self.nodes.lock().unwrap();
            match mode {
                RenameMode::NoReplace => {
                    if guard.contains_key(&dst) {
                        return Err(FsError::AlreadyExists);
                    }
                }
                RenameMode::AtomicReplace => {
                    if !self.capabilities.posix_rename {
                        return Err(FsError::Unsupported);
                    }
                }
            }
            let node = guard.remove(&src).ok_or(FsError::NotFound)?;
            guard.insert(dst, node);
            Ok(())
        }

        async fn open_read(&self, path: &FsPath) -> FsResult<Box<dyn FileReader>> {
            let k = key(path)?;
            let data = self
                .nodes
                .lock()
                .unwrap()
                .get(&k)
                .ok_or(FsError::NotFound)?
                .data
                .clone();
            Ok(Box::new(MemReader {
                data,
                offset: 0,
                nodes: self.nodes.clone(),
                mutate_on_eof: self.mutate_source_on_eof.clone(),
                read_limit: self.capabilities.max_read_size as usize,
                block_read: self.block_read,
                started: self.reader_started.clone(),
                dropped: self.reader_dropped.clone(),
            }))
        }

        async fn open_write(
            &self,
            path: &FsPath,
            opts: WriteOptions,
        ) -> FsResult<Box<dyn FileWriter>> {
            let k = key(path)?;
            let mut guard = self.nodes.lock().unwrap();
            if opts.create_new && guard.contains_key(&k) {
                return Err(FsError::AlreadyExists);
            }
            guard.insert(
                k.clone(),
                MemNode {
                    kind: FileKind::File,
                    data: Vec::new(),
                    permissions: 0o100000 | (opts.mode & 0o777),
                    mtime: 100,
                },
            );
            Ok(Box::new(MemWriter {
                key: k,
                nodes: self.nodes.clone(),
                disconnect_after: self.disconnect_after_bytes,
                written: 0,
                write_limit: self.capabilities.max_write_size as usize,
                cancel_on_sync: self.cancel_on_sync.clone(),
            }))
        }

        async fn set_metadata(&self, path: &FsPath, patch: &MetadataPatch) -> FsResult<()> {
            let k = key(path)?;
            let mut guard = self.nodes.lock().unwrap();
            let node = guard.get_mut(&k).ok_or(FsError::NotFound)?;
            if let Some(perms) = patch.permissions {
                node.permissions = (node.permissions & 0o170000) | (perms & 0o0777);
            }
            if let Some(mtime) = patch.mtime {
                node.mtime = mtime;
            }
            Ok(())
        }

        async fn free_space(&self, _path: &FsPath) -> FsResult<Option<u64>> {
            Ok(Some(1024 * 1024 * 1024))
        }
    }

    struct MemReader {
        data: Vec<u8>,
        offset: usize,
        nodes: Arc<Mutex<BTreeMap<Vec<u8>, MemNode>>>,
        mutate_on_eof: Option<Vec<u8>>,
        read_limit: usize,
        block_read: bool,
        started: Arc<Notify>,
        dropped: Arc<AtomicBool>,
    }

    impl Drop for MemReader {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl FileReader for MemReader {
        async fn read_chunk(&mut self, max_bytes: usize) -> FsResult<Vec<u8>> {
            assert!(max_bytes <= self.read_limit);
            self.started.notify_one();
            if self.block_read {
                std::future::pending::<()>().await;
            }
            if self.offset >= self.data.len() {
                if let Some(path) = &self.mutate_on_eof
                    && let Some(node) = self.nodes.lock().unwrap().get_mut(path)
                {
                    node.mtime += 1;
                }
                return Ok(Vec::new());
            }
            let end = (self.offset + max_bytes).min(self.data.len());
            let chunk = self.data[self.offset..end].to_vec();
            self.offset = end;
            Ok(chunk)
        }

        async fn close(self: Box<Self>) -> FsResult<()> {
            Ok(())
        }
    }

    struct MemWriter {
        key: Vec<u8>,
        nodes: Arc<Mutex<BTreeMap<Vec<u8>, MemNode>>>,
        disconnect_after: Option<usize>,
        written: usize,
        write_limit: usize,
        cancel_on_sync: Option<CancelToken>,
    }

    #[async_trait]
    impl FileWriter for MemWriter {
        async fn write_all(&mut self, buf: &[u8]) -> FsResult<()> {
            assert!(buf.len() <= self.write_limit);
            if let Some(limit) = self.disconnect_after
                && self.written + buf.len() > limit
            {
                return Err(FsError::ConnectionLost);
            }
            self.written += buf.len();
            self.nodes
                .lock()
                .unwrap()
                .get_mut(&self.key)
                .ok_or(FsError::NotFound)?
                .data
                .extend_from_slice(buf);
            Ok(())
        }

        async fn sync(&mut self) -> FsResult<SyncOutcome> {
            if let Some(cancel) = &self.cancel_on_sync {
                cancel.cancel();
            }
            Ok(SyncOutcome::Synced)
        }

        async fn close(self: Box<Self>) -> FsResult<()> {
            Ok(())
        }
    }

    fn fixture(
        source: MemFs,
        destination: MemFs,
    ) -> (FsResolver, Arc<MemFs>, Arc<MemFs>, TransferRequest) {
        source.put_file(b"/source", vec![42; 64 * 1024], 0o644, 100);
        let source = Arc::new(source);
        let destination = Arc::new(destination);
        let resolver = FsResolver::new();
        resolver.register_remote(HostId("ssh:a".into()), source.clone());
        resolver.register_remote(HostId("ssh:b".into()), destination.clone());
        let request = TransferRequest::new(
            "regression",
            Endpoint::remote(
                HostId("ssh:a".into()),
                RemotePath::from_posix("/source").unwrap(),
            ),
            Endpoint::remote(
                HostId("ssh:b".into()),
                RemotePath::from_posix("/destination").unwrap(),
            ),
        );
        (resolver, source, destination, request)
    }

    #[tokio::test]
    async fn colliding_temporary_file_is_never_deleted() {
        let (resolver, _, destination, request) = fixture(
            MemFs::new_remote("ssh:a", true),
            MemFs::new_remote("ssh:b", true),
        );
        destination.put_file(b"/destination", b"old".to_vec(), 0o600, 50);
        let temp = temp_part_path(request.destination.path(), &request.id).unwrap();
        destination.put_file(&key(&temp).unwrap(), b"another job".to_vec(), 0o600, 50);
        let report = run_transfer(
            &resolver,
            &TransferConfig::default(),
            &TargetLocks::new(),
            request,
            ConflictPolicy::Overwrite,
            &CancelToken::new(),
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(report.state, JobState::Failed);
        assert!(report.residual_temp_files.is_empty());
        assert_eq!(
            destination.get_file(&key(&temp).unwrap()).unwrap(),
            b"another job"
        );
        assert_eq!(destination.get_file(b"/destination").unwrap(), b"old");
    }

    #[tokio::test]
    async fn failed_atomic_commit_preserves_old_target_and_cleans_owned_temp() {
        let mut destination = MemFs::new_remote("ssh:b", true);
        destination.fail_rename = true;
        let (resolver, _, destination, request) =
            fixture(MemFs::new_remote("ssh:a", true), destination);
        destination.put_file(b"/destination", b"old".to_vec(), 0o600, 50);
        let temp = temp_part_path(request.destination.path(), &request.id).unwrap();
        let report = run_transfer(
            &resolver,
            &TransferConfig::default(),
            &TargetLocks::new(),
            request,
            ConflictPolicy::Overwrite,
            &CancelToken::new(),
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(report.state, JobState::Failed);
        assert!(report.residual_temp_files.is_empty());
        assert!(destination.get_file(&key(&temp).unwrap()).is_none());
        assert_eq!(destination.get_file(b"/destination").unwrap(), b"old");
    }

    #[tokio::test]
    async fn a_target_created_after_planning_is_not_overwritten() {
        let (resolver, _, destination, request) = fixture(
            MemFs::new_remote("ssh:a", true),
            MemFs::new_remote("ssh:b", true),
        );
        let config = TransferConfig::default();
        let cancel = CancelToken::new();
        let plan = plan_transfer(&resolver, &config, request, &cancel)
            .await
            .unwrap();
        destination.put_file(b"/destination", b"arrived later".to_vec(), 0o600, 50);
        let report = execute_plan(
            &resolver,
            &config,
            &TargetLocks::new(),
            plan,
            ConflictPolicy::Overwrite,
            &cancel,
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(report.state, JobState::Failed);
        assert!(report.residual_temp_files.is_empty());
        assert_eq!(
            destination.get_file(b"/destination").unwrap(),
            b"arrived later"
        );
    }

    #[tokio::test]
    async fn cancellation_during_sync_prevents_commit() {
        let cancel = CancelToken::new();
        let mut destination = MemFs::new_remote("ssh:b", true);
        destination.cancel_on_sync = Some(cancel.clone());
        let (resolver, _, destination, request) =
            fixture(MemFs::new_remote("ssh:a", true), destination);
        destination.put_file(b"/destination", b"old".to_vec(), 0o600, 50);
        let report = run_transfer(
            &resolver,
            &TransferConfig::default(),
            &TargetLocks::new(),
            request,
            ConflictPolicy::Overwrite,
            &cancel,
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(report.state, JobState::Cancelled);
        assert!(report.residual_temp_files.is_empty());
        assert_eq!(destination.get_file(b"/destination").unwrap(), b"old");
    }

    #[tokio::test]
    async fn cancellation_interrupts_pending_planning_io() {
        let mut source = MemFs::new_remote("ssh:a", true);
        source.block_stat = true;
        let (resolver, _, _, request) = fixture(source, MemFs::new_remote("ssh:b", true));
        let cancel = CancelToken::new();
        let signal = cancel.clone();
        let task = tokio::spawn(async move {
            plan_transfer(&resolver, &TransferConfig::default(), request, &cancel).await
        });
        tokio::task::yield_now().await;
        signal.cancel();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap(),
            Err(TransferError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn cancelling_a_stalled_read_cleans_temp_and_drops_reader() {
        let mut source = MemFs::new_remote("ssh:a", true);
        source.block_read = true;
        let (resolver, source, destination, request) =
            fixture(source, MemFs::new_remote("ssh:b", true));
        let cancel = CancelToken::new();
        let signal = cancel.clone();
        let task = tokio::spawn(async move {
            run_transfer(
                &resolver,
                &TransferConfig::default(),
                &TargetLocks::new(),
                request,
                ConflictPolicy::Overwrite,
                &cancel,
                |_| {},
            )
            .await
        });
        source.reader_started.notified().await;
        signal.cancel();
        let report = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(report.state, JobState::Cancelled);
        assert!(report.residual_temp_files.is_empty());
        assert!(destination.get_file(b"/destination").is_none());
        tokio::task::yield_now().await;
        assert!(source.reader_dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn dropping_transfer_future_aborts_its_stalled_reader() {
        let mut source = MemFs::new_remote("ssh:a", true);
        source.block_read = true;
        let (resolver, source, _, request) = fixture(source, MemFs::new_remote("ssh:b", true));
        let task = tokio::spawn(async move {
            run_transfer(
                &resolver,
                &TransferConfig::default(),
                &TargetLocks::new(),
                request,
                ConflictPolicy::Overwrite,
                &CancelToken::new(),
                |_| {},
            )
            .await
        });
        source.reader_started.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::task::yield_now().await;
        assert!(source.reader_dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn small_server_chunks_are_respected_and_zero_config_is_rejected() {
        let mut source = MemFs::new_remote("ssh:a", true);
        source.capabilities.max_read_size = 128;
        let mut destination = MemFs::new_remote("ssh:b", true);
        destination.capabilities.max_write_size = 64;
        let (resolver, source, destination, request) = fixture(source, destination);
        let config = TransferConfig::default();
        let cancel = CancelToken::new();
        let report = run_transfer(
            &resolver,
            &config,
            &TargetLocks::new(),
            request.clone(),
            ConflictPolicy::Overwrite,
            &cancel,
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(report.state, JobState::Completed);
        assert_eq!(
            source.get_file(b"/source"),
            destination.get_file(b"/destination")
        );
        let invalid = TransferConfig {
            chunk_size: 0,
            ..config
        };
        assert!(matches!(
            plan_transfer(&resolver, &invalid, request, &cancel).await,
            Err(TransferError::InvalidConfig(_))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn target_locks_cover_aliases_of_the_same_parent_directory() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        tokio::fs::write(&source, b"data").await.unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(root.path(), &alias).unwrap();
        let locks = TargetLocks::new();
        let canonical = tokio::fs::canonicalize(root.path())
            .await
            .unwrap()
            .join("target");
        let _guard = locks
            .acquire(&Location::Local, &[FsPath::Local(canonical.clone())])
            .unwrap();
        let request = TransferRequest::new(
            "alias",
            Endpoint::local(source),
            Endpoint::local(alias.join("target")),
        );
        assert!(matches!(
            run_transfer(
                &FsResolver::new(),
                &TransferConfig::default(),
                &locks,
                request,
                ConflictPolicy::Overwrite,
                &CancelToken::new(),
                |_| {}
            )
            .await,
            Err(TransferError::TargetLocked(_))
        ));
        assert!(!canonical.exists());
    }

    #[tokio::test]
    async fn streams_large_file_commits_atomically_and_strips_setuid() {
        let host_a = MemFs::new_remote("ssh:a", true);
        let host_b = MemFs::new_remote("ssh:b", true);
        // 4 MiB payload through a 16 KiB * 2 = 32 KiB bounded buffer (128x buffer size).
        let payload: Vec<u8> = (0..(4 * 1024 * 1024)).map(|i| (i * 13 + 7) as u8).collect();
        // Source has setuid/setgid/sticky bits set (0o7755); target must receive 0o0755.
        host_a.put_file(b"/src.bin", payload.clone(), 0o7755, 1_700_000_123);

        let resolver = FsResolver::new();
        resolver.register_remote(HostId("ssh:a".into()), Arc::new(host_a));
        let host_b_arc = Arc::new(host_b);
        resolver.register_remote(HostId("ssh:b".into()), host_b_arc.clone());

        let config = TransferConfig {
            chunk_size: 16 * 1024,
            channel_chunks: 2,
            ..TransferConfig::default()
        };
        let locks = TargetLocks::new();
        let cancel = CancelToken::new();
        let req = TransferRequest::new(
            "job-1",
            Endpoint::remote(
                HostId("ssh:a".into()),
                RemotePath::from_posix("/src.bin").unwrap(),
            ),
            Endpoint::remote(
                HostId("ssh:b".into()),
                RemotePath::from_posix("/dst.bin").unwrap(),
            ),
        );

        let report = run_transfer(
            &resolver,
            &config,
            &locks,
            req,
            ConflictPolicy::Ask,
            &cancel,
            |_| {},
        )
        .await
        .unwrap();

        assert_eq!(report.state, JobState::Completed);
        assert_eq!(report.bytes_transferred, payload.len() as u64);
        assert!(report.residual_temp_files.is_empty());
        assert_eq!(host_b_arc.get_file(b"/dst.bin").unwrap(), payload);
        let dst_meta = host_b_arc
            .lstat(&FsPath::Remote(RemotePath::from_posix("/dst.bin").unwrap()))
            .await
            .unwrap();
        assert_eq!(dst_meta.sanitized_permissions(), Some(0o755));
        assert_eq!(dst_meta.permissions.unwrap() & 0o7000, 0);
        assert_eq!(dst_meta.mtime, Some(1_700_000_123));
    }

    #[tokio::test]
    async fn refuses_overwrite_without_posix_rename_and_preserves_old_file() {
        let host_a = MemFs::new_remote("ssh:a", true);
        let host_b = MemFs::new_remote("ssh:b", false); // No posix-rename!
        host_a.put_file(b"/new.txt", b"new-content".to_vec(), 0o644, 200);
        host_b.put_file(
            b"/target.txt",
            b"original-safe-content".to_vec(),
            0o644,
            100,
        );

        let resolver = FsResolver::new();
        resolver.register_remote(HostId("ssh:a".into()), Arc::new(host_a));
        let host_b_arc = Arc::new(host_b);
        resolver.register_remote(HostId("ssh:b".into()), host_b_arc.clone());

        let config = TransferConfig::default();
        let locks = TargetLocks::new();
        let cancel = CancelToken::new();
        let req = TransferRequest::new(
            "job-no-posix",
            Endpoint::remote(
                HostId("ssh:a".into()),
                RemotePath::from_posix("/new.txt").unwrap(),
            ),
            Endpoint::remote(
                HostId("ssh:b".into()),
                RemotePath::from_posix("/target.txt").unwrap(),
            ),
        );

        // Policy::Ask reports Conflict during planning.
        assert!(matches!(
            run_transfer(
                &resolver,
                &config,
                &locks,
                req.clone(),
                ConflictPolicy::Ask,
                &cancel,
                |_| {},
            )
            .await,
            Err(TransferError::Conflict { .. })
        ));

        // Policy::Overwrite is rejected with Unsupported because posix_rename is false!
        assert!(matches!(
            run_transfer(
                &resolver,
                &config,
                &locks,
                req,
                ConflictPolicy::Overwrite,
                &cancel,
                |_| {},
            )
            .await,
            Err(TransferError::Fs(FsError::Unsupported))
        ));
        assert_eq!(
            host_b_arc.get_file(b"/target.txt").unwrap(),
            b"original-safe-content"
        );
    }

    #[tokio::test]
    async fn source_modification_and_midstream_disconnect_never_corrupt_target() {
        // 1. Source modified during transfer -> Failed, temp cleaned up, target absent.
        let mut host_a = MemFs::new_remote("ssh:a", true);
        host_a.put_file(b"/changing.txt", b"initial-data".to_vec(), 0o644, 100);
        host_a.mutate_source_on_eof = Some(b"/changing.txt".to_vec());
        let host_b = MemFs::new_remote("ssh:b", true);

        let resolver = FsResolver::new();
        resolver.register_remote(HostId("ssh:a".into()), Arc::new(host_a));
        let host_b_arc = Arc::new(host_b);
        resolver.register_remote(HostId("ssh:b".into()), host_b_arc.clone());

        let config = TransferConfig::default();
        let locks = TargetLocks::new();
        let cancel = CancelToken::new();
        let req = TransferRequest::new(
            "job-mod",
            Endpoint::remote(
                HostId("ssh:a".into()),
                RemotePath::from_posix("/changing.txt").unwrap(),
            ),
            Endpoint::remote(
                HostId("ssh:b".into()),
                RemotePath::from_posix("/out.txt").unwrap(),
            ),
        );
        let report = run_transfer(
            &resolver,
            &config,
            &locks,
            req,
            ConflictPolicy::Overwrite,
            &cancel,
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(report.state, JobState::Failed);
        assert!(host_b_arc.get_file(b"/out.txt").is_none());
        assert!(report.residual_temp_files.is_empty());

        // 2. Mid-stream disconnect while overwriting -> Interrupted, old target intact, residual temp reported.
        let host_a2 = MemFs::new_remote("ssh:a2", true);
        host_a2.put_file(b"/big.bin", vec![b'X'; 64 * 1024], 0o644, 100);
        let mut host_b2 = MemFs::new_remote("ssh:b2", true);
        host_b2.put_file(b"/keep.bin", b"intact-old-file".to_vec(), 0o644, 50);
        host_b2.disconnect_after_bytes = Some(16 * 1024);

        resolver.register_remote(HostId("ssh:a2".into()), Arc::new(host_a2));
        let host_b2_arc = Arc::new(host_b2);
        resolver.register_remote(HostId("ssh:b2".into()), host_b2_arc.clone());

        let req2 = TransferRequest::new(
            "job-disc",
            Endpoint::remote(
                HostId("ssh:a2".into()),
                RemotePath::from_posix("/big.bin").unwrap(),
            ),
            Endpoint::remote(
                HostId("ssh:b2".into()),
                RemotePath::from_posix("/keep.bin").unwrap(),
            ),
        );
        let report2 = run_transfer(
            &resolver,
            &config,
            &locks,
            req2,
            ConflictPolicy::Overwrite,
            &cancel,
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(report2.state, JobState::Interrupted);
        assert_eq!(
            host_b2_arc.get_file(b"/keep.bin").unwrap(),
            b"intact-old-file"
        );
        assert_eq!(report2.residual_temp_files.len(), 1);
    }

    #[tokio::test]
    async fn rejects_same_endpoint_subdirectory_copy_and_concurrent_target_locks() {
        let host_a = MemFs::new_remote("ssh:a", true);
        host_a.put_dir(b"/dir", 0o755);
        let resolver = FsResolver::new();
        resolver.register_remote(HostId("ssh:a".into()), Arc::new(host_a));
        let config = TransferConfig::default();
        let cancel = CancelToken::new();

        // Same endpoint
        let same_req = TransferRequest::new(
            "j1",
            Endpoint::remote(
                HostId("ssh:a".into()),
                RemotePath::from_posix("/dir").unwrap(),
            ),
            Endpoint::remote(
                HostId("ssh:a".into()),
                RemotePath::from_posix("/dir").unwrap(),
            ),
        );
        assert!(matches!(
            plan_transfer(&resolver, &config, same_req, &cancel).await,
            Err(TransferError::SameEndpoint)
        ));

        // Subdirectory of source
        let sub_req = TransferRequest::new(
            "j2",
            Endpoint::remote(
                HostId("ssh:a".into()),
                RemotePath::from_posix("/dir").unwrap(),
            ),
            Endpoint::remote(
                HostId("ssh:a".into()),
                RemotePath::from_posix("/dir/sub").unwrap(),
            ),
        );
        assert!(matches!(
            plan_transfer(&resolver, &config, sub_req, &cancel).await,
            Err(TransferError::SubdirectoryTarget)
        ));

        // Exclusive target lock prevents two jobs from writing to the same destination.
        let locks = TargetLocks::new();
        let target = FsPath::Remote(RemotePath::from_posix("/dir/out.txt").unwrap());
        let _guard = locks
            .acquire(
                &Location::Remote(HostId("ssh:a".into())),
                std::slice::from_ref(&target),
            )
            .unwrap();
        assert!(matches!(
            locks.acquire(&Location::Remote(HostId("ssh:a".into())), &[target]),
            Err(TransferError::TargetLocked(_))
        ));
    }
}
