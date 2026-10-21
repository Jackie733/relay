use super::*;

fn key(byte: u8) -> HostKey {
    let mut blob = Vec::from(&b"\0\0\0\x0bssh-ed25519\0\0\0\x20"[..]);
    blob.extend([byte; 32]);
    HostKey::from_public_key(&PublicKey::from_bytes(&blob).unwrap()).unwrap()
}

fn store(root: &Path) -> TrustStore {
    TrustStore {
        sources: vec![root.join("user"), root.join("global")],
        relay_file: root.join("relay"),
    }
}

#[test]
fn merges_sources_and_rejects_conflicts_and_revocation_before_matches() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let current = key(1);
    store.remember("prod", &current).unwrap();
    assert_eq!(
        store.assess("prod", &current).unwrap().decision,
        TrustDecision::Trusted
    );
    fs::write(&store.sources[1], format!("prod {}\n", key(2).openssh())).unwrap();
    assert_eq!(
        store.assess("prod", &current).unwrap().decision,
        TrustDecision::Changed
    );
    let old = fs::read(&store.relay_file).unwrap();
    assert!(matches!(
        store.remember("prod", &current),
        Err(TrustError::Changed)
    ));
    assert_eq!(fs::read(&store.relay_file).unwrap(), old);
    fs::write(
        &store.sources[1],
        format!("@revoked prod {}\n", current.openssh()),
    )
    .unwrap();
    assert_eq!(
        store.assess("prod", &current).unwrap().decision,
        TrustDecision::Revoked
    );
    assert!(matches!(
        store.remember("prod", &current),
        Err(TrustError::Revoked)
    ));
}

#[test]
fn patterns_negation_tabs_ports_ipv6_and_alias_lookup() {
    let current = key(1);
    let text = format!(
        "# comment\n*.example,!bad.example\t{}\tcomment\n[::1]:2222 {}\n",
        current.openssh(),
        current.openssh()
    );
    for (name, expected) in [
        ("prod.example", true),
        ("BAD.example", false),
        ("[::1]:2222", true),
        ("[::1]:22", false),
    ] {
        let report = assess(std::slice::from_ref(&text), name, &current).unwrap();
        assert_eq!(
            report.decision == TrustDecision::Trusted,
            expected,
            "{name}"
        );
    }
    assert_eq!(TrustStore::lookup_name("PROD", 22).unwrap(), "prod");
    assert_eq!(TrustStore::lookup_name("::1", 2222).unwrap(), "[::1]:2222");
    for host in ["", "*", "x\nprod", "prod,other", "!prod", "[::1]"] {
        assert!(TrustStore::lookup_name(host, 22).is_err());
    }
    assert!(TrustStore::lookup_name("prod", 0).is_err());
    assert!(wildcard(b"p?o*", b"PROD"));
}

#[test]
fn hashed_entries_and_algorithm_changes_are_not_unknown_hosts() {
    let current = key(1);
    let salt = [42; 20];
    let mut hash = Hmac::<Sha1>::new_from_slice(&salt).unwrap();
    hash.update(b"[prod]:2222");
    let hashed = format!(
        "|1|{}|{} {}",
        STANDARD.encode(salt),
        STANDARD.encode(hash.finalize().into_bytes()),
        current.openssh()
    );
    assert_eq!(
        assess(std::slice::from_ref(&hashed), "[prod]:2222", &current)
            .unwrap()
            .decision,
        TrustDecision::Trusted
    );
    assert_eq!(
        assess(&[hashed], "prod", &current).unwrap().decision,
        TrustDecision::Unknown
    );
    let mut other = Vec::from(&b"\0\0\0\x07ssh-rsa"[..]);
    other.extend([0, 0, 0, 1, 3, 0, 0, 0, 1, 7]);
    let text = format!(
        "prod ssh-rsa {}\n@cert-authority prod {}",
        STANDARD.encode(other),
        current.openssh()
    );
    let report = assess(&[text], "prod", &current).unwrap();
    assert_eq!(report.decision, TrustDecision::AlgorithmChanged);
    assert_eq!(report.known_algorithms, ["ssh-rsa"]);
    assert!(report.ignored_certificate_authority);
    let report = assess(
        &[format!("@cert-authority prod {}", current.openssh())],
        "prod",
        &current,
    )
    .unwrap();
    assert_eq!(report.decision, TrustDecision::Unknown);
    assert!(report.ignored_certificate_authority);
}

#[test]
fn malformed_or_unreadable_sources_fail_closed() {
    let current = key(1);
    for text in [
        "prod ssh-ed25519 invalid!",
        "@unknown prod key value",
        "prod",
        "prod ssh-rsa AAAA",
        "|2|salt|hash ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB",
    ] {
        assert!(assess(&[text.into()], "prod", &current).is_err(), "{text}");
    }
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    fs::create_dir(&store.sources[0]).unwrap();
    assert!(store.assess("prod", &current).is_err());
}

#[test]
fn concurrent_acceptance_cannot_append_two_conflicting_keys() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = [1, 2]
        .into_iter()
        .map(|byte| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.remember("prod", &key(byte))
            })
        })
        .collect();
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        fs::read_to_string(&store.relay_file)
            .unwrap()
            .lines()
            .count(),
        1
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&store.relay_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[cfg(unix)]
#[test]
fn never_appends_to_symlinks_or_permissive_files() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let victim = root.path().join("victim");
    fs::write(&victim, "unchanged").unwrap();
    symlink(&victim, &store.relay_file).unwrap();
    assert!(matches!(
        store.remember("prod", &key(1)),
        Err(TrustError::InsecureStore)
    ));
    assert_eq!(fs::read_to_string(&victim).unwrap(), "unchanged");
    fs::remove_file(&store.relay_file).unwrap();
    fs::write(&store.relay_file, "").unwrap();
    fs::set_permissions(&store.relay_file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        store.remember("prod", &key(1)),
        Err(TrustError::InsecureStore)
    ));
}
