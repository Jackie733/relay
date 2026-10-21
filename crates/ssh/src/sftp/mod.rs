//! Self-developed minimal SFTP v3 client (`RemoteFs`) over russh channels.
//! Preserves raw POSIX path bytes, probes OpenSSH extensions (`posix-rename`,
//! `fsync`, `statvfs`, `limits`), and bounds in-flight read/write pipelines.

pub mod client;
pub mod wire;

#[cfg(test)]
mod regression_tests;

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use async_trait::async_trait;
use common::{
    Capabilities, DirEntry, FileReader, FileSystem, FileWriter, FsError, FsPath, FsResult,
    Location, Metadata, MetadataPatch, RemotePath, RenameMode, WriteOptions,
};
use russh::ChannelMsg;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    time::timeout,
};

use crate::connection::{ConnectError, Connection};
pub use client::SftpClient;

#[derive(Clone)]
pub struct RemoteFs {
    location: Location,
    client: SftpClient,
}

impl RemoteFs {
    pub fn new(location: Location, client: SftpClient) -> Self {
        Self { location, client }
    }

    pub fn client(&self) -> &SftpClient {
        &self.client
    }
}

fn extract_remote_path(path: &FsPath) -> FsResult<&RemotePath> {
    match path {
        FsPath::Remote(remote) => Ok(remote),
        FsPath::Local(_) => Err(FsError::PathLocationMismatch),
    }
}

#[async_trait]
impl FileSystem for RemoteFs {
    fn location(&self) -> &Location {
        &self.location
    }

    fn capabilities(&self) -> &Capabilities {
        self.client.capabilities()
    }

    async fn read_dir(&self, path: &FsPath) -> FsResult<Vec<DirEntry>> {
        self.client.read_dir(extract_remote_path(path)?).await
    }

    async fn lstat(&self, path: &FsPath) -> FsResult<Metadata> {
        self.client.lstat(extract_remote_path(path)?).await
    }

    async fn stat(&self, path: &FsPath) -> FsResult<Metadata> {
        self.client.stat(extract_remote_path(path)?).await
    }

    async fn canonicalize(&self, path: &FsPath) -> FsResult<FsPath> {
        let resolved = self.client.canonicalize(extract_remote_path(path)?).await?;
        Ok(FsPath::Remote(resolved))
    }

    async fn create_dir(&self, path: &FsPath, mode: u32) -> FsResult<()> {
        self.client
            .create_dir(extract_remote_path(path)?, mode)
            .await
    }

    async fn remove_file(&self, path: &FsPath) -> FsResult<()> {
        self.client.remove_file(extract_remote_path(path)?).await
    }

    async fn remove_dir(&self, path: &FsPath) -> FsResult<()> {
        self.client.remove_dir(extract_remote_path(path)?).await
    }

    async fn rename(&self, from: &FsPath, to: &FsPath, mode: RenameMode) -> FsResult<()> {
        self.client
            .rename(extract_remote_path(from)?, extract_remote_path(to)?, mode)
            .await
    }

    async fn open_read(&self, path: &FsPath) -> FsResult<Box<dyn FileReader>> {
        self.client.open_read(extract_remote_path(path)?).await
    }

    async fn open_write(&self, path: &FsPath, opts: WriteOptions) -> FsResult<Box<dyn FileWriter>> {
        self.client
            .open_write(extract_remote_path(path)?, opts)
            .await
    }

    async fn set_metadata(&self, path: &FsPath, patch: &MetadataPatch) -> FsResult<()> {
        self.client
            .set_metadata(extract_remote_path(path)?, patch)
            .await
    }

    async fn free_space(&self, path: &FsPath) -> FsResult<Option<u64>> {
        self.client.free_space(extract_remote_path(path)?).await
    }
}

/// Prepends any bytes received on the SSH channel before `ChannelMsg::Success`
/// to the `ChannelStream` so `into_stream()` never loses early subsystem bytes.
struct PrefixedStream<S> {
    prefix: Vec<u8>,
    offset: usize,
    inner: S,
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.offset < self.prefix.len() {
            let remaining = &self.prefix[self.offset..];
            let take = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..take]);
            self.offset += take;
            if self.offset == self.prefix.len() {
                self.prefix.clear();
                self.offset = 0;
            }
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl Connection {
    /// Opens an SFTP v3 subsystem channel and wraps it in a [`RemoteFs`].
    pub async fn open_sftp(&self) -> Result<RemoteFs, ConnectError> {
        let mut channel = self.open_session().await?;
        let limit = self.network_timeout();
        channel.request_subsystem(true, "sftp").await?;
        let prefix = timeout(limit, async {
            let mut early = Vec::new();
            loop {
                match channel.wait().await {
                    Some(ChannelMsg::Success) => return Ok(early),
                    Some(ChannelMsg::Failure) | Some(ChannelMsg::Close) | None => {
                        return Err(ConnectError::ChannelRequest("sftp"));
                    }
                    Some(ChannelMsg::Data { data }) => {
                        early.extend_from_slice(&data);
                    }
                    Some(_) => {}
                }
            }
        })
        .await
        .map_err(|_| ConnectError::NetworkTimeout)??;

        let stream = PrefixedStream {
            prefix,
            offset: 0,
            inner: channel.into_stream(),
        };
        let client = SftpClient::from_stream(stream, limit)
            .await
            .map_err(|_| ConnectError::ChannelRequest("sftp-init"))?;
        Ok(RemoteFs::new(
            Location::Remote(self.host().id.clone()),
            client,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use common::{
        FileKind, FileSystem, FsError, FsPath, HostId, Location, MAX_DIR_ENTRIES, RemotePath,
        RenameMode, SyncOutcome, WriteOptions,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    use super::{
        RemoteFs, SftpClient,
        wire::{
            EXT_FSYNC, EXT_LIMITS, EXT_POSIX_RENAME, EXT_STATVFS, FileAttrs, MAX_PACKET_LEN,
            PacketDecoder, PacketEncoder, SFTP_VERSION, SSH_FX_EOF, SSH_FX_OK, SSH_FXP_ATTRS,
            SSH_FXP_CLOSE, SSH_FXP_DATA, SSH_FXP_EXTENDED, SSH_FXP_EXTENDED_REPLY, SSH_FXP_HANDLE,
            SSH_FXP_INIT, SSH_FXP_LSTAT, SSH_FXP_NAME, SSH_FXP_OPEN, SSH_FXP_OPENDIR, SSH_FXP_READ,
            SSH_FXP_READDIR, SSH_FXP_STATUS, SSH_FXP_VERSION, SSH_FXP_WRITE,
        },
    };

    async fn recv_packet(stream: &mut (impl AsyncReadExt + Unpin)) -> (u8, PacketDecoder<'static>) {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await.unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload).await.unwrap();
        let leaked: &'static [u8] = Box::leak(payload.into_boxed_slice());
        let mut dec = PacketDecoder::new(leaked);
        let ptype = dec.get_u8().unwrap();
        (ptype, dec)
    }

    async fn send_version(
        stream: &mut (impl AsyncReadExt + AsyncWriteExt + Unpin),
        exts: &[(&str, &str)],
    ) {
        let (ptype, mut dec) = recv_packet(stream).await;
        assert_eq!(ptype, SSH_FXP_INIT);
        assert_eq!(dec.get_u32().unwrap(), SFTP_VERSION);
        let mut ver = PacketEncoder::new(SSH_FXP_VERSION);
        ver.put_u32(SFTP_VERSION);
        for (name, data) in exts {
            ver.put_str(name);
            ver.put_str(data);
        }
        stream.write_all(&ver.finish()).await.unwrap();
        stream.flush().await.unwrap();
    }

    #[tokio::test]
    async fn quirky_server_probes_capabilities_and_rejects_unsupported_atomic_replace() {
        // 1. Quirky server without posix-rename, fsync, statvfs, or limits.
        let (client_io, mut server_io) = duplex(64 * 1024);
        let server = tokio::spawn(async move {
            send_version(&mut server_io, &[]).await;
            // Handle OPEN -> WRITE -> CLOSE (no fsync sent since unsupported).
            let (ptype, mut dec) = recv_packet(&mut server_io).await;
            assert_eq!(ptype, SSH_FXP_OPEN);
            let req_id = dec.get_u32().unwrap();
            let mut h = PacketEncoder::with_request(SSH_FXP_HANDLE, req_id);
            h.put_bytes(b"h1");
            server_io.write_all(&h.finish()).await.unwrap();

            let (ptype, mut dec) = recv_packet(&mut server_io).await;
            assert_eq!(ptype, SSH_FXP_WRITE);
            let req_id = dec.get_u32().unwrap();
            let mut ok = PacketEncoder::with_request(SSH_FXP_STATUS, req_id);
            ok.put_u32(SSH_FX_OK);
            ok.put_str("ok");
            ok.put_str("en");
            server_io.write_all(&ok.finish()).await.unwrap();

            let (ptype, mut dec) = recv_packet(&mut server_io).await;
            assert_eq!(ptype, SSH_FXP_CLOSE);
            let req_id = dec.get_u32().unwrap();
            let mut ok = PacketEncoder::with_request(SSH_FXP_STATUS, req_id);
            ok.put_u32(SSH_FX_OK);
            ok.put_str("ok");
            ok.put_str("en");
            server_io.write_all(&ok.finish()).await.unwrap();
        });

        let client = SftpClient::from_stream(client_io, Duration::from_secs(5))
            .await
            .unwrap();
        let fs = RemoteFs::new(Location::Remote(HostId("ssh:quirky".into())), client);
        assert!(!fs.capabilities().posix_rename);
        assert!(!fs.capabilities().fsync);
        assert!(!fs.capabilities().statvfs);

        let from = FsPath::remote(b"/tmp/new.part".to_vec()).unwrap();
        let to = FsPath::remote(b"/tmp/existing.txt".to_vec()).unwrap();
        assert!(matches!(
            fs.rename(&from, &to, RenameMode::AtomicReplace).await,
            Err(FsError::Unsupported)
        ));
        assert_eq!(fs.free_space(&to).await.unwrap(), None);

        let mut writer = fs
            .open_write(&from, WriteOptions::create_new(0o600))
            .await
            .unwrap();
        writer.write_all(b"payload").await.unwrap();
        assert_eq!(writer.sync().await.unwrap(), SyncOutcome::Unsupported);
        writer.close().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn quirky_server_filters_malicious_names_and_preserves_non_utf8_bytes() {
        let (client_io, mut server_io) = duplex(64 * 1024);
        let server = tokio::spawn(async move {
            send_version(
                &mut server_io,
                &[
                    (EXT_POSIX_RENAME, "1"),
                    (EXT_FSYNC, "1"),
                    (EXT_STATVFS, "2"),
                    (EXT_LIMITS, "1"),
                ],
            )
            .await;

            // Respond to limits@openssh.com
            let (ptype, mut dec) = recv_packet(&mut server_io).await;
            assert_eq!(ptype, SSH_FXP_EXTENDED);
            let req_id = dec.get_u32().unwrap();
            assert_eq!(dec.get_bytes().unwrap(), EXT_LIMITS.as_bytes());
            let mut reply = PacketEncoder::with_request(SSH_FXP_EXTENDED_REPLY, req_id);
            reply.put_u64(262_144);
            reply.put_u64(131_072);
            reply.put_u64(131_072);
            reply.put_u64(128);
            server_io.write_all(&reply.finish()).await.unwrap();

            // Respond to OPENDIR
            let (ptype, mut dec) = recv_packet(&mut server_io).await;
            assert_eq!(ptype, SSH_FXP_OPENDIR);
            let req_id = dec.get_u32().unwrap();
            let mut h = PacketEncoder::with_request(SSH_FXP_HANDLE, req_id);
            h.put_bytes(b"dir-h");
            server_io.write_all(&h.finish()).await.unwrap();

            // Respond to READDIR with malicious + non-UTF-8 + U+FFFD entries
            let (ptype, mut dec) = recv_packet(&mut server_io).await;
            assert_eq!(ptype, SSH_FXP_READDIR);
            let req_id = dec.get_u32().unwrap();
            let mut names = PacketEncoder::with_request(SSH_FXP_NAME, req_id);
            let items: &[(&[u8], u32)] = &[
                (b".", 0o040755),
                (b"..", 0o040755),
                (b"", 0o100644),
                (b"../../etc/passwd", 0o100644),
                (b"evil/slash", 0o100644),
                (b"nul\0byte", 0o100644),
                (b"raw-\xff-\xfe.bin", 0o100640),
                ("lossy-\u{fffd}.txt".as_bytes(), 0o100644),
            ];
            names.put_u32(items.len() as u32);
            for &(name, mode) in items {
                names.put_bytes(name);
                names.put_bytes(b"-rw-r-----");
                FileAttrs {
                    size: Some(42),
                    uid_gid: Some((1000, 1000)),
                    permissions: Some(mode),
                    acmodtime: Some((10, 20)),
                }
                .encode(&mut names);
            }
            server_io.write_all(&names.finish()).await.unwrap();

            // Second READDIR -> EOF
            let (ptype, mut dec) = recv_packet(&mut server_io).await;
            assert_eq!(ptype, SSH_FXP_READDIR);
            let req_id = dec.get_u32().unwrap();
            let mut eof = PacketEncoder::with_request(SSH_FXP_STATUS, req_id);
            eof.put_u32(SSH_FX_EOF);
            eof.put_str("eof");
            eof.put_str("en");
            server_io.write_all(&eof.finish()).await.unwrap();

            // CLOSE
            let (ptype, mut dec) = recv_packet(&mut server_io).await;
            assert_eq!(ptype, SSH_FXP_CLOSE);
            let req_id = dec.get_u32().unwrap();
            let mut ok = PacketEncoder::with_request(SSH_FXP_STATUS, req_id);
            ok.put_u32(SSH_FX_OK);
            ok.put_str("ok");
            ok.put_str("en");
            server_io.write_all(&ok.finish()).await.unwrap();
        });

        let client = SftpClient::from_stream(client_io, Duration::from_secs(5))
            .await
            .unwrap();
        let fs = RemoteFs::new(Location::Remote(HostId("ssh:quirky".into())), client);
        assert!(fs.capabilities().posix_rename);
        assert!(fs.capabilities().fsync);
        assert!(fs.capabilities().statvfs);
        assert_eq!(fs.capabilities().max_read_size, 131_072);
        assert_eq!(fs.capabilities().max_write_size, 131_072);
        assert_eq!(fs.capabilities().max_open_handles, Some(128));

        let dir = FsPath::remote(b"/srv/data".to_vec()).unwrap();
        let entries = fs.read_dir(&dir).await.unwrap();
        // Only the 2 safe-structured entries remain; all 6 malicious entries were dropped.
        assert_eq!(entries.len(), 2);

        let lossy = &entries[0];
        assert_eq!(lossy.raw_name, "lossy-\u{fffd}.txt".as_bytes());
        assert!(lossy.unsafe_name);
        assert!(matches!(lossy.ensure_mutable(), Err(FsError::UnsafeName)));
        assert!(matches!(
            fs.remove_file(&lossy.path).await,
            Err(FsError::UnsafeName)
        ));

        let raw = &entries[1];
        assert_eq!(raw.raw_name, b"raw-\xff-\xfe.bin");
        assert_eq!(raw.kind, FileKind::File);
        assert_eq!(raw.metadata.size, 42);
        assert!(!raw.unsafe_name);
        assert!(raw.ensure_mutable().is_ok());
        assert_eq!(
            raw.path,
            FsPath::Remote(RemotePath::new(b"/srv/data/raw-\xff-\xfe.bin".to_vec()).unwrap())
        );

        server.await.unwrap();
    }

    #[tokio::test]
    async fn quirky_server_oversized_directory_and_abnormal_packets_are_rejected() {
        // 1. Oversized directory (> MAX_DIR_ENTRIES)
        let (client_io, mut server_io) = duplex(256 * 1024);
        tokio::spawn(async move {
            send_version(&mut server_io, &[]).await;
            let (ptype, mut dec) = recv_packet(&mut server_io).await;
            assert_eq!(ptype, SSH_FXP_OPENDIR);
            let req_id = dec.get_u32().unwrap();
            let mut h = PacketEncoder::with_request(SSH_FXP_HANDLE, req_id);
            h.put_bytes(b"dir");
            server_io.write_all(&h.finish()).await.unwrap();

            for batch_idx in 0..11 {
                let (ptype, mut dec) = recv_packet(&mut server_io).await;
                if ptype == SSH_FXP_CLOSE {
                    let req_id = dec.get_u32().unwrap();
                    let mut ok = PacketEncoder::with_request(SSH_FXP_STATUS, req_id);
                    ok.put_u32(SSH_FX_OK);
                    ok.put_str("ok");
                    ok.put_str("en");
                    let _ = server_io.write_all(&ok.finish()).await;
                    break;
                }
                assert_eq!(ptype, SSH_FXP_READDIR);
                let req_id = dec.get_u32().unwrap();
                let mut names = PacketEncoder::with_request(SSH_FXP_NAME, req_id);
                names.put_u32(1000);
                for i in 0..1000 {
                    let name = format!("f-{batch_idx}-{i}");
                    names.put_bytes(name.as_bytes());
                    names.put_bytes(b"-rw-r--r--");
                    FileAttrs::with_mode(0o100644).encode(&mut names);
                }
                server_io.write_all(&names.finish()).await.unwrap();
            }
        });

        let client = SftpClient::from_stream(client_io, Duration::from_secs(5))
            .await
            .unwrap();
        let fs = RemoteFs::new(Location::Remote(HostId("ssh:quirky".into())), client);
        let dir = FsPath::remote(b"/big".to_vec()).unwrap();
        assert!(matches!(
            fs.read_dir(&dir).await,
            Err(FsError::DirectoryTooLarge(MAX_DIR_ENTRIES))
        ));

        // 2. Oversized packet frame header (> MAX_PACKET_LEN) rejected without OOM
        let (client_io, mut server_io) = duplex(64 * 1024);
        tokio::spawn(async move {
            send_version(&mut server_io, &[]).await;
            let (_ptype, _dec) = recv_packet(&mut server_io).await;
            let bogus_len = (MAX_PACKET_LEN + 1).to_be_bytes();
            let _ = server_io.write_all(&bogus_len).await;
        });
        let client = SftpClient::from_stream(client_io, Duration::from_secs(5))
            .await
            .unwrap();
        let fs = RemoteFs::new(Location::Remote(HostId("ssh:quirky".into())), client);
        assert!(matches!(fs.lstat(&dir).await, Err(FsError::Protocol(_))));

        // 3. Unsolicited / mismatched request ID rejected with Protocol error
        let (client_io, mut server_io) = duplex(64 * 1024);
        tokio::spawn(async move {
            send_version(&mut server_io, &[]).await;
            let (ptype, mut dec) = recv_packet(&mut server_io).await;
            assert_eq!(ptype, SSH_FXP_LSTAT);
            let req_id = dec.get_u32().unwrap();
            let mut bad = PacketEncoder::with_request(SSH_FXP_ATTRS, req_id + 999);
            FileAttrs::default().encode(&mut bad);
            let _ = server_io.write_all(&bad.finish()).await;
        });
        let client = SftpClient::from_stream(client_io, Duration::from_secs(5))
            .await
            .unwrap();
        let fs = RemoteFs::new(Location::Remote(HostId("ssh:quirky".into())), client);
        assert!(matches!(fs.lstat(&dir).await, Err(FsError::Protocol(_))));

        // 4. Mid-operation disconnect reported as ConnectionLost
        let (client_io, mut server_io) = duplex(64 * 1024);
        tokio::spawn(async move {
            send_version(&mut server_io, &[]).await;
            let (ptype, mut dec) = recv_packet(&mut server_io).await;
            assert_eq!(ptype, SSH_FXP_OPEN);
            let req_id = dec.get_u32().unwrap();
            let mut h = PacketEncoder::with_request(SSH_FXP_HANDLE, req_id);
            h.put_bytes(b"h-read");
            server_io.write_all(&h.finish()).await.unwrap();
            // Receive first READ and immediately drop the connection.
            let (ptype, _dec) = recv_packet(&mut server_io).await;
            assert_eq!(ptype, SSH_FXP_READ);
            drop(server_io);
        });
        let client = SftpClient::from_stream(client_io, Duration::from_secs(5))
            .await
            .unwrap();
        let fs = RemoteFs::new(Location::Remote(HostId("ssh:quirky".into())), client);
        let mut reader = fs.open_read(&dir).await.unwrap();
        assert!(matches!(
            reader.read_chunk(4096).await,
            Err(FsError::ConnectionLost)
        ));

        // 5. Slow server cancelled cleanly and sends background SSH_FXP_CLOSE on drop
        let (client_io, mut server_io) = duplex(64 * 1024);
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            send_version(&mut server_io, &[]).await;
            let (ptype, mut dec) = recv_packet(&mut server_io).await;
            assert_eq!(ptype, SSH_FXP_OPEN);
            let req_id = dec.get_u32().unwrap();
            let mut h = PacketEncoder::with_request(SSH_FXP_HANDLE, req_id);
            h.put_bytes(b"h-slow");
            server_io.write_all(&h.finish()).await.unwrap();

            // Wait until the client drops the reader and sends SSH_FXP_CLOSE.
            loop {
                let (ptype, mut dec) = recv_packet(&mut server_io).await;
                if ptype == SSH_FXP_CLOSE {
                    let _id = dec.get_u32().unwrap();
                    let handle = dec.get_bytes().unwrap();
                    assert_eq!(handle, b"h-slow");
                    let _ = closed_tx.send(());
                    break;
                }
                assert_eq!(ptype, SSH_FXP_READ);
                let _ = SSH_FXP_DATA;
            }
        });
        let client = SftpClient::from_stream(client_io, Duration::from_secs(5))
            .await
            .unwrap();
        let fs = RemoteFs::new(Location::Remote(HostId("ssh:quirky".into())), client);
        let mut reader = fs.open_read(&dir).await.unwrap();
        let timed_out =
            tokio::time::timeout(Duration::from_millis(50), reader.read_chunk(4096)).await;
        assert!(timed_out.is_err());
        drop(reader);
        tokio::time::timeout(Duration::from_secs(2), closed_rx)
            .await
            .expect("detached SSH_FXP_CLOSE sent on reader drop")
            .unwrap();
    }
}
