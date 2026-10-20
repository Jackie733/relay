use std::path::PathBuf;

use crate::{FsError, FsResult};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct HostId(pub String);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Location {
    Local,
    Remote(HostId),
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

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum FsPath {
    Local(PathBuf),
    Remote(RemotePath),
}

impl FsPath {
    pub fn display_name(&self) -> String {
        match self {
            Self::Local(path) => display_text(&path.to_string_lossy()),
            Self::Remote(path) => display_text(&String::from_utf8_lossy(path.as_bytes())),
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
                || matches!(ch, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
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
    fn remote_paths_preserve_non_utf8_bytes() {
        let bytes = b"/tmp/\xff name".to_vec();
        let path = RemotePath::new(bytes.clone()).unwrap();
        assert_eq!(path.as_bytes(), bytes);
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
    fn untrusted_labels_cannot_emit_control_sequences() {
        assert_eq!(
            display_text("中文\x1b]52;secret\x07\n\u{202e}"),
            "中文\\u{1b}]52;secret\\u{7}\\n\\u{202e}"
        );
    }
}
