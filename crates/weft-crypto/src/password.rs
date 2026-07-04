//! Password verification (§6.1: "password compares constant-time",
//! security invariant 5).
//!
//! Argon2id with default parameters, carried as PHC strings — the format
//! that persists to the account store (M3) without this crate knowing
//! anything about storage. Argon2 verification recomputes the full hash
//! regardless of where a wrong password differs, which is what makes the
//! timing uniform; the uniform *unknown-account* path lives in weft-core
//! (dummy-hash verify).

use argon2::password_hash::{PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

use crate::CryptoError;

/// An argon2id hash in PHC string form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordHash(String);

impl PasswordHash {
    pub fn new(password: &str) -> Self {
        let salt = SaltString::generate(&mut rand::rngs::OsRng);
        let phc = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .expect("argon2 default params are valid")
            .to_string();
        Self(phc)
    }

    pub fn verify(&self, password: &str) -> bool {
        argon2::PasswordHash::new(&self.0)
            .map(|parsed| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &parsed)
                    .is_ok()
            })
            .unwrap_or(false)
    }

    /// The PHC string, e.g. for the account store.
    pub fn as_phc(&self) -> &str {
        &self.0
    }

    /// Re-hydrate a stored PHC string (validated).
    pub fn from_phc(phc: &str) -> Result<Self, CryptoError> {
        argon2::PasswordHash::new(phc).map_err(|_| CryptoError::BadEncoding)?;
        Ok(Self(phc.to_string()))
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

    #[test]
    fn phc_round_trips_through_storage_form() {
        let hash = PasswordHash::new("correct horse battery");
        let restored = PasswordHash::from_phc(hash.as_phc()).unwrap();
        assert!(restored.verify("correct horse battery"));
        assert!(PasswordHash::from_phc("not-a-phc-string").is_err());
    }

    #[test]
    fn salts_differ_per_hash() {
        // Same password, different PHC strings — no rainbow-table reuse.
        assert_ne!(
            PasswordHash::new("same password!").as_phc(),
            PasswordHash::new("same password!").as_phc()
        );
    }
}
