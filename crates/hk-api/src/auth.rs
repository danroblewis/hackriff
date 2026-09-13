//! Bearer token for the HTTP API and the WebSocket bridge (docs/stream-contract.md §11: remote
//! listeners must authenticate).
//!
//! - One token per server, generated at start from the OS CSPRNG (`/dev/urandom`, 256 bits, hex)
//!   or supplied by configuration (at least [`MIN_TOKEN_LEN`] characters).
//! - Compared in constant time over the full length ([`Token::verify`]).
//! - Clients send it as `Authorization: Bearer <token>` or, where a header cannot be set (the
//!   browser `WebSocket` API), as the `token` query parameter.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};

/// Shortest configured token accepted.
pub const MIN_TOKEN_LEN: usize = 16;

/// An API token.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// A fresh 256-bit token, hex encoded.
    pub fn generate() -> io::Result<Self> {
        let mut raw = [0u8; 32];
        File::open("/dev/urandom")?.read_exact(&mut raw)?;
        Ok(Self(raw.iter().map(|b| format!("{b:02x}")).collect()))
    }

    /// A configured token: [`MIN_TOKEN_LEN`]..=256 printable ASCII characters, no spaces.
    pub fn from_config(value: &str) -> Result<Self, String> {
        let ok_chars = value.bytes().all(|b| b.is_ascii_graphic());
        if !(MIN_TOKEN_LEN..=256).contains(&value.len()) || !ok_chars {
            return Err(format!(
                "token must be {MIN_TOKEN_LEN}..=256 printable ASCII characters without spaces"
            ));
        }
        Ok(Self(value.to_owned()))
    }

    /// The token text (to print the connect URL once at start).
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether `candidate` equals the token. Runs in time that depends only on the lengths: every
    /// byte is compared, and a length mismatch still walks the full token.
    pub fn verify(&self, candidate: &str) -> bool {
        let (a, b) = (self.0.as_bytes(), candidate.as_bytes());
        let mut diff = (a.len() ^ b.len()) as u64;
        for (i, &x) in a.iter().enumerate() {
            let y = b.get(i).copied().unwrap_or(0);
            diff |= u64::from(x ^ y);
        }
        diff == 0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_long_and_distinct() {
        let a = Token::generate().unwrap();
        let b = Token::generate().unwrap();
        assert_eq!(a.expose().len(), 64);
        assert_ne!(a.expose(), b.expose());
        assert!(a.verify(a.expose()));
        assert!(!a.verify(b.expose()));
    }

    #[test]
    fn verify_rejects_prefixes_extensions_and_empty() {
        let t = Token::from_config("0123456789abcdef").unwrap();
        assert!(t.verify("0123456789abcdef"));
        assert!(!t.verify("0123456789abcde"));
        assert!(!t.verify("0123456789abcdef0"));
        assert!(!t.verify(""));
        assert!(!t.verify("0123456789abcdeF"));
        assert!(Token::from_config("short").is_err());
        assert!(Token::from_config("has a space in it!").is_err());
        assert_eq!(format!("{t:?}"), "Token(<redacted>)");
    }
}
