//! Minimal P0 probe. Opens an authenticated connection, then disconnects.
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
};

use common::{HostId, display_text};
use secrecy::SecretString;
use ssh::{
    ConnectOptions, Connection, HostProfile, Prompt, PromptBroker, PromptKind, PromptResponse,
    TrustDecision, TrustStore,
    config::{ResolveMode, resolve},
};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Relay SSH: {}", display_text(&error.to_string()));
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err("interactive terminal required for SSH confirmation".into());
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or("cannot determine user home directory")?;
    let home = PathBuf::from(home);
    let options = if args.first().is_some_and(|arg| arg == "--config")
        && (args.len() == 3 || (args.len() == 4 && args[3] == "--fallback"))
    {
        let mode = if args.len() == 4 {
            ResolveMode::Fallback
        } else {
            ResolveMode::OpenSsh
        };
        let config_dir = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let default_user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_default();
        let resolved = resolve(
            args[1].to_str().ok_or("invalid alias")?,
            &PathBuf::from(&args[2]),
            &home,
            &config_dir,
            &default_user,
            mode,
        )
        .await?;
        for warning in resolved.warnings {
            eprintln!("{}", display_text(&warning));
        }
        resolved.options
    } else if args.len() == 5 {
        let mut trust = TrustStore::for_user(&home, &home.join(".config"));
        trust.relay_file = PathBuf::from(&args[4]);
        let host = HostProfile {
            id: HostId("p0-probe".into()),
            hostname: args[0].to_str().ok_or("invalid hostname")?.into(),
            port: args[1].to_str().ok_or("invalid port")?.parse()?,
            username: args[2].to_str().ok_or("invalid username")?.into(),
        };
        ConnectOptions::new(host, trust, PathBuf::from(&args[3]))
    } else {
        return Err("Usage: connect HOST PORT USER IDENTITY RELAY_KNOWN_HOSTS\n       connect --config ALIAS CONFIG_FILE [--fallback]".into());
    };
    let (broker, mut requests) = PromptBroker::channel(1);
    let connect = Connection::connect(options, broker);
    tokio::pin!(connect);
    let connection = loop {
        tokio::select! {
            result = &mut connect => break result?,
            Some(prompt) = requests.recv() => {
                // A plain thread lets process shutdown finish if a prompt's
                // deadline expires while the terminal read is still blocked.
                std::thread::spawn(move || answer(prompt));
            }
        }
    };
    eprintln!(
        "Authenticated to {}:{} as {}.",
        display_text(&connection.host().hostname),
        connection.host().port,
        display_text(&connection.host().username)
    );
    connection.disconnect().await?;
    Ok(())
}

fn answer(prompt: Prompt) {
    eprintln!(
        "SSH {}:{} ({})",
        display_text(&prompt.host.hostname),
        prompt.host.port,
        display_text(&prompt.host.username)
    );
    let response = match prompt.kind {
        PromptKind::HostKey {
            key,
            report,
            peer_address,
        } => {
            eprintln!("Remote address: {peer_address}");
            if report.decision == TrustDecision::AlgorithmChanged {
                eprintln!(
                    "WARNING: different host-key algorithm; known: {}",
                    display_text(&report.known_algorithms.join(", "))
                );
            }
            if report.ignored_certificate_authority {
                eprintln!(
                    "Certificate-authority entries are not supported; verify this host key independently."
                );
            }
            eprintln!(
                "{} {}",
                display_text(&key.algorithm),
                display_text(&key.fingerprint)
            );
            eprint!("Verify the fingerprint independently. Trust and save this key? Type yes: ");
            let _ = io::stderr().flush();
            let mut input = String::new();
            if io::stdin().read_line(&mut input).is_ok() && input.trim() == "yes" {
                PromptResponse::Trust
            } else {
                PromptResponse::Reject
            }
        }
        PromptKind::KeyPassphrase { identity_file } => {
            eprintln!(
                "Passphrase for {}",
                display_text(&identity_file.display().to_string())
            );
            match rpassword::prompt_password("Passphrase (hidden): ") {
                Ok(value) => PromptResponse::Passphrase(SecretString::from(value)),
                Err(_) => PromptResponse::Reject,
            }
        }
    };
    let _ = prompt.response.send(response);
}
