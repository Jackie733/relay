//! Command-line parsing. SSH options are a P0 entry point until the Hosts
//! workspace can connect; prompts are answered on the terminal before the
//! TUI starts.

use std::{ffi::OsString, path::PathBuf};

use common::HostId;
use ssh::{
    ConnectOptions, HostProfile, TrustStore,
    config::{ResolveMode, resolve},
};

use crate::terminal::KeyBinding;

pub const HELP: &str = "Relay — SSH workspace (development scaffold)

Usage: relay [OPTIONS] [DIRECTORY]

  --ssh HOST PORT USER IDENTITY RELAY_KNOWN_HOSTS
                         Open a P0 SSH terminal with an OpenSSH private key
  --ssh-config ALIAS CONFIG_FILE
                         Resolve ALIAS with `ssh -G -F CONFIG_FILE` (may run Match exec)
  --fallback             With --ssh-config: built-in parser, never runs commands
  --escape-key KEY       Key returning from Terminal Input (repeatable;
                         default F12 and Ctrl+]); e.g. F10, ctrl+g, alt+q
  -h, --help             Show help
  -V, --version          Show version

Local browsing is read-only. Press ? inside Relay for keys.";

#[derive(Debug, Eq, PartialEq)]
pub enum Target {
    Direct {
        hostname: String,
        port: u16,
        username: String,
        identity: PathBuf,
        known_hosts: PathBuf,
    },
    Config {
        alias: String,
        file: PathBuf,
        fallback: bool,
    },
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct Args {
    pub help: bool,
    pub version: bool,
    pub path: Option<PathBuf>,
    pub ssh: Option<Target>,
    pub escape_keys: Vec<KeyBinding>,
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Args, String> {
    let mut args = args.into_iter();
    let mut parsed = Args::default();
    let mut fallback = false;
    let mut positional = false;
    let text = |value: Option<OsString>, name: &str| -> Result<String, String> {
        value
            .ok_or_else(|| format!("{name} is missing; use relay --help"))?
            .into_string()
            .map_err(|_| format!("{name} must be valid UTF-8"))
    };
    while let Some(arg) = args.next() {
        if !positional {
            match arg.to_str() {
                Some("-h" | "--help") => parsed.help = true,
                Some("-V" | "--version") => parsed.version = true,
                Some("--") => positional = true,
                Some("--fallback") => fallback = true,
                Some("--escape-key") => {
                    let value = text(args.next(), "escape key")?;
                    parsed.escape_keys.push(
                        KeyBinding::parse(&value)
                            .ok_or("invalid escape key; use F1-F12, ctrl+X or alt+X")?,
                    );
                }
                Some("--ssh") if parsed.ssh.is_none() => {
                    parsed.ssh = Some(Target::Direct {
                        hostname: text(args.next(), "host")?,
                        port: text(args.next(), "port")?
                            .parse()
                            .map_err(|_| "invalid port")?,
                        username: text(args.next(), "user")?,
                        identity: args.next().ok_or("identity file is missing")?.into(),
                        known_hosts: args.next().ok_or("known_hosts file is missing")?.into(),
                    });
                }
                Some("--ssh-config") if parsed.ssh.is_none() => {
                    parsed.ssh = Some(Target::Config {
                        alias: text(args.next(), "alias")?,
                        file: args.next().ok_or("config file is missing")?.into(),
                        fallback: false,
                    });
                }
                Some("--ssh" | "--ssh-config") => {
                    return Err("only one SSH target is supported".into());
                }
                _ if arg.to_string_lossy().starts_with('-') => {
                    return Err("unknown option; use relay --help".into());
                }
                _ => {}
            }
            if arg.to_string_lossy().starts_with('-') {
                continue;
            }
        }
        if parsed.path.replace(PathBuf::from(arg)).is_some() {
            return Err("expected at most one directory; use relay --help".into());
        }
    }
    match &mut parsed.ssh {
        Some(Target::Config { fallback: f, .. }) => *f = fallback,
        _ if fallback => return Err("--fallback requires --ssh-config".into()),
        _ => {}
    }
    if parsed.escape_keys.is_empty() {
        parsed.escape_keys = KeyBinding::defaults();
    }
    Ok(parsed)
}

/// Builds connection options; warnings are printed before the TUI starts.
pub async fn connect_options(target: Target) -> Result<(ConnectOptions, Vec<String>), String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or("cannot determine user home directory")?;
    let config_dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    match target {
        Target::Direct {
            hostname,
            port,
            username,
            identity,
            known_hosts,
        } => {
            let mut trust = TrustStore::for_user(&home, &config_dir);
            trust.relay_file = known_hosts;
            let host = HostProfile {
                id: HostId(format!("p0:{hostname}:{port}")),
                hostname,
                port,
                username,
            };
            Ok((ConnectOptions::new(host, trust, identity), Vec::new()))
        }
        Target::Config {
            alias,
            file,
            fallback,
        } => {
            let user = std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_default();
            let mode = if fallback {
                ResolveMode::Fallback
            } else {
                ResolveMode::OpenSsh
            };
            let resolved = resolve(&alias, &file, &home, &config_dir, &user, mode)
                .await
                .map_err(|error| error.to_string())?;
            Ok((resolved.options, resolved.warnings))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args, String> {
        parse(list.iter().map(OsString::from))
    }

    #[test]
    fn parses_paths_ssh_targets_and_escape_keys() {
        let parsed = args(&["/tmp"]).unwrap();
        assert_eq!(parsed.path, Some("/tmp".into()));
        assert_eq!(parsed.escape_keys, KeyBinding::defaults());

        let parsed = args(&[
            "--ssh",
            "h",
            "2221",
            "u",
            "key",
            "kh",
            "--escape-key",
            "F10",
        ])
        .unwrap();
        assert!(matches!(
            parsed.ssh,
            Some(Target::Direct { port: 2221, .. })
        ));
        assert_eq!(parsed.escape_keys.len(), 1);

        let parsed = args(&["--fallback", "--ssh-config", "a", "cfg", "--", "-dir"]).unwrap();
        assert!(matches!(
            parsed.ssh,
            Some(Target::Config { fallback: true, .. })
        ));
        assert_eq!(parsed.path, Some("-dir".into()));

        for invalid in [
            &["--ssh", "h", "x", "u", "k", "kh"][..],
            &["--ssh", "h"],
            &["--fallback"],
            &["--escape-key", "q"],
            &["--ssh-config", "a", "c", "--ssh-config", "b", "d"],
            &["a", "b"],
            &["--unknown"],
        ] {
            assert!(args(invalid).is_err(), "{invalid:?}");
        }
    }
}
