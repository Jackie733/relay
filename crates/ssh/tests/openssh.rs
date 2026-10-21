//! Uses only disposable localhost fixtures and a temporary trust store.
use std::{path::Path, process::Command, time::Duration};

use common::{
    FileKind, FileSystem, FsError, FsPath, HostId, MetadataPatch, RemotePath, RenameMode,
    SyncOutcome, WriteOptions,
};
use russh::{ChannelMsg, keys::PublicKey};
use secrecy::SecretString;
use ssh::{
    ConnectError, ConnectOptions, Connection, HostKey, HostKeyPolicy, HostProfile, PromptBroker,
    PromptError, PromptKind, PromptResponse, PtyCommand, PtyEvent, PtyReader, PtySize,
    TrustDecision, TrustError, TrustStore,
};

fn options(root: &Path) -> ConnectOptions {
    ConnectOptions::new(
        HostProfile {
            id: HostId("fixture-a".into()),
            hostname: "127.0.0.1".into(),
            port: 2221,
            username: "relay".into(),
        },
        TrustStore {
            sources: vec![root.join("user"), root.join("global")],
            relay_file: root.join("relay"),
        },
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/integration/.fixtures/id_ed25519"),
    )
}

async fn no_prompt(options: ConnectOptions) -> Result<Connection, ConnectError> {
    let (broker, receiver) = PromptBroker::channel(1);
    drop(receiver);
    Connection::connect(options, broker).await
}

async fn expect_echo(connection: Connection) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut channel = connection.open_session().await.unwrap();
        channel.exec(true, "printf relay-native-ok").await.unwrap();
        let (mut output, mut status) = (Vec::new(), None);
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Data { data } => output.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                _ => {}
            }
        }
        assert_eq!(output, b"relay-native-ok");
        assert_eq!(status, Some(0));
        connection.disconnect().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires tests/integration OpenSSH containers on localhost:2221/2223"]
async fn safe_connections_against_real_openssh() {
    let root = tempfile::tempdir().unwrap();
    let mut config = options(root.path());
    config.host_key_policy = HostKeyPolicy::Strict;
    // A missing identity proves host verification happens before key loading.
    let mut strict = config.clone();
    strict.identity_files = vec![root.path().join("missing-key")];
    assert!(matches!(
        no_prompt(strict).await,
        Err(ConnectError::UnknownHostKey)
    ));
    assert!(!config.trust.relay_file.exists());

    config.host_key_policy = HostKeyPolicy::Ask;
    let (broker, mut receiver) = PromptBroker::channel(1);
    let connect = tokio::spawn(Connection::connect(config.clone(), broker));
    let prompt = receiver.recv().await.unwrap();
    assert_eq!(prompt.host.id, config.host.id);
    assert!(matches!(prompt.kind, PromptKind::HostKey { .. }));
    prompt.response.send(PromptResponse::Reject).unwrap();
    assert!(matches!(
        connect.await.unwrap(),
        Err(ConnectError::Prompt(PromptError::Rejected))
    ));
    assert!(!config.trust.relay_file.exists());

    let (broker, mut receiver) = PromptBroker::channel(1);
    let connect = tokio::spawn(Connection::connect(config.clone(), broker));
    let prompt = receiver.recv().await.unwrap();
    let PromptKind::HostKey {
        key,
        report,
        peer_address,
    } = prompt.kind
    else {
        panic!("expected host-key prompt")
    };
    assert_eq!(report.decision, TrustDecision::Unknown);
    assert_eq!(peer_address.port(), 2221);
    assert!(key.fingerprint.starts_with("SHA256:"));
    prompt.response.send(PromptResponse::Trust).unwrap();
    expect_echo(connect.await.unwrap().unwrap()).await;

    config.host_key_policy = HostKeyPolicy::Strict;
    expect_echo(no_prompt(config.clone()).await.unwrap()).await;

    let mut wrong_user = config.clone();
    wrong_user.host.username = "relay-nonexistent-user".into();
    assert!(matches!(
        no_prompt(wrong_user).await,
        Err(ConnectError::Authentication)
    ));

    // Host C presents another real key for the same recorded endpoint.
    config.trust.remember("[127.0.0.1]:2223", &key).unwrap();
    let mut changed = config.clone();
    changed.host.port = 2223;
    assert!(matches!(
        no_prompt(changed).await,
        Err(ConnectError::Trust(TrustError::Changed))
    ));

    std::fs::write(
        &config.trust.sources[1],
        format!("@revoked [127.0.0.1]:2221 {}\n", key.openssh()),
    )
    .unwrap();
    assert!(matches!(
        no_prompt(config.clone()).await,
        Err(ConnectError::Trust(TrustError::Revoked))
    ));
    std::fs::write(&config.trust.sources[1], "").unwrap();

    // OpenSSH independently generates the hashed lookup, including its port.
    let hash_root = tempfile::tempdir().unwrap();
    let mut hashed = options(hash_root.path());
    std::fs::write(
        &hashed.trust.sources[0],
        format!("[127.0.0.1]:2221 {}\n", key.openssh()),
    )
    .unwrap();
    assert!(
        Command::new("ssh-keygen")
            .args(["-H", "-f"])
            .arg(&hashed.trust.sources[0])
            .output()
            .unwrap()
            .status
            .success()
    );
    hashed.host_key_policy = HostKeyPolicy::Strict;
    expect_echo(no_prompt(hashed.clone()).await.unwrap()).await;
    std::fs::write(
        &hashed.trust.sources[0],
        format!("[127.0.0.?]:2221,!excluded {}\n", key.openssh()),
    )
    .unwrap();
    expect_echo(no_prompt(hashed.clone()).await.unwrap()).await;

    let rsa = hash_root.path().join("rsa");
    assert!(
        Command::new("ssh-keygen")
            .args(["-q", "-t", "rsa", "-b", "2048", "-N", "", "-f"])
            .arg(&rsa)
            .status()
            .unwrap()
            .success()
    );
    let rsa = std::fs::read_to_string(rsa.with_extension("pub")).unwrap();
    std::fs::write(&hashed.trust.sources[0], format!("[127.0.0.1]:2221 {rsa}")).unwrap();
    hashed.host_key_policy = HostKeyPolicy::Ask;
    let (broker, mut receiver) = PromptBroker::channel(1);
    let connect = tokio::spawn(Connection::connect(hashed, broker));
    let prompt = receiver.recv().await.unwrap();
    let PromptKind::HostKey { report, .. } = prompt.kind else {
        panic!("expected algorithm-change warning")
    };
    assert_eq!(report.decision, TrustDecision::AlgorithmChanged);
    assert_eq!(report.known_algorithms, ["ssh-rsa"]);
    prompt.response.send(PromptResponse::Reject).unwrap();
    assert!(matches!(
        connect.await.unwrap(),
        Err(ConnectError::Prompt(PromptError::Rejected))
    ));

    // Encrypted copies of the disposable test identity; no developer key used.
    let encrypted = root.path().join("encrypted-key");
    std::fs::copy(&config.identity_files[0], &encrypted).unwrap();
    assert!(
        Command::new("ssh-keygen")
            .args(["-p", "-P", "", "-N", "test-only-passphrase", "-f"])
            .arg(&encrypted)
            .output()
            .unwrap()
            .status
            .success()
    );
    let mut encrypted_options = config.clone();
    encrypted_options.identity_files = vec![encrypted];
    for (passphrase, success) in [("wrong-passphrase", false), ("test-only-passphrase", true)] {
        let (broker, mut receiver) = PromptBroker::channel(1);
        let connect = tokio::spawn(Connection::connect(encrypted_options.clone(), broker));
        let prompt = receiver.recv().await.unwrap();
        assert!(matches!(prompt.kind, PromptKind::KeyPassphrase { .. }));
        prompt
            .response
            .send(PromptResponse::Passphrase(SecretString::from(passphrase)))
            .unwrap();
        let result = connect.await.unwrap();
        if success {
            expect_echo(result.unwrap()).await;
        } else {
            assert!(matches!(result, Err(ConnectError::KeyDecryption)));
        }
    }

    let pending_root = tempfile::tempdir().unwrap();
    let mut pending = options(pending_root.path());
    pending.prompt_timeout = Duration::from_millis(100);
    let (broker, mut receiver) = PromptBroker::channel(1);
    let connect = tokio::spawn(Connection::connect(pending.clone(), broker));
    let mut prompt = receiver.recv().await.unwrap();
    assert!(matches!(
        connect.await.unwrap(),
        Err(ConnectError::Prompt(PromptError::TimedOut))
    ));
    assert!(prompt.response.is_closed());
    assert!(!pending.trust.relay_file.exists());

    pending.prompt_timeout = Duration::from_secs(30);
    let (broker, mut receiver) = PromptBroker::channel(1);
    let connect = tokio::spawn(Connection::connect(pending.clone(), broker));
    prompt = receiver.recv().await.unwrap();
    connect.abort();
    assert!(matches!(connect.await, Err(error) if error.is_cancelled()));
    tokio::time::timeout(Duration::from_secs(2), prompt.response.closed())
        .await
        .unwrap();
    assert!(!pending.trust.relay_file.exists());

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    pending.host.port = listener.local_addr().unwrap().port();
    drop(listener);
    assert!(matches!(
        no_prompt(pending).await,
        Err(ConnectError::Network(_))
    ));

    // The saved public key is byte-for-byte usable by OpenSSH tooling.
    let parsed = PublicKey::from_openssh(&key.openssh()).unwrap();
    assert_eq!(HostKey::from_public_key(&parsed).unwrap(), key);
}

async fn trusted(root: &Path) -> Connection {
    let (broker, mut receiver) = PromptBroker::channel(1);
    let connect = tokio::spawn(Connection::connect(options(root), broker));
    // Only the disposable fixture key is trusted, inside a temporary store.
    if let Some(prompt) = receiver.recv().await {
        assert!(matches!(prompt.kind, PromptKind::HostKey { .. }));
        prompt.response.send(PromptResponse::Trust).unwrap();
    }
    connect.await.unwrap().unwrap()
}

async fn read_until(reader: &mut PtyReader, output: &mut Vec<u8>, needle: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !String::from_utf8_lossy(output).contains(needle) {
            match reader.next().await {
                Some(PtyEvent::Output(data)) => output.extend_from_slice(&data),
                event => panic!("unexpected {event:?} while waiting for {needle:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}"));
}

#[tokio::test]
#[ignore = "requires tests/integration OpenSSH containers on localhost:2221"]
async fn interactive_pty_against_real_openssh() {
    let root = tempfile::tempdir().unwrap();
    let connection = trusted(root.path()).await;
    let (mut reader, writer) = connection
        .open_pty(PtySize::new(80, 24), PtyCommand::Shell)
        .await
        .unwrap();
    let mut output = Vec::new();
    // Markers are assembled remotely so the echoed command cannot match.
    writer
        .write(b"stty size; echo \"$TERM\"; locale charmap; echo relay-''one\r")
        .await
        .unwrap();
    read_until(&mut reader, &mut output, "relay-one").await;
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("24 80"), "{text}");
    assert!(text.contains("xterm-256color"), "{text}");
    assert!(text.contains("UTF-8"), "{text}");

    writer.resize(PtySize::new(132, 43)).await.unwrap();
    output.clear();
    writer
        .write(b"stty size; echo relay-''two\r")
        .await
        .unwrap();
    read_until(&mut reader, &mut output, "relay-two").await;
    assert!(String::from_utf8_lossy(&output).contains("43 132"));

    // Ctrl+C interrupts the foreground command without closing the shell.
    output.clear();
    writer.write(b"sleep 30\r").await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    writer.write(b"\x03echo relay-''three\r").await.unwrap();
    read_until(&mut reader, &mut output, "relay-three").await;

    writer.write(b"exit 7\r").await.unwrap();
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = reader.next().await {
            if !matches!(event, PtyEvent::Output(_)) {
                events.push(event);
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(events.first(), Some(&PtyEvent::Exited(7)));
    assert_eq!(events.last(), Some(&PtyEvent::Closed));

    let (mut reader, _writer) = connection
        .open_pty(PtySize::new(80, 24), PtyCommand::Exec("exit 3".into()))
        .await
        .unwrap();
    let mut status = None;
    while let Some(event) = reader.next().await {
        if let PtyEvent::Exited(code) = event {
            status = Some(code);
        }
    }
    assert_eq!(status, Some(3));
    connection.disconnect().await.unwrap();
}

fn current_rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|num| num.parse::<u64>().ok());
        }
    }
    None
}

#[tokio::test]
#[ignore = "requires tests/integration OpenSSH containers on localhost:2221"]
async fn sftp_filesystem_and_1gib_streaming_against_real_openssh() {
    let root = tempfile::tempdir().unwrap();
    let connection = trusted(root.path()).await;
    let fs = connection.open_sftp().await.unwrap();

    // 1. Capability probing against real OpenSSH internal-sftp.
    let caps = fs.capabilities();
    assert!(
        caps.posix_rename,
        "OpenSSH supports posix-rename@openssh.com"
    );
    assert!(caps.fsync, "OpenSSH supports fsync@openssh.com");
    assert!(caps.statvfs, "OpenSSH supports statvfs@openssh.com");
    assert!((64 * 1024..=256 * 1024).contains(&caps.max_read_size));
    assert!((64 * 1024..=256 * 1024).contains(&caps.max_write_size));
    assert!(caps.max_open_handles.is_some());

    let dot = FsPath::Remote(RemotePath::from_posix(".").unwrap());
    let home = fs.canonicalize(&dot).await.unwrap();
    assert_eq!(
        home,
        FsPath::Remote(RemotePath::from_posix("/home/relay").unwrap())
    );
    let free = fs.free_space(&home).await.unwrap();
    assert!(free.is_some_and(|bytes| bytes > 1024 * 1024));

    // 2. Directory creation, stat/lstat, read/write, metadata, and rename semantics.
    let work_raw =
        RemotePath::from_posix(&format!("/tmp/relay-sftp-{}", std::process::id())).unwrap();
    let work_dir = FsPath::Remote(work_raw.clone());
    let _ = fs.remove_dir(&work_dir).await;
    fs.create_dir(&work_dir, 0o700).await.unwrap();
    let dir_meta = fs.stat(&work_dir).await.unwrap();
    assert_eq!(dir_meta.kind, FileKind::Directory);
    assert_eq!(dir_meta.sanitized_permissions(), Some(0o700));

    let file_a = FsPath::Remote(work_raw.join(b"alpha.txt").unwrap());
    let file_b = FsPath::Remote(work_raw.join(b"beta.txt").unwrap());
    let mut writer = fs
        .open_write(&file_a, WriteOptions::create_new(0o640))
        .await
        .unwrap();
    writer.write_all(b"first-version").await.unwrap();
    assert_eq!(writer.sync().await.unwrap(), SyncOutcome::Synced);
    writer.close().await.unwrap();

    assert!(matches!(
        fs.open_write(&file_a, WriteOptions::create_new(0o640))
            .await,
        Err(FsError::AlreadyExists)
    ));

    fs.set_metadata(
        &file_a,
        &MetadataPatch {
            permissions: Some(0o600),
            mtime: Some(1_700_000_000),
            atime: Some(1_700_000_000),
        },
    )
    .await
    .unwrap();
    let meta_a = fs.lstat(&file_a).await.unwrap();
    assert_eq!(meta_a.kind, FileKind::File);
    assert_eq!(meta_a.size, 13);
    assert_eq!(meta_a.sanitized_permissions(), Some(0o600));
    assert_eq!(meta_a.mtime, Some(1_700_000_000));

    // NoReplace rename succeeds to a new target, and fails when target exists.
    fs.rename(&file_a, &file_b, RenameMode::NoReplace)
        .await
        .unwrap();
    let mut writer = fs
        .open_write(&file_a, WriteOptions::create_new(0o600))
        .await
        .unwrap();
    writer.write_all(b"atomic-replacement").await.unwrap();
    writer.close().await.unwrap();

    assert!(matches!(
        fs.rename(&file_a, &file_b, RenameMode::NoReplace).await,
        Err(FsError::AlreadyExists)
    ));
    // AtomicReplace via posix-rename@openssh.com replaces existing target atomically.
    fs.rename(&file_a, &file_b, RenameMode::AtomicReplace)
        .await
        .unwrap();
    let mut reader = fs.open_read(&file_b).await.unwrap();
    let data = reader.read_chunk(1024).await.unwrap();
    reader.close().await.unwrap();
    assert_eq!(data, b"atomic-replacement");
    fs.remove_file(&file_b).await.unwrap();

    // 3. Permission denied and directory not found (P0-FS-01).
    let missing_dir =
        FsPath::Remote(RemotePath::from_posix("/tmp/relay-missing-dir-99999").unwrap());
    assert!(matches!(
        fs.read_dir(&missing_dir).await,
        Err(FsError::NotFound)
    ));
    let root_dir = FsPath::Remote(RemotePath::from_posix("/root").unwrap());
    assert!(matches!(
        fs.read_dir(&root_dir).await,
        Err(FsError::PermissionDenied)
    ));
    let ro_dir = FsPath::Remote(work_raw.join(b"readonly").unwrap());
    fs.create_dir(&ro_dir, 0o500).await.unwrap();
    let forbidden_file = FsPath::Remote(
        work_raw
            .join(b"readonly")
            .unwrap()
            .join(b"denied.txt")
            .unwrap(),
    );
    assert!(matches!(
        fs.open_write(&forbidden_file, WriteOptions::create_new(0o600))
            .await,
        Err(FsError::PermissionDenied)
    ));
    fs.set_metadata(
        &ro_dir,
        &MetadataPatch {
            permissions: Some(0o700),
            ..MetadataPatch::default()
        },
    )
    .await
    .unwrap();
    fs.remove_dir(&ro_dir).await.unwrap();

    // 4. Non-UTF-8 filename round-trip (P0-FS-02) & U+FFFD mutation guard.
    let non_utf8_name = b"non-utf8-\xff-\xfe-\x80.bin";
    let non_utf8_path = FsPath::Remote(work_raw.join(non_utf8_name).unwrap());
    let mut writer = fs
        .open_write(&non_utf8_path, WriteOptions::create_new(0o600))
        .await
        .unwrap();
    writer.write_all(b"non-utf8-bytes-ok").await.unwrap();
    writer.close().await.unwrap();

    let listed = fs.read_dir(&work_dir).await.unwrap();
    let found = listed
        .iter()
        .find(|entry| entry.raw_name == non_utf8_name)
        .expect("non-UTF-8 entry preserved in read_dir");
    assert!(!found.is_valid_utf8());
    assert!(!found.unsafe_name);
    assert_eq!(found.metadata.size, 17);

    let mut reader = fs.open_read(&found.path).await.unwrap();
    let read_back = reader.read_chunk(1024).await.unwrap();
    reader.close().await.unwrap();
    assert_eq!(read_back, b"non-utf8-bytes-ok");

    let renamed_non_utf8 = FsPath::Remote(work_raw.join(b"renamed-\xff.bin").unwrap());
    fs.rename(&found.path, &renamed_non_utf8, RenameMode::AtomicReplace)
        .await
        .unwrap();
    fs.remove_file(&renamed_non_utf8).await.unwrap();

    let lossy_path = FsPath::Remote(work_raw.join("bad-\u{fffd}.bin".as_bytes()).unwrap());
    assert!(matches!(
        fs.open_write(&lossy_path, WriteOptions::create_new(0o600))
            .await,
        Err(FsError::UnsafeName)
    ));

    // 5. 1 GiB streaming write + read with bounded memory verification.
    const ONE_GIB: u64 = 1024 * 1024 * 1024;
    const CHUNK_LEN: usize = 256 * 1024;
    let gib_path = FsPath::Remote(work_raw.join(b"stream-1gib.bin").unwrap());
    let mut pattern = vec![0u8; CHUNK_LEN];
    for (idx, byte) in pattern.iter_mut().enumerate() {
        *byte = (idx.wrapping_mul(31).wrapping_add(17)) as u8;
    }

    let rss_before = current_rss_kib().unwrap_or(0);
    let mut peak_rss_delta_kib = 0u64;

    let write_start = std::time::Instant::now();
    let mut writer = fs
        .open_write(&gib_path, WriteOptions::create_new(0o600))
        .await
        .unwrap();
    let mut written = 0u64;
    while written < ONE_GIB {
        writer.write_all(&pattern).await.unwrap();
        written += CHUNK_LEN as u64;
        if written.is_multiple_of(64 * 1024 * 1024)
            && let Some(rss) = current_rss_kib()
        {
            peak_rss_delta_kib = peak_rss_delta_kib.max(rss.saturating_sub(rss_before));
        }
    }
    writer.sync().await.unwrap();
    writer.close().await.unwrap();
    let write_elapsed = write_start.elapsed();

    let gib_meta = fs.lstat(&gib_path).await.unwrap();
    assert_eq!(gib_meta.size, ONE_GIB);

    let read_start = std::time::Instant::now();
    let mut reader = fs.open_read(&gib_path).await.unwrap();
    let mut total_read = 0u64;
    while {
        let chunk = reader.read_chunk(CHUNK_LEN).await.unwrap();
        if chunk.is_empty() {
            false
        } else {
            let offset = (total_read as usize) % CHUNK_LEN;
            assert_eq!(chunk[0], pattern[offset]);
            total_read += chunk.len() as u64;
            if total_read.is_multiple_of(64 * 1024 * 1024)
                && let Some(rss) = current_rss_kib()
            {
                peak_rss_delta_kib = peak_rss_delta_kib.max(rss.saturating_sub(rss_before));
            }
            true
        }
    } {}
    reader.close().await.unwrap();
    let read_elapsed = read_start.elapsed();
    assert_eq!(total_read, ONE_GIB);

    // Peak RSS growth during a 1 GiB stream must stay bounded (< 64 MiB), not O(file_size).
    assert!(
        peak_rss_delta_kib < 64 * 1024,
        "peak RSS delta {peak_rss_delta_kib} KiB exceeded bounded streaming limit"
    );
    eprintln!(
        "1 GiB SFTP baseline: write={:.2}s ({:.1} MiB/s), read={:.2}s ({:.1} MiB/s), peak_rss_delta={} KiB",
        write_elapsed.as_secs_f64(),
        1024.0 / write_elapsed.as_secs_f64(),
        read_elapsed.as_secs_f64(),
        1024.0 / read_elapsed.as_secs_f64(),
        peak_rss_delta_kib
    );

    fs.remove_file(&gib_path).await.unwrap();
    fs.remove_dir(&work_dir).await.unwrap();
    connection.disconnect().await.unwrap();
}
