//! SCRAM-SHA-256 client (RFC 5802 / RFC 7677) as used by PostgreSQL, plus
//! the small HMAC/PBKDF2 helpers it needs.

use base64::Engine as _;
use sha2::{Digest, Sha256};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
  let mut k = [0u8; 64];
  if key.len() > 64 {
    k[..32].copy_from_slice(&Sha256::digest(key));
  } else {
    k[..key.len()].copy_from_slice(key);
  }
  let mut inner = Sha256::new();
  inner.update(k.map(|b| b ^ 0x36));
  inner.update(data);
  let mut outer = Sha256::new();
  outer.update(k.map(|b| b ^ 0x5c));
  outer.update(inner.finalize());
  outer.finalize().into()
}

/// PBKDF2-HMAC-SHA256 with a 32-byte output (a single block).
pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
  let mut block = salt.to_vec();
  block.extend_from_slice(&1u32.to_be_bytes());
  let mut u = hmac_sha256(password, &block);
  let mut out = u;
  for _ in 1..iterations {
    u = hmac_sha256(password, &u);
    for (o, x) in out.iter_mut().zip(u) {
      *o ^= x;
    }
  }
  out
}

pub struct Scram {
  password: String,
  client_nonce: String,
  client_first_bare: String,
  /// Expected server signature, set after `client_final`.
  server_signature: Option<[u8; 32]>,
}

impl Scram {
  pub fn new(password: &str) -> Scram {
    let mut raw = [0u8; 18];
    let _ = getrandom::getrandom(&mut raw);
    let client_nonce = B64.encode(raw);
    // PostgreSQL takes the user from the startup message; send it empty.
    let client_first_bare = format!("n=,r={client_nonce}");
    Scram { password: password.to_string(), client_nonce, client_first_bare, server_signature: None }
  }

  pub fn client_first(&self) -> String {
    format!("n,,{}", self.client_first_bare)
  }

  /// Builds the client-final message from the server-first message.
  pub fn client_final(&mut self, server_first: &str) -> Result<String, String> {
    let mut nonce = None;
    let mut salt = None;
    let mut iterations = None;
    for attr in server_first.split(',') {
      match attr.split_once('=') {
        Some(("r", v)) => nonce = Some(v),
        Some(("s", v)) => salt = Some(B64.decode(v).map_err(|e| format!("SCRAM: bad salt: {e}"))?),
        Some(("i", v)) => iterations = v.parse::<u32>().ok(),
        _ => {}
      }
    }
    let (Some(nonce), Some(salt), Some(iterations)) = (nonce, salt, iterations) else {
      return Err("SCRAM: malformed server-first message".into());
    };
    if !nonce.starts_with(&self.client_nonce) {
      return Err("SCRAM: server nonce does not extend the client nonce".into());
    }
    let salted = pbkdf2_sha256(self.password.as_bytes(), &salt, iterations);
    let client_key = hmac_sha256(&salted, b"Client Key");
    let stored_key: [u8; 32] = Sha256::digest(client_key).into();
    let without_proof = format!("c=biws,r={nonce}");
    let auth_message = format!("{},{server_first},{without_proof}", self.client_first_bare);
    let signature = hmac_sha256(&stored_key, auth_message.as_bytes());
    let proof: Vec<u8> = client_key.iter().zip(signature).map(|(a, b)| a ^ b).collect();
    let server_key = hmac_sha256(&salted, b"Server Key");
    self.server_signature = Some(hmac_sha256(&server_key, auth_message.as_bytes()));
    Ok(format!("{without_proof},p={}", B64.encode(proof)))
  }

  /// Checks the server-final message (mutual authentication).
  pub fn verify_server(&self, server_final: &str) -> Result<(), String> {
    let v = server_final
      .split(',')
      .find_map(|a| a.strip_prefix("v="))
      .ok_or_else(|| format!("SCRAM: server error: {server_final}"))?;
    let got = B64.decode(v).map_err(|e| format!("SCRAM: bad server signature: {e}"))?;
    match self.server_signature {
      Some(expected) if expected[..] == got[..] => Ok(()),
      _ => Err("SCRAM: server signature mismatch".into()),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn hmac_vectors() {
    // RFC 4231 test case 2
    let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
    assert_eq!(
      mac.iter().map(|b| format!("{b:02x}")).collect::<String>(),
      "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
  }

  #[test]
  fn pbkdf2_vector() {
    // RFC 7914 section 11 (PBKDF2-HMAC-SHA256, c=1, first 32 bytes)
    let dk = pbkdf2_sha256(b"passwd", b"salt", 1);
    assert_eq!(
      dk.iter().map(|b| format!("{b:02x}")).collect::<String>(),
      "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc"
    );
  }
}
