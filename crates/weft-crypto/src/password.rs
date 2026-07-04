//! Password verification (§6.1: "password compares constant-time",
//! security invariant 5).
//!
//! M2 holds accounts in memory only, so this is a plain SHA-256 digest
//! compared in constant time — enough to make verification timing
//! independent of *where* two passwords differ. When M3 persists accounts,
//! upgrade to a real KDF (argon2) before hashes ever touch disk.

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordHash([u8; 32]);

impl PasswordHash {
    pub fn new(password: &str) -> Self {
        Self(Sha256::digest(password.as_bytes()).into())
    }

    /// Constant-time comparison — never early-exits on a prefix match.
    pub fn verify(&self, password: &str) -> bool {
        let candidate: [u8; 32] = Sha256::digest(password.as_bytes()).into();
        candidate.ct_eq(&self.0).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_only_the_exact_password() {
        let hash = PasswordHash::new("correct horse battery");
        assert!(hash.verify("correct horse battery"));
        assert!(!hash.verify("correct horse batterz"));
        assert!(!hash.verify(""));
    }
}
