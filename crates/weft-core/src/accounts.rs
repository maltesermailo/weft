//! In-memory account registry: password hashes + enrolled device keys.
//!
//! M2-scoped: accounts live for the process (a restart forgets them). M3
//! introduces the `AccountStore` trait in weft-store with a PostgreSQL
//! implementation; this type becomes its memory backend. The *semantics*
//! here — uniform failure, constant-time verification — are permanent.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use weft_crypto::{PasswordHash, PublicKey};
use weft_proto::Account;

struct Record {
    password: PasswordHash,
    devices: Vec<PublicKey>,
}

#[derive(Default)]
pub struct Accounts {
    records: Mutex<HashMap<Account, Record>>,
}

pub enum RegisterOutcome {
    Created,
    /// Name taken → `ERR CONFLICT` (§6.1).
    Exists,
}

/// Hash compared against when the account does not exist, so unknown-account
/// and wrong-password take the same code path AND the same time
/// (security invariant 5: `AUTH-FAILED` is uniform).
fn dummy_hash() -> &'static PasswordHash {
    static DUMMY: OnceLock<PasswordHash> = OnceLock::new();
    DUMMY.get_or_init(|| PasswordHash::new("weftd-nonexistent-account-dummy"))
}

impl Accounts {
    pub fn register(&self, account: &Account, password: &str) -> RegisterOutcome {
        let mut records = self.records.lock().expect("accounts lock");
        if records.contains_key(account) {
            return RegisterOutcome::Exists;
        }
        records.insert(
            account.clone(),
            Record {
                password: PasswordHash::new(password),
                devices: Vec::new(),
            },
        );
        RegisterOutcome::Created
    }

    /// Constant-time, uniform: a missing account verifies (and fails)
    /// against a dummy hash instead of returning early.
    pub fn verify_password(&self, account: &Account, password: &str) -> bool {
        let records = self.records.lock().expect("accounts lock");
        let record = records.get(account);
        let hash = record.map_or(dummy_hash(), |r| &r.password);
        let matched = hash.verify(password);
        matched && record.is_some()
    }

    /// Add a device key (idempotent). False iff the account is unknown —
    /// unreachable from the session layer, which only enrolls while authed.
    pub fn enroll_device(&self, account: &Account, device: PublicKey) -> bool {
        let mut records = self.records.lock().expect("accounts lock");
        match records.get_mut(account) {
            None => false,
            Some(record) => {
                if !record.devices.contains(&device) {
                    record.devices.push(device);
                }
                true
            }
        }
    }

    pub fn device_enrolled(&self, account: &Account, device: &PublicKey) -> bool {
        let records = self.records.lock().expect("accounts lock");
        records
            .get(account)
            .is_some_and(|record| record.devices.contains(device))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(s: &str) -> Account {
        s.parse().unwrap()
    }

    #[test]
    fn register_verify_and_conflict() {
        let accounts = Accounts::default();
        assert!(matches!(
            accounts.register(&account("ada"), "pw-longenough"),
            RegisterOutcome::Created
        ));
        assert!(matches!(
            accounts.register(&account("ada"), "other-password"),
            RegisterOutcome::Exists
        ));
        assert!(accounts.verify_password(&account("ada"), "pw-longenough"));
        assert!(!accounts.verify_password(&account("ada"), "wrong-password"));
        // Unknown account takes the same path and fails.
        assert!(!accounts.verify_password(&account("ghost"), "pw-longenough"));
    }

    #[test]
    fn device_enrollment() {
        let accounts = Accounts::default();
        accounts.register(&account("ada"), "pw-longenough");
        let device = weft_crypto::Keypair::generate().public();
        assert!(!accounts.device_enrolled(&account("ada"), &device));
        assert!(accounts.enroll_device(&account("ada"), device));
        assert!(accounts.enroll_device(&account("ada"), device)); // idempotent
        assert!(accounts.device_enrolled(&account("ada"), &device));
        assert!(!accounts.enroll_device(&account("ghost"), device));
    }
}
