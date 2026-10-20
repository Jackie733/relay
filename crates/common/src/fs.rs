use std::ffi::OsString;

use async_trait::async_trait;
use thiserror::Error;

use crate::{FsPath, Location, display_text};

pub type FsResult<T> = Result<T, FsError>;

#[derive(Debug, Error)]
pub enum FsError {
    #[error("filesystem I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("path is empty or contains NUL")]
    UnsafePath,
    #[error("path type does not match the filesystem location")]
    PathLocationMismatch,
    #[error("directory exceeds the scaffold limit of {0} entries")]
    DirectoryTooLarge(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: OsString,
    pub path: FsPath,
    pub kind: FileKind,
}

impl DirEntry {
    pub fn display_name(&self) -> String {
        display_text(&self.name.to_string_lossy())
    }
}

/// Initial read-only boundary. Write and streaming APIs arrive with P0-FS.
#[async_trait]
pub trait FileSystem: Send + Sync {
    fn location(&self) -> &Location;
    async fn read_dir(&self, path: &FsPath) -> FsResult<Vec<DirEntry>>;
}

#[derive(Default)]
pub struct LocalFs;

#[async_trait]
impl FileSystem for LocalFs {
    fn location(&self) -> &Location {
        &Location::Local
    }

    async fn read_dir(&self, path: &FsPath) -> FsResult<Vec<DirEntry>> {
        const MAX_ENTRIES: usize = 10_000;
        let FsPath::Local(path) = path else {
            return Err(FsError::PathLocationMismatch);
        };
        let mut reader = tokio::fs::read_dir(path).await?;
        let mut entries = Vec::new();
        while let Some(entry) = reader.next_entry().await? {
            if entries.len() == MAX_ENTRIES {
                return Err(FsError::DirectoryTooLarge(MAX_ENTRIES));
            }
            let kind = entry.file_type().await?;
            entries.push(DirEntry {
                name: entry.file_name(),
                path: FsPath::Local(entry.path()),
                kind: if kind.is_symlink() {
                    FileKind::Symlink
                } else if kind.is_dir() {
                    FileKind::Directory
                } else if kind.is_file() {
                    FileKind::File
                } else {
                    FileKind::Other
                },
            });
        }
        entries.sort_by(|a, b| {
            (a.kind != FileKind::Directory, &a.name).cmp(&(b.kind != FileKind::Directory, &b.name))
        });
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lists_real_directory_and_reports_missing_paths() {
        let root = tempfile::tempdir().unwrap();
        tokio::fs::write(root.path().join("a 中文.txt"), b"relay")
            .await
            .unwrap();
        tokio::fs::create_dir(root.path().join("z-dir"))
            .await
            .unwrap();
        let entries = LocalFs
            .read_dir(&FsPath::Local(root.path().into()))
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, FileKind::Directory);
        assert_eq!(entries[1].name, "a 中文.txt");
        assert!(
            LocalFs
                .read_dir(&FsPath::Local(root.path().join("missing")))
                .await
                .is_err()
        );
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
        let entries = LocalFs
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
        let entries = LocalFs
            .read_dir(&FsPath::Local(root.path().into()))
            .await
            .unwrap();
        assert_eq!(entries[0].name, name);
        let FsPath::Local(path) = &entries[0].path else {
            panic!("expected local path")
        };
        assert_eq!(tokio::fs::read(path).await.unwrap(), b"relay");
    }
}
