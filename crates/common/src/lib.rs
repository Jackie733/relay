//! Shared domain and filesystem boundary. No SSH or UI dependencies.

pub mod fs;
pub mod path;

pub use fs::{DirEntry, FileKind, FileSystem, FsError, FsResult, LocalFs};
pub use path::{Endpoint, FsPath, HostId, Location, RemotePath, display_text};
