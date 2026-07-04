//! In-memory backend: the test workhorse and the storage for deployments
//! that never leave `ephemeral`-adjacent setups. Also the reference
//! semantics the PostgreSQL backend (M3b) must match.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use weft_proto::{Account, ChannelName, MsgId, RetentionPolicy, Ulid};

use crate::compact::compaction_plan;
use crate::traits::{AccountStore, ChannelStore, EventStore};
use crate::types::{EventRecord, Page, Scope, Verification};
use crate::StoreError;

struct AccountRecord {
    password_phc: String,
    devices: Vec<[u8; 32]>,
    /// target key → read marker (§6.3 MARK).
    marks: HashMap<String, MsgId>,
    /// kind → (subject, verified_at).
    verifications: HashMap<String, (String, Option<u64>)>,
}

#[derive(Default)]
struct Inner {
    /// (scope key, event ulid) → record; BTreeMap gives ordered range
    /// scans per scope — the msgid order IS the channel order (§9.1).
    events: BTreeMap<(String, Ulid), EventRecord>,
    /// Root ulid → its (scope key, ulid) — EDIT/DELETE/REACT lookups
    /// arrive with only a msgid.
    roots: HashMap<Ulid, (String, Ulid)>,
    /// Roots that already carry a tombstone.
    deleted: HashSet<(String, Ulid)>,
    /// Purge watermarks (ms) for honest `truncated` flags.
    watermarks: HashMap<String, u64>,
    accounts: HashMap<Account, AccountRecord>,
    channels: HashMap<ChannelName, RetentionPolicy>,
}

#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
}

impl MemoryStore {
    fn scope_range(key: &str) -> std::ops::RangeInclusive<(String, Ulid)> {
        (key.to_string(), Ulid(0))..=(key.to_string(), Ulid(u128::MAX))
    }
}

#[async_trait]
impl EventStore for MemoryStore {
    async fn append(&self, record: EventRecord) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        let key = record.scope.as_key();
        let ulid = record.msgid.ulid();
        if record.is_root() {
            inner.roots.insert(ulid, (key.clone(), ulid));
        }
        if matches!(record.kind, crate::types::EventKind::Delete) {
            inner.deleted.insert((key.clone(), record.root.ulid()));
        }
        inner.events.insert((key, ulid), record);
        Ok(())
    }

    async fn roots(&self, scope: &Scope, page: Page) -> Result<Vec<EventRecord>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        let key = scope.as_key();
        // Newest-anchored: walk backwards, take `limit`, then flip to
        // ascending — that's the §6.4 "last N before the cursor" page.
        let mut selected: Vec<EventRecord> = inner
            .events
            .range(Self::scope_range(&key))
            .rev()
            .map(|(_, record)| record)
            .filter(|record| record.is_root())
            .filter(|record| {
                let ulid = record.msgid.ulid();
                page.before.map_or(true, |b| ulid < b) && page.after.map_or(true, |a| ulid > a)
            })
            .take(page.limit)
            .cloned()
            .collect();
        selected.reverse();
        Ok(selected)
    }

    async fn children(
        &self,
        scope: &Scope,
        roots: &[Ulid],
    ) -> Result<Vec<EventRecord>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        let wanted: HashSet<&Ulid> = roots.iter().collect();
        Ok(inner
            .events
            .range(Self::scope_range(&scope.as_key()))
            .map(|(_, record)| record)
            .filter(|record| !record.is_root() && wanted.contains(&record.root.ulid()))
            .cloned()
            .collect())
    }

    async fn find_root(&self, ulid: Ulid) -> Result<Option<EventRecord>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .roots
            .get(&ulid)
            .and_then(|key| inner.events.get(key))
            .cloned())
    }

    async fn is_deleted(&self, scope: &Scope, root: Ulid) -> Result<bool, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner.deleted.contains(&(scope.as_key(), root)))
    }

    async fn purge_before(&self, scope: &Scope, cutoff_ms: u64) -> Result<u64, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        Ok(inner.purge_scope(&scope.as_key(), cutoff_ms))
    }

    async fn purged_before(&self, scope: &Scope) -> Result<Option<u64>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner.watermarks.get(&scope.as_key()).copied())
    }

    async fn purge_dms_before(&self, cutoff_ms: u64) -> Result<u64, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        let dm_scopes: Vec<String> = inner
            .events
            .keys()
            .map(|(scope, _)| scope.clone())
            .filter(|scope| scope.starts_with("dm:"))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let mut purged = 0;
        for scope in dm_scopes {
            purged += inner.purge_scope(&scope, cutoff_ms);
        }
        Ok(purged)
    }

    async fn compact_before(&self, cutoff_ms: u64) -> Result<u64, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        // Group every scope's rows into root families, plan, delete.
        let mut families: HashMap<(String, Ulid), Vec<EventRecord>> = HashMap::new();
        for ((scope, _), record) in &inner.events {
            families
                .entry((scope.clone(), record.root.ulid()))
                .or_default()
                .push(record.clone());
        }
        let mut dropped = 0;
        for ((scope, _), family) in families {
            for ulid in compaction_plan(&family, cutoff_ms) {
                if inner.events.remove(&(scope.clone(), ulid)).is_some() {
                    dropped += 1;
                }
            }
        }
        Ok(dropped)
    }
}

impl Inner {
    /// A message expires as a unit: root + children (tombstone included)
    /// go when the ROOT's timestamp passes the cutoff — children never
    /// outlive their message.
    fn purge_scope(&mut self, key: &str, cutoff_ms: u64) -> u64 {
        let expired: HashSet<Ulid> = self
            .events
            .range(MemoryStore::scope_range(key))
            .map(|(_, r)| r)
            .filter(|r| r.is_root() && r.at_ms() < cutoff_ms)
            .map(|r| r.msgid.ulid())
            .collect();
        let doomed: Vec<(String, Ulid)> = self
            .events
            .range(MemoryStore::scope_range(key))
            .filter(|(_, r)| expired.contains(&r.root.ulid()))
            .map(|(k, _)| k.clone())
            .collect();
        for k in &doomed {
            self.events.remove(k);
        }
        for ulid in &expired {
            self.roots.remove(ulid);
            self.deleted.remove(&(key.to_string(), *ulid));
        }
        let watermark = self.watermarks.entry(key.to_string()).or_insert(0);
        *watermark = (*watermark).max(cutoff_ms);
        expired.len() as u64
    }
}

#[async_trait]
impl AccountStore for MemoryStore {
    async fn register(&self, account: &Account, password_phc: &str) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        if inner.accounts.contains_key(account) {
            return Ok(false);
        }
        inner.accounts.insert(
            account.clone(),
            AccountRecord {
                password_phc: password_phc.to_string(),
                devices: Vec::new(),
                marks: HashMap::new(),
                verifications: HashMap::new(),
            },
        );
        Ok(true)
    }

    async fn password_phc(&self, account: &Account) -> Result<Option<String>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .accounts
            .get(account)
            .map(|record| record.password_phc.clone()))
    }

    async fn enroll_device(&self, account: &Account, device: [u8; 32]) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        match inner.accounts.get_mut(account) {
            None => Ok(false),
            Some(record) => {
                if !record.devices.contains(&device) {
                    record.devices.push(device);
                }
                Ok(true)
            }
        }
    }

    async fn device_enrolled(
        &self,
        account: &Account,
        device: &[u8; 32],
    ) -> Result<bool, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .accounts
            .get(account)
            .is_some_and(|record| record.devices.contains(device)))
    }

    async fn set_mark(
        &self,
        account: &Account,
        target: &str,
        msgid: &MsgId,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        if let Some(record) = inner.accounts.get_mut(account) {
            record.marks.insert(target.to_string(), msgid.clone());
        }
        Ok(())
    }

    async fn marks(&self, account: &Account) -> Result<Vec<(String, MsgId)>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .accounts
            .get(account)
            .map(|record| {
                record
                    .marks
                    .iter()
                    .map(|(target, msgid)| (target.clone(), msgid.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn upsert_verification(
        &self,
        account: &Account,
        kind: &str,
        subject: &str,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        if let Some(record) = inner.accounts.get_mut(account) {
            record
                .verifications
                .insert(kind.to_string(), (subject.to_string(), None));
        }
        Ok(())
    }

    async fn confirm_verification(
        &self,
        account: &Account,
        kind: &str,
        verified_at: u64,
    ) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        Ok(inner
            .accounts
            .get_mut(account)
            .and_then(|record| record.verifications.get_mut(kind))
            .map(|(_, at)| *at = Some(verified_at))
            .is_some())
    }

    async fn verifications(&self, account: &Account) -> Result<Vec<Verification>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .accounts
            .get(account)
            .map(|record| {
                record
                    .verifications
                    .iter()
                    .map(|(kind, (subject, verified_at))| Verification {
                        kind: kind.clone(),
                        subject: subject.clone(),
                        verified_at: *verified_at,
                    })
                    .collect()
            })
            .unwrap_or_default())
    }
}

#[async_trait]
impl ChannelStore for MemoryStore {
    async fn upsert_channel(
        &self,
        name: &ChannelName,
        policy: RetentionPolicy,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        inner.channels.insert(name.clone(), policy);
        Ok(())
    }

    async fn list_channels(&self) -> Result<Vec<(ChannelName, RetentionPolicy)>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        let mut channels: Vec<_> = inner
            .channels
            .iter()
            .map(|(name, policy)| (name.clone(), *policy))
            .collect();
        channels.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(channels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EventKind;
    use weft_proto::{MsgId, MsgMeta};

    fn record(scope: &Scope, seq: u64, root_seq: u64, kind: EventKind) -> EventRecord {
        let mid = |s: u64| -> MsgId {
            format!("test.example/{}", Ulid::from_parts(1_000 + s, s as u128))
                .parse()
                .unwrap()
        };
        EventRecord {
            scope: scope.clone(),
            msgid: mid(seq),
            root: mid(root_seq),
            sender: "ada@test.example".parse().unwrap(),
            kind,
        }
    }

    fn message(scope: &Scope, seq: u64) -> EventRecord {
        record(
            scope,
            seq,
            seq,
            EventKind::Message {
                body: format!("m{seq}"),
                meta: MsgMeta::default(),
            },
        )
    }

    #[tokio::test]
    async fn pages_are_newest_anchored_and_ascending() {
        let store = MemoryStore::default();
        let scope = Scope::Channel("#t".parse().unwrap());
        for seq in 1..=9 {
            store.append(message(&scope, seq)).await.unwrap();
        }
        let page = store
            .roots(
                &scope,
                Page {
                    before: None,
                    after: None,
                    limit: 3,
                },
            )
            .await
            .unwrap();
        let bodies: Vec<_> = page
            .iter()
            .map(|r| match &r.kind {
                EventKind::Message { body, .. } => body.as_str(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(bodies, ["m7", "m8", "m9"], "last N, ascending");

        // Page backwards from the oldest of that page.
        let older = store
            .roots(
                &scope,
                Page {
                    before: Some(page[0].msgid.ulid()),
                    after: None,
                    limit: 3,
                },
            )
            .await
            .unwrap();
        assert_eq!(older.len(), 3);
        assert!(older.last().unwrap().msgid < page[0].msgid);
    }

    #[tokio::test]
    async fn scopes_are_isolated() {
        let store = MemoryStore::default();
        let a = Scope::Channel("#a".parse().unwrap());
        let b = Scope::Channel("#b".parse().unwrap());
        store.append(message(&a, 1)).await.unwrap();
        store.append(message(&b, 2)).await.unwrap();
        let page = Page {
            before: None,
            after: None,
            limit: 10,
        };
        assert_eq!(store.roots(&a, page).await.unwrap().len(), 1);
        assert_eq!(store.roots(&b, page).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn find_root_and_tombstone_tracking() {
        let store = MemoryStore::default();
        let scope = Scope::Channel("#t".parse().unwrap());
        let msg = message(&scope, 1);
        let root_ulid = msg.msgid.ulid();
        store.append(msg).await.unwrap();

        let found = store.find_root(root_ulid).await.unwrap().unwrap();
        assert_eq!(found.msgid.ulid(), root_ulid);
        assert!(!store.is_deleted(&scope, root_ulid).await.unwrap());

        store
            .append(record(&scope, 2, 1, EventKind::Delete))
            .await
            .unwrap();
        assert!(store.is_deleted(&scope, root_ulid).await.unwrap());
        // Children are not roots.
        assert!(store
            .find_root(Ulid::from_parts(1_002, 2))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn purge_drops_whole_messages_and_sets_watermark() {
        let store = MemoryStore::default();
        let scope = Scope::Channel("#t".parse().unwrap());
        store.append(message(&scope, 1)).await.unwrap(); // at 1001 ms
                                                         // A LATE child of the old root: dies with its message.
        store
            .append(record(&scope, 50, 1, EventKind::Delete))
            .await
            .unwrap();
        store.append(message(&scope, 100)).await.unwrap(); // at 1100 ms

        assert_eq!(store.purge_before(&scope, 1_050).await.unwrap(), 1);
        assert_eq!(store.purged_before(&scope).await.unwrap(), Some(1_050));
        let page = Page {
            before: None,
            after: None,
            limit: 10,
        };
        let remaining = store.roots(&scope, page).await.unwrap();
        assert_eq!(remaining.len(), 1);
        // The late tombstone went with its root.
        let children = store
            .children(&scope, &[Ulid::from_parts(1_001, 1)])
            .await
            .unwrap();
        assert!(children.is_empty());
        // Watermark never regresses.
        store.purge_before(&scope, 900).await.unwrap();
        assert_eq!(store.purged_before(&scope).await.unwrap(), Some(1_050));
    }

    #[tokio::test]
    async fn dm_scope_normalizes_participant_order() {
        let ada: Account = "ada".parse().unwrap();
        let bob: Account = "bob".parse().unwrap();
        assert_eq!(
            Scope::dm(ada.clone(), bob.clone()).as_key(),
            Scope::dm(bob, ada).as_key()
        );
    }
}
