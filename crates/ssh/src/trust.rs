//! OpenSSH-format host trust, merged across user, global and Relay sources.

use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use russh::keys::{HashAlg, PublicKey};
use sha1::Sha1;
use thiserror::Error;

const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

#[cfg(test)]
#[path = "trust_tests.rs"]
mod tests;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostKey {
    pub algorithm: String,
    pub fingerprint: String,
    encoded: String,
}

impl HostKey {
    pub fn from_public_key(key: &PublicKey) -> Result<Self, TrustError> {
        Ok(Self {
            algorithm: key.algorithm().as_str().to_owned(),
            fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
            encoded: STANDARD.encode(key.to_bytes().map_err(|_| TrustError::InvalidKey)?),
        })
    }

    pub fn openssh(&self) -> String {
        format!("{} {}", self.algorithm, self.encoded)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HostKeyPolicy {
    #[default]
    Ask,
    /// Equivalent to StrictHostKeyChecking=yes: no new trust, even with a prompt.
    Strict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TrustDecision {
    Trusted,
    Unknown,
    AlgorithmChanged,
    Changed,
    Revoked,
}

#[derive(Clone, Debug)]
pub struct TrustReport {
    pub decision: TrustDecision,
    pub known_algorithms: Vec<String>,
    pub ignored_certificate_authority: bool,
}

#[derive(Debug, Error)]
pub enum TrustError {
    #[error("invalid host name or port for host-key lookup")]
    InvalidTarget,
    #[error("invalid host public key")]
    InvalidKey,
    #[error("invalid known_hosts entry at line {line}; repair the trust source before connecting")]
    InvalidEntry { line: usize },
    #[error("known_hosts source exceeds the 16 MiB limit")]
    TooLarge,
    #[error("Relay trust file must be a regular file with mode 0600")]
    InsecureStore,
    #[error("server host key changed; connection refused")]
    Changed,
    #[error("server host key is revoked; connection refused")]
    Revoked,
    #[error("host trust I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// These paths are explicit: tests never read or write a developer's trust files.
/// Methods do local file I/O; async callers must use a blocking worker.
#[derive(Clone, Debug)]
pub struct TrustStore {
    pub sources: Vec<PathBuf>,
    pub relay_file: PathBuf,
}

impl TrustStore {
    pub fn for_user(home: &Path, config_dir: &Path) -> Self {
        Self {
            sources: vec![
                home.join(".ssh/known_hosts"),
                home.join(".ssh/known_hosts2"),
                PathBuf::from("/etc/ssh/ssh_known_hosts"),
                PathBuf::from("/etc/ssh/ssh_known_hosts2"),
            ],
            relay_file: config_dir.join("relay/known_hosts"),
        }
    }

    pub fn lookup_name(host: &str, port: u16) -> Result<String, TrustError> {
        if port == 0
            || host.is_empty()
            || host
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control() || "*,?!|#[]".contains(ch))
        {
            return Err(TrustError::InvalidTarget);
        }
        let host = host.to_ascii_lowercase();
        Ok(if port == 22 {
            host
        } else {
            format!("[{host}]:{port}")
        })
    }

    pub fn assess(&self, name: &str, key: &HostKey) -> Result<TrustReport, TrustError> {
        let mut records = self.read_sources()?;
        records.push(read_optional(&self.relay_file)?);
        assess(&records, name, key)
    }

    pub fn known_algorithms(&self, name: &str) -> Result<Vec<String>, TrustError> {
        let mut records = self.read_sources()?;
        records.push(read_optional(&self.relay_file)?);
        let mut algorithms = BTreeSet::new();
        for text in &records {
            for entry in entries(text)? {
                if entry.marker.is_none() && matches_host(entry.hosts, name)? {
                    algorithms.insert(entry.algorithm.to_owned());
                }
            }
        }
        Ok(algorithms.into_iter().collect())
    }

    /// Call only after an explicit confirmation of this exact key. Lock and
    /// recheck all sources so concurrent confirmations cannot overwrite trust.
    pub fn remember(&self, name: &str, key: &HostKey) -> Result<(), TrustError> {
        // A lookup name can contain IPv6 colons and port brackets, but no
        // OpenSSH pattern/hash syntax or whitespace that could inject a record.
        if name.is_empty()
            || name
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control() || "*,?!|#".contains(ch))
        {
            return Err(TrustError::InvalidTarget);
        }
        if let Some(parent) = self.relay_file.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Ok(metadata) = fs::symlink_metadata(&self.relay_file)
            && !metadata.is_file()
        {
            return Err(TrustError::InsecureStore);
        }
        let mut options = OpenOptions::new();
        options.read(true).append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&self.relay_file)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(TrustError::InsecureStore);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.mode() & 0o777 != 0o600 {
                return Err(TrustError::InsecureStore);
            }
        }
        file.lock()?;
        let mut records = self.read_sources()?;
        let existing = read_limited(&mut file)?;
        records.push(existing.clone());
        match assess(&records, name, key)?.decision {
            TrustDecision::Changed => return Err(TrustError::Changed),
            TrustDecision::Revoked => return Err(TrustError::Revoked),
            TrustDecision::Trusted => return Ok(()),
            TrustDecision::Unknown | TrustDecision::AlgorithmChanged => {}
        }
        if !existing.is_empty() && !existing.ends_with('\n') {
            file.write_all(b"\n")?;
        }
        writeln!(file, "{name} {}", key.openssh())?;
        file.sync_all()?;
        Ok(())
    }

    fn read_sources(&self) -> Result<Vec<String>, TrustError> {
        self.sources
            .iter()
            .filter(|path| **path != self.relay_file)
            .map(|path| read_optional(path))
            .collect()
    }
}

fn read_optional(path: &Path) -> Result<String, TrustError> {
    match File::open(path) {
        Ok(mut file) => read_limited(&mut file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

fn read_limited(file: &mut File) -> Result<String, TrustError> {
    let mut text = String::new();
    file.take(MAX_FILE_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err(TrustError::TooLarge);
    }
    Ok(text)
}

struct Entry<'a> {
    marker: Option<&'a str>,
    hosts: &'a str,
    algorithm: &'a str,
    encoded: &'a str,
}

fn entries(text: &str) -> Result<Vec<Entry<'_>>, TrustError> {
    let mut result = Vec::new();
    for (line, text) in text.lines().enumerate() {
        let text = text.trim();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        let invalid = || TrustError::InvalidEntry { line: line + 1 };
        let mut fields = text.split_whitespace();
        let first = fields.next().ok_or_else(invalid)?;
        let (marker, hosts) = if first.starts_with('@') {
            if !matches!(first, "@revoked" | "@cert-authority") {
                return Err(invalid());
            }
            (Some(first), fields.next().ok_or_else(invalid)?)
        } else {
            (None, first)
        };
        let algorithm = fields.next().ok_or_else(invalid)?;
        let encoded = fields.next().ok_or_else(invalid)?;
        let bytes = STANDARD.decode(encoded).map_err(|_| invalid())?;
        let length = bytes.get(..4).ok_or_else(invalid)?;
        let length = u32::from_be_bytes(length.try_into().map_err(|_| invalid())?) as usize;
        if bytes.get(4..4usize.saturating_add(length)) != Some(algorithm.as_bytes()) {
            return Err(invalid());
        }
        result.push(Entry {
            marker,
            hosts,
            algorithm,
            encoded,
        });
    }
    Ok(result)
}

fn assess(texts: &[String], name: &str, key: &HostKey) -> Result<TrustReport, TrustError> {
    let mut known = BTreeSet::new();
    let (mut matched, mut changed, mut revoked, mut ca) = (false, false, false, false);
    for text in texts {
        for entry in entries(text)? {
            if !matches_host(entry.hosts, name)? {
                continue;
            }
            match entry.marker {
                Some("@revoked") => revoked |= entry.encoded == key.encoded,
                Some("@cert-authority") => ca = true,
                _ => {
                    known.insert(entry.algorithm.to_owned());
                    if entry.algorithm == key.algorithm {
                        if entry.encoded == key.encoded {
                            matched = true;
                        } else {
                            changed = true;
                        }
                    }
                }
            }
        }
    }
    let decision = if revoked {
        TrustDecision::Revoked
    } else if changed {
        TrustDecision::Changed
    } else if matched {
        TrustDecision::Trusted
    } else if !known.is_empty() {
        TrustDecision::AlgorithmChanged
    } else {
        TrustDecision::Unknown
    };
    Ok(TrustReport {
        decision,
        known_algorithms: known.into_iter().collect(),
        ignored_certificate_authority: ca,
    })
}

fn matches_host(patterns: &str, name: &str) -> Result<bool, TrustError> {
    if patterns.starts_with('|') {
        let fields: Vec<_> = patterns.split('|').collect();
        if fields.len() != 4 || fields[1] != "1" {
            return Err(TrustError::InvalidEntry { line: 0 });
        }
        let salt = STANDARD
            .decode(fields[2])
            .map_err(|_| TrustError::InvalidKey)?;
        let hash = STANDARD
            .decode(fields[3])
            .map_err(|_| TrustError::InvalidKey)?;
        if salt.len() != 20 || hash.len() != 20 {
            return Err(TrustError::InvalidEntry { line: 0 });
        }
        let mut mac = Hmac::<Sha1>::new_from_slice(&salt).map_err(|_| TrustError::InvalidKey)?;
        mac.update(name.as_bytes());
        return Ok(mac.verify_slice(&hash).is_ok());
    }
    let mut matched = false;
    for pattern in patterns.split(',') {
        let (negative, pattern) = pattern
            .strip_prefix('!')
            .map_or((false, pattern), |p| (true, p));
        if wildcard(pattern.as_bytes(), name.as_bytes()) {
            if negative {
                return Ok(false);
            }
            matched = true;
        }
    }
    Ok(matched)
}

// Greedy wildcard matching with a single backtracking point; no regex or
// exponential recursive expansion for hostile/unusually long patterns.
fn wildcard(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t, mut star, mut retry) = (0, 0, None, 0);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p].eq_ignore_ascii_case(&text[t])) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = t;
        } else if let Some(index) = star {
            retry += 1;
            t = retry;
            p = index + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}
