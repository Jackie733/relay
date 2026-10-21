use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use common::{
    Capabilities, DEFAULT_IO_CHUNK_SIZE, DirEntry, FileReader, FileWriter, FsError, FsResult,
    MAX_DIR_ENTRIES, MAX_IO_CHUNK_SIZE, Metadata, MetadataPatch, RemotePath, RenameMode,
    SyncOutcome, WriteOptions, contains_replacement_bytes, is_valid_entry_name,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, sleep_until, timeout, timeout_at},
};

use super::wire::{
    EXT_FSYNC, EXT_LIMITS, EXT_POSIX_RENAME, EXT_STATVFS, FileAttrs, MAX_PACKET_LEN, NameEntry,
    PacketDecoder, PacketEncoder, ResponseBody, ResponsePacket, SFTP_VERSION, SSH_FX_EOF,
    SSH_FX_FAILURE, SSH_FX_OK, SSH_FX_OP_UNSUPPORTED, SSH_FXF_CREAT, SSH_FXF_EXCL, SSH_FXF_READ,
    SSH_FXF_TRUNC, SSH_FXF_WRITE, SSH_FXP_CLOSE, SSH_FXP_EXTENDED, SSH_FXP_INIT, SSH_FXP_LSTAT,
    SSH_FXP_MKDIR, SSH_FXP_OPEN, SSH_FXP_OPENDIR, SSH_FXP_READ, SSH_FXP_READDIR, SSH_FXP_REALPATH,
    SSH_FXP_REMOVE, SSH_FXP_RENAME, SSH_FXP_RMDIR, SSH_FXP_SETSTAT, SSH_FXP_STAT, SSH_FXP_WRITE,
    ServerLimits, ServerVersion, StatVfsReply, status_to_error,
};

const MAX_READ_PIPELINE: usize = 8;
const MAX_WRITE_PIPELINE: usize = 8;
const COMMAND_QUEUE_CAPACITY: usize = 32;
const MAX_PENDING_REQUESTS: usize = 64;
// Includes the packet length, type, request ID and fixed fields; excludes handle/data bytes.
const WRITE_PACKET_OVERHEAD: usize = 25;
const DATA_PACKET_OVERHEAD: u32 = 13;

#[derive(Clone)]
enum TerminalFailure {
    ConnectionLost,
    Protocol(String),
}

impl TerminalFailure {
    fn to_error(&self) -> FsError {
        match self {
            Self::ConnectionLost => FsError::ConnectionLost,
            Self::Protocol(msg) => FsError::Protocol(msg.clone()),
        }
    }
}

struct PendingState {
    waiters: HashMap<u32, PendingRequest>,
    failure: Option<TerminalFailure>,
    changed: Arc<Notify>,
}

struct PendingRequest {
    reply: Option<oneshot::Sender<FsResult<ResponseBody>>>,
    deadline: Instant,
    _permit: OwnedSemaphorePermit,
}

struct OutboundFrame {
    id: u32,
    frame: Vec<u8>,
    pending: PendingRequest,
}

struct DriverGuard {
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
    watchdog: JoinHandle<()>,
}

impl Drop for DriverGuard {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
        self.watchdog.abort();
    }
}

#[derive(Clone)]
pub struct SftpClient {
    tx: mpsc::Sender<OutboundFrame>,
    pending: Arc<Mutex<PendingState>>,
    next_id: Arc<AtomicU32>,
    capabilities: Capabilities,
    operation_timeout: Duration,
    requests: Arc<Semaphore>,
    max_packet_len: u32,
    _driver: Arc<DriverGuard>,
}

impl SftpClient {
    pub async fn from_stream<S>(mut stream: S, operation_timeout: Duration) -> FsResult<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        if operation_timeout.is_zero() {
            return Err(FsError::Protocol("SFTP timeout must be nonzero".into()));
        }
        let server_version = timeout(operation_timeout, async {
            let mut init = PacketEncoder::new(SSH_FXP_INIT);
            init.put_u32(SFTP_VERSION);
            let frame = init.finish();
            stream.write_all(&frame).await?;
            stream.flush().await?;
            let payload = read_frame(&mut stream).await?;
            ServerVersion::decode(&payload)
        })
        .await
        .map_err(|_| FsError::ConnectionLost)??;

        let changed = Arc::new(Notify::new());
        let pending = Arc::new(Mutex::new(PendingState {
            waiters: HashMap::new(),
            failure: None,
            changed: changed.clone(),
        }));
        let (mut read_half, mut write_half) = tokio::io::split(stream);
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(COMMAND_QUEUE_CAPACITY);

        let writer_pending = pending.clone();
        let writer = tokio::spawn(async move {
            while let Some(item) = rx.recv().await {
                {
                    let mut guard = writer_pending.lock().expect("SFTP pending lock poisoned");
                    if let Some(failure) = &guard.failure {
                        if let Some(reply) = item.pending.reply {
                            let _ = reply.send(Err(failure.to_error()));
                        }
                        break;
                    }
                    if guard.waiters.insert(item.id, item.pending).is_some() {
                        fail_all(
                            &mut guard,
                            TerminalFailure::Protocol("duplicate SFTP request ID".into()),
                        );
                        break;
                    }
                    guard.changed.notify_one();
                }
                if write_half.write_all(&item.frame).await.is_err()
                    || write_half.flush().await.is_err()
                {
                    let mut guard = writer_pending.lock().expect("SFTP pending lock poisoned");
                    fail_all(&mut guard, TerminalFailure::ConnectionLost);
                    break;
                }
            }
        });

        let reader_pending = pending.clone();
        let reader = tokio::spawn(async move {
            loop {
                let payload = match read_frame(&mut read_half).await {
                    Ok(payload) => payload,
                    Err(FsError::Protocol(msg)) => {
                        let mut guard = reader_pending.lock().expect("SFTP pending lock poisoned");
                        fail_all(&mut guard, TerminalFailure::Protocol(msg));
                        break;
                    }
                    Err(_) => {
                        let mut guard = reader_pending.lock().expect("SFTP pending lock poisoned");
                        fail_all(&mut guard, TerminalFailure::ConnectionLost);
                        break;
                    }
                };
                let packet = match ResponsePacket::decode(&payload) {
                    Ok(packet) => packet,
                    Err(FsError::Protocol(msg)) => {
                        let mut guard = reader_pending.lock().expect("SFTP pending lock poisoned");
                        fail_all(&mut guard, TerminalFailure::Protocol(msg));
                        break;
                    }
                    Err(_) => {
                        let mut guard = reader_pending.lock().expect("SFTP pending lock poisoned");
                        fail_all(&mut guard, TerminalFailure::ConnectionLost);
                        break;
                    }
                };
                let mut guard = reader_pending.lock().expect("SFTP pending lock poisoned");
                let Some(waiter) = guard.waiters.remove(&packet.request_id) else {
                    fail_all(
                        &mut guard,
                        TerminalFailure::Protocol(format!(
                            "unsolicited SFTP response ID {}",
                            packet.request_id
                        )),
                    );
                    break;
                };
                guard.changed.notify_one();
                drop(guard);
                if let Some(reply) = waiter.reply {
                    let _ = reply.send(Ok(packet.body));
                }
            }
        });

        // Deadlines outlive callers, including externally cancelled futures. A timeout
        // terminates this SFTP channel so stale replies/handles cannot accumulate.
        let watchdog_pending = pending.clone();
        let reader_abort = reader.abort_handle();
        let writer_abort = writer.abort_handle();
        let watchdog = tokio::spawn(async move {
            loop {
                let notified = changed.notified();
                let (failed, deadline) = {
                    let mut guard = watchdog_pending.lock().expect("SFTP pending lock poisoned");
                    let deadline = guard.waiters.values().map(|p| p.deadline).min();
                    if deadline.is_some_and(|d| d <= Instant::now()) {
                        fail_all(&mut guard, TerminalFailure::ConnectionLost);
                    }
                    (guard.failure.is_some(), deadline)
                };
                if failed {
                    reader_abort.abort();
                    writer_abort.abort();
                    break;
                }
                if let Some(deadline) = deadline {
                    tokio::select! {
                        _ = sleep_until(deadline) => {},
                        _ = notified => {},
                    }
                } else {
                    notified.await;
                }
            }
        });

        let mut client = Self {
            tx,
            pending,
            next_id: Arc::new(AtomicU32::new(1)),
            capabilities: Capabilities {
                posix_rename: server_version.posix_rename,
                fsync: server_version.fsync,
                statvfs: server_version.statvfs,
                max_read_size: DEFAULT_IO_CHUNK_SIZE,
                max_write_size: DEFAULT_IO_CHUNK_SIZE,
                max_open_handles: None,
            },
            operation_timeout,
            requests: Arc::new(Semaphore::new(MAX_PENDING_REQUESTS)),
            max_packet_len: MAX_PACKET_LEN + 4,
            _driver: Arc::new(DriverGuard {
                reader,
                writer,
                watchdog,
            }),
        };

        if server_version.limits
            && let Ok(limits) = client.query_limits().await
        {
            if limits.max_packet_len > 0 {
                client.max_packet_len =
                    limits.max_packet_len.min(u64::from(MAX_PACKET_LEN + 4)) as u32;
            }
            client.capabilities.max_read_size = negotiated_chunk_size(
                limits.max_read_len,
                client.max_packet_len.saturating_sub(DATA_PACKET_OVERHEAD),
            )?;
            client.capabilities.max_write_size = negotiated_chunk_size(
                limits.max_write_len,
                client
                    .max_packet_len
                    .saturating_sub(WRITE_PACKET_OVERHEAD as u32),
            )?;
            if limits.max_open_handles > 0 {
                client.capabilities.max_open_handles = Some(limits.max_open_handles);
            }
        }

        client.check_failure()?;
        Ok(client)
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn alloc_id(&self) -> u32 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if id == 0 {
            self.next_id.fetch_add(1, Ordering::Relaxed)
        } else {
            id
        }
    }

    fn check_failure(&self) -> FsResult<()> {
        let guard = self.pending.lock().expect("SFTP pending lock poisoned");
        if let Some(failure) = &guard.failure {
            return Err(failure.to_error());
        }
        Ok(())
    }

    fn terminate(&self, failure: TerminalFailure) -> FsError {
        let mut guard = self.pending.lock().expect("SFTP pending lock poisoned");
        fail_all(&mut guard, failure);
        self._driver.reader.abort();
        self._driver.writer.abort();
        self.requests.close();
        guard.failure.as_ref().expect("failure recorded").to_error()
    }

    async fn enqueue_raw(
        &self,
        id: u32,
        frame: Vec<u8>,
    ) -> FsResult<oneshot::Receiver<FsResult<ResponseBody>>> {
        self.check_failure()?;
        if frame.len() > self.max_packet_len as usize {
            return Err(FsError::Protocol(
                "SFTP request exceeds negotiated packet limit".into(),
            ));
        }
        let deadline = Instant::now() + self.operation_timeout;
        let (reply, rx) = oneshot::channel();
        timeout_at(deadline, async {
            let permit = self
                .requests
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| FsError::ConnectionLost)?;
            self.check_failure()?;
            self.tx
                .send(OutboundFrame {
                    id,
                    frame,
                    pending: PendingRequest {
                        reply: Some(reply),
                        deadline,
                        _permit: permit,
                    },
                })
                .await
                .map_err(|_| {
                    self.check_failure()
                        .err()
                        .unwrap_or(FsError::ConnectionLost)
                })
        })
        .await
        .map_err(|_| self.terminate(TerminalFailure::ConnectionLost))??;
        Ok(rx)
    }

    async fn await_reply(
        &self,
        rx: &mut oneshot::Receiver<FsResult<ResponseBody>>,
    ) -> FsResult<ResponseBody> {
        let res = timeout(self.operation_timeout, rx)
            .await
            .map_err(|_| self.terminate(TerminalFailure::ConnectionLost))?;
        match res {
            Ok(body) => body,
            Err(_) => Err(self
                .check_failure()
                .err()
                .unwrap_or(FsError::ConnectionLost)),
        }
    }

    async fn request(&self, id: u32, frame: Vec<u8>) -> FsResult<ResponseBody> {
        let mut rx = self.enqueue_raw(id, frame).await?;
        self.await_reply(&mut rx).await
    }

    async fn query_limits(&self) -> FsResult<ServerLimits> {
        let id = self.alloc_id();
        let mut enc = PacketEncoder::with_request(SSH_FXP_EXTENDED, id);
        enc.put_str(EXT_LIMITS);
        match self.request(id, enc.finish()).await? {
            ResponseBody::ExtendedReply(payload) => {
                let mut dec = PacketDecoder::new(&payload);
                ServerLimits::decode(&mut dec)
            }
            ResponseBody::Status { code, message } => Err(status_to_error(code, message)),
            _ => Err(FsError::Protocol(
                "unexpected response to limits@openssh.com".into(),
            )),
        }
    }

    pub async fn read_dir(&self, path: &RemotePath) -> FsResult<Vec<DirEntry>> {
        let handle = self.open_dir(path.as_bytes()).await?;
        let result = self.read_dir_entries(path, &handle.handle).await;
        let close_result = handle.close().await;
        let mut entries = result?;
        close_result?;
        entries.sort_by(|a, b| {
            (a.kind != common::FileKind::Directory, &a.raw_name)
                .cmp(&(b.kind != common::FileKind::Directory, &b.raw_name))
        });
        Ok(entries)
    }

    async fn open_dir(&self, path: &[u8]) -> FsResult<DirectoryHandle> {
        let id = self.alloc_id();
        let mut enc = PacketEncoder::with_request(SSH_FXP_OPENDIR, id);
        enc.put_bytes(path);
        let frame = enc.finish();
        let client = self.clone();
        let (reply, rx) = oneshot::channel();
        // Finish opening even if the caller cancels. An undelivered handle is
        // dropped and closed, including cancellation before OPENDIR replies.
        tokio::spawn(async move {
            let result = match client.request(id, frame).await {
                Ok(ResponseBody::Handle(handle)) => Ok(DirectoryHandle {
                    client,
                    handle,
                    closed: false,
                }),
                Ok(ResponseBody::Status { code, message }) => Err(status_to_error(code, message)),
                Ok(_) => Err(FsError::Protocol(
                    "unexpected response to SSH_FXP_OPENDIR".into(),
                )),
                Err(error) => Err(error),
            };
            let _ = reply.send(result);
        });
        rx.await.map_err(|_| FsError::ConnectionLost)?
    }

    async fn read_dir_entries(
        &self,
        dir_path: &RemotePath,
        handle: &[u8],
    ) -> FsResult<Vec<DirEntry>> {
        let mut entries = Vec::new();
        loop {
            let id = self.alloc_id();
            let mut enc = PacketEncoder::with_request(SSH_FXP_READDIR, id);
            enc.put_bytes(handle);
            match self.request(id, enc.finish()).await? {
                ResponseBody::Names(batch) => {
                    for NameEntry {
                        filename,
                        longname,
                        attrs,
                    } in batch
                    {
                        // Malicious server protection (ARCHITECTURE §6.2, SECURITY §4):
                        // Discard `.`, `..`, empty names, and names containing `/` or NUL.
                        if !is_valid_entry_name(&filename) {
                            continue;
                        }
                        if entries.len() == MAX_DIR_ENTRIES {
                            return Err(FsError::DirectoryTooLarge(MAX_DIR_ENTRIES));
                        }
                        let child_path = dir_path.join(&filename)?;
                        let metadata = attrs.to_metadata(Some(&longname));
                        entries.push(DirEntry::new_remote(filename, child_path, metadata));
                    }
                }
                ResponseBody::Status {
                    code: SSH_FX_EOF, ..
                } => break,
                ResponseBody::Status { code, message } => {
                    return Err(status_to_error(code, message));
                }
                _ => {
                    return Err(FsError::Protocol(
                        "unexpected response to SSH_FXP_READDIR".into(),
                    ));
                }
            }
        }
        Ok(entries)
    }

    pub async fn lstat(&self, path: &RemotePath) -> FsResult<Metadata> {
        self.stat_op(SSH_FXP_LSTAT, path.as_bytes()).await
    }

    pub async fn stat(&self, path: &RemotePath) -> FsResult<Metadata> {
        self.stat_op(SSH_FXP_STAT, path.as_bytes()).await
    }

    async fn stat_op(&self, opcode: u8, path: &[u8]) -> FsResult<Metadata> {
        let id = self.alloc_id();
        let mut enc = PacketEncoder::with_request(opcode, id);
        enc.put_bytes(path);
        match self.request(id, enc.finish()).await? {
            ResponseBody::Attrs(attrs) => Ok(attrs.to_metadata(None)),
            ResponseBody::Status { code, message } => Err(status_to_error(code, message)),
            _ => Err(FsError::Protocol(
                "unexpected response to SFTP stat request".into(),
            )),
        }
    }

    pub async fn canonicalize(&self, path: &RemotePath) -> FsResult<RemotePath> {
        let id = self.alloc_id();
        let mut enc = PacketEncoder::with_request(SSH_FXP_REALPATH, id);
        enc.put_bytes(path.as_bytes());
        match self.request(id, enc.finish()).await? {
            ResponseBody::Names(mut names) => {
                if names.len() != 1 {
                    return Err(FsError::Protocol(format!(
                        "SSH_FXP_REALPATH returned {} entries, expected 1",
                        names.len()
                    )));
                }
                let entry = names.pop().expect("checked len == 1");
                RemotePath::new(entry.filename).map_err(|_| {
                    FsError::Protocol("SSH_FXP_REALPATH returned empty or NUL path".into())
                })
            }
            ResponseBody::Status { code, message } => Err(status_to_error(code, message)),
            _ => Err(FsError::Protocol(
                "unexpected response to SSH_FXP_REALPATH".into(),
            )),
        }
    }

    pub async fn create_dir(&self, path: &RemotePath, mode: u32) -> FsResult<()> {
        ensure_safe_remote_mutation(path)?;
        let id = self.alloc_id();
        let mut enc = PacketEncoder::with_request(SSH_FXP_MKDIR, id);
        enc.put_bytes(path.as_bytes());
        FileAttrs::with_mode(mode).encode(&mut enc);
        match self.request(id, enc.finish()).await? {
            ResponseBody::Status {
                code: SSH_FX_OK, ..
            } => Ok(()),
            ResponseBody::Status {
                code: SSH_FX_FAILURE,
                message,
            } => {
                if self.lstat(path).await.is_ok() {
                    Err(FsError::AlreadyExists)
                } else {
                    Err(status_to_error(SSH_FX_FAILURE, message))
                }
            }
            ResponseBody::Status { code, message } => Err(status_to_error(code, message)),
            _ => Err(FsError::Protocol(
                "unexpected response to SSH_FXP_MKDIR".into(),
            )),
        }
    }

    pub async fn remove_file(&self, path: &RemotePath) -> FsResult<()> {
        ensure_safe_remote_mutation(path)?;
        self.simple_path_status(SSH_FXP_REMOVE, path.as_bytes(), "SSH_FXP_REMOVE")
            .await
    }

    pub async fn remove_dir(&self, path: &RemotePath) -> FsResult<()> {
        ensure_safe_remote_mutation(path)?;
        self.simple_path_status(SSH_FXP_RMDIR, path.as_bytes(), "SSH_FXP_RMDIR")
            .await
    }

    async fn simple_path_status(
        &self,
        opcode: u8,
        path: &[u8],
        label: &'static str,
    ) -> FsResult<()> {
        let id = self.alloc_id();
        let mut enc = PacketEncoder::with_request(opcode, id);
        enc.put_bytes(path);
        match self.request(id, enc.finish()).await? {
            ResponseBody::Status {
                code: SSH_FX_OK, ..
            } => Ok(()),
            ResponseBody::Status { code, message } => Err(status_to_error(code, message)),
            _ => Err(FsError::Protocol(format!("unexpected response to {label}"))),
        }
    }

    pub async fn rename(
        &self,
        from: &RemotePath,
        to: &RemotePath,
        mode: RenameMode,
    ) -> FsResult<()> {
        ensure_safe_remote_mutation(from)?;
        ensure_safe_remote_mutation(to)?;
        match mode {
            RenameMode::NoReplace => {
                // Pre-check prevents non-compliant servers from overwriting existing targets,
                // while SFTP v3 SSH_FXP_RENAME provides atomic link+unlink on compliant servers.
                if self.lstat(to).await.is_ok() {
                    return Err(FsError::AlreadyExists);
                }
                let id = self.alloc_id();
                let mut enc = PacketEncoder::with_request(SSH_FXP_RENAME, id);
                enc.put_bytes(from.as_bytes());
                enc.put_bytes(to.as_bytes());
                match self.request(id, enc.finish()).await? {
                    ResponseBody::Status {
                        code: SSH_FX_OK, ..
                    } => Ok(()),
                    ResponseBody::Status {
                        code: SSH_FX_FAILURE,
                        message,
                    } => {
                        if self.lstat(to).await.is_ok() {
                            Err(FsError::AlreadyExists)
                        } else {
                            Err(status_to_error(SSH_FX_FAILURE, message))
                        }
                    }
                    ResponseBody::Status { code, message } => Err(status_to_error(code, message)),
                    _ => Err(FsError::Protocol(
                        "unexpected response to SSH_FXP_RENAME".into(),
                    )),
                }
            }
            RenameMode::AtomicReplace => {
                // ADR-014 & ARCHITECTURE §6.2: Never fall back to delete-then-rename.
                if !self.capabilities.posix_rename {
                    return Err(FsError::Unsupported);
                }
                let id = self.alloc_id();
                let mut enc = PacketEncoder::with_request(SSH_FXP_EXTENDED, id);
                enc.put_str(EXT_POSIX_RENAME);
                enc.put_bytes(from.as_bytes());
                enc.put_bytes(to.as_bytes());
                match self.request(id, enc.finish()).await? {
                    ResponseBody::Status {
                        code: SSH_FX_OK, ..
                    } => Ok(()),
                    ResponseBody::Status { code, message } => Err(status_to_error(code, message)),
                    _ => Err(FsError::Protocol(
                        "unexpected response to posix-rename@openssh.com".into(),
                    )),
                }
            }
        }
    }

    pub async fn open_read(&self, path: &RemotePath) -> FsResult<Box<dyn FileReader>> {
        let id = self.alloc_id();
        let mut enc = PacketEncoder::with_request(SSH_FXP_OPEN, id);
        enc.put_bytes(path.as_bytes());
        enc.put_u32(SSH_FXF_READ);
        FileAttrs::default().encode(&mut enc);
        let handle = match self.request(id, enc.finish()).await? {
            ResponseBody::Handle(handle) => handle,
            ResponseBody::Status { code, message } => return Err(status_to_error(code, message)),
            _ => {
                return Err(FsError::Protocol(
                    "unexpected response to SSH_FXP_OPEN (read)".into(),
                ));
            }
        };
        Ok(Box::new(RemoteFileReader {
            client: self.clone(),
            handle,
            max_read_size: self.capabilities.max_read_size,
            next_request_offset: 0,
            confirmed_offset: 0,
            in_flight: VecDeque::with_capacity(MAX_READ_PIPELINE),
            buffered: Vec::new(),
            buffered_offset: 0,
            eof: false,
            closed: false,
        }))
    }

    pub async fn open_write(
        &self,
        path: &RemotePath,
        opts: WriteOptions,
    ) -> FsResult<Box<dyn FileWriter>> {
        ensure_safe_remote_mutation(path)?;
        let flags = if opts.create_new {
            SSH_FXF_WRITE | SSH_FXF_CREAT | SSH_FXF_EXCL
        } else {
            SSH_FXF_WRITE | SSH_FXF_CREAT | SSH_FXF_TRUNC
        };
        let id = self.alloc_id();
        let mut enc = PacketEncoder::with_request(SSH_FXP_OPEN, id);
        enc.put_bytes(path.as_bytes());
        enc.put_u32(flags);
        FileAttrs::with_mode(opts.mode).encode(&mut enc);
        let handle = match self.request(id, enc.finish()).await? {
            ResponseBody::Handle(handle) => handle,
            ResponseBody::Status {
                code: SSH_FX_FAILURE,
                message,
            } if opts.create_new => {
                if self.lstat(path).await.is_ok() {
                    return Err(FsError::AlreadyExists);
                }
                return Err(status_to_error(SSH_FX_FAILURE, message));
            }
            ResponseBody::Status { code, message } => return Err(status_to_error(code, message)),
            _ => {
                return Err(FsError::Protocol(
                    "unexpected response to SSH_FXP_OPEN (write)".into(),
                ));
            }
        };
        let packet_capacity =
            (self.max_packet_len as usize).saturating_sub(WRITE_PACKET_OVERHEAD + handle.len());
        if packet_capacity == 0 {
            self.close_handle_detached(&handle);
            return Err(FsError::Protocol(
                "SFTP packet limit cannot fit a write".into(),
            ));
        }
        Ok(Box::new(RemoteFileWriter {
            client: self.clone(),
            handle,
            max_write_size: self.capabilities.max_write_size.min(packet_capacity as u32),
            fsync_supported: self.capabilities.fsync,
            offset: 0,
            in_flight: VecDeque::with_capacity(MAX_WRITE_PIPELINE),
            closed: false,
        }))
    }

    pub async fn set_metadata(&self, path: &RemotePath, patch: &MetadataPatch) -> FsResult<()> {
        ensure_safe_remote_mutation(path)?;
        let current = if patch.atime.is_some() != patch.mtime.is_some() {
            Some(self.lstat(path).await?)
        } else {
            None
        };
        let acmodtime = match (patch.atime, patch.mtime) {
            (Some(a), Some(m)) => Some((a as u32, m as u32)),
            (Some(a), None) => {
                let m = current.and_then(|c| c.mtime).unwrap_or(a);
                Some((a as u32, m as u32))
            }
            (None, Some(m)) => {
                let a = current.and_then(|c| c.atime).unwrap_or(m);
                Some((a as u32, m as u32))
            }
            (None, None) => None,
        };
        let attrs = FileAttrs {
            size: None,
            uid_gid: None,
            permissions: patch.permissions.map(|mode| mode & 0o0777),
            acmodtime,
        };
        let id = self.alloc_id();
        let mut enc = PacketEncoder::with_request(SSH_FXP_SETSTAT, id);
        enc.put_bytes(path.as_bytes());
        attrs.encode(&mut enc);
        match self.request(id, enc.finish()).await? {
            ResponseBody::Status {
                code: SSH_FX_OK, ..
            } => Ok(()),
            ResponseBody::Status { code, message } => Err(status_to_error(code, message)),
            _ => Err(FsError::Protocol(
                "unexpected response to SSH_FXP_SETSTAT".into(),
            )),
        }
    }

    pub async fn free_space(&self, path: &RemotePath) -> FsResult<Option<u64>> {
        if !self.capabilities.statvfs {
            return Ok(None);
        }
        let id = self.alloc_id();
        let mut enc = PacketEncoder::with_request(SSH_FXP_EXTENDED, id);
        enc.put_str(EXT_STATVFS);
        enc.put_bytes(path.as_bytes());
        match self.request(id, enc.finish()).await? {
            ResponseBody::ExtendedReply(payload) => {
                let mut dec = PacketDecoder::new(&payload);
                let reply = StatVfsReply::decode(&mut dec)?;
                Ok(Some(reply.available_bytes()))
            }
            ResponseBody::Status {
                code: SSH_FX_OP_UNSUPPORTED,
                ..
            } => Ok(None),
            ResponseBody::Status { code, message } => Err(status_to_error(code, message)),
            _ => Err(FsError::Protocol(
                "unexpected response to statvfs@openssh.com".into(),
            )),
        }
    }

    async fn close_handle(&self, handle: &[u8]) -> FsResult<()> {
        let id = self.alloc_id();
        let mut enc = PacketEncoder::with_request(SSH_FXP_CLOSE, id);
        enc.put_bytes(handle);
        match self.request(id, enc.finish()).await? {
            ResponseBody::Status {
                code: SSH_FX_OK, ..
            } => Ok(()),
            ResponseBody::Status { code, message } => Err(status_to_error(code, message)),
            _ => Err(FsError::Protocol(
                "unexpected response to SSH_FXP_CLOSE".into(),
            )),
        }
    }

    fn close_handle_detached(&self, handle: &[u8]) {
        if self.check_failure().is_err() {
            return;
        }
        let Ok(permit) = self.requests.clone().try_acquire_owned() else {
            self.terminate(TerminalFailure::ConnectionLost);
            return;
        };
        let id = self.alloc_id();
        let mut enc = PacketEncoder::with_request(SSH_FXP_CLOSE, id);
        enc.put_bytes(handle);
        let frame = enc.finish();
        if frame.len() > self.max_packet_len as usize {
            self.terminate(TerminalFailure::ConnectionLost);
            return;
        }
        if self
            .tx
            .try_send(OutboundFrame {
                id,
                frame,
                pending: PendingRequest {
                    reply: None,
                    deadline: Instant::now() + self.operation_timeout,
                    _permit: permit,
                },
            })
            .is_err()
        {
            // Closing the channel releases remote handles if cleanup cannot be queued.
            self.terminate(TerminalFailure::ConnectionLost);
        }
    }
}

fn negotiated_chunk_size(server_limit: u64, packet_capacity: u32) -> FsResult<u32> {
    let limit = if server_limit == 0 {
        u64::from(MAX_IO_CHUNK_SIZE)
    } else {
        server_limit
    };
    let size = limit
        .min(u64::from(MAX_IO_CHUNK_SIZE))
        .min(u64::from(packet_capacity)) as u32;
    if size == 0 {
        return Err(FsError::Protocol(
            "SFTP packet limit cannot fit file data".into(),
        ));
    }
    Ok(size)
}

struct DirectoryHandle {
    client: SftpClient,
    handle: Vec<u8>,
    closed: bool,
}

impl DirectoryHandle {
    async fn close(mut self) -> FsResult<()> {
        let id = self.client.alloc_id();
        let mut enc = PacketEncoder::with_request(SSH_FXP_CLOSE, id);
        enc.put_bytes(&self.handle);
        let mut rx = self.client.enqueue_raw(id, enc.finish()).await?;
        // Once queued, the driver owns the close and its deadline. Cancellation
        // must not send a duplicate close for a potentially reused handle token.
        self.closed = true;
        match self.client.await_reply(&mut rx).await? {
            ResponseBody::Status {
                code: SSH_FX_OK, ..
            } => Ok(()),
            ResponseBody::Status { code, message } => Err(status_to_error(code, message)),
            _ => Err(FsError::Protocol(
                "unexpected response to SSH_FXP_CLOSE".into(),
            )),
        }
    }
}

impl Drop for DirectoryHandle {
    fn drop(&mut self) {
        if !self.closed {
            self.client.close_handle_detached(&self.handle);
        }
    }
}

fn ensure_safe_remote_mutation(path: &RemotePath) -> FsResult<()> {
    // Reject paths containing U+FFFD replacement bytes (ADR-010, ARCHITECTURE §6.3, SECURITY §4).
    if contains_replacement_bytes(path.as_bytes()) {
        return Err(FsError::UnsafeName);
    }
    if let Some(name) = path.file_name() {
        if !is_valid_entry_name(name) {
            return Err(FsError::UnsafeName);
        }
    } else {
        return Err(FsError::UnsafeName);
    }
    Ok(())
}

fn fail_all(state: &mut PendingState, failure: TerminalFailure) {
    if state.failure.is_none() {
        state.failure = Some(failure.clone());
    }
    for (_, waiter) in state.waiters.drain() {
        if let Some(reply) = waiter.reply {
            let _ = reply.send(Err(failure.to_error()));
        }
    }
    state.changed.notify_one();
}

async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> FsResult<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if !(5..=MAX_PACKET_LEN).contains(&len) {
        return Err(FsError::Protocol(format!(
            "SFTP packet length {len} outside valid range 5..={MAX_PACKET_LEN}"
        )));
    }
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

struct InFlightRead {
    requested: u32,
    rx: oneshot::Receiver<FsResult<ResponseBody>>,
}

struct RemoteFileReader {
    client: SftpClient,
    handle: Vec<u8>,
    max_read_size: u32,
    next_request_offset: u64,
    confirmed_offset: u64,
    in_flight: VecDeque<InFlightRead>,
    buffered: Vec<u8>,
    buffered_offset: usize,
    eof: bool,
    closed: bool,
}

#[async_trait]
impl FileReader for RemoteFileReader {
    async fn read_chunk(&mut self, max_bytes: usize) -> FsResult<Vec<u8>> {
        if max_bytes == 0 {
            return Ok(Vec::new());
        }
        if self.buffered_offset < self.buffered.len() {
            let take = max_bytes.min(self.buffered.len() - self.buffered_offset);
            let data = self.buffered[self.buffered_offset..self.buffered_offset + take].to_vec();
            self.buffered_offset += take;
            if self.buffered_offset == self.buffered.len() {
                self.buffered.clear();
                self.buffered_offset = 0;
            }
            return Ok(data);
        }
        if self.eof {
            return Ok(Vec::new());
        }
        let step = (max_bytes.clamp(1, self.max_read_size as usize)) as u32;
        while self.in_flight.len() < MAX_READ_PIPELINE {
            let id = self.client.alloc_id();
            let mut enc = PacketEncoder::with_request(SSH_FXP_READ, id);
            enc.put_bytes(&self.handle);
            enc.put_u64(self.next_request_offset);
            enc.put_u32(step);
            let rx = self.client.enqueue_raw(id, enc.finish()).await?;
            self.next_request_offset = self.next_request_offset.saturating_add(u64::from(step));
            self.in_flight.push_back(InFlightRead {
                requested: step,
                rx,
            });
        }

        // Keep the receiver in the queue across await so cancellation can be retried.
        let front = self.in_flight.front_mut().expect("read pipeline non-empty");
        let response = self.client.await_reply(&mut front.rx).await;
        let front = self.in_flight.pop_front().expect("read pipeline non-empty");
        match response? {
            ResponseBody::Data(data) => {
                if data.len() > front.requested as usize {
                    return Err(self.client.terminate(TerminalFailure::Protocol(
                        "SFTP DATA exceeds requested read length".into(),
                    )));
                }
                if data.is_empty() {
                    self.eof = true;
                    self.in_flight.clear();
                    return Ok(Vec::new());
                }
                self.confirmed_offset = self.confirmed_offset.saturating_add(data.len() as u64);
                if (data.len() as u32) < front.requested {
                    // Short read: discard speculative requests issued at higher offsets.
                    self.in_flight.clear();
                    self.next_request_offset = self.confirmed_offset;
                }
                if data.len() > max_bytes {
                    self.buffered = data;
                    self.buffered_offset = max_bytes;
                    Ok(self.buffered[..max_bytes].to_vec())
                } else {
                    Ok(data)
                }
            }
            ResponseBody::Status {
                code: SSH_FX_EOF, ..
            } => {
                self.eof = true;
                self.in_flight.clear();
                Ok(Vec::new())
            }
            ResponseBody::Status { code, message } => {
                self.in_flight.clear();
                Err(status_to_error(code, message))
            }
            _ => {
                self.in_flight.clear();
                Err(FsError::Protocol(
                    "unexpected response to SSH_FXP_READ".into(),
                ))
            }
        }
    }

    async fn close(mut self: Box<Self>) -> FsResult<()> {
        self.in_flight.clear();
        self.closed = true;
        self.client.close_handle(&self.handle).await
    }
}

impl Drop for RemoteFileReader {
    fn drop(&mut self) {
        if !self.closed {
            self.client.close_handle_detached(&self.handle);
        }
    }
}

struct RemoteFileWriter {
    client: SftpClient,
    handle: Vec<u8>,
    max_write_size: u32,
    fsync_supported: bool,
    offset: u64,
    in_flight: VecDeque<oneshot::Receiver<FsResult<ResponseBody>>>,
    closed: bool,
}

impl RemoteFileWriter {
    async fn await_one(
        &mut self,
        mut rx: oneshot::Receiver<FsResult<ResponseBody>>,
    ) -> FsResult<()> {
        match self.client.await_reply(&mut rx).await? {
            ResponseBody::Status {
                code: SSH_FX_OK, ..
            } => Ok(()),
            ResponseBody::Status { code, message } => Err(status_to_error(code, message)),
            _ => Err(FsError::Protocol(
                "unexpected response to SSH_FXP_WRITE".into(),
            )),
        }
    }

    async fn flush_in_flight(&mut self) -> FsResult<()> {
        while let Some(rx) = self.in_flight.pop_front() {
            self.await_one(rx).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl FileWriter for RemoteFileWriter {
    async fn write_all(&mut self, mut buf: &[u8]) -> FsResult<()> {
        let max_chunk = (self.max_write_size as usize).max(1);
        while !buf.is_empty() {
            while self.in_flight.len() >= MAX_WRITE_PIPELINE {
                let rx = self
                    .in_flight
                    .pop_front()
                    .expect("write pipeline non-empty");
                self.await_one(rx).await?;
            }
            let take = buf.len().min(max_chunk);
            let chunk = &buf[..take];
            let id = self.client.alloc_id();
            let mut enc = PacketEncoder::with_request(SSH_FXP_WRITE, id);
            enc.put_bytes(&self.handle);
            enc.put_u64(self.offset);
            enc.put_bytes(chunk);
            let rx = self.client.enqueue_raw(id, enc.finish()).await?;
            self.in_flight.push_back(rx);
            self.offset = self.offset.saturating_add(take as u64);
            buf = &buf[take..];
        }
        Ok(())
    }

    async fn sync(&mut self) -> FsResult<SyncOutcome> {
        self.flush_in_flight().await?;
        if !self.fsync_supported {
            return Ok(SyncOutcome::Unsupported);
        }
        let id = self.client.alloc_id();
        let mut enc = PacketEncoder::with_request(SSH_FXP_EXTENDED, id);
        enc.put_str(EXT_FSYNC);
        enc.put_bytes(&self.handle);
        match self.client.request(id, enc.finish()).await? {
            ResponseBody::Status {
                code: SSH_FX_OK, ..
            } => Ok(SyncOutcome::Synced),
            ResponseBody::Status {
                code: super::wire::SSH_FX_OP_UNSUPPORTED,
                ..
            } => Ok(SyncOutcome::Unsupported),
            ResponseBody::Status { code, message } => Err(status_to_error(code, message)),
            _ => Err(FsError::Protocol(
                "unexpected response to fsync@openssh.com".into(),
            )),
        }
    }

    async fn close(mut self: Box<Self>) -> FsResult<()> {
        let flush_res = self.flush_in_flight().await;
        self.closed = true;
        let close_res = self.client.close_handle(&self.handle).await;
        flush_res?;
        close_res
    }
}

impl Drop for RemoteFileWriter {
    fn drop(&mut self) {
        if !self.closed {
            self.client.close_handle_detached(&self.handle);
        }
    }
}
