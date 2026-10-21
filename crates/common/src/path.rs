use std::{collections::HashMap, ffi::OsString, path::PathBuf};

use crate::{FsError, FsResult};

/// UTF-8 encoding of U+FFFD REPLACEMENT CHARACTER (`\xef\xbf\xbd`).
pub const UTF8_REPLACEMENT_BYTES: &[u8] = b"\xef\xbf\xbd";

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct HostId(pub String);

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub enum Location {
    #[default]
    Local,
    Remote(HostId),
}

impl Location {
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Local => "Local",
            Self::Remote(host) => &host.0,
        }
    }
}

/// POSIX path bytes are preserved verbatim; decoding is only for display.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RemotePath(Vec<u8>);

impl RemotePath {
    pub fn new(bytes: Vec<u8>) -> FsResult<Self> {
        if bytes.is_empty() || bytes.contains(&0) {
            return Err(FsError::UnsafePath);
        }
        Ok(Self(bytes))
    }

    pub fn from_posix(path: &str) -> FsResult<Self> {
        Self::new(path.as_bytes().to_vec())
    }

    pub fn root() -> Self {
        Self(b"/".to_vec())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// True if the raw bytes contain the UTF-8 replacement character U+FFFD,
    /// indicating a previously lossy conversion that must not be mutated.
    pub fn contains_replacement_char(&self) -> bool {
        contains_replacement_bytes(&self.0)
    }

    pub fn is_valid_utf8(&self) -> bool {
        std::str::from_utf8(&self.0).is_ok()
    }

    pub fn file_name(&self) -> Option<&[u8]> {
        let trimmed = trim_trailing_slashes(&self.0);
        if trimmed == b"/" {
            return None;
        }
        match trimmed.iter().rposition(|&b| b == b'/') {
            Some(index) => Some(&trimmed[index + 1..]),
            None => Some(trimmed),
        }
    }

    pub fn parent(&self) -> Option<Self> {
        let trimmed = trim_trailing_slashes(&self.0);
        if trimmed == b"/" {
            return None;
        }
        let pos = trimmed.iter().rposition(|&b| b == b'/')?;
        if pos == 0 {
            return Some(Self::root());
        }
        let parent = trim_trailing_slashes(&trimmed[..pos]);
        Some(Self(parent.to_vec()))
    }

    pub fn join(&self, name: &[u8]) -> FsResult<Self> {
        if !is_valid_entry_name(name) {
            return Err(FsError::UnsafeName);
        }
        let mut out = Vec::with_capacity(self.0.len() + 1 + name.len());
        out.extend_from_slice(&self.0);
        if !out.ends_with(b"/") {
            out.push(b'/');
        }
        out.extend_from_slice(name);
        Ok(Self(out))
    }

    pub fn display(&self) -> String {
        display_text(&String::from_utf8_lossy(&self.0))
    }
}

fn trim_trailing_slashes(mut bytes: &[u8]) -> &[u8] {
    while bytes.len() > 1 && bytes.ends_with(b"/") {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

pub fn contains_replacement_bytes(bytes: &[u8]) -> bool {
    bytes
        .windows(UTF8_REPLACEMENT_BYTES.len())
        .any(|window| window == UTF8_REPLACEMENT_BYTES)
}

/// Validates a single directory entry name returned by a server or caller.
/// Rejects empty names, `.`, `..`, path separators `/`, and NUL bytes.
pub fn is_valid_entry_name(name: &[u8]) -> bool {
    !name.is_empty() && name != b"." && name != b".." && !name.contains(&b'/') && !name.contains(&0)
}

/// Validates that a remote entry name can be safely created on a Windows
/// filesystem (ADR-010, ARCHITECTURE §6.4, SECURITY §4).
pub fn validate_windows_name(name: &[u8]) -> FsResult<&str> {
    if !is_valid_entry_name(name) || contains_replacement_bytes(name) {
        return Err(FsError::UnsafeName);
    }
    let text = std::str::from_utf8(name).map_err(|_| FsError::UnsafeName)?;
    if text.ends_with(' ') || text.ends_with('.') {
        return Err(FsError::UnsafeName);
    }
    if text.bytes().any(|b| b <= 0x1f)
        || text
            .chars()
            .any(|ch| matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'))
    {
        return Err(FsError::UnsafeName);
    }
    let stem = text
        .split('.')
        .next()
        .unwrap_or(text)
        .trim_end_matches([' ', '.']);
    let upper = stem.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "COM\u{b9}"
            | "COM\u{b2}"
            | "COM\u{b3}"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
            | "LPT\u{b9}"
            | "LPT\u{b2}"
            | "LPT\u{b3}"
    ) {
        return Err(FsError::UnsafeName);
    }
    Ok(text)
}

/// Detects case-folding collisions when writing a set of names to a
/// case-insensitive filesystem (such as Windows NTFS / macOS APFS default).
pub fn find_case_insensitive_conflict<S: AsRef<str>>(names: &[S]) -> Option<(String, String)> {
    let mut seen = HashMap::<String, &str>::with_capacity(names.len());
    for item in names {
        let name = item.as_ref();
        let folded = name.to_lowercase();
        if let Some(&existing) = seen.get(&folded) {
            if existing != name {
                return Some((existing.to_owned(), name.to_owned()));
            }
        } else {
            seen.insert(folded, name);
        }
    }
    None
}

/// Converts raw entry bytes into an [`OsString`] without loss on Unix.
pub fn os_string_from_bytes(bytes: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes.to_vec())
    }
    #[cfg(not(unix))]
    {
        OsString::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// Extracts raw bytes from a local [` std::ffi::OsStr`].
pub fn bytes_from_os_str(name: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        name.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        name.to_string_lossy().into_owned().into_bytes()
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum FsPath {
    Local(PathBuf),
    Remote(RemotePath),
}

impl FsPath {
    pub fn local(path: impl Into<PathBuf>) -> Self {
        Self::Local(path.into())
    }

    pub fn remote(bytes: impl Into<Vec<u8>>) -> FsResult<Self> {
        Ok(Self::Remote(RemotePath::new(bytes.into())?))
    }

    pub fn display_name(&self) -> String {
        match self {
            Self::Local(path) => display_text(&path.to_string_lossy()),
            Self::Remote(path) => path.display(),
        }
    }

    pub fn parent(&self) -> Option<Self> {
        match self {
            Self::Local(path) => path
                .parent()
                .map(|parent| Self::Local(parent.to_path_buf())),
            Self::Remote(path) => path.parent().map(Self::Remote),
        }
    }

    pub fn join_entry(&self, name: &[u8]) -> FsResult<Self> {
        if !is_valid_entry_name(name) {
            return Err(FsError::UnsafeName);
        }
        match self {
            Self::Local(path) => {
                #[cfg(windows)]
                {
                    let valid = validate_windows_name(name)?;
                    Ok(Self::Local(path.join(valid)))
                }
                #[cfg(not(windows))]
                {
                    if name.contains(&b'\\') && cfg!(windows) {
                        return Err(FsError::UnsafeName);
                    }
                    Ok(Self::Local(path.join(os_string_from_bytes(name))))
                }
            }
            Self::Remote(path) => Ok(Self::Remote(path.join(name)?)),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Endpoint {
    location: Location,
    path: FsPath,
}

impl Endpoint {
    pub fn new(location: Location, path: FsPath) -> FsResult<Self> {
        if !matches!(
            (&location, &path),
            (Location::Local, FsPath::Local(_)) | (Location::Remote(_), FsPath::Remote(_))
        ) {
            return Err(FsError::PathLocationMismatch);
        }
        Ok(Self { location, path })
    }

    pub fn local(path: impl Into<PathBuf>) -> Self {
        Self {
            location: Location::Local,
            path: FsPath::Local(path.into()),
        }
    }

    pub fn remote(host: HostId, path: RemotePath) -> Self {
        Self {
            location: Location::Remote(host),
            path: FsPath::Remote(path),
        }
    }

    pub fn location(&self) -> &Location {
        &self.location
    }

    pub fn path(&self) -> &FsPath {
        &self.path
    }
}

/// Prevent terminal control sequences and invisible direction overrides in labels.
pub fn display_text(text: &str) -> String {
    text.chars()
        .flat_map(|ch| {
            if ch.is_control()
                || matches!(
                    ch,
                    '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
                )
            {
                ch.escape_default().collect::<Vec<_>>()
            } else {
                vec![ch]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_paths_preserve_non_utf8_bytes_and_navigate() {
        let bytes = b"/tmp/\xff name".to_vec();
        let path = RemotePath::new(bytes.clone()).unwrap();
        assert_eq!(path.as_bytes(), bytes);
        assert!(!path.is_valid_utf8());
        assert!(!path.contains_replacement_char());
        assert_eq!(path.file_name(), Some(&b"\xff name"[..]));
        assert_eq!(path.parent(), Some(RemotePath::from_posix("/tmp").unwrap()));
        assert_eq!(
            RemotePath::from_posix("/tmp").unwrap().parent(),
            Some(RemotePath::root())
        );
        assert_eq!(RemotePath::root().parent(), None);

        let child = path.join(b"sub\xfe.bin").unwrap();
        assert_eq!(child.as_bytes(), b"/tmp/\xff name/sub\xfe.bin");
        assert!(path.join(b"..").is_err());
        assert!(path.join(b".").is_err());
        assert!(path.join(b"").is_err());
        assert!(path.join(b"a/b").is_err());
        assert!(path.join(b"a\0b").is_err());
        assert!(RemotePath::new(vec![b'a', 0]).is_err());
        assert!(RemotePath::new(vec![]).is_err());
    }

    #[test]
    fn rejects_mismatched_endpoint_types() {
        assert!(
            Endpoint::new(
                Location::Local,
                FsPath::Remote(RemotePath::new(b"/".to_vec()).unwrap())
            )
            .is_err()
        );
        assert!(
            Endpoint::new(
                Location::Remote(HostId("ssh:test".into())),
                FsPath::Local("/".into())
            )
            .is_err()
        );
    }

    #[test]
    fn windows_filename_validation_and_case_conflicts() {
        assert_eq!(
            validate_windows_name(b"normal-file_1.txt").unwrap(),
            "normal-file_1.txt"
        );
        for invalid in [
            &b""[..],
            b".",
            b"..",
            b"CON",
            b"con.txt",
            b"NUL.tar.gz",
            b"com1",
            b"lpt9.log",
            b"trailing.",
            b"trailing ",
            b"bad:name",
            b"bad<name>",
            b"bad|name",
            b"bad?name",
            b"bad*name",
            b"bad\\name",
            b"bad/name",
            b"ctrl\x07name",
            b"non-utf8-\xff",
            "lossy-\u{fffd}.txt".as_bytes(),
        ] {
            assert!(
                validate_windows_name(invalid).is_err(),
                "expected invalid Windows name: {invalid:?}"
            );
        }

        assert_eq!(
            find_case_insensitive_conflict(&["Readme.md", "src", "readme.md"]),
            Some(("Readme.md".into(), "readme.md".into()))
        );
        assert_eq!(
            find_case_insensitive_conflict(&["Readme.md", "src", "LICENSE"]),
            None
        );
    }

    #[test]
    fn untrusted_labels_cannot_emit_control_sequences() {
        assert_eq!(
            display_text("中文\x1b]52;secret\x07\n\u{202e}"),
            "中文\\u{1b}]52;secret\\u{7}\\n\\u{202e}"
        );
    }
}
