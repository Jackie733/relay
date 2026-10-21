//! Effective SSH configuration for the P0 connection subset.
//!
//! OpenSsh mode may execute the user's Match exec via `ssh -G`. Call only
//! after an explicit connection action. Fallback mode executes no commands.
use std::{
    collections::HashMap,
    io::Cursor,
    path::{Path, PathBuf},
    time::Duration,
};

use common::HostId;
use ssh2_config::{ParseRule, SshConfig};
use thiserror::Error;

use crate::{ConnectOptions, HostKeyPolicy, HostProfile, TrustStore};

#[derive(Clone, Copy, Debug)]
pub enum ResolveMode {
    OpenSsh,
    Fallback,
}

#[derive(Debug)]
pub struct ResolvedConfig {
    pub options: ConnectOptions,
    /// Callers must display these before connecting.
    pub warnings: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("SSH configuration I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("SSH configuration is invalid or cannot be resolved")]
    Invalid,
    #[error("SSH configuration directive is not supported in this mode: {0}")]
    Unsupported(String),
    #[error("ssh -G failed or timed out")]
    OpenSsh,
}

/// Resolves an explicit file, analogous to `ssh -F FILE`. The fallback only
/// handles simple Host blocks; Include/Match require OpenSsh mode.
pub async fn resolve(
    alias: &str,
    file: &Path,
    home: &Path,
    config_dir: &Path,
    default_user: &str,
    mode: ResolveMode,
) -> Result<ResolvedConfig, ConfigError> {
    TrustStore::lookup_name(alias, 22).map_err(|_| ConfigError::Invalid)?;
    let fields = match mode {
        ResolveMode::OpenSsh => {
            let output = tokio::time::timeout(
                Duration::from_secs(10),
                tokio::process::Command::new("ssh")
                    .args(["-G", "-F"])
                    .arg(file)
                    .arg("--")
                    .arg(alias)
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .map_err(|_| ConfigError::OpenSsh)??;
            if !output.status.success() {
                return Err(ConfigError::OpenSsh);
            }
            let output = String::from_utf8(output.stdout).map_err(|_| ConfigError::Invalid)?;
            let mut fields: HashMap<String, Vec<String>> = HashMap::new();
            for line in output.lines() {
                let (key, value) = line.split_once(' ').ok_or(ConfigError::Invalid)?;
                if matches!(key, "userknownhostsfile" | "globalknownhostsfile") {
                    // OpenSSH's dump does not quote filenames. Refuse an
                    // existing path with spaces rather than splitting trust.
                    fields.insert(key.into(), trust_paths_from_dump(value)?);
                } else {
                    fields.entry(key.into()).or_default().push(value.into());
                }
            }
            fields
        }
        ResolveMode::Fallback => {
            let text = tokio::fs::read_to_string(file).await?;
            fallback_fields(alias, &text, home)?
        }
    };
    build(alias, home, config_dir, default_user, fields, mode)
}

type Fields = HashMap<String, Vec<String>>;

fn fallback_fields(alias: &str, text: &str, home: &Path) -> Result<Fields, ConfigError> {
    let mut seen = std::collections::HashSet::new();
    for line in text.lines() {
        let keyword = line.trim().split([' ', '\t', '=']).next().unwrap_or("");
        if keyword.eq_ignore_ascii_case("match") || keyword.eq_ignore_ascii_case("include") {
            return Err(ConfigError::Unsupported(keyword.into()));
        }
        let keyword = keyword.to_ascii_lowercase();
        if keyword == "host" {
            seen.clear();
        }
        if matches!(
            keyword.as_str(),
            "stricthostkeychecking"
                | "hostkeyalias"
                | "userknownhostsfile"
                | "globalknownhostsfile"
        ) && !seen.insert(keyword)
        {
            return Err(ConfigError::Unsupported(
                "repeated trust directive in one Host block".into(),
            ));
        }
    }
    // ssh2-config expands IdentityFile's tilde using process HOME. Resolve
    // those tokens first against the caller's explicit home (also in tests).
    let home = home
        .to_str()
        .ok_or(ConfigError::Invalid)?
        .replace('\\', "/");
    let mut normalized = String::new();
    for line in text.lines() {
        let keyword = line.trim().split([' ', '\t', '=']).next().unwrap_or("");
        if keyword.eq_ignore_ascii_case("identityfile") {
            let mut previous = 0;
            for (index, _) in line.match_indices("~/") {
                if index > 0 && line[..index].ends_with([' ', '\t', '=', '"']) {
                    normalized.push_str(&line[previous..index]);
                    normalized.push_str(&home);
                    normalized.push('/');
                    previous = index + 2;
                }
            }
            normalized.push_str(&line[previous..]);
        } else {
            normalized.push_str(line);
        }
        normalized.push('\n');
    }
    let config = SshConfig::default()
        .parse(
            &mut Cursor::new(normalized),
            ParseRule::ALLOW_UNSUPPORTED_FIELDS,
        )
        .map_err(|_| ConfigError::Invalid)?;
    let params = config.query(alias);
    if params.bind_address.is_some()
        || params.bind_interface.is_some()
        || !params.remote_forward.is_empty()
    {
        return Err(ConfigError::Unsupported("binding or forwarding".into()));
    }
    let algorithms = params.host_key_algorithms.algorithms();
    let mut fields: Fields = params
        .unsupported_fields
        .into_iter()
        .map(|(name, values)| (name.to_ascii_lowercase(), values))
        .collect();
    fields.insert("hostkeyalgorithms".into(), vec![algorithms.join(",")]);
    for (name, value) in [("hostname", params.host_name), ("user", params.user)] {
        if let Some(value) = value {
            fields.insert(name.into(), vec![value]);
        }
    }
    if let Some(port) = params.port {
        fields.insert("port".into(), vec![port.to_string()]);
    }
    if let Some(paths) = params.identity_file {
        fields.insert(
            "identityfile".into(),
            paths
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
        );
    }
    if let Some(value) = params.connect_timeout {
        fields.insert("connecttimeout".into(), vec![value.as_secs().to_string()]);
    }
    if let Some(value) = params.server_alive_interval {
        fields.insert(
            "serveraliveinterval".into(),
            vec![value.as_secs().to_string()],
        );
    }
    if let Some(value) = params.forward_agent {
        fields.insert(
            "forwardagent".into(),
            vec![if value { "yes" } else { "no" }.into()],
        );
    }
    if let Some(value) = params.proxy_jump {
        fields.insert("proxyjump".into(), value);
    }
    if let Some(value) = params.pubkey_authentication {
        fields.insert(
            "pubkeyauthentication".into(),
            vec![if value { "yes" } else { "no" }.into()],
        );
    }
    if let Some(value) = params.certificate_file {
        fields.insert(
            "certificatefile".into(),
            vec![value.to_string_lossy().into_owned()],
        );
    }
    Ok(fields)
}

fn build(
    alias: &str,
    home: &Path,
    config_dir: &Path,
    default_user: &str,
    fields: Fields,
    mode: ResolveMode,
) -> Result<ResolvedConfig, ConfigError> {
    let value = |key: &str| {
        fields
            .get(key)
            .and_then(|values| values.first())
            .map(String::as_str)
    };
    let active = |key: &str| value(key).is_some_and(|v| !matches!(v, "none" | "no" | "false"));
    for key in [
        "proxyjump",
        "proxycommand",
        "localcommand",
        "knownhostscommand",
        "certificatefile",
        "revokedhostkeys",
        "bindaddress",
        "bindinterface",
        "localforward",
        "remoteforward",
        "dynamicforward",
    ] {
        if active(key) {
            return Err(ConfigError::Unsupported(key.into()));
        }
    }
    if matches!(mode, ResolveMode::Fallback) && active("canonicalizehostname") {
        return Err(ConfigError::Unsupported("CanonicalizeHostname".into()));
    }
    if value("pubkeyauthentication") == Some("no") {
        return Err(ConfigError::Unsupported("PubkeyAuthentication no".into()));
    }
    let host = HostProfile {
        id: HostId(format!("ssh:{alias}")),
        hostname: value("hostname").unwrap_or(alias).into(),
        username: value("user").unwrap_or(default_user).into(),
        port: value("port")
            .unwrap_or("22")
            .parse()
            .map_err(|_| ConfigError::Invalid)?,
    };
    TrustStore::lookup_name(&host.hostname, host.port).map_err(|_| ConfigError::Invalid)?;
    let mut trust = TrustStore::for_user(home, config_dir);
    let mut warnings = vec!["P0 supports Ed25519/ECDSA identity files; SSH Agent, RSA and password authentication are unavailable.".into()];
    if matches!(mode, ResolveMode::Fallback) {
        warnings
            .push("Fallback resolves simple Host blocks only; OpenSSH defaults may differ.".into());
    }
    let mut paths = Vec::new();
    for key in ["userknownhostsfile", "globalknownhostsfile"] {
        if let Some(values) = fields.get(key) {
            for path in values {
                if path == "none" || path == "/dev/null" {
                    warnings.push(format!(
                        "{key} {path} does not disable Relay host-key confirmation."
                    ));
                } else {
                    paths.push(expand(path, home)?);
                }
            }
        } else {
            paths.extend(
                trust
                    .sources
                    .iter()
                    .filter(|path| {
                        if key == "userknownhostsfile" {
                            path.starts_with(home)
                        } else {
                            !path.starts_with(home)
                        }
                    })
                    .cloned(),
            );
        }
    }
    trust.sources = paths;
    let identities = fields
        .get("identityfile")
        .cloned()
        .unwrap_or_else(|| vec!["~/.ssh/id_ed25519".into(), "~/.ssh/id_ecdsa".into()]);
    let mut options = ConnectOptions::new(host, trust, PathBuf::new());
    options.identity_files = identities
        .iter()
        .filter(|path| path.as_str() != "none")
        .map(|path| expand(path, home))
        .collect::<Result<_, _>>()?;
    options.host_key_alias = value("hostkeyalias")
        .filter(|v| *v != "none")
        .map(str::to_owned);
    options.host_key_algorithms =
        value("hostkeyalgorithms").map(|value| value.split(',').map(str::to_owned).collect());
    options.host_key_policy = match value("stricthostkeychecking").unwrap_or("ask") {
        "yes" | "true" => HostKeyPolicy::Strict,
        "ask" => HostKeyPolicy::Ask,
        "no" | "false" | "off" | "accept-new" => {
            warnings.push("StrictHostKeyChecking is strengthened to ask; unknown keys require explicit confirmation.".into());
            HostKeyPolicy::Ask
        }
        _ => return Err(ConfigError::Invalid),
    };
    if let Some(seconds) = value("connecttimeout").filter(|v| *v != "none") {
        options.network_timeout =
            Duration::from_secs(seconds.parse().map_err(|_| ConfigError::Invalid)?);
        if options.network_timeout.is_zero() {
            return Err(ConfigError::Invalid);
        }
    }
    options.keepalive_interval = value("serveraliveinterval")
        .map(|v| v.parse::<u64>())
        .transpose()
        .map_err(|_| ConfigError::Invalid)?
        .filter(|v| *v > 0)
        .map(Duration::from_secs);
    options.keepalive_max = value("serveralivecountmax")
        .unwrap_or("3")
        .parse()
        .map_err(|_| ConfigError::Invalid)?;
    if active("forwardagent") {
        warnings.push("ForwardAgent is disabled in Relay.".into());
    }
    Ok(ResolvedConfig { options, warnings })
}

fn expand(value: &str, home: &Path) -> Result<PathBuf, ConfigError> {
    // Fail closed on unresolved tokens instead of reading a different trust file.
    if value.contains('%') || (value.starts_with('~') && !value.starts_with("~/")) {
        return Err(ConfigError::Unsupported(
            "path expansion token or named-user tilde".into(),
        ));
    }
    Ok(if let Some(path) = value.strip_prefix("~/") {
        home.join(path)
    } else {
        PathBuf::from(value)
    })
}

fn trust_paths_from_dump(value: &str) -> Result<Vec<String>, ConfigError> {
    let parts: Vec<_> = value.split_whitespace().collect();
    // Check every compound path, including a spaced filename followed by
    // another source. Silently splitting it could miss a revocation record.
    for start in 0..parts.len() {
        for end in start + 2..=parts.len() {
            match std::fs::metadata(parts[start..end].join(" ")) {
                Ok(_) => {
                    return Err(ConfigError::Unsupported(
                        "ambiguous known_hosts paths with spaces in ssh -G output; use Fallback"
                            .into(),
                    ));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(parts.into_iter().map(str::to_owned).collect())
}
