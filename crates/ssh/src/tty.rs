//! Line-based prompt answers for command-line tools, before any TUI exists.
use std::io::{self, Write};

use common::display_text;
use secrecy::SecretString;

use crate::{Prompt, PromptKind, PromptResponse, TrustDecision};

/// Blocks on stdin/stderr; call from a plain thread. Anything other than an
/// explicit `yes` (or a read error) rejects the request.
pub fn answer(prompt: Prompt) {
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

/// Connects while answering prompts on the controlling terminal.
pub async fn connect(
    options: crate::ConnectOptions,
) -> Result<crate::Connection, crate::ConnectError> {
    let (broker, mut requests) = crate::PromptBroker::channel(1);
    let connect = crate::Connection::connect(options, broker);
    tokio::pin!(connect);
    loop {
        tokio::select! {
            result = &mut connect => return result,
            Some(prompt) = requests.recv() => {
                // A plain thread lets process shutdown finish if a prompt's
                // deadline expires while the terminal read is still blocked.
                std::thread::spawn(move || answer(prompt));
            }
        }
    }
}
