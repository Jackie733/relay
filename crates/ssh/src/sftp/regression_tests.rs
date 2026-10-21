use std::time::Duration;

use common::{FileReader, FsError, RemotePath, WriteOptions};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};

use super::{SftpClient, wire::*};

const TEST_TIMEOUT: Duration = Duration::from_secs(3);

async fn recv(stream: &mut DuplexStream) -> Vec<u8> {
    let len = stream.read_u32().await.unwrap();
    assert!(len <= MAX_PACKET_LEN);
    let mut payload = vec![0; len as usize];
    stream.read_exact(&mut payload).await.unwrap();
    payload
}

fn request(payload: &[u8], opcode: u8) -> (u32, PacketDecoder<'_>) {
    let mut dec = PacketDecoder::new(payload);
    assert_eq!(dec.get_u8().unwrap(), opcode);
    (dec.get_u32().unwrap(), dec)
}

async fn handshake(stream: &mut DuplexStream, limits: Option<[u64; 4]>) {
    let payload = recv(stream).await;
    let mut dec = PacketDecoder::new(&payload);
    assert_eq!(dec.get_u8().unwrap(), SSH_FXP_INIT);
    assert_eq!(dec.get_u32().unwrap(), SFTP_VERSION);
    let mut version = PacketEncoder::new(SSH_FXP_VERSION);
    version.put_u32(SFTP_VERSION);
    if limits.is_some() {
        version.put_str(EXT_LIMITS);
        version.put_str("1");
    }
    stream.write_all(&version.finish()).await.unwrap();
    if let Some(limits) = limits {
        let payload = recv(stream).await;
        let (id, mut dec) = request(&payload, SSH_FXP_EXTENDED);
        assert_eq!(dec.get_bytes().unwrap(), EXT_LIMITS.as_bytes());
        let mut reply = PacketEncoder::with_request(SSH_FXP_EXTENDED_REPLY, id);
        for value in limits {
            reply.put_u64(value);
        }
        stream.write_all(&reply.finish()).await.unwrap();
    }
}

async fn send_handle(stream: &mut DuplexStream, opcode: u8) {
    let payload = recv(stream).await;
    let (id, _) = request(&payload, opcode);
    let mut reply = PacketEncoder::with_request(SSH_FXP_HANDLE, id);
    reply.put_bytes(b"handle");
    stream.write_all(&reply.finish()).await.unwrap();
}

async fn send_status(stream: &mut DuplexStream, id: u32, code: u32) {
    let mut reply = PacketEncoder::with_request(SSH_FXP_STATUS, id);
    reply.put_u32(code);
    reply.put_str("");
    reply.put_str("");
    stream.write_all(&reply.finish()).await.unwrap();
}

async fn serve_reads(
    mut stream: DuplexStream,
    contents: Vec<u8>,
    response_limit: usize,
    gate: Option<oneshot::Receiver<()>>,
) {
    handshake(&mut stream, None).await;
    send_handle(&mut stream, SSH_FXP_OPEN).await;
    let mut gate = gate;
    loop {
        let payload = recv(&mut stream).await;
        if payload[0] == SSH_FXP_CLOSE {
            let (id, _) = request(&payload, SSH_FXP_CLOSE);
            send_status(&mut stream, id, SSH_FX_OK).await;
            break;
        }
        let (id, mut dec) = request(&payload, SSH_FXP_READ);
        assert_eq!(dec.get_bytes().unwrap(), b"handle");
        let offset = dec.get_u64().unwrap() as usize;
        let len = dec.get_u32().unwrap() as usize;
        if let Some(gate) = gate.take() {
            gate.await.unwrap();
        }
        if offset >= contents.len() {
            send_status(&mut stream, id, SSH_FX_EOF).await;
        } else {
            let end = offset + len.min(response_limit).min(contents.len() - offset);
            let mut reply = PacketEncoder::with_request(SSH_FXP_DATA, id);
            reply.put_bytes(&contents[offset..end]);
            stream.write_all(&reply.finish()).await.unwrap();
        }
    }
}

async fn finish(task: JoinHandle<()>) {
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
}

async fn collect(reader: &mut dyn FileReader, max_bytes: usize) -> Vec<u8> {
    let mut result = Vec::new();
    loop {
        let chunk = reader.read_chunk(max_bytes).await.unwrap();
        assert!(chunk.len() <= max_bytes);
        if chunk.is_empty() {
            return result;
        }
        result.extend(chunk);
    }
}

#[tokio::test]
async fn cancelled_read_can_resume_without_skipping_bytes() {
    let contents: Vec<u8> = (0..128).collect();
    let (client_io, server_io) = duplex(64 * 1024);
    let (resume, gate) = oneshot::channel();
    let server = tokio::spawn(serve_reads(
        server_io,
        contents.clone(),
        usize::MAX,
        Some(gate),
    ));
    let client = SftpClient::from_stream(client_io, TEST_TIMEOUT)
        .await
        .unwrap();
    let mut reader = client.open_read(&RemotePath::root()).await.unwrap();
    assert!(
        timeout(Duration::from_millis(20), reader.read_chunk(4))
            .await
            .is_err()
    );
    resume.send(()).unwrap();
    assert_eq!(collect(reader.as_mut(), 4).await, contents);
    reader.close().await.unwrap();
    finish(server).await;
}

#[tokio::test]
async fn changing_read_limits_preserves_prefetched_bytes() {
    let contents: Vec<u8> = (0..255).collect();
    let (client_io, server_io) = duplex(64 * 1024);
    let server = tokio::spawn(serve_reads(server_io, contents.clone(), usize::MAX, None));
    let client = SftpClient::from_stream(client_io, TEST_TIMEOUT)
        .await
        .unwrap();
    let mut reader = client.open_read(&RemotePath::root()).await.unwrap();
    let mut actual = reader.read_chunk(64).await.unwrap();
    assert_eq!(actual.len(), 64);
    assert!(reader.read_chunk(0).await.unwrap().is_empty());
    for _ in 0..70 {
        let chunk = reader.read_chunk(1).await.unwrap();
        assert_eq!(chunk.len(), 1);
        actual.extend(chunk);
    }
    actual.extend(collect(reader.as_mut(), 17).await);
    assert_eq!(actual, contents);
    reader.close().await.unwrap();
    finish(server).await;
}

#[tokio::test]
async fn short_reads_do_not_duplicate_or_skip_bytes() {
    let contents: Vec<u8> = (0..129).collect();
    let (client_io, server_io) = duplex(64 * 1024);
    let server = tokio::spawn(serve_reads(server_io, contents.clone(), 3, None));
    let client = SftpClient::from_stream(client_io, TEST_TIMEOUT)
        .await
        .unwrap();
    let mut reader = client.open_read(&RemotePath::root()).await.unwrap();
    assert_eq!(collect(reader.as_mut(), 64).await, contents);
    reader.close().await.unwrap();
    finish(server).await;
}

#[tokio::test]
async fn cancelling_directory_read_closes_its_handle() {
    let (client_io, mut server_io) = duplex(64 * 1024);
    let server = tokio::spawn(async move {
        handshake(&mut server_io, None).await;
        send_handle(&mut server_io, SSH_FXP_OPENDIR).await;
        let payload = recv(&mut server_io).await;
        request(&payload, SSH_FXP_READDIR);
        let payload = recv(&mut server_io).await;
        let (id, mut dec) = request(&payload, SSH_FXP_CLOSE);
        assert_eq!(dec.get_bytes().unwrap(), b"handle");
        send_status(&mut server_io, id, SSH_FX_OK).await;
    });
    let client = SftpClient::from_stream(client_io, TEST_TIMEOUT)
        .await
        .unwrap();
    assert!(
        timeout(
            Duration::from_millis(20),
            client.read_dir(&RemotePath::root())
        )
        .await
        .is_err()
    );
    finish(server).await;
}

#[tokio::test]
async fn cancelling_directory_open_closes_a_late_handle() {
    let (client_io, mut server_io) = duplex(64 * 1024);
    let (resume, gate) = oneshot::channel();
    let server = tokio::spawn(async move {
        handshake(&mut server_io, None).await;
        let payload = recv(&mut server_io).await;
        let (id, _) = request(&payload, SSH_FXP_OPENDIR);
        gate.await.unwrap();
        let mut reply = PacketEncoder::with_request(SSH_FXP_HANDLE, id);
        reply.put_bytes(b"late-dir");
        server_io.write_all(&reply.finish()).await.unwrap();
        let payload = recv(&mut server_io).await;
        let (id, mut dec) = request(&payload, SSH_FXP_CLOSE);
        assert_eq!(dec.get_bytes().unwrap(), b"late-dir");
        send_status(&mut server_io, id, SSH_FX_OK).await;
    });
    let client = SftpClient::from_stream(client_io, TEST_TIMEOUT)
        .await
        .unwrap();
    assert!(
        timeout(
            Duration::from_millis(20),
            client.read_dir(&RemotePath::root())
        )
        .await
        .is_err()
    );
    resume.send(()).unwrap();
    finish(server).await;
}

#[tokio::test]
async fn stalled_requests_are_bounded_and_timeout_closes_channel() {
    let (client_io, mut server_io) = duplex(64 * 1024);
    let server = tokio::spawn(async move {
        handshake(&mut server_io, None).await;
        let mut count = 0;
        // Consume requests but never reply, leaving the stream open.
        while let Ok(len) = server_io.read_u32().await {
            let mut payload = vec![0; len as usize];
            server_io.read_exact(&mut payload).await.unwrap();
            request(&payload, SSH_FXP_LSTAT);
            count += 1;
        }
        assert!(
            (1..=64).contains(&count),
            "outstanding request count: {count}"
        );
    });
    let client = SftpClient::from_stream(client_io, Duration::from_millis(100))
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for _ in 0..128 {
        let client = client.clone();
        tasks.push(tokio::spawn(async move {
            assert!(matches!(
                client.lstat(&RemotePath::root()).await,
                Err(FsError::ConnectionLost)
            ));
        }));
    }
    for task in tasks {
        finish(task).await;
    }
    for _ in 0..64 {
        assert!(matches!(
            client.lstat(&RemotePath::root()).await,
            Err(FsError::ConnectionLost)
        ));
    }
    finish(server).await;
}

#[tokio::test]
async fn externally_cancelled_request_keeps_its_deadline() {
    let (client_io, mut server_io) = duplex(64 * 1024);
    let server = tokio::spawn(async move {
        handshake(&mut server_io, None).await;
        let payload = recv(&mut server_io).await;
        request(&payload, SSH_FXP_LSTAT);
        let mut byte = [0];
        assert_eq!(server_io.read(&mut byte).await.unwrap(), 0);
    });
    let client = SftpClient::from_stream(client_io, Duration::from_millis(100))
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_millis(20), client.lstat(&RemotePath::root()))
            .await
            .is_err()
    );
    finish(server).await;
    assert!(matches!(
        client.lstat(&RemotePath::root()).await,
        Err(FsError::ConnectionLost)
    ));
}

#[tokio::test]
async fn small_server_limits_bound_read_write_chunks_and_complete_packets() {
    for limits in [[512, 128, 128, 16], [64, 0, 0, 0]] {
        let (client_io, mut server_io) = duplex(64 * 1024);
        let server = tokio::spawn(async move {
            handshake(&mut server_io, Some(limits)).await;
            let mut contents = Vec::new();
            let mut closes = 0;
            while closes < 2 {
                let payload = recv(&mut server_io).await;
                assert!(payload.len() + 4 <= limits[0] as usize);
                let (id, mut dec) = request(&payload, payload[0]);
                match payload[0] {
                    SSH_FXP_OPEN => {
                        let mut reply = PacketEncoder::with_request(SSH_FXP_HANDLE, id);
                        reply.put_bytes(b"handle");
                        server_io.write_all(&reply.finish()).await.unwrap();
                    }
                    SSH_FXP_WRITE => {
                        assert_eq!(dec.get_bytes().unwrap(), b"handle");
                        let offset = dec.get_u64().unwrap() as usize;
                        let data = dec.get_bytes().unwrap();
                        assert!(limits[2] == 0 || data.len() <= limits[2] as usize);
                        contents.resize(contents.len().max(offset + data.len()), 0);
                        contents[offset..offset + data.len()].copy_from_slice(data);
                        send_status(&mut server_io, id, SSH_FX_OK).await;
                    }
                    SSH_FXP_READ => {
                        assert_eq!(dec.get_bytes().unwrap(), b"handle");
                        let offset = dec.get_u64().unwrap() as usize;
                        let len = dec.get_u32().unwrap() as usize;
                        assert!(limits[1] == 0 || len <= limits[1] as usize);
                        if offset >= contents.len() {
                            send_status(&mut server_io, id, SSH_FX_EOF).await;
                        } else {
                            let end = contents.len().min(offset + len);
                            let mut reply = PacketEncoder::with_request(SSH_FXP_DATA, id);
                            reply.put_bytes(&contents[offset..end]);
                            let frame = reply.finish();
                            assert!(frame.len() <= limits[0] as usize);
                            server_io.write_all(&frame).await.unwrap();
                        }
                    }
                    SSH_FXP_CLOSE => {
                        closes += 1;
                        send_status(&mut server_io, id, SSH_FX_OK).await;
                    }
                    opcode => panic!("unexpected opcode {opcode}"),
                }
            }
        });
        let client = SftpClient::from_stream(client_io, TEST_TIMEOUT)
            .await
            .unwrap();
        if limits[1] > 0 {
            assert!(u64::from(client.capabilities().max_read_size) <= limits[1]);
            assert!(u64::from(client.capabilities().max_write_size) <= limits[2]);
        }
        let path = RemotePath::from_posix("/f").unwrap();
        let contents: Vec<u8> = (0..255).collect();
        let mut writer = client
            .open_write(&path, WriteOptions::create_new(0o600))
            .await
            .unwrap();
        writer.write_all(&contents).await.unwrap();
        writer.close().await.unwrap();
        let mut reader = client.open_read(&path).await.unwrap();
        assert_eq!(collect(reader.as_mut(), 256 * 1024).await, contents);
        reader.close().await.unwrap();
        finish(server).await;
    }
}

#[tokio::test]
async fn impossible_packet_limits_are_rejected() {
    let (client_io, mut server_io) = duplex(64 * 1024);
    let server = tokio::spawn(async move {
        handshake(&mut server_io, Some([10, 0, 0, 0])).await;
        let mut byte = [0];
        assert_eq!(server_io.read(&mut byte).await.unwrap(), 0);
    });
    assert!(matches!(
        SftpClient::from_stream(client_io, TEST_TIMEOUT).await,
        Err(FsError::Protocol(_))
    ));
    finish(server).await;
}
