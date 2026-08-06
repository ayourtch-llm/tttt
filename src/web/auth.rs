//! Authentication for the web UI.
//!
//! Two auth schemes, both optional:
//! - **Token**: a random or user-supplied secret, sent as `?token=` or
//!   `Authorization: Bearer <token>`.
//! - **htpasswd**: an Apache-style `username:hash` file supporting bcrypt
//!   (`$2y$`/`$2a$`/`$2b$`), apr1 (`$apr1$`), `{SHA}` and plaintext entries.
//!
//! When neither is configured, no auth is required (loopback default). The
//! caller is responsible for generating a token when binding to a non-loopback
//! address.

use axum::http::header::AUTHORIZATION;
use axum::http::HeaderMap;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::path::Path;
use subtle::ConstantTimeEq;

/// Combined auth configuration.
#[derive(Clone, Debug)]
pub struct Auth {
    token: Option<String>,
    htpasswd: Option<Htpasswd>,
}

impl Auth {
    /// No authentication.
    pub fn none() -> Self {
        Self {
            token: None,
            htpasswd: None,
        }
    }

    /// Token-only auth.
    pub fn with_token(token: String) -> Self {
        Self {
            token: Some(token),
            htpasswd: None,
        }
    }

    /// Load an htpasswd file.
    pub fn with_htpasswd(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            token: None,
            htpasswd: Some(Htpasswd::load(path)?),
        })
    }

    /// True if any credential is required.
    pub fn required(&self) -> bool {
        self.token.is_some() || self.htpasswd.is_some()
    }

    /// True if the configured scheme is token-based (vs htpasswd basic auth).
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// Validate a request using query params and/or HTTP headers.
    pub fn check_request(&self, query: &HashMap<String, String>, headers: &HeaderMap) -> bool {
        if let Some(tok) = &self.token {
            // ?token=xxx
            if let Some(q) = query.get("token") {
                if constant_time_eq_str(q, tok) {
                    return true;
                }
            }
            // Authorization: Bearer xxx
            if let Some(auth) = headers.get(AUTHORIZATION) {
                if let Ok(s) = auth.to_str() {
                    if let Some(bearer) = s.strip_prefix("Bearer ") {
                        if constant_time_eq_str(bearer, tok) {
                            return true;
                        }
                    }
                }
            }
            return false;
        }

        if let Some(hp) = &self.htpasswd {
            // Authorization: Basic base64(user:pass)
            if let Some(auth) = headers.get(AUTHORIZATION) {
                if let Ok(s) = auth.to_str() {
                    if let Some(b64) = s.strip_prefix("Basic ") {
                        if let Some((u, p)) = decode_basic(b64) {
                            if hp.verify(&u, &p) {
                                return true;
                            }
                        }
                    }
                }
            }
            // ?auth=base64(user:pass) — used by the JS client for WebSocket
            if let Some(a) = query.get("auth") {
                if let Some((u, p)) = decode_basic(a) {
                    if hp.verify(&u, &p) {
                        return true;
                    }
                }
            }
            return false;
        }

        true
    }
}

fn decode_basic(b64: &str) -> Option<(String, String)> {
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    let creds = String::from_utf8(decoded).ok()?;
    let (u, p) = creds.split_once(':')?;
    Some((u.to_string(), p.to_string()))
}

fn constant_time_eq_str(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

fn constant_time_eq_bytes(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

/// An Apache htpasswd file.
#[derive(Clone, Debug)]
pub struct Htpasswd {
    entries: Vec<(String, String)>,
}

impl Htpasswd {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = std::fs::read_to_string(path)?;
        let mut entries = Vec::new();
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((user, hash)) = line.split_once(':') {
                entries.push((user.to_string(), hash.to_string()));
            }
        }
        if entries.is_empty() {
            return Err("htpasswd file contains no valid entries".into());
        }
        Ok(Self { entries })
    }

    pub fn verify(&self, user: &str, password: &str) -> bool {
        self.entries
            .iter()
            .find(|(u, _)| u == user)
            .map(|(_, hash)| verify_hash(hash, password))
            .unwrap_or(false)
    }
}

/// Verify a password against a stored htpasswd hash.
fn verify_hash(stored: &str, password: &str) -> bool {
    if stored.starts_with("$2y$") || stored.starts_with("$2a$") || stored.starts_with("$2b$") {
        bcrypt::verify(password, stored).unwrap_or(false)
    } else if let Some(b64) = stored.strip_prefix("{SHA}") {
        use base64::Engine;
        if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(b64) {
            let mut hasher = Sha1::new();
            hasher.update(password.as_bytes());
            let digest = hasher.finalize();
            constant_time_eq_bytes(&digest, &decoded)
        } else {
            false
        }
    } else if stored.starts_with("$apr1$") {
        apr1_verify(stored, password)
    } else {
        // Plaintext fallback
        constant_time_eq_str(stored, password)
    }
}

/// Verify an apr1 (`$apr1$<salt>$<hash>`) MD5-based hash.
fn apr1_verify(stored: &str, password: &str) -> bool {
    let rest = match stored.strip_prefix("$apr1$") {
        Some(r) => r,
        None => return false,
    };
    let (salt, expected) = match rest.split_once('$') {
        Some(pair) => pair,
        None => return false,
    };
    let computed = apr1_md5(password, salt);
    constant_time_eq_str(&computed, expected)
}

/// Compute the apr1 MD5 crypt hash (Apache's `apr_md5_encode`).
fn apr1_md5(password: &str, salt: &str) -> String {
    use md5::{Digest as Md5Digest, Md5};

    let pw = password.as_bytes();
    let salt_bytes = salt.as_bytes();

    // ctx = md5(pw + "$apr1$" + salt) — this context carries into the mixing.
    let mut ctx = Md5::new();
    ctx.update(pw);
    ctx.update(b"$apr1$");
    ctx.update(salt_bytes);

    // d1 = md5(pw + salt + pw)
    let mut ctx1 = Md5::new();
    ctx1.update(pw);
    ctx1.update(salt_bytes);
    ctx1.update(pw);
    let d1 = ctx1.finalize();

    // Odd mixture: feed d1 into ctx in 16-byte chunks.
    let mut pl = pw.len();
    while pl > 0 {
        let chunk = if pl >= 16 { 16 } else { pl };
        ctx.update(&d1[..chunk]);
        pl -= chunk;
    }

    // Even mixture: the C code zeroes `final` here, so when the bit is set it
    // feeds a single 0x00 byte, otherwise one byte of the password.
    let mut i = pw.len();
    while i != 0 {
        if i & 1 == 1 {
            ctx.update([0x00]);
        } else {
            ctx.update(&pw[..1]);
        }
        i >>= 1;
    }

    let mut final_digest = ctx.finalize();

    // 1000 rounds of mixing.
    for round in 0..1000 {
        let mut ctx2 = Md5::new();
        if round & 1 == 1 {
            ctx2.update(pw);
        } else {
            ctx2.update(final_digest);
        }
        if round % 3 != 0 {
            ctx2.update(salt_bytes);
        }
        if round % 7 != 0 {
            ctx2.update(pw);
        }
        if round & 1 == 1 {
            ctx2.update(final_digest);
        } else {
            ctx2.update(pw);
        }
        final_digest = ctx2.finalize();
    }

    encode_apr1(&final_digest)
}

/// The crypt base64 alphabet used by apr1.
const APR1_ALPHABET: &[u8] =
    b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Encode a 16-byte digest in apr1's base64 form (22 chars).
fn encode_apr1(digest: &[u8]) -> String {
    let mut out = String::with_capacity(22);
    let to64 = |v: u32, n: usize, out: &mut String| {
        let mut v = v;
        for _ in 0..n {
            out.push(APR1_ALPHABET[(v & 0x3f) as usize] as char);
            v >>= 6;
        }
    };
    let d = digest;
    to64(
        ((d[0] as u32) << 16) | ((d[6] as u32) << 8) | (d[12] as u32),
        4,
        &mut out,
    );
    to64(
        ((d[1] as u32) << 16) | ((d[7] as u32) << 8) | (d[13] as u32),
        4,
        &mut out,
    );
    to64(
        ((d[2] as u32) << 16) | ((d[8] as u32) << 8) | (d[14] as u32),
        4,
        &mut out,
    );
    to64(
        ((d[3] as u32) << 16) | ((d[9] as u32) << 8) | (d[15] as u32),
        4,
        &mut out,
    );
    to64(
        ((d[4] as u32) << 16) | ((d[10] as u32) << 8) | (d[5] as u32),
        4,
        &mut out,
    );
    to64(d[11] as u32, 2, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWORD: &str = "password123";

    // Vectors generated with `htpasswd` for password "password123".
    const BCRYPT_HASH: &str = "$2y$10$Xvruuk4A0fPDbk9t5SgS/.FMkQ5THbYIdKNkh1zXx/5rVt7QphuaW";
    const APR1_HASH: &str = "$apr1$8d4ohTdK$FWBvPWXVMFDdmFyjURvpN.";
    const SHA_HASH: &str = "{SHA}y/2sYAj5yrQIN4TL0YdPdmGNKpc=";

    #[test]
    fn test_bcrypt_verify() {
        assert!(verify_hash(BCRYPT_HASH, PASSWORD));
        assert!(!verify_hash(BCRYPT_HASH, "wrong"));
    }

    #[test]
    fn test_apr1_verify() {
        assert!(verify_hash(APR1_HASH, PASSWORD));
        assert!(!verify_hash(APR1_HASH, "wrong"));
    }

    #[test]
    fn test_sha_verify() {
        assert!(verify_hash(SHA_HASH, PASSWORD));
        assert!(!verify_hash(SHA_HASH, "wrong"));
    }

    #[test]
    fn test_plaintext_verify() {
        assert!(verify_hash("plainsecret", "plainsecret"));
        assert!(!verify_hash("plainsecret", "wrong"));
    }

    #[test]
    fn test_apr1_known_vector() {
        // Round-trip: our encoder must reproduce the hash we verified.
        let computed = apr1_md5(PASSWORD, "8d4ohTdK");
        assert_eq!(computed, "FWBvPWXVMFDdmFyjURvpN.");
    }

    #[test]
    fn test_token_auth() {
        let auth = Auth::with_token("sekrit".to_string());
        let mut q = HashMap::new();
        q.insert("token".to_string(), "sekrit".to_string());
        assert!(auth.check_request(&q, &HeaderMap::new()));

        q.insert("token".to_string(), "wrong".to_string());
        assert!(!auth.check_request(&q, &HeaderMap::new()));
    }

    #[test]
    fn test_no_auth_allows_all() {
        let auth = Auth::none();
        assert!(!auth.required());
        assert!(auth.check_request(&HashMap::new(), &HeaderMap::new()));
    }

    #[test]
    fn test_decode_basic() {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode("user:pass");
        let (u, p) = decode_basic(&b64).unwrap();
        assert_eq!(u, "user");
        assert_eq!(p, "pass");
    }

    #[test]
    fn test_htpasswd_query_auth() {
        use base64::Engine;
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("htpasswd");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "user:{}", BCRYPT_HASH).unwrap();
        drop(f);

        let auth = Auth::with_htpasswd(&path).unwrap();
        assert!(auth.required());

        let mut q = HashMap::new();
        q.insert(
            "auth".to_string(),
            base64::engine::general_purpose::STANDARD.encode("user:password123"),
        );
        assert!(auth.check_request(&q, &HeaderMap::new()));

        let mut q2 = HashMap::new();
        q2.insert(
            "auth".to_string(),
            base64::engine::general_purpose::STANDARD.encode("user:wrong"),
        );
        assert!(!auth.check_request(&q2, &HeaderMap::new()));
    }
}
