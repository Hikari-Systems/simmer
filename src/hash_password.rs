//! `server hash-password` — mint an argon2id PHC string for `server.auth.users`
//! (D-071), as Slater's `slater hash-password` does.
//!
//! Reads the password from **stdin**, never from argv: an argument lands in
//! shell history, `ps` output and the container's recorded command line, which
//! are three places a credential should not be. One line is read and its line
//! ending stripped, so both of these work:
//!
//! ```sh
//! printf '%s' "$PASSWORD" | docker run -i --rm simmer:local hash-password
//! docker run -it --rm simmer:local hash-password      # then type it, and Enter
//! ```
//!
//! The parameters are argon2's defaults — `m=19456, t=2, p=1`, the §4.1 example
//! and OWASP's argon2id baseline — so a hash minted here costs what every other
//! one in the file costs, which is the uniform case D-066's decoy is exact for.
//!
//! Like `healthcheck`, it runs before the async runtime and before the config is
//! read: minting a credential must not depend on having a valid configuration,
//! since the configuration is what needs the credential.

use std::io::{BufRead, Write};

use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
use argon2::Argon2;

/// If argv[1] is `hash-password`, mint a hash from stdin, print it, and exit.
/// Otherwise return and let the process start normally.
pub fn check_subcommand() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("hash-password") {
        return;
    }
    if args.len() > 1 {
        eprintln!(
            "usage: server hash-password   (the password is read from stdin, never from \
             arguments)"
        );
        std::process::exit(2);
    }

    let mut line = String::new();
    if let Err(e) = std::io::stdin().lock().read_line(&mut line) {
        eprintln!("hash-password: reading stdin: {e}");
        std::process::exit(1);
    }
    match hash(strip_line_ending(&line)) {
        Ok(phc) => {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{phc}");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("hash-password: {e}");
            std::process::exit(1);
        }
    }
}

/// One trailing `\n` or `\r\n`, and nothing else — a password may legitimately
/// begin or end with a space.
fn strip_line_ending(line: &str) -> &str {
    line.strip_suffix('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .unwrap_or(line)
}

/// The PHC string for `password`, with a fresh random salt.
pub fn hash(password: &str) -> Result<String, String> {
    if password.is_empty() {
        return Err("refusing to hash an empty password".to_string());
    }
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use argon2::{PasswordHash, PasswordVerifier};

    #[test]
    fn a_minted_hash_verifies_and_is_argon2id_at_the_default_parameters() {
        let phc = hash("local-dev-password").unwrap();
        assert!(phc.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"), "{phc}");
        let parsed = PasswordHash::new(&phc).unwrap();
        assert!(Argon2::default()
            .verify_password(b"local-dev-password", &parsed)
            .is_ok());
        assert!(Argon2::default()
            .verify_password(b"something else", &parsed)
            .is_err());
    }

    #[test]
    fn every_hash_has_its_own_salt() {
        assert_ne!(hash("same").unwrap(), hash("same").unwrap());
    }

    #[test]
    fn an_empty_password_is_refused() {
        assert!(hash("").is_err());
    }

    #[test]
    fn only_the_line_ending_is_stripped() {
        assert_eq!(strip_line_ending("pw\n"), "pw");
        assert_eq!(strip_line_ending("pw\r\n"), "pw");
        assert_eq!(strip_line_ending("pw"), "pw");
        assert_eq!(strip_line_ending(" pw \n"), " pw ");
        assert_eq!(strip_line_ending("pw\n\n"), "pw\n");
    }
}
