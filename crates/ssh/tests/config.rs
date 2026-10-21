use std::time::Duration;

use ssh::{
    HostKeyPolicy,
    config::{ResolveMode, resolve},
};

#[tokio::test]
async fn fallback_preserves_connection_and_trust_settings() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("config");
    std::fs::write(&file, "Host prod\n HostName 127.0.0.1\n User relay\n Port 2221\n IdentityFile ~/.ssh/id_ed25519\n HostKeyAlias trusted-name\n StrictHostKeyChecking yes\n ConnectTimeout 7\n ServerAliveInterval 9\n ServerAliveCountMax 2\n UserKnownHostsFile ~/.ssh/known_hosts\n GlobalKnownHostsFile none\nHost *\n User wrong-default\n").unwrap();
    let resolved = resolve(
        "prod",
        &file,
        root.path(),
        root.path(),
        "default",
        ResolveMode::Fallback,
    )
    .await
    .unwrap();
    let options = resolved.options;
    assert_eq!(options.host.hostname, "127.0.0.1");
    assert_eq!(options.host.username, "relay");
    assert_eq!(options.host.port, 2221);
    assert_eq!(options.host_key_alias.as_deref(), Some("trusted-name"));
    assert_eq!(options.host_key_policy, HostKeyPolicy::Strict);
    assert_eq!(options.network_timeout, Duration::from_secs(7));
    assert_eq!(options.keepalive_interval, Some(Duration::from_secs(9)));
    assert_eq!(options.keepalive_max, 2);
    assert_eq!(
        options.identity_files,
        [root.path().join(".ssh/id_ed25519")]
    );
    assert_eq!(
        options.trust.sources,
        [root.path().join(".ssh/known_hosts")]
    );
    assert_eq!(
        options.trust.relay_file,
        root.path().join("relay/known_hosts")
    );
}

#[tokio::test]
async fn fallback_rejects_unsupported_behavior_and_never_runs_match_exec() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("config");
    let marker = root.path().join("must-not-exist");
    for directive in [
        format!("Match exec \"touch {}\"", marker.display()),
        "Include other.conf".into(),
        "ProxyCommand run-something".into(),
        "ProxyJump bastion".into(),
        "KnownHostsCommand run-something".into(),
        "LocalCommand run-something".into(),
        "CertificateFile user-cert.pub".into(),
        "CanonicalizeHostname yes".into(),
        "UserKnownHostsFile %h".into(),
        "StrictHostKeyChecking yes\n StrictHostKeyChecking no".into(),
        "PubkeyAuthentication no".into(),
        "BindAddress 127.0.0.2".into(),
        "RemoteForward 1234 localhost:5678".into(),
    ] {
        std::fs::write(&file, format!("Host prod\n {directive}\n")).unwrap();
        assert!(
            resolve(
                "prod",
                &file,
                root.path(),
                root.path(),
                "user",
                ResolveMode::Fallback
            )
            .await
            .is_err(),
            "{directive}"
        );
    }
    assert!(!marker.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn openssh_dump_never_silently_splits_an_existing_spaced_trust_source() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("config");
    let spaced = root.path().join("known hosts");
    std::fs::write(&spaced, "").unwrap();
    for extra in [
        String::new(),
        format!(" {}", root.path().join("another-source").display()),
    ] {
        std::fs::write(
            &file,
            format!(
                "Host prod\n UserKnownHostsFile \"{}\"{extra}\n",
                spaced.display()
            ),
        )
        .unwrap();
        assert!(matches!(
            resolve(
                "prod",
                &file,
                root.path(),
                root.path(),
                "user",
                ResolveMode::OpenSsh
            )
            .await,
            Err(ssh::config::ConfigError::Unsupported(_))
        ));
    }
}

#[tokio::test]
async fn weaker_host_checking_settings_always_require_confirmation() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("config");
    for value in ["no", "accept-new"] {
        std::fs::write(
            &file,
            format!("Host prod\n StrictHostKeyChecking {value}\n UserKnownHostsFile /dev/null\n"),
        )
        .unwrap();
        let resolved = resolve(
            "prod",
            &file,
            root.path(),
            root.path(),
            "user",
            ResolveMode::Fallback,
        )
        .await
        .unwrap();
        assert_eq!(resolved.options.host_key_policy, HostKeyPolicy::Ask);
        assert!(
            resolved
                .warnings
                .iter()
                .any(|warning| warning.contains("strengthened"))
        );
        assert!(
            resolved
                .warnings
                .iter()
                .any(|warning| warning.contains("/dev/null"))
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn openssh_and_fallback_agree_on_the_supported_explicit_subset() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("config");
    std::fs::write(&file, "Host prod\n HostName 127.0.0.1\n User relay\n Port 2221\n IdentityFile ~/.ssh/id_ed25519\n HostKeyAlias trusted-name\n StrictHostKeyChecking no\n ConnectTimeout 7\n ServerAliveInterval 9\n ServerAliveCountMax 2\n UserKnownHostsFile /dev/null\n GlobalKnownHostsFile none\nHost *\n User wrong-default\n").unwrap();
    let openssh = resolve(
        "prod",
        &file,
        root.path(),
        root.path(),
        "default",
        ResolveMode::OpenSsh,
    )
    .await
    .unwrap();
    let fallback = resolve(
        "prod",
        &file,
        root.path(),
        root.path(),
        "default",
        ResolveMode::Fallback,
    )
    .await
    .unwrap();
    let a = openssh.options;
    let b = fallback.options;
    assert_eq!(
        (a.host.hostname, a.host.username, a.host.port),
        (b.host.hostname, b.host.username, b.host.port)
    );
    assert_eq!(a.identity_files, b.identity_files);
    assert_eq!(a.trust.sources, b.trust.sources);
    assert_eq!(a.host_key_alias, b.host_key_alias);
    assert_eq!(a.host_key_policy, b.host_key_policy);
    assert_eq!(a.network_timeout, b.network_timeout);
    assert_eq!(a.keepalive_interval, b.keepalive_interval);
    assert_eq!(a.keepalive_max, b.keepalive_max);

    let included = root.path().join("included");
    std::fs::write(&included, "Host prod\n HostName 127.0.0.1\n User relay\n").unwrap();
    std::fs::write(&file, format!("Include {}\n", included.display())).unwrap();
    let resolved = resolve(
        "prod",
        &file,
        root.path(),
        root.path(),
        "default",
        ResolveMode::OpenSsh,
    )
    .await
    .unwrap();
    assert_eq!(resolved.options.host.hostname, "127.0.0.1");
    assert!(matches!(
        resolve(
            "prod",
            &file,
            root.path(),
            root.path(),
            "default",
            ResolveMode::Fallback
        )
        .await,
        Err(ssh::config::ConfigError::Unsupported(_))
    ));
}
