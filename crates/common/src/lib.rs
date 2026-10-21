//! Shared domain and filesystem boundary. No SSH or UI dependencies.

pub mod fs;
pub mod path;

pub use fs::{
    Capabilities, DEFAULT_IO_CHUNK_SIZE, DirEntry, FileKind, FileReader, FileSystem, FileWriter,
    FsError, FsResolver, FsResult, LocalFs, MAX_DIR_ENTRIES, MAX_IO_CHUNK_SIZE, Metadata,
    MetadataPatch, RenameMode, SyncOutcome, WriteOptions,
};
pub use path::{
    Endpoint, FsPath, HostId, Location, RemotePath, UTF8_REPLACEMENT_BYTES, bytes_from_os_str,
    contains_replacement_bytes, display_text, find_case_insensitive_conflict, is_valid_entry_name,
    os_string_from_bytes, validate_windows_name,
};
