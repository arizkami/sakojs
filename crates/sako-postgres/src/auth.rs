// SPDX-License-Identifier: BSD-3-Clause

//! Proving who is connecting.
//!
//! PostgreSQL 14 and later default to SCRAM-SHA-256, and everything older that
//! is still running is likely on md5. Both are here, because a driver that
//! only speaks the new one cannot connect to a database that already exists.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use hmac::{Hmac, Mac as _};
use md5::Digest as _;
use rand::Rng as _;
use sha2::Sha256;

use crate::PostgresError;

type HmacSha256 = Hmac<Sha256>;

/// The `md5` authentication response: the password and user name hashed
/// together, then hashed again with the salt the server chose, so the stored
/// hash alone cannot be replayed against a different salt.
pub fn md5_password(user: &str, password: &str, salt: [u8; 4]) -> String {
    let mut first = md5::Md5::new();
    first.update(password.as_bytes());
    first.update(user.as_bytes());
    let inner = hex(&first.finalize());

    let mut second = md5::Md5::new();
    second.update(inner.as_bytes());
    second.update(salt);
    format!("md5{}", hex(&second.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// The SCRAM-SHA-256 exchange, as a small state machine.
///
/// Three messages each way: the client offers a nonce, the server answers with
/// its own nonce plus a salt and an iteration count, the client proves it knows
/// the password without sending it, and the server proves the same in return.
/// That last step is not decoration -- skipping it is how a client ends up
/// trusting a server that never knew the password.
pub struct Scram {
    password: String,
    client_nonce: String,
    client_first_bare: String,
    salted_password: Vec<u8>,
    auth_message: String,
    channel_binding: &'static str,
}

impl Scram {
    /// `SCRAM-SHA-256`, without channel binding. The `-PLUS` variant binds the
    /// exchange to the TLS channel; this driver connects in the clear, so
    /// there is no channel to bind to and claiming otherwise would be a lie
    /// the server would catch anyway.
    pub const MECHANISM: &'static str = "SCRAM-SHA-256";

    pub fn new(password: &str) -> Self {
        Self {
            password: password.to_owned(),
            client_nonce: nonce(),
            client_first_bare: String::new(),
            salted_password: Vec::new(),
            auth_message: String::new(),
            // "n" says the client does not support channel binding at all.
            channel_binding: "n,,",
        }
    }

    /// The first message: no channel binding, no authorization identity, and
    /// the client's nonce. The user name is deliberately empty -- it was
    /// already sent in the startup message, and SCRAM's own field would need
    /// its own escaping rules for no gain.
    pub fn client_first(&mut self) -> Vec<u8> {
        self.client_first_bare = format!("n=,r={}", self.client_nonce);
        format!("{}{}", self.channel_binding, self.client_first_bare).into_bytes()
    }

    /// Answers the server's challenge with the client proof.
    pub fn client_final(&mut self, server_first: &[u8]) -> Result<Vec<u8>, PostgresError> {
        let server_first = std::str::from_utf8(server_first)
            .map_err(|_| PostgresError::Protocol("SCRAM challenge is not UTF-8".to_owned()))?;
        let mut nonce = None;
        let mut salt = None;
        let mut iterations = None;
        for attribute in server_first.split(',') {
            let (key, value) = attribute.split_at(attribute.find('=').unwrap_or(0));
            let value = value.strip_prefix('=').unwrap_or("");
            match key {
                "r" => nonce = Some(value.to_owned()),
                "s" => salt = Some(value.to_owned()),
                "i" => iterations = value.parse::<u32>().ok(),
                _ => {}
            }
        }
        let (Some(nonce), Some(salt), Some(iterations)) = (nonce, salt, iterations) else {
            return Err(PostgresError::Protocol(
                "SCRAM challenge is missing a nonce, salt, or iteration count".to_owned(),
            ));
        };
        if !nonce.starts_with(&self.client_nonce) {
            return Err(PostgresError::Protocol(
                "SCRAM server nonce does not extend the client nonce".to_owned(),
            ));
        }
        // A server that asks for too few rounds is asking for a weaker proof
        // than the standard's floor; one that asks for too many can hang the
        // connection on PBKDF2 alone.
        if !(4_096..=1_000_000).contains(&iterations) {
            return Err(PostgresError::Protocol(format!(
                "SCRAM iteration count is out of range: {iterations}"
            )));
        }
        let salt = BASE64
            .decode(salt.as_bytes())
            .map_err(|_| PostgresError::Protocol("SCRAM salt is not base64".to_owned()))?;

        self.salted_password = vec![0; 32];
        pbkdf2::pbkdf2::<HmacSha256>(
            self.password.as_bytes(),
            &salt,
            iterations,
            &mut self.salted_password,
        )
        .map_err(|_| PostgresError::Protocol("SCRAM key derivation failed".to_owned()))?;

        let channel_binding = BASE64.encode(self.channel_binding.as_bytes());
        let client_final_without_proof = format!("c={channel_binding},r={nonce}");
        self.auth_message = format!(
            "{},{},{}",
            self.client_first_bare, server_first, client_final_without_proof
        );

        let client_key = hmac(&self.salted_password, b"Client Key");
        let stored_key = sha256(&client_key);
        let client_signature = hmac(&stored_key, self.auth_message.as_bytes());
        let proof: Vec<u8> = client_key
            .iter()
            .zip(client_signature.iter())
            .map(|(key, signature)| key ^ signature)
            .collect();

        Ok(format!("{client_final_without_proof},p={}", BASE64.encode(proof)).into_bytes())
    }

    /// Checks the server's own proof. A mismatch means whatever answered is
    /// not holding the password, whatever else it may have said.
    pub fn verify(&self, server_final: &[u8]) -> Result<(), PostgresError> {
        let server_final = std::str::from_utf8(server_final)
            .map_err(|_| PostgresError::Protocol("SCRAM final message is not UTF-8".to_owned()))?;
        let mut signature = None;
        for attribute in server_final.split(',') {
            if let Some(value) = attribute.strip_prefix("v=") {
                signature = Some(value.to_owned());
            }
            if let Some(error) = attribute.strip_prefix("e=") {
                return Err(PostgresError::Protocol(format!(
                    "SCRAM authentication failed: {error}"
                )));
            }
        }
        let Some(signature) = signature else {
            return Err(PostgresError::Protocol(
                "SCRAM final message carries no server signature".to_owned(),
            ));
        };
        let signature = BASE64
            .decode(signature.as_bytes())
            .map_err(|_| PostgresError::Protocol("SCRAM signature is not base64".to_owned()))?;
        let server_key = hmac(&self.salted_password, b"Server Key");
        let expected = hmac(&server_key, self.auth_message.as_bytes());
        if signature.len() != expected.len() || !constant_time_equal(&signature, &expected) {
            return Err(PostgresError::Protocol(
                "SCRAM server signature does not match".to_owned(),
            ));
        }
        Ok(())
    }
}

fn hmac(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts a key of any length");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    use sha2::Digest as _;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().to_vec()
}

/// Comparison that does not stop at the first difference. The server signature
/// is a secret-derived value, and an early exit tells whoever is listening how
/// much of a guess was right.
fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

/// A fresh client nonce. SCRAM requires it to be unpredictable, so it comes
/// from the system generator rather than a clock.
fn nonce() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut generator = rand::thread_rng();
    (0..24)
        .map(|_| ALPHABET[generator.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example PostgreSQL's own documentation and every other client
    /// checks against: md5 of the password and user, hashed again with the
    /// salt.
    #[test]
    fn md5_matches_the_documented_construction() {
        let answer = md5_password("postgres", "secret", [0x01, 0x02, 0x03, 0x04]);
        assert!(answer.starts_with("md5"));
        assert_eq!(answer.len(), 35);

        let inner = format!("{:x}", md5::Md5::digest(b"secretpostgres"));
        let mut outer = md5::Md5::new();
        outer.update(inner.as_bytes());
        outer.update([0x01, 0x02, 0x03, 0x04]);
        assert_eq!(answer, format!("md5{:x}", outer.finalize()));
    }

    /// RFC 7677's worked example, which pins the cryptography.
    ///
    /// The exchange itself cannot be replayed against it: the RFC's client
    /// sends `n=user`, and PostgreSQL's clients send `n=` because the user was
    /// already named in the startup message. So this checks the parts the RFC
    /// actually fixes -- the derivation, the proof, and the server signature --
    /// against its published values, and `an_exchange_verifies_end_to_end`
    /// checks the message flow.
    #[test]
    fn the_proof_matches_the_rfc_vector() {
        let salt = BASE64.decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let mut salted = vec![0; 32];
        pbkdf2::pbkdf2::<HmacSha256>(b"pencil", &salt, 4096, &mut salted).unwrap();

        let auth_message = "n=user,r=rOprNGfwEbeRWgbNEkqO,\
r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096,\
c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";

        let client_key = hmac(&salted, b"Client Key");
        let stored_key = sha256(&client_key);
        let signature = hmac(&stored_key, auth_message.as_bytes());
        let proof: Vec<u8> = client_key
            .iter()
            .zip(signature.iter())
            .map(|(key, sign)| key ^ sign)
            .collect();
        assert_eq!(
            BASE64.encode(proof),
            "dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
        );

        let server_key = hmac(&salted, b"Server Key");
        assert_eq!(
            BASE64.encode(hmac(&server_key, auth_message.as_bytes())),
            "6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4="
        );
    }

    /// The whole exchange against a stand-in server that knows the password,
    /// in the message shape PostgreSQL actually uses.
    #[test]
    fn an_exchange_verifies_end_to_end() {
        let password = "hunter2";
        let salt = b"0123456789abcdef";
        let iterations = 4_096u32;

        let mut client = Scram::new(password);
        let first = String::from_utf8(client.client_first()).unwrap();
        assert!(first.starts_with("n,,n=,r="));
        let client_nonce = first.rsplit("r=").next().unwrap().to_owned();

        // What a server answers: the client's nonce with its own appended.
        let server_nonce = format!("{client_nonce}serverpart");
        let server_first = format!("r={server_nonce},s={},i={iterations}", BASE64.encode(salt));
        let client_final =
            String::from_utf8(client.client_final(server_first.as_bytes()).unwrap()).unwrap();

        // The server recomputes everything from the password it stored.
        let mut salted = vec![0; 32];
        pbkdf2::pbkdf2::<HmacSha256>(password.as_bytes(), salt, iterations, &mut salted).unwrap();
        let auth_message = format!(
            "n=,r={client_nonce},{server_first},{}",
            client_final.rsplit_once(",p=").unwrap().0
        );
        let client_key = hmac(&salted, b"Client Key");
        let stored_key = sha256(&client_key);
        let expected: Vec<u8> = client_key
            .iter()
            .zip(hmac(&stored_key, auth_message.as_bytes()).iter())
            .map(|(key, sign)| key ^ sign)
            .collect();
        let offered = BASE64
            .decode(client_final.rsplit_once(",p=").unwrap().1)
            .unwrap();
        assert_eq!(offered, expected, "the server should accept the proof");

        // And the client accepts the server's own proof in return.
        let server_key = hmac(&salted, b"Server Key");
        let signature = BASE64.encode(hmac(&server_key, auth_message.as_bytes()));
        client
            .verify(format!("v={signature}").as_bytes())
            .expect("a correct server signature verifies");
    }

    #[test]
    fn a_wrong_server_signature_is_rejected() {
        let mut scram = Scram::new("pencil");
        scram.client_nonce = "rOprNGfwEbeRWgbNEkqO".to_owned();
        scram.client_first();
        scram
            .client_final(b"r=rOprNGfwEbeRWgbNEkqOmore,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096")
            .unwrap();
        let error = scram
            .verify(b"v=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .unwrap_err();
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn a_server_nonce_that_does_not_extend_the_client_nonce_is_rejected() {
        let mut scram = Scram::new("pencil");
        scram.client_first();
        let error = scram
            .client_final(b"r=somethingelse,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096")
            .unwrap_err();
        assert!(error.to_string().contains("does not extend"));
    }

    #[test]
    fn an_absurd_iteration_count_is_rejected_rather_than_computed() {
        let mut scram = Scram::new("pencil");
        let client_nonce = scram.client_nonce.clone();
        scram.client_first();
        let error = scram
            .client_final(
                format!("r={client_nonce}x,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=100000000").as_bytes(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("iteration count"));
    }

    #[test]
    fn nonces_differ_between_exchanges() {
        assert_ne!(nonce(), nonce());
        assert_eq!(nonce().len(), 24);
    }
}
