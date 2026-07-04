//! Account operations over the [`AccountStore`] port. The *semantics* live
//! here — uniform failure, constant-time verification (invariant 5) —
//! while the storage backend (memory now, PostgreSQL in M3b) only holds
//! rows. Password hashes cross the store boundary as PHC strings.

use std::sync::{Arc, OnceLock};

use weft_crypto::{PasswordHash, PublicKey};
use weft_proto::{Account, MsgId};
use weft_store::{AccountStore, StoreError};

pub struct Accounts {
    store: Arc<dyn AccountStore>,
}

pub enum RegisterOutcome {
    Created,
    /// Name taken → `ERR CONFLICT` (§6.1).
    Exists,
}

/// Hash verified against when the account does not exist, so
/// unknown-account and wrong-password do the same argon2 work and take the
/// same code path (invariant 5: `AUTH-FAILED` is uniform).
fn dummy_hash() -> &'static PasswordHash {
    static DUMMY: OnceLock<PasswordHash> = OnceLock::new();
    DUMMY.get_or_init(|| PasswordHash::new("weftd-nonexistent-account-dummy"))
}

impl Accounts {
    pub fn new(store: Arc<dyn AccountStore>) -> Self {
        Self { store }
    }

    pub async fn register(
        &self,
        account: &Account,
        password: &str,
    ) -> Result<RegisterOutcome, StoreError> {
        let hash = PasswordHash::new(password);
        Ok(if self.store.register(account, hash.as_phc()).await? {
            RegisterOutcome::Created
        } else {
            RegisterOutcome::Exists
        })
    }

    /// Constant-time, uniform: a missing (or corrupt) stored hash verifies
    /// — and fails — against the dummy instead of returning early.
    pub async fn verify_password(
        &self,
        account: &Account,
        password: &str,
    ) -> Result<bool, StoreError> {
        let stored = self
            .store
            .password_phc(account)
            .await?
            .and_then(|phc| PasswordHash::from_phc(&phc).ok());
        let known = stored.is_some();
        let hash = stored.unwrap_or_else(|| dummy_hash().clone());
        Ok(hash.verify(password) && known)
    }

    pub async fn enroll_device(
        &self,
        account: &Account,
        device: PublicKey,
    ) -> Result<bool, StoreError> {
        self.store.enroll_device(account, *device.as_bytes()).await
    }

    pub async fn device_enrolled(
        &self,
        account: &Account,
        device: &PublicKey,
    ) -> Result<bool, StoreError> {
        self.store.device_enrolled(account, device.as_bytes()).await
    }

    pub async fn set_mark(
        &self,
        account: &Account,
        target: &str,
        msgid: &MsgId,
    ) -> Result<(), StoreError> {
        self.store.set_mark(account, target, msgid).await
    }

    pub async fn marks(&self, account: &Account) -> Result<Vec<(String, MsgId)>, StoreError> {
        self.store.marks(account).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weft_store::MemoryStore;

    fn accounts() -> Accounts {
        Accounts::new(Arc::new(MemoryStore::default()))
    }

    fn account(s: &str) -> Account {
        s.parse().unwrap()
    }

    #[tokio::test]
    async fn register_verify_and_conflict() {
        let accounts = accounts();
        assert!(matches!(
            accounts.register(&account("ada"), "pw-longenough").await,
            Ok(RegisterOutcome::Created)
        ));
        assert!(matches!(
            accounts.register(&account("ada"), "other-password").await,
            Ok(RegisterOutcome::Exists)
        ));
        assert!(accounts
            .verify_password(&account("ada"), "pw-longenough")
            .await
            .unwrap());
        assert!(!accounts
            .verify_password(&account("ada"), "wrong-password")
            .await
            .unwrap());
        // Unknown account takes the same path and fails.
        assert!(!accounts
            .verify_password(&account("ghost"), "pw-longenough")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn device_enrollment() {
        let accounts = accounts();
        accounts
            .register(&account("ada"), "pw-longenough")
            .await
            .unwrap();
        let device = weft_crypto::Keypair::generate().public();
        assert!(!accounts
            .device_enrolled(&account("ada"), &device)
            .await
            .unwrap());
        assert!(accounts
            .enroll_device(&account("ada"), device)
            .await
            .unwrap());
        assert!(accounts
            .device_enrolled(&account("ada"), &device)
            .await
            .unwrap());
        assert!(!accounts
            .enroll_device(&account("ghost"), device)
            .await
            .unwrap());
    }
}
