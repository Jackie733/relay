//! Minimal SFTP v3 wire codec (draft-ietf-secsh-filexfer-02 + OpenSSH extensions).
//! All paths and filenames are encoded and decoded as raw byte slices without UTF-8 coercion.

use common::{FileKind, FsError, FsResult, Metadata, display_text};

pub const SFTP_VERSION: u32 = 3;

/// Hard upper bound on a single SFTP frame payload to prevent malicious length OOM.
pub const MAX_PACKET_LEN: u32 = 1024 * 1024;
/// Maximum handle length accepted from a server (SFTP v3 spec caps handles at 256 bytes).
pub const MAX_HANDLE_LEN: usize = 256;
/// Maximum number of entries allowed in a single `SSH_FXP_NAME` response packet.
pub const MAX_NAME_PACKET_ENTRIES: u32 = 4096;

pub const SSH_FXP_INIT: u8 = 1;
pub const SSH_FXP_VERSION: u8 = 2;
pub const SSH_FXP_OPEN: u8 = 3;
pub const SSH_FXP_CLOSE: u8 = 4;
pub const SSH_FXP_READ: u8 = 5;
pub const SSH_FXP_WRITE: u8 = 6;
pub const SSH_FXP_LSTAT: u8 = 7;
pub const SSH_FXP_FSTAT: u8 = 8;
pub const SSH_FXP_SETSTAT: u8 = 9;
pub const SSH_FXP_FSETSTAT: u8 = 10;
pub const SSH_FXP_OPENDIR: u8 = 11;
pub const SSH_FXP_READDIR: u8 = 12;
pub const SSH_FXP_REMOVE: u8 = 13;
pub const SSH_FXP_MKDIR: u8 = 14;
pub const SSH_FXP_RMDIR: u8 = 15;
pub const SSH_FXP_REALPATH: u8 = 16;
pub const SSH_FXP_STAT: u8 = 17;
pub const SSH_FXP_RENAME: u8 = 18;
pub const SSH_FXP_STATUS: u8 = 101;
pub const SSH_FXP_HANDLE: u8 = 102;
pub const SSH_FXP_DATA: u8 = 103;
pub const SSH_FXP_NAME: u8 = 104;
pub const SSH_FXP_ATTRS: u8 = 105;
pub const SSH_FXP_EXTENDED: u8 = 200;
pub const SSH_FXP_EXTENDED_REPLY: u8 = 201;

pub const SSH_FXF_READ: u32 = 0x0000_0001;
pub const SSH_FXF_WRITE: u32 = 0x0000_0002;
pub const SSH_FXF_APPEND: u32 = 0x0000_0004;
pub const SSH_FXF_CREAT: u32 = 0x0000_0008;
pub const SSH_FXF_TRUNC: u32 = 0x0000_0010;
pub const SSH_FXF_EXCL: u32 = 0x0000_0020;

pub const SSH_FILEXFER_ATTR_SIZE: u32 = 0x0000_0001;
pub const SSH_FILEXFER_ATTR_UIDGID: u32 = 0x0000_0002;
pub const SSH_FILEXFER_ATTR_PERMISSIONS: u32 = 0x0000_0004;
pub const SSH_FILEXFER_ATTR_ACMODTIME: u32 = 0x0000_0008;
pub const SSH_FILEXFER_ATTR_EXTENDED: u32 = 0x8000_0000;

pub const SSH_FX_OK: u32 = 0;
pub const SSH_FX_EOF: u32 = 1;
pub const SSH_FX_NO_SUCH_FILE: u32 = 2;
pub const SSH_FX_PERMISSION_DENIED: u32 = 3;
pub const SSH_FX_FAILURE: u32 = 4;
pub const SSH_FX_BAD_MESSAGE: u32 = 5;
pub const SSH_FX_NO_CONNECTION: u32 = 6;
pub const SSH_FX_CONNECTION_LOST: u32 = 7;
pub const SSH_FX_OP_UNSUPPORTED: u32 = 8;

pub const EXT_POSIX_RENAME: &str = "posix-rename@openssh.com";
pub const EXT_FSYNC: &str = "fsync@openssh.com";
pub const EXT_STATVFS: &str = "statvfs@openssh.com";
pub const EXT_LIMITS: &str = "limits@openssh.com";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FileAttrs {
    pub size: Option<u64>,
    pub uid_gid: Option<(u32, u32)>,
    pub permissions: Option<u32>,
    pub acmodtime: Option<(u32, u32)>,
}

impl FileAttrs {
    pub fn with_mode(mode: u32) -> Self {
        Self {
            permissions: Some(mode & 0o0777),
            ..Self::default()
        }
    }

    pub fn encode(&self, out: &mut PacketEncoder) {
        let mut flags = 0u32;
        if self.size.is_some() {
            flags |= SSH_FILEXFER_ATTR_SIZE;
        }
        if self.uid_gid.is_some() {
            flags |= SSH_FILEXFER_ATTR_UIDGID;
        }
        if self.permissions.is_some() {
            flags |= SSH_FILEXFER_ATTR_PERMISSIONS;
        }
        if self.acmodtime.is_some() {
            flags |= SSH_FILEXFER_ATTR_ACMODTIME;
        }
        out.put_u32(flags);
        if let Some(size) = self.size {
            out.put_u64(size);
        }
        if let Some((uid, gid)) = self.uid_gid {
            out.put_u32(uid);
            out.put_u32(gid);
        }
        if let Some(permissions) = self.permissions {
            out.put_u32(permissions);
        }
        if let Some((atime, mtime)) = self.acmodtime {
            out.put_u32(atime);
            out.put_u32(mtime);
        }
    }

    pub fn decode(dec: &mut PacketDecoder<'_>) -> FsResult<Self> {
        let flags = dec.get_u32()?;
        let size = if flags & SSH_FILEXFER_ATTR_SIZE != 0 {
            Some(dec.get_u64()?)
        } else {
            None
        };
        let uid_gid = if flags & SSH_FILEXFER_ATTR_UIDGID != 0 {
            Some((dec.get_u32()?, dec.get_u32()?))
        } else {
            None
        };
        let permissions = if flags & SSH_FILEXFER_ATTR_PERMISSIONS != 0 {
            Some(dec.get_u32()?)
        } else {
            None
        };
        let acmodtime = if flags & SSH_FILEXFER_ATTR_ACMODTIME != 0 {
            Some((dec.get_u32()?, dec.get_u32()?))
        } else {
            None
        };
        if flags & SSH_FILEXFER_ATTR_EXTENDED != 0 {
            let count = dec.get_u32()?;
            if count > MAX_NAME_PACKET_ENTRIES {
                return Err(FsError::Protocol(
                    "excessive SFTP extended attribute count".into(),
                ));
            }
            for _ in 0..count {
                let _ = dec.get_bytes()?;
                let _ = dec.get_bytes()?;
            }
        }
        Ok(Self {
            size,
            uid_gid,
            permissions,
            acmodtime,
        })
    }

    pub fn to_metadata(&self, longname: Option<&[u8]>) -> Metadata {
        let kind = match self.permissions {
            Some(mode) if mode & 0o170000 != 0 => FileKind::from_unix_mode(mode),
            _ => match longname.and_then(|ln| ln.first()).copied() {
                Some(b'd') => FileKind::Directory,
                Some(b'l') => FileKind::Symlink,
                Some(b'-') => FileKind::File,
                _ => FileKind::Other,
            },
        };
        Metadata {
            kind,
            size: self.size.unwrap_or(0),
            permissions: self.permissions,
            atime: self.acmodtime.map(|(a, _)| u64::from(a)),
            mtime: self.acmodtime.map(|(_, m)| u64::from(m)),
            uid: self.uid_gid.map(|(u, _)| u),
            gid: self.uid_gid.map(|(_, g)| g),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ServerVersion {
    pub version: u32,
    pub posix_rename: bool,
    pub fsync: bool,
    pub statvfs: bool,
    pub limits: bool,
}

impl ServerVersion {
    pub fn decode(payload: &[u8]) -> FsResult<Self> {
        let mut dec = PacketDecoder::new(payload);
        let packet_type = dec.get_u8()?;
        if packet_type != SSH_FXP_VERSION {
            return Err(FsError::Protocol(format!(
                "expected SSH_FXP_VERSION (2), got {packet_type}"
            )));
        }
        let version = dec.get_u32()?;
        if version != SFTP_VERSION {
            return Err(FsError::Protocol(format!(
                "unsupported SFTP version {version}; expected {SFTP_VERSION}"
            )));
        }
        let mut info = Self {
            version,
            ..Self::default()
        };
        while !dec.is_empty() {
            let name = dec.get_bytes()?;
            let data = dec.get_bytes()?;
            match (name, data) {
                (b"posix-rename@openssh.com", b"1") => info.posix_rename = true,
                (b"fsync@openssh.com", b"1") => info.fsync = true,
                (b"statvfs@openssh.com", b"2") => info.statvfs = true,
                (b"limits@openssh.com", b"1") => info.limits = true,
                _ => {}
            }
        }
        Ok(info)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServerLimits {
    pub max_packet_len: u64,
    pub max_read_len: u64,
    pub max_write_len: u64,
    pub max_open_handles: u64,
}

impl ServerLimits {
    pub fn decode(dec: &mut PacketDecoder<'_>) -> FsResult<Self> {
        let limits = Self {
            max_packet_len: dec.get_u64()?,
            max_read_len: dec.get_u64()?,
            max_write_len: dec.get_u64()?,
            max_open_handles: dec.get_u64()?,
        };
        dec.finish()?;
        Ok(limits)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatVfsReply {
    pub bsize: u64,
    pub frsize: u64,
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub favail: u64,
    pub fsid: u64,
    pub flag: u64,
    pub namemax: u64,
}

impl StatVfsReply {
    pub fn decode(dec: &mut PacketDecoder<'_>) -> FsResult<Self> {
        let reply = Self {
            bsize: dec.get_u64()?,
            frsize: dec.get_u64()?,
            blocks: dec.get_u64()?,
            bfree: dec.get_u64()?,
            bavail: dec.get_u64()?,
            files: dec.get_u64()?,
            ffree: dec.get_u64()?,
            favail: dec.get_u64()?,
            fsid: dec.get_u64()?,
            flag: dec.get_u64()?,
            namemax: dec.get_u64()?,
        };
        dec.finish()?;
        Ok(reply)
    }

    pub fn available_bytes(&self) -> u64 {
        let unit = if self.frsize > 0 {
            self.frsize
        } else {
            self.bsize
        };
        self.bavail.saturating_mul(unit)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NameEntry {
    pub filename: Vec<u8>,
    pub longname: Vec<u8>,
    pub attrs: FileAttrs,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResponseBody {
    Status { code: u32, message: String },
    Handle(Vec<u8>),
    Data(Vec<u8>),
    Names(Vec<NameEntry>),
    Attrs(FileAttrs),
    ExtendedReply(Vec<u8>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsePacket {
    pub request_id: u32,
    pub body: ResponseBody,
}

impl ResponsePacket {
    pub fn decode(payload: &[u8]) -> FsResult<Self> {
        let mut dec = PacketDecoder::new(payload);
        let packet_type = dec.get_u8()?;
        let request_id = dec.get_u32()?;
        let body = match packet_type {
            SSH_FXP_STATUS => {
                let code = dec.get_u32()?;
                // SFTP v3 status includes message + language tag; some quirky servers omit them.
                let message = if !dec.is_empty() {
                    let raw = dec.get_bytes()?;
                    if !dec.is_empty() {
                        let _lang = dec.get_bytes()?;
                    }
                    display_text(&String::from_utf8_lossy(raw))
                } else {
                    String::new()
                };
                dec.finish()?;
                ResponseBody::Status { code, message }
            }
            SSH_FXP_HANDLE => {
                let handle = dec.get_bytes()?;
                if handle.is_empty() || handle.len() > MAX_HANDLE_LEN {
                    return Err(FsError::Protocol(format!(
                        "invalid SFTP handle length {}",
                        handle.len()
                    )));
                }
                dec.finish()?;
                ResponseBody::Handle(handle.to_vec())
            }
            SSH_FXP_DATA => {
                let data = dec.get_bytes()?.to_vec();
                dec.finish()?;
                ResponseBody::Data(data)
            }
            SSH_FXP_NAME => {
                let count = dec.get_u32()?;
                if count > MAX_NAME_PACKET_ENTRIES {
                    return Err(FsError::Protocol(format!(
                        "SSH_FXP_NAME entry count {count} exceeds limit"
                    )));
                }
                let mut entries = Vec::with_capacity((count as usize).min(256));
                for _ in 0..count {
                    let filename = dec.get_bytes()?.to_vec();
                    let longname = dec.get_bytes()?.to_vec();
                    let attrs = FileAttrs::decode(&mut dec)?;
                    entries.push(NameEntry {
                        filename,
                        longname,
                        attrs,
                    });
                }
                dec.finish()?;
                ResponseBody::Names(entries)
            }
            SSH_FXP_ATTRS => {
                let attrs = FileAttrs::decode(&mut dec)?;
                dec.finish()?;
                ResponseBody::Attrs(attrs)
            }
            SSH_FXP_EXTENDED_REPLY => {
                let remaining = dec.remaining().to_vec();
                ResponseBody::ExtendedReply(remaining)
            }
            other => {
                return Err(FsError::Protocol(format!(
                    "unexpected SFTP packet type {other}"
                )));
            }
        };
        Ok(Self { request_id, body })
    }
}

pub fn status_to_error(code: u32, message: String) -> FsError {
    match code {
        SSH_FX_NO_SUCH_FILE => FsError::NotFound,
        SSH_FX_PERMISSION_DENIED => FsError::PermissionDenied,
        SSH_FX_OP_UNSUPPORTED => FsError::Unsupported,
        SSH_FX_BAD_MESSAGE => FsError::Protocol(if message.is_empty() {
            "server reported SSH_FX_BAD_MESSAGE".into()
        } else {
            message
        }),
        SSH_FX_NO_CONNECTION | SSH_FX_CONNECTION_LOST => FsError::ConnectionLost,
        SSH_FX_FAILURE => {
            let lower = message.to_ascii_lowercase();
            if lower.contains("no space") || lower.contains("disk quota") {
                FsError::NoSpace
            } else {
                FsError::Failure {
                    server_message: if message.is_empty() {
                        "SSH_FX_FAILURE".into()
                    } else {
                        message
                    },
                }
            }
        }
        other => FsError::Protocol(format!("unexpected SFTP status code {other}: {message}")),
    }
}

pub struct PacketEncoder {
    buf: Vec<u8>,
}

impl PacketEncoder {
    pub fn new(packet_type: u8) -> Self {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&[0, 0, 0, 0, packet_type]);
        Self { buf }
    }

    pub fn with_request(packet_type: u8, request_id: u32) -> Self {
        let mut enc = Self::new(packet_type);
        enc.put_u32(request_id);
        enc
    }

    pub fn put_u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    pub fn put_u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub fn put_u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub fn put_bytes(&mut self, bytes: &[u8]) {
        self.put_u32(bytes.len() as u32);
        self.buf.extend_from_slice(bytes);
    }

    pub fn put_str(&mut self, text: &str) {
        self.put_bytes(text.as_bytes());
    }

    pub fn finish(mut self) -> Vec<u8> {
        let payload_len = (self.buf.len() - 4) as u32;
        self.buf[0..4].copy_from_slice(&payload_len.to_be_bytes());
        self.buf
    }
}

pub struct PacketDecoder<'a> {
    buf: &'a [u8],
}

impl<'a> PacketDecoder<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn remaining(&self) -> &'a [u8] {
        self.buf
    }

    pub fn finish(&self) -> FsResult<()> {
        if !self.buf.is_empty() {
            return Err(FsError::Protocol(format!(
                "trailing {} bytes in SFTP packet",
                self.buf.len()
            )));
        }
        Ok(())
    }

    pub fn get_u8(&mut self) -> FsResult<u8> {
        let (&first, rest) = self
            .buf
            .split_first()
            .ok_or_else(|| FsError::Protocol("truncated SFTP u8".into()))?;
        self.buf = rest;
        Ok(first)
    }

    pub fn get_u32(&mut self) -> FsResult<u32> {
        if self.buf.len() < 4 {
            return Err(FsError::Protocol("truncated SFTP u32".into()));
        }
        let (head, rest) = self.buf.split_at(4);
        self.buf = rest;
        Ok(u32::from_be_bytes([head[0], head[1], head[2], head[3]]))
    }

    pub fn get_u64(&mut self) -> FsResult<u64> {
        if self.buf.len() < 8 {
            return Err(FsError::Protocol("truncated SFTP u64".into()));
        }
        let (head, rest) = self.buf.split_at(8);
        self.buf = rest;
        Ok(u64::from_be_bytes([
            head[0], head[1], head[2], head[3], head[4], head[5], head[6], head[7],
        ]))
    }

    pub fn get_bytes(&mut self) -> FsResult<&'a [u8]> {
        let len = self.get_u32()? as usize;
        if self.buf.len() < len {
            return Err(FsError::Protocol(format!(
                "truncated SFTP byte string: needed {len}, got {}",
                self.buf.len()
            )));
        }
        let (data, rest) = self.buf.split_at(len);
        self.buf = rest;
        Ok(data)
    }
}
