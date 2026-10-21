//! Uses only disposable localhost fixtures and a temporary trust store.
use std::{path::Path, process::Command, time::Duration};

use common::HostId;
use russh::{ChannelMsg, keys::PublicKey};
use secrecy::SecretString;
use ssh::{
    ConnectError, ConnectOptions, Connection, HostKey, HostKeyPolicy, HostProfile, PromptBroker,
    PromptError, PromptKind, PromptResponse, TrustDecision, TrustError, TrustStore,
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
