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

    /// The account's immutable ULID (§10.4), or `None` if unknown.
    pub async fn account_ulid(&self, account: &Account) -> Result<Option<String>, StoreError> {
        self.store.account_ulid(account).await
    }

    /// WC7: whether the account is suspended (blocked from authenticating).
    pub async fn is_suspended(&self, account: &Account) -> Result<bool, StoreError> {
        self.store.is_suspended(account).await
    }

    /// WC7: suspend/unsuspend an account. False iff unknown.
    pub async fn set_suspended(
        &self,
        account: &Account,
        suspended: bool,
    ) -> Result<bool, StoreError> {
        self.store.set_suspended(account, suspended).await
    }

    /// §10.4: whether the account holds operator authority (DB-backed).
    pub async fn is_operator(&self, account: &Account) -> Result<bool, StoreError> {
        self.store.is_operator(account).await
    }

    /// §10.4: grant/revoke operator authority. False iff the account is unknown.
    pub async fn set_operator(
        &self,
        account: &Account,
        operator: bool,
    ) -> Result<bool, StoreError> {
        self.store.set_operator(account, operator).await
    }

    /// §10.4: every operator account.
    pub async fn list_operators(&self) -> Result<Vec<Account>, StoreError> {
        self.store.list_operators().await
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

    /// §10.5 record (or replace) a verification claim — starts unverified.
    pub async fn upsert_verification(
        &self,
        account: &Account,
        kind: &str,
        subject: &str,
    ) -> Result<(), StoreError> {
        self.store.upsert_verification(account, kind, subject).await
    }

    /// §10.5 confirm a pending claim. `false` = no such claim.
    pub async fn confirm_verification(
        &self,
        account: &Account,
        kind: &str,
        verified_at: u64,
    ) -> Result<bool, StoreError> {
        self.store
            .confirm_verification(account, kind, verified_at)
            .await
    }

    /// §10.5 the account's verification claims.
    pub async fn verifications(
        &self,
        account: &Account,
    ) -> Result<Vec<weft_store::Verification>, StoreError> {
        self.store.verifications(account).await
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
