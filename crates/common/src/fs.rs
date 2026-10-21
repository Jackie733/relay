use std::{
    collections::HashMap,
    ffi::OsString,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{Duration, UNIX_EPOCH},
};

use async_trait::async_trait;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{
    FsPath, HostId, Location, RemotePath,
    path::{
        bytes_from_os_str, contains_replacement_bytes, display_text, is_valid_entry_name,
        os_string_from_bytes,
    },
};

pub const MAX_DIR_ENTRIES: usize = 10_000;
pub const DEFAULT_IO_CHUNK_SIZE: u32 = 64 * 1024;
pub const MAX_IO_CHUNK_SIZE: u32 = 256 * 1024;

pub type FsResult<T> = Result<T, FsError>;

#[derive(Debug, Error)]
pub enum FsError {
    #[error("path not found")]
    NotFound,
    #[error("permission denied")]
    PermissionDenied,
    #[error("target already exists")]
    AlreadyExists,
    #[error("no space left on device")]
    NoSpace,
    #[error("operation is not supported by this filesystem")]
    Unsupported,
    #[error("filename cannot be safely represented or mutated")]
    UnsafeName,
    #[error("path is empty or contains NUL")]
    UnsafePath,
    #[error("path type does not match the filesystem location")]
    PathLocationMismatch,
    #[error("directory exceeds limit of {0} entries")]
    DirectoryTooLarge(usize),
    #[error("connection lost during filesystem operation")]
    ConnectionLost,
    #[error("filesystem operation cancelled")]
    Cancelled,
    #[error("remote filesystem failure: {server_message}")]
    Failure { server_message: String },
    #[error("SFTP protocol error: {0}")]
    Protocol(String),
    #[error("filesystem I/O: {0}")]
    Io(#[source] std::io::Error),
}

impl From<std::io::Error> for FsError {
    fn from(error: std::io::Error) -> Self {
        match error.kind() {
            ErrorKind::NotFound => Self::NotFound,
            ErrorKind::PermissionDenied => Self::PermissionDenied,
            ErrorKind::AlreadyExists => Self::AlreadyExists,
            ErrorKind::StorageFull => Self::NoSpace,
            ErrorKind::Unsupported => Self::Unsupported,
            ErrorKind::BrokenPipe
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::UnexpectedEof
            | ErrorKind::NotConnected => Self::ConnectionLost,
            _ => Self::Io(error),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Capabilities {
    pub posix_rename: bool,
    pub fsync: bool,
    pub statvfs: bool,
    pub max_read_size: u32,
    pub max_write_size: u32,
    pub max_open_handles: Option<u64>,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            posix_rename: false,
            fsync: false,
            statvfs: false,
            max_read_size: DEFAULT_IO_CHUNK_SIZE,
            max_write_size: DEFAULT_IO_CHUNK_SIZE,
            max_open_handles: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FileKind {
    Directory,
    #[default]
    File,
    Symlink,
    Other,
}

impl FileKind {
    pub fn from_unix_mode(mode: u32) -> Self {
        match mode & 0o170000 {
            0o040000 => Self::Directory,
            0o100000 => Self::File,
            0o120000 => Self::Symlink,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Metadata {
    pub kind: FileKind,
    pub size: u64,
    pub permissions: Option<u32>,
    pub mtime: Option<u64>,
    pub atime: Option<u64>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

impl Metadata {
    /// Strips setuid, setgid, and sticky bits (0o7000), keeping only 0o0777.
    pub fn sanitized_permissions(&self) -> Option<u32> {
        self.permissions.map(|mode| mode & 0o0777)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MetadataPatch {
    pub permissions: Option<u32>,
    pub mtime: Option<u64>,
    pub atime: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenameMode {
    NoReplace,
    AtomicReplace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriteOptions {
    pub create_new: bool,
    pub mode: u32,
}

impl WriteOptions {
    pub fn create_new(mode: u32) -> Self {
        Self {
            create_new: true,
            mode: mode & 0o0777,
        }
    }

    pub fn overwrite(mode: u32) -> Self {
        Self {
            create_new: false,
            mode: mode & 0o0777,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncOutcome {
    Synced,
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirEntry {
    pub name: OsString,
    pub raw_name: Vec<u8>,
    pub path: FsPath,
    pub kind: FileKind,
    pub metadata: Metadata,
    /// True if the name contains U+FFFD from lossy decoding and must not be mutated.
    pub unsafe_name: bool,
}

impl DirEntry {
    pub fn new_local(name: OsString, path: PathBuf, metadata: Metadata) -> Self {
        let raw_name = bytes_from_os_str(&name);
        let unsafe_name = contains_replacement_bytes(&raw_name);
        let kind = metadata.kind;
        Self {
            name,
            raw_name,
            path: FsPath::Local(path),
            kind,
            metadata,
            unsafe_name,
        }
    }

    pub fn new_remote(raw_name: Vec<u8>, path: RemotePath, metadata: Metadata) -> Self {
        let unsafe_name = contains_replacement_bytes(&raw_name);
        let name = os_string_from_bytes(&raw_name);
        let kind = metadata.kind;
        Self {
            name,
            raw_name,
            path: FsPath::Remote(path),
            kind,
            metadata,
            unsafe_name,
        }
    }

    pub fn display_name(&self) -> String {
        let rendered = display_text(&String::from_utf8_lossy(&self.raw_name));
        if self.unsafe_name {
            format!("{rendered} [unsafe name]")
        } else {
            rendered
        }
    }

    pub fn is_valid_utf8(&self) -> bool {
        std::str::from_utf8(&self.raw_name).is_ok()
    }

    /// Rejects write/delete/rename/copy on entries marked with U+FFFD lossy names.
    pub fn ensure_mutable(&self) -> FsResult<()> {
        if self.unsafe_name {
            return Err(FsError::UnsafeName);
        }
        Ok(())
    }
}

#[async_trait]
pub trait FileReader: Send {
    /// Returns up to `max_bytes`, or an empty `Vec` at EOF or when `max_bytes` is zero.
    async fn read_chunk(&mut self, max_bytes: usize) -> FsResult<Vec<u8>>;
    async fn close(self: Box<Self>) -> FsResult<()>;
}

#[async_trait]
pub trait FileWriter: Send {
    async fn write_all(&mut self, buf: &[u8]) -> FsResult<()>;
    async fn sync(&mut self) -> FsResult<SyncOutcome>;
    async fn close(self: Box<Self>) -> FsResult<()>;
}

#[async_trait]
pub trait FileSystem: Send + Sync {
    fn location(&self) -> &Location;
    fn capabilities(&self) -> &Capabilities;

    async fn read_dir(&self, path: &FsPath) -> FsResult<Vec<DirEntry>>;
    async fn lstat(&self, path: &FsPath) -> FsResult<Metadata>;
    async fn stat(&self, path: &FsPath) -> FsResult<Metadata>;
    async fn canonicalize(&self, path: &FsPath) -> FsResult<FsPath>;
    async fn create_dir(&self, path: &FsPath, mode: u32) -> FsResult<()>;
    async fn remove_file(&self, path: &FsPath) -> FsResult<()>;
    async fn remove_dir(&self, path: &FsPath) -> FsResult<()>;
    async fn rename(&self, from: &FsPath, to: &FsPath, mode: RenameMode) -> FsResult<()>;
    async fn open_read(&self, path: &FsPath) -> FsResult<Box<dyn FileReader>>;
    async fn open_write(&self, path: &FsPath, opts: WriteOptions) -> FsResult<Box<dyn FileWriter>>;
    async fn set_metadata(&self, path: &FsPath, patch: &MetadataPatch) -> FsResult<()>;
    async fn free_space(&self, path: &FsPath) -> FsResult<Option<u64>>;
}

#[derive(Clone, Debug)]
pub struct LocalFs {
    capabilities: Capabilities,
}

impl Default for LocalFs {
    fn default() -> Self {
        Self {
            capabilities: Capabilities {
                posix_rename: true,
                fsync: true,
                statvfs: false,
                max_read_size: MAX_IO_CHUNK_SIZE,
                max_write_size: MAX_IO_CHUNK_SIZE,
                max_open_handles: None,
            },
        }
    }
}

impl LocalFs {
    pub fn new() -> Self {
        Self::default()
    }
}

fn extract_local_path(path: &FsPath) -> FsResult<&Path> {
    let FsPath::Local(local) = path else {
        return Err(FsError::PathLocationMismatch);
    };
    let raw = bytes_from_os_str(local.as_os_str());
    if raw.is_empty() || raw.contains(&0) {
        return Err(FsError::UnsafePath);
    }
    Ok(local)
}

fn ensure_safe_local_mutation(path: &Path) -> FsResult<()> {
    if let Some(name) = path.file_name() {
        let raw = bytes_from_os_str(name);
        if !is_valid_entry_name(&raw) || contains_replacement_bytes(&raw) {
            return Err(FsError::UnsafeName);
        }
    }
    Ok(())
}

fn map_std_metadata(meta: &std::fs::Metadata) -> Metadata {
    let ft = meta.file_type();
    let kind = if ft.is_symlink() {
        FileKind::Symlink
    } else if ft.is_dir() {
        FileKind::Directory
    } else if ft.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let atime = meta
        .accessed()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    #[cfg(unix)]
    let (permissions, uid, gid) = {
        use std::os::unix::fs::MetadataExt;
        (Some(meta.mode()), Some(meta.uid()), Some(meta.gid()))
    };
    #[cfg(not(unix))]
    let (permissions, uid, gid) = (None, None, None);

    Metadata {
        kind,
        size: meta.len(),
        permissions,
        mtime,
        atime,
        uid,
        gid,
    }
}

#[async_trait]
impl FileSystem for LocalFs {
    fn location(&self) -> &Location {
        &Location::Local
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn read_dir(&self, path: &FsPath) -> FsResult<Vec<DirEntry>> {
        let local = extract_local_path(path)?;
        let mut reader = tokio::fs::read_dir(local).await?;
        let mut entries = Vec::new();
        while let Some(entry) = reader.next_entry().await? {
            if entries.len() == MAX_DIR_ENTRIES {
                return Err(FsError::DirectoryTooLarge(MAX_DIR_ENTRIES));
            }
            let name = entry.file_name();
            let raw = bytes_from_os_str(&name);
            if !is_valid_entry_name(&raw) {
                continue;
            }
            let entry_path = entry.path();
            let meta = tokio::fs::symlink_metadata(&entry_path).await?;
            entries.push(DirEntry::new_local(
                name,
                entry_path,
                map_std_metadata(&meta),
            ));
        }
        entries.sort_by(|a, b| {
            (a.kind != FileKind::Directory, &a.raw_name)
                .cmp(&(b.kind != FileKind::Directory, &b.raw_name))
        });
        Ok(entries)
    }

    async fn lstat(&self, path: &FsPath) -> FsResult<Metadata> {
        let local = extract_local_path(path)?;
        let meta = tokio::fs::symlink_metadata(local).await?;
        Ok(map_std_metadata(&meta))
    }

    async fn stat(&self, path: &FsPath) -> FsResult<Metadata> {
        let local = extract_local_path(path)?;
        let meta = tokio::fs::metadata(local).await?;
        Ok(map_std_metadata(&meta))
    }

    async fn canonicalize(&self, path: &FsPath) -> FsResult<FsPath> {
        let local = extract_local_path(path)?;
        let canonical = tokio::fs::canonicalize(local).await?;
        Ok(FsPath::Local(canonical))
    }

    async fn create_dir(&self, path: &FsPath, mode: u32) -> FsResult<()> {
        let local = extract_local_path(path)?;
        ensure_safe_local_mutation(local)?;
        let mut builder = tokio::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            builder.mode(mode & 0o0777);
        }
        #[cfg(not(unix))]
        let _ = mode;
        builder.create(local).await?;
        Ok(())
    }

    async fn remove_file(&self, path: &FsPath) -> FsResult<()> {
        let local = extract_local_path(path)?;
        ensure_safe_local_mutation(local)?;
        tokio::fs::remove_file(local).await?;
        Ok(())
    }

    async fn remove_dir(&self, path: &FsPath) -> FsResult<()> {
        let local = extract_local_path(path)?;
        ensure_safe_local_mutation(local)?;
        tokio::fs::remove_dir(local).await?;
        Ok(())
    }

    async fn rename(&self, from: &FsPath, to: &FsPath, mode: RenameMode) -> FsResult<()> {
        let src = extract_local_path(from)?;
        let dst = extract_local_path(to)?;
        ensure_safe_local_mutation(src)?;
        ensure_safe_local_mutation(dst)?;
        match mode {
            RenameMode::AtomicReplace => {
                tokio::fs::rename(src, dst).await?;
                Ok(())
            }
            RenameMode::NoReplace => {
                let src_meta = tokio::fs::symlink_metadata(src).await?;
                if src_meta.file_type().is_dir() {
                    if tokio::fs::symlink_metadata(dst).await.is_ok() {
                        return Err(FsError::AlreadyExists);
                    }
                    tokio::fs::rename(src, dst).await?;
                    Ok(())
                } else {
                    tokio::fs::hard_link(src, dst).await?;
                    if let Err(err) = tokio::fs::remove_file(src).await {
                        let _ = tokio::fs::remove_file(dst).await;
                        return Err(err.into());
                    }
                    Ok(())
                }
            }
        }
    }

    async fn open_read(&self, path: &FsPath) -> FsResult<Box<dyn FileReader>> {
        let local = extract_local_path(path)?;
        let file = tokio::fs::File::open(local).await?;
        Ok(Box::new(LocalFileReader { file }))
    }

    async fn open_write(&self, path: &FsPath, opts: WriteOptions) -> FsResult<Box<dyn FileWriter>> {
        let local = extract_local_path(path)?;
        ensure_safe_local_mutation(local)?;
        let mut open = tokio::fs::OpenOptions::new();
        open.write(true);
        if opts.create_new {
            open.create_new(true);
        } else {
            open.create(true).truncate(true);
        }
        #[cfg(unix)]
        {
            open.mode(opts.mode & 0o0777);
        }
        let file = open.open(local).await?;
        Ok(Box::new(LocalFileWriter { file }))
    }

    async fn set_metadata(&self, path: &FsPath, patch: &MetadataPatch) -> FsResult<()> {
        let local = extract_local_path(path)?;
        ensure_safe_local_mutation(local)?;
        #[cfg(not(unix))]
        if patch.permissions.is_some() {
            return Err(FsError::Unsupported);
        }
        if patch.atime.is_some() || patch.mtime.is_some() {
            let to_time = |seconds| {
                UNIX_EPOCH
                    .checked_add(Duration::from_secs(seconds))
                    .ok_or_else(|| {
                        FsError::Io(std::io::Error::new(
                            ErrorKind::InvalidInput,
                            "timestamp out of range",
                        ))
                    })
            };
            let mut times = tokio::fs::FileTimes::new();
            if let Some(atime) = patch.atime {
                times = times.set_accessed(to_time(atime)?);
            }
            if let Some(mtime) = patch.mtime {
                times = times.set_modified(to_time(mtime)?);
            }
            let mut open = tokio::fs::OpenOptions::new();
            open.read(true);
            #[cfg(windows)]
            {
                // FILE_WRITE_ATTRIBUTES and FILE_FLAG_BACKUP_SEMANTICS also allow
                // timestamp updates on directories without requesting data writes.
                open.access_mode(0x100).custom_flags(0x02000000);
            }
            open.open(local).await?.set_times(times).await?;
        }
        // Apply restrictive permissions last so they cannot prevent timestamp updates.
        #[cfg(unix)]
        if let Some(mode) = patch.permissions {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(mode & 0o0777);
            tokio::fs::set_permissions(local, perms).await?;
        }
        Ok(())
    }

    async fn free_space(&self, path: &FsPath) -> FsResult<Option<u64>> {
        let _ = extract_local_path(path)?;
        Ok(None)
    }
}

struct LocalFileReader {
    file: tokio::fs::File,
}

#[async_trait]
impl FileReader for LocalFileReader {
    async fn read_chunk(&mut self, max_bytes: usize) -> FsResult<Vec<u8>> {
        let len = max_bytes.min(MAX_IO_CHUNK_SIZE as usize);
        let mut buf = vec![0u8; len];
        let n = self.file.read(&mut buf).await?;
        buf.truncate(n);
        Ok(buf)
    }

    async fn close(self: Box<Self>) -> FsResult<()> {
        Ok(())
    }
}

struct LocalFileWriter {
    file: tokio::fs::File,
}

#[async_trait]
impl FileWriter for LocalFileWriter {
    async fn write_all(&mut self, buf: &[u8]) -> FsResult<()> {
        self.file.write_all(buf).await?;
        Ok(())
    }

    async fn sync(&mut self) -> FsResult<SyncOutcome> {
        self.file.sync_all().await?;
        Ok(SyncOutcome::Synced)
    }

    async fn close(mut self: Box<Self>) -> FsResult<()> {
        self.file.flush().await?;
        Ok(())
    }
}

/// Maps [`Location`] to its [`FileSystem`] implementation without coupling
/// callers (`transfer`, `tui::services`) to `ssh`.
#[derive(Clone)]
pub struct FsResolver {
    local: Arc<dyn FileSystem>,
    remotes: Arc<RwLock<HashMap<HostId, Arc<dyn FileSystem>>>>,
}

impl Default for FsResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl FsResolver {
    pub fn new() -> Self {
        Self {
            local: Arc::new(LocalFs::new()),
            remotes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn register_remote(&self, host: HostId, fs: Arc<dyn FileSystem>) {
        self.remotes
            .write()
            .expect("FsResolver lock poisoned")
            .insert(host, fs);
    }

    pub fn resolve(&self, location: &Location) -> FsResult<Arc<dyn FileSystem>> {
        match location {
            Location::Local => Ok(self.local.clone()),
            Location::Remote(host) => self
                .remotes
                .read()
                .expect("FsResolver lock poisoned")
                .get(host)
                .cloned()
                .ok_or(FsError::ConnectionLost),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_metadata_updates_timestamps_and_preserves_unselected_fields() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file");
        tokio::fs::write(&file, b"unchanged").await.unwrap();
        let fs = LocalFs::new();
        for path in [file.clone(), root.path().to_path_buf()] {
            let path = FsPath::Local(path);
            fs.set_metadata(
                &path,
                &MetadataPatch {
                    atime: Some(1_600_000_000),
                    mtime: Some(1_700_000_000),
                    permissions: None,
                },
            )
            .await
            .unwrap();
            fs.set_metadata(
                &path,
                &MetadataPatch {
                    mtime: Some(1_700_000_100),
                    ..MetadataPatch::default()
                },
            )
            .await
            .unwrap();
            let meta = fs.stat(&path).await.unwrap();
            assert_eq!(meta.atime, Some(1_600_000_000));
            assert_eq!(meta.mtime, Some(1_700_000_100));
            fs.set_metadata(
                &path,
                &MetadataPatch {
                    atime: Some(1_600_000_100),
                    ..MetadataPatch::default()
                },
            )
            .await
            .unwrap();
            let meta = fs.stat(&path).await.unwrap();
            assert_eq!(meta.atime, Some(1_600_000_100));
            assert_eq!(meta.mtime, Some(1_700_000_100));
        }
        let mut reader = fs.open_read(&FsPath::Local(file.clone())).await.unwrap();
        assert!(reader.read_chunk(0).await.unwrap().is_empty());
        assert_eq!(reader.read_chunk(100).await.unwrap(), b"unchanged");
        reader.close().await.unwrap();
        assert!(
            fs.set_metadata(
                &FsPath::Local(file),
                &MetadataPatch {
                    mtime: Some(u64::MAX),
                    ..MetadataPatch::default()
                }
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn lists_real_directory_and_reports_missing_paths() {
        let root = tempfile::tempdir().unwrap();
        tokio::fs::write(root.path().join("a 中文.txt"), b"relay")
            .await
            .unwrap();
        tokio::fs::create_dir(root.path().join("z-dir"))
            .await
            .unwrap();
        let fs = LocalFs::new();
        let entries = fs
            .read_dir(&FsPath::Local(root.path().into()))
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, FileKind::Directory);
        assert_eq!(entries[1].name, "a 中文.txt");
        assert_eq!(entries[1].metadata.size, 5);
        assert!(matches!(
            fs.read_dir(&FsPath::Local(root.path().join("missing")))
                .await,
            Err(FsError::NotFound)
        ));
    }

    #[tokio::test]
    async fn local_fs_read_write_rename_and_unsafe_name_guards() {
        let root = tempfile::tempdir().unwrap();
        let fs = LocalFs::new();
        let file_a = FsPath::Local(root.path().join("alpha.txt"));
        let file_b = FsPath::Local(root.path().join("beta.txt"));

        let mut writer = fs
            .open_write(&file_a, WriteOptions::create_new(0o600))
            .await
            .unwrap();
        writer.write_all(b"hello ").await.unwrap();
        writer.write_all(b"world").await.unwrap();
        assert_eq!(writer.sync().await.unwrap(), SyncOutcome::Synced);
        writer.close().await.unwrap();

        // Creating again with create_new fails with AlreadyExists.
        assert!(matches!(
            fs.open_write(&file_a, WriteOptions::create_new(0o600))
                .await,
            Err(FsError::AlreadyExists)
        ));

        // Read back in chunks.
        let mut reader = fs.open_read(&file_a).await.unwrap();
        let mut collected = Vec::new();
        loop {
            let chunk = reader.read_chunk(4).await.unwrap();
            if chunk.is_empty() {
                break;
            }
            collected.extend_from_slice(&chunk);
        }
        reader.close().await.unwrap();
        assert_eq!(collected, b"hello world");

        // NoReplace rename succeeds when target does not exist, fails when it does.
        fs.rename(&file_a, &file_b, RenameMode::NoReplace)
            .await
            .unwrap();
        let mut writer = fs
            .open_write(&file_a, WriteOptions::create_new(0o600))
            .await
            .unwrap();
        writer.write_all(b"replacement").await.unwrap();
        writer.close().await.unwrap();

        assert!(matches!(
            fs.rename(&file_a, &file_b, RenameMode::NoReplace).await,
            Err(FsError::AlreadyExists)
        ));
        fs.rename(&file_a, &file_b, RenameMode::AtomicReplace)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read(root.path().join("beta.txt")).await.unwrap(),
            b"replacement"
        );

        // Paths containing U+FFFD replacement characters are rejected for mutation.
        let lossy = FsPath::Local(root.path().join("bad-\u{fffd}.txt"));
        assert!(matches!(
            fs.open_write(&lossy, WriteOptions::create_new(0o600)).await,
            Err(FsError::UnsafeName)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preserves_names_and_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let name = OsString::from("name\x1b");
        tokio::fs::write(root.path().join(&name), b"relay")
            .await
            .unwrap();
        symlink("missing-target", root.path().join("link")).unwrap();
        let fs = LocalFs::new();
        let entries = fs
            .read_dir(&FsPath::Local(root.path().into()))
            .await
            .unwrap();
        assert!(entries.iter().any(|entry| entry.name == name));
        assert!(entries.iter().any(|entry| entry.kind == FileKind::Symlink));
        assert!(
            entries
                .iter()
                .all(|entry| !entry.display_name().contains('\x1b'))
        );
    }

    // macOS filesystems reject invalid UTF-8 filenames; Linux permits them.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn local_non_utf8_names_round_trip() {
        use std::os::unix::ffi::OsStringExt;
        let root = tempfile::tempdir().unwrap();
        let name = OsString::from_vec(b"name\xff".to_vec());
        tokio::fs::write(root.path().join(&name), b"relay")
            .await
            .unwrap();
        let fs = LocalFs::new();
        let entries = fs
            .read_dir(&FsPath::Local(root.path().into()))
            .await
            .unwrap();
        assert_eq!(entries[0].name, name);
        assert_eq!(entries[0].raw_name, b"name\xff");
        assert!(!entries[0].unsafe_name);
        let FsPath::Local(path) = &entries[0].path else {
            panic!("expected local path")
        };
        assert_eq!(tokio::fs::read(path).await.unwrap(), b"relay");
    }
}
