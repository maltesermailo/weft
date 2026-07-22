//! In-memory backend: the test workhorse and the storage for deployments
//! that never leave `ephemeral`-adjacent setups. Also the reference
//! semantics the PostgreSQL backend (M3b) must match.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use weft_proto::{Account, ChannelName, MsgId, NamespaceName, NetworkName, RetentionPolicy, Ulid};

use crate::blob::BlobRecord;
use crate::compact::compaction_plan;
use crate::traits::{
    AccountStore, AuditStore, CapabilityStore, ChannelStore, EmojiStore, EventStore, InviteStore,
    MediaBlocklistStore, MediaStore, MembershipStore, ModerationStore, NamespaceStore,
    NetblockStore, PeerStore, PinStore, ProfileStore, ReportStore, RoleStore, HOLD_RADIUS,
};
use crate::types::{
    audit_hash, AuditEntry, AuditRecord, ChannelRecord, EventRecord, GrantRecord, InviteRecord,
    MediaBlockRecord, ModKind, ModRecord, NamespaceRecord, NetblockRecord, Page, PeerRecord,
    PendingRecovery, ProfileRecord, RedeemOutcome, ReportRecord, ReportResolution, RoleDef,
    RootHistoryEntry, Scope, Verification, AUDIT_GENESIS,
};
use crate::StoreError;
use weft_proto::{ContentState, ReportStatus};

struct AccountRecord {
    password_phc: String,
    /// Immutable per-account ULID — the stable cap-subject key (§10.4).
    ulid: String,
    devices: Vec<[u8; 32]>,
    /// target key → read marker (§6.3 MARK).
    marks: HashMap<String, MsgId>,
    /// kind → (subject, verified_at).
    verifications: HashMap<String, (String, Option<u64>)>,
    /// WC3 soft delete: scheduled hard-delete time (ms), `None` when not pending.
    purge_at: Option<u64>,
    /// WC7 moderation: suspended accounts can't authenticate.
    suspended: bool,
    /// §10.4 operator authority (managed via `weftd admin`).
    operator: bool,
}

/// The domain of an email address — the lowercased part after the last `@`
/// (empty if there is none). Matches PG's `split_part(subject,'@',2)`.
fn email_domain(email: &str) -> String {
    email
        .rsplit('@')
        .next()
        .filter(|_| email.contains('@'))
        .unwrap_or("")
        .to_lowercase()
}

/// A stored role's value: (color, caps, hoist, position).
type RoleEntry = (String, Vec<String>, bool, i32);

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
    channels: HashMap<ChannelName, ChannelRecord>,
    /// (subject, scope) → grant.
    grants: HashMap<(String, String), GrantRecord>,
    /// scope → revocation epoch.
    epochs: HashMap<String, u64>,
    /// invite id → record.
    invites: HashMap<String, InviteRecord>,
    /// namespace name → record.
    namespaces: HashMap<NamespaceName, NamespaceRecord>,
    /// namespace name → append-only root rotation audit (§2.4).
    root_history: HashMap<NamespaceName, Vec<RootHistoryEntry>>,
    /// report id → record (§6.7).
    reports: HashMap<String, ReportRecord>,
    /// (scope key, root ulid) → number of reports holding it. A root is
    /// under a retention hold while its count > 0 — purge/compaction skip
    /// it (invariant 11). Refcounting handles overlapping report contexts.
    holds: HashMap<(String, Ulid), u32>,
    /// peer network → bridge peering + signed manifests (§11.1).
    peers: HashMap<NetworkName, PeerRecord>,
    /// blocked network name → blocklist entry (§11.6, name-keyed).
    netblocks: HashMap<NetworkName, NetblockRecord>,
    /// blocked BLAKE3 media hash → blocklist entry (§13, content-addressed).
    blocked_hashes: HashMap<String, MediaBlockRecord>,
    /// WC1 admin audit log, in append order — index i is `seq == i + 1`. The
    /// `Vec` position IS the chain order (single-writer, like ULID minting).
    audit: Vec<AuditRecord>,
    /// (scope, account, kind) → moderation deny record (§6.7).
    moderation: HashMap<(String, Account, ModKind), ModRecord>,
    /// channel → pinned msgids, ordered by ULID (§6.4).
    pins: HashMap<ChannelName, std::collections::BTreeMap<Ulid, MsgId>>,
    /// §9.4 custom emoji: namespace → (name → media ref), name-sorted.
    emoji: HashMap<NamespaceName, std::collections::BTreeMap<String, String>>,
    /// account → channels it's a member of (§6.3 persistent membership).
    memberships: HashMap<Account, std::collections::HashSet<ChannelName>>,
    /// scope → role name → (color, caps) (§6.5 role definitions).
    // scope → name → (color, caps, hoist, position)
    roles: HashMap<String, std::collections::BTreeMap<String, RoleEntry>>,
    /// Explicit role membership: (scope, role name, account).
    /// (scope, role name, subject) — subject is a local name or `account@network`.
    role_assignments: HashSet<(String, String, String)>,
    /// §13 media: blob hash → full metadata record (mime, size, dims, thumb,
    /// created_ms). The `created_ms` doubles as the GC grace anchor.
    blobs: HashMap<String, BlobRecord>,
    /// §13 media reference rows: (scope, msgid, blob hash).
    media_refs: Vec<(Scope, MsgId, String)>,
    /// §10.3 account handle → display profile (nick + avatar hash).
    profiles: HashMap<String, ProfileRecord>,
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

    async fn unread_counts(
        &self,
        scope: &Scope,
        account: &Account,
        since: Ulid,
    ) -> Result<(u64, u64), StoreError> {
        let inner = self.inner.lock().expect("store lock");
        let at_account = format!("@{account}");
        let mut unread = 0u64;
        let mut mentions = 0u64;
        for (_, record) in inner.events.range(Self::scope_range(&scope.as_key())) {
            // Only real (non-system) root messages from other senders, newer
            // than the marker, count — join/part system rows never do.
            let crate::types::EventKind::Message { body, meta } = &record.kind else {
                continue;
            };
            if meta.system.is_some()
                || record.msgid.ulid() <= since
                || record.sender.account == *account
            {
                continue;
            }
            unread += 1;
            if body.contains(&at_account) || body.contains("@everyone") || body.contains("@here") {
                mentions += 1;
            }
        }
        Ok((unread, mentions))
    }

    async fn search(
        &self,
        scope: &Scope,
        query: &str,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        let key = scope.as_key();
        let needle = query.to_lowercase();
        let hits: Vec<EventRecord> = inner
            .events
            .range(Self::scope_range(&key))
            .rev() // newest-first
            .map(|(_, record)| record)
            .filter(|record| {
                let crate::types::EventKind::Message { body, meta } = &record.kind else {
                    return false;
                };
                meta.system.is_none()
                    && !inner.deleted.contains(&(key.clone(), record.msgid.ulid()))
                    && body.to_lowercase().contains(&needle)
            })
            .take(limit)
            .cloned()
            .collect();
        Ok(hits)
    }

    async fn thread_roots(
        &self,
        scope: &Scope,
        root: &MsgId,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        let hits: Vec<EventRecord> = inner
            .events
            .range(Self::scope_range(&scope.as_key())) // ascending = oldest-first
            .map(|(_, record)| record)
            .filter(|record| {
                let crate::types::EventKind::Message { meta, .. } = &record.kind else {
                    return false;
                };
                &record.msgid == root || meta.thread.as_ref() == Some(root)
            })
            .take(limit)
            .cloned()
            .collect();
        Ok(hits)
    }

    async fn find_root(&self, ulid: Ulid) -> Result<Option<EventRecord>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .roots
            .get(&ulid)
            .and_then(|key| inner.events.get(key))
            .cloned())
    }

    async fn messages_by_sender(
        &self,
        sender: &str,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        let mut hits: Vec<EventRecord> = inner
            .events
            .values()
            .filter(|r| r.is_root() && r.sender.to_string() == sender)
            .cloned()
            .collect();
        // Newest-first (ULID is time-ordered); the msgid IS the key order.
        hits.sort_by(|a, b| b.msgid.ulid().cmp(&a.msgid.ulid()));
        hits.truncate(limit);
        Ok(hits)
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
        for ((scope, root), family) in families {
            // Retention hold: a held message family is exempt from
            // compaction until its report resolves + grace (invariant 11).
            if inner.holds.contains_key(&(scope.clone(), root)) {
                continue;
            }
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
            // Retention hold: a held root survives purge until its report
            // resolves + grace (invariant 11).
            .filter(|r| !self.holds.contains_key(&(key.to_string(), r.msgid.ulid())))
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

    /// The reported root plus up to `radius` roots on each side, in the same
    /// scope — the §12.1 hold context. Returns roots that actually exist
    /// (an expired-context report simply holds fewer).
    fn context_roots(&self, key: &str, root: Ulid, radius: usize) -> Vec<Ulid> {
        let roots: Vec<Ulid> = self
            .events
            .range(MemoryStore::scope_range(key))
            .map(|(_, r)| r)
            .filter(|r| r.is_root())
            .map(|r| r.msgid.ulid())
            .collect();
        match roots.iter().position(|u| *u == root) {
            None => Vec::new(),
            Some(i) => roots[i.saturating_sub(radius)..(i + radius + 1).min(roots.len())].to_vec(),
        }
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
                ulid: weft_proto::Ulid::new().to_string(),
                devices: Vec::new(),
                marks: HashMap::new(),
                verifications: HashMap::new(),
                purge_at: None,
                suspended: false,
                operator: false,
            },
        );
        Ok(true)
    }

    async fn account_ulid(&self, account: &Account) -> Result<Option<String>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner.accounts.get(account).map(|a| a.ulid.clone()))
    }

    async fn password_phc(&self, account: &Account) -> Result<Option<String>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .accounts
            .get(account)
            .map(|record| record.password_phc.clone()))
    }

    async fn list_accounts(&self) -> Result<Vec<Account>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        let mut names: Vec<Account> = inner.accounts.keys().cloned().collect();
        names.sort();
        Ok(names)
    }

    async fn delete_account(&self, account: &Account) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        let Some(record) = inner.accounts.remove(account) else {
            return Ok(false); // unknown — messages/devices/marks lived here
        };
        // Cascade the per-account data. Grants key by the account's stable ULID;
        // moderation + memberships key by the account name.
        let ulid = record.ulid;
        inner.memberships.remove(account);
        inner.grants.retain(|(subject, _), _| subject != &ulid);
        inner.moderation.retain(|(_, acct, _), _| acct != account);
        inner
            .role_assignments
            .retain(|(_, _, subject)| subject != account.as_str());
        Ok(true)
    }

    async fn schedule_deletion(
        &self,
        account: &Account,
        purge_at_ms: u64,
    ) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        match inner.accounts.get_mut(account) {
            Some(record) => {
                record.purge_at = Some(purge_at_ms);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn cancel_deletion(&self, account: &Account) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        Ok(inner
            .accounts
            .get_mut(account)
            .is_some_and(|r| r.purge_at.take().is_some()))
    }

    async fn deletion_scheduled(&self, account: &Account) -> Result<Option<u64>, StoreError> {
        Ok(self
            .inner
            .lock()
            .expect("store lock")
            .accounts
            .get(account)
            .and_then(|r| r.purge_at))
    }

    async fn due_deletions(&self, now_ms: u64) -> Result<Vec<Account>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        let mut due: Vec<Account> = inner
            .accounts
            .iter()
            .filter(|(_, r)| r.purge_at.is_some_and(|at| at <= now_ms))
            .map(|(a, _)| a.clone())
            .collect();
        due.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(due)
    }

    async fn set_suspended(&self, account: &Account, suspended: bool) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        match inner.accounts.get_mut(account) {
            Some(record) => {
                record.suspended = suspended;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn is_suspended(&self, account: &Account) -> Result<bool, StoreError> {
        Ok(self
            .inner
            .lock()
            .expect("store lock")
            .accounts
            .get(account)
            .is_some_and(|r| r.suspended))
    }

    async fn set_operator(&self, account: &Account, operator: bool) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        match inner.accounts.get_mut(account) {
            Some(record) => {
                record.operator = operator;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn is_operator(&self, account: &Account) -> Result<bool, StoreError> {
        Ok(self
            .inner
            .lock()
            .expect("store lock")
            .accounts
            .get(account)
            .is_some_and(|r| r.operator))
    }

    async fn list_operators(&self) -> Result<Vec<Account>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        let mut ops: Vec<Account> = inner
            .accounts
            .iter()
            .filter(|(_, r)| r.operator)
            .map(|(name, _)| name.clone())
            .collect();
        ops.sort();
        Ok(ops)
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

    async fn devices(&self, account: &Account) -> Result<Vec<[u8; 32]>, StoreError> {
        Ok(self
            .inner
            .lock()
            .expect("store lock")
            .accounts
            .get(account)
            .map(|r| r.devices.clone())
            .unwrap_or_default())
    }

    async fn accounts_by_email_domain(&self, domain: &str) -> Result<Vec<Account>, StoreError> {
        let want = domain.to_lowercase();
        let inner = self.inner.lock().expect("store lock");
        let mut out: Vec<Account> = inner
            .accounts
            .iter()
            .filter(|(_, r)| {
                r.verifications
                    .get("email")
                    .is_some_and(|(subject, _)| email_domain(subject) == want)
            })
            .map(|(a, _)| a.clone())
            .collect();
        out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(out)
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
        kind: weft_proto::ChannelKind,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        inner
            .channels
            .entry(name.clone())
            // §16 kind is immutable after creation — only `policy` updates.
            .and_modify(|record| record.policy = policy)
            .or_insert(ChannelRecord {
                policy,
                topic: None,
                view_gated: false,
                restricted: false,
                category: None,
                position: 0,
                kind,
            });
        Ok(())
    }

    async fn list_channels(&self) -> Result<Vec<(ChannelName, RetentionPolicy)>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        let mut channels: Vec<_> = inner
            .channels
            .iter()
            .map(|(name, record)| (name.clone(), record.policy))
            .collect();
        channels.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(channels)
    }

    async fn channel(&self, name: &ChannelName) -> Result<Option<ChannelRecord>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner.channels.get(name).cloned())
    }

    async fn set_channel_topic(&self, name: &ChannelName, topic: &str) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        if let Some(record) = inner.channels.get_mut(name) {
            record.topic = Some(topic.to_string());
        }
        Ok(())
    }

    async fn set_channel_view_gated(
        &self,
        name: &ChannelName,
        gated: bool,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        if let Some(record) = inner.channels.get_mut(name) {
            record.view_gated = gated;
        }
        Ok(())
    }

    async fn set_channel_restricted(
        &self,
        name: &ChannelName,
        restricted: bool,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        if let Some(record) = inner.channels.get_mut(name) {
            record.restricted = restricted;
        }
        Ok(())
    }

    async fn delete_channel(&self, name: &ChannelName) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        Ok(inner.channels.remove(name).is_some())
    }

    async fn rename_channel(
        &self,
        old: &ChannelName,
        new: &ChannelName,
    ) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        // Old must exist; new must be free.
        if !inner.channels.contains_key(old) || inner.channels.contains_key(new) {
            return Ok(false);
        }
        let ok = old.to_string();
        let nk = new.to_string();

        // 1. channel record (the name is the map key only; no inner field).
        if let Some(rec) = inner.channels.remove(old) {
            inner.channels.insert(new.clone(), rec);
        }
        // 2. events — re-scope every (scope, ulid) entry.
        let ev: Vec<(String, Ulid)> = inner
            .events
            .keys()
            .filter(|(s, _)| *s == ok)
            .cloned()
            .collect();
        for k in ev {
            if let Some(v) = inner.events.remove(&k) {
                inner.events.insert((nk.clone(), k.1), v);
            }
        }
        // 3. root index — value carries the scope key.
        for (s, _) in inner.roots.values_mut() {
            if *s == ok {
                *s = nk.clone();
            }
        }
        // 4. tombstoned roots.
        let del: Vec<(String, Ulid)> = inner
            .deleted
            .iter()
            .filter(|(s, _)| *s == ok)
            .cloned()
            .collect();
        for k in del {
            inner.deleted.remove(&k);
            inner.deleted.insert((nk.clone(), k.1));
        }
        // 5. purge watermark.
        if let Some(w) = inner.watermarks.remove(&ok) {
            inner.watermarks.insert(nk.clone(), w);
        }
        // 6. capability grants (key scope + record scope).
        let gk: Vec<(String, String)> = inner
            .grants
            .keys()
            .filter(|(_, s)| *s == ok)
            .cloned()
            .collect();
        for k in gk {
            if let Some(mut rec) = inner.grants.remove(&k) {
                rec.scope = nk.clone();
                inner.grants.insert((k.0, nk.clone()), rec);
            }
        }
        // 7. revocation epoch.
        if let Some(e) = inner.epochs.remove(&ok) {
            inner.epochs.insert(nk.clone(), e);
        }
        // 8. retention holds (invariant 11 — they follow the content).
        let hk: Vec<(String, Ulid)> = inner
            .holds
            .keys()
            .filter(|(s, _)| *s == ok)
            .cloned()
            .collect();
        for k in hk {
            if let Some(v) = inner.holds.remove(&k) {
                inner.holds.insert((nk.clone(), k.1), v);
            }
        }
        // 9. moderation deny-list (key scope + record scope).
        let mk: Vec<(String, Account, ModKind)> = inner
            .moderation
            .keys()
            .filter(|(s, _, _)| *s == ok)
            .cloned()
            .collect();
        for k in mk {
            if let Some(mut rec) = inner.moderation.remove(&k) {
                rec.scope = nk.clone();
                inner.moderation.insert((nk.clone(), k.1, k.2), rec);
            }
        }
        // 10. pins.
        if let Some(p) = inner.pins.remove(old) {
            inner.pins.insert(new.clone(), p);
        }
        // 11. memberships (per account).
        for set in inner.memberships.values_mut() {
            if set.remove(old) {
                set.insert(new.clone());
            }
        }
        // 12. channel-scoped role definitions.
        if let Some(r) = inner.roles.remove(&ok) {
            inner.roles.insert(nk.clone(), r);
        }
        // 13. channel-scoped role assignments.
        let ra: Vec<(String, String, String)> = inner
            .role_assignments
            .iter()
            .filter(|(s, _, _)| *s == ok)
            .cloned()
            .collect();
        for k in ra {
            inner.role_assignments.remove(&k);
            inner.role_assignments.insert((nk.clone(), k.1, k.2));
        }
        // 14. per-account read markers (§6.3 MARK).
        for acct in inner.accounts.values_mut() {
            if let Some(m) = acct.marks.remove(&ok) {
                acct.marks.insert(nk.clone(), m);
            }
        }
        Ok(true)
    }

    async fn set_channel_layout(
        &self,
        name: &ChannelName,
        category: Option<&str>,
        position: i64,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        if let Some(record) = inner.channels.get_mut(name) {
            record.category = category.map(str::to_string);
            record.position = position;
        }
        Ok(())
    }

    async fn channels_in_namespace(
        &self,
        namespace: &str,
    ) -> Result<Vec<(ChannelName, ChannelRecord)>, StoreError> {
        let prefix = format!("#{namespace}/");
        let inner = self.inner.lock().expect("store lock");
        let mut out: Vec<(ChannelName, ChannelRecord)> = inner
            .channels
            .iter()
            .filter(|(name, _)| name.as_str().starts_with(&prefix))
            .map(|(name, record)| (name.clone(), record.clone()))
            .collect();
        out.sort_by(|(an, ar), (bn, br)| {
            ar.category
                .cmp(&br.category)
                .then(ar.position.cmp(&br.position))
                .then(an.cmp(bn))
        });
        Ok(out)
    }
}

#[async_trait]
impl CapabilityStore for MemoryStore {
    async fn record_grant(
        &self,
        subject: &str,
        scope: &str,
        caps: &[String],
        epoch: u64,
        expiry: Option<u64>,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        inner.grants.insert(
            (subject.to_string(), scope.to_string()),
            GrantRecord {
                subject: subject.to_string(),
                scope: scope.to_string(),
                caps: caps.to_vec(),
                epoch,
                expiry,
            },
        );
        Ok(())
    }

    async fn grants_for(&self, subject: &str) -> Result<Vec<GrantRecord>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .grants
            .values()
            .filter(|g| g.subject == subject)
            .cloned()
            .collect())
    }

    async fn grants_at_scope(&self, scope: &str) -> Result<Vec<GrantRecord>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .grants
            .values()
            .filter(|g| g.scope == scope)
            .cloned()
            .collect())
    }

    async fn revoke_grants(
        &self,
        subject: &str,
        scope: &str,
        caps: Option<&[String]>,
    ) -> Result<u64, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        let key = (subject.to_string(), scope.to_string());
        match caps {
            None => Ok(inner.grants.remove(&key).is_some() as u64),
            Some(drop) => {
                let Some(grant) = inner.grants.get_mut(&key) else {
                    return Ok(0);
                };
                let before = grant.caps.len();
                grant.caps.retain(|c| !drop.contains(c));
                let removed = (before - grant.caps.len()) as u64;
                if grant.caps.is_empty() {
                    inner.grants.remove(&key);
                }
                Ok(removed)
            }
        }
    }

    async fn scope_epoch(&self, scope: &str) -> Result<u64, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner.epochs.get(scope).copied().unwrap_or(0))
    }

    async fn bump_epoch(&self, scope: &str) -> Result<u64, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        let epoch = inner.epochs.entry(scope.to_string()).or_insert(0);
        *epoch += 1;
        Ok(*epoch)
    }
}

#[async_trait]
impl InviteStore for MemoryStore {
    async fn create_invite(&self, invite: InviteRecord) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        inner.invites.insert(invite.id.clone(), invite);
        Ok(())
    }

    async fn invite(&self, id: &str) -> Result<Option<InviteRecord>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner.invites.get(id).cloned())
    }

    async fn redeem_invite(&self, id: &str, now: u64) -> Result<RedeemOutcome, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        let Some(invite) = inner.invites.get_mut(id) else {
            return Ok(RedeemOutcome::Gone);
        };
        if invite.expiry.is_some_and(|e| now >= e) {
            return Ok(RedeemOutcome::Gone);
        }
        match invite.uses_left {
            Some(0) => Ok(RedeemOutcome::Exhausted),
            Some(n) => {
                invite.uses_left = Some(n - 1);
                Ok(RedeemOutcome::Redeemed(invite.clone()))
            }
            None => Ok(RedeemOutcome::Redeemed(invite.clone())),
        }
    }

    async fn revoke_invite(&self, id: &str) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        Ok(inner.invites.remove(id).is_some())
    }

    async fn revoke_invites_for_namespace(&self, ns: &str) -> Result<u64, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        let ns_scope = format!("ns:{ns}");
        let chan_prefix = format!("#{ns}/");
        let before = inner.invites.len();
        inner
            .invites
            .retain(|_, inv| inv.scope != ns_scope && !inv.scope.starts_with(&chan_prefix));
        Ok((before - inner.invites.len()) as u64)
    }
}

#[async_trait]
impl NamespaceStore for MemoryStore {
    async fn create_namespace(&self, record: NamespaceRecord) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        if inner.namespaces.contains_key(&record.name) {
            return Ok(false);
        }
        inner.namespaces.insert(record.name.clone(), record);
        Ok(true)
    }

    async fn namespace(&self, name: &NamespaceName) -> Result<Option<NamespaceRecord>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner.namespaces.get(name).cloned())
    }

    async fn namespaces_owned(&self, owner: &str) -> Result<u64, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .namespaces
            .values()
            .filter(|ns| ns.owner.as_str() == owner)
            .count() as u64)
    }

    async fn list_public(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<NamespaceRecord>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        let mut public: Vec<NamespaceRecord> = inner
            .namespaces
            .values()
            .filter(|ns| ns.visibility == "public")
            .filter(|ns| after.map_or(true, |cursor| ns.name.as_str() > cursor))
            .cloned()
            .collect();
        public.sort_by(|a, b| a.name.cmp(&b.name));
        public.truncate(limit);
        Ok(public)
    }

    async fn set_namespace_meta(
        &self,
        name: &NamespaceName,
        key: &str,
        value: &str,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        if let Some(ns) = inner.namespaces.get_mut(name) {
            match key {
                "title" => ns.title = Some(value.to_string()),
                "description" => ns.description = Some(value.to_string()),
                "icon" => ns.icon = Some(value.to_string()),
                "categories" => {
                    ns.categories = value
                        .split(',')
                        .filter(|c| !c.is_empty())
                        .map(str::to_string)
                        .collect()
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn set_namespace_visibility(
        &self,
        name: &NamespaceName,
        visibility: &str,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        if let Some(ns) = inner.namespaces.get_mut(name) {
            ns.visibility = visibility.to_string();
        }
        Ok(())
    }

    async fn set_namespace_federation(
        &self,
        name: &NamespaceName,
        open: bool,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        if let Some(ns) = inner.namespaces.get_mut(name) {
            ns.federation = open;
        }
        Ok(())
    }

    async fn delete_namespace(&self, name: &NamespaceName) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        Ok(inner.namespaces.remove(name).is_some())
    }

    async fn rotate_root(
        &self,
        name: &NamespaceName,
        new_owner: &str,
        new_root_key: &str,
        operator_initiated: bool,
        at_ms: u64,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        if let Some(ns) = inner.namespaces.get_mut(name) {
            if let Ok(owner) = new_owner.parse() {
                ns.owner = owner;
                ns.root_key = new_root_key.to_string();
                ns.pending_recovery = None;
            }
        }
        inner
            .root_history
            .entry(name.clone())
            .or_default()
            .push(RootHistoryEntry {
                root_key: new_root_key.to_string(),
                owner: new_owner.to_string(),
                at_ms,
                operator_initiated,
            });
        Ok(())
    }

    async fn set_recovery_set(
        &self,
        name: &NamespaceName,
        m: u32,
        keys: &[String],
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        if let Some(ns) = inner.namespaces.get_mut(name) {
            ns.recovery_set = Some((m, keys.to_vec()));
        }
        Ok(())
    }

    async fn set_pending_recovery(
        &self,
        name: &NamespaceName,
        pending: PendingRecovery,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        if let Some(ns) = inner.namespaces.get_mut(name) {
            ns.pending_recovery = Some(pending);
        }
        Ok(())
    }

    async fn clear_pending_recovery(&self, name: &NamespaceName) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        if let Some(ns) = inner.namespaces.get_mut(name) {
            ns.pending_recovery = None;
        }
        Ok(())
    }

    async fn due_recoveries(&self, now_ms: u64) -> Result<Vec<NamespaceRecord>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .namespaces
            .values()
            .filter(|ns| {
                ns.pending_recovery
                    .as_ref()
                    .is_some_and(|p| p.eta_ms <= now_ms)
            })
            .cloned()
            .collect())
    }

    async fn root_history(
        &self,
        name: &NamespaceName,
    ) -> Result<Vec<RootHistoryEntry>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner.root_history.get(name).cloned().unwrap_or_default())
    }
}

#[async_trait]
impl ReportStore for MemoryStore {
    async fn file_report(&self, mut record: ReportRecord) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        // Verified reports place retention holds on the reported root + its
        // context (invariant 11); other states hold nothing.
        if record.state == ContentState::Verified {
            let key = record.scope.as_key();
            record.held_roots = inner.context_roots(&key, record.msgid.ulid(), HOLD_RADIUS);
            for root in &record.held_roots {
                *inner.holds.entry((key.clone(), *root)).or_insert(0) += 1;
            }
        }
        inner.reports.insert(record.id.clone(), record);
        Ok(())
    }

    async fn report(&self, id: &str) -> Result<Option<ReportRecord>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner.reports.get(id).cloned())
    }

    async fn list_reports(
        &self,
        scope: &str,
        status: Option<ReportStatus>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ReportRecord>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        let mut out: Vec<ReportRecord> = inner
            .reports
            .values()
            .filter(|r| r.queue_scopes.iter().any(|s| s == scope))
            .filter(|r| status.map_or(true, |want| r.status == want))
            .cloned()
            .collect();
        // Newest first; ids are ULIDs so lexical desc = time desc.
        out.sort_by(|a, b| b.id.cmp(&a.id));
        if let Some(cursor) = after {
            out.retain(|r| r.id.as_str() < cursor);
        }
        out.truncate(limit);
        Ok(out)
    }

    async fn resolve_report(
        &self,
        id: &str,
        resolution: ReportResolution,
    ) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        let Some(report) = inner.reports.get_mut(id) else {
            return Ok(false);
        };
        if report.status == ReportStatus::Resolved {
            return Ok(false);
        }
        report.status = ReportStatus::Resolved;
        report.resolution = Some(resolution);
        Ok(true)
    }

    async fn escalate_report(&self, id: &str) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        let Some(report) = inner.reports.get_mut(id) else {
            return Ok(false);
        };
        if report.status == ReportStatus::Resolved {
            return Ok(false);
        }
        if !report.queue_scopes.iter().any(|s| s == "*") {
            report.queue_scopes.push("*".to_string());
        }
        Ok(true)
    }

    async fn reports_by_since(&self, reporter: &Account, since_ms: u64) -> Result<u64, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .reports
            .values()
            .filter(|r| &r.reporter == reporter && r.filed_at_ms >= since_ms)
            .count() as u64)
    }

    async fn release_due_holds(&self, now_ms: u64) -> Result<u64, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        // Collect the (scope, root) decrements first — can't mutate `holds`
        // while iterating `reports`.
        let mut released_ids = Vec::new();
        let mut decrements: Vec<(String, Ulid)> = Vec::new();
        for report in inner.reports.values() {
            let due = report
                .resolution
                .as_ref()
                .is_some_and(|r| r.hold_release_at <= now_ms);
            if report.status == ReportStatus::Resolved && !report.holds_released && due {
                released_ids.push(report.id.clone());
                let key = report.scope.as_key();
                decrements.extend(report.held_roots.iter().map(|u| (key.clone(), *u)));
            }
        }
        for slot in decrements {
            if let Some(count) = inner.holds.get_mut(&slot) {
                *count -= 1;
                if *count == 0 {
                    inner.holds.remove(&slot);
                }
            }
        }
        for id in &released_ids {
            if let Some(report) = inner.reports.get_mut(id) {
                report.holds_released = true;
            }
        }
        Ok(released_ids.len() as u64)
    }
}

#[async_trait]
impl PeerStore for MemoryStore {
    async fn upsert_peer(&self, record: PeerRecord) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner.peers.insert(record.peer.clone(), record);
        Ok(())
    }

    async fn peer(&self, peer: &NetworkName) -> Result<Option<PeerRecord>, StoreError> {
        Ok(self.inner.lock().unwrap().peers.get(peer).cloned())
    }

    async fn list_peers(&self) -> Result<Vec<PeerRecord>, StoreError> {
        let mut peers: Vec<PeerRecord> =
            self.inner.lock().unwrap().peers.values().cloned().collect();
        peers.sort_by(|a, b| a.peer.as_str().cmp(b.peer.as_str()));
        Ok(peers)
    }

    async fn remove_peer(&self, peer: &NetworkName) -> Result<bool, StoreError> {
        Ok(self.inner.lock().unwrap().peers.remove(peer).is_some())
    }
}

#[async_trait]
impl ModerationStore for MemoryStore {
    async fn set_moderation(&self, record: ModRecord) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        inner.moderation.insert(
            (record.scope.clone(), record.account.clone(), record.kind),
            record,
        );
        Ok(())
    }

    async fn clear_moderation(
        &self,
        scope: &str,
        account: &Account,
        kind: ModKind,
    ) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        Ok(inner
            .moderation
            .remove(&(scope.to_string(), account.clone(), kind))
            .is_some())
    }

    async fn is_moderated(
        &self,
        account: &Account,
        scopes: &[String],
        kind: ModKind,
    ) -> Result<bool, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(scopes.iter().any(|scope| {
            inner
                .moderation
                .contains_key(&(scope.clone(), account.clone(), kind))
        }))
    }

    async fn list_moderation(&self, scope: &str) -> Result<Vec<ModRecord>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        let mut records: Vec<ModRecord> = inner
            .moderation
            .values()
            .filter(|r| r.scope == scope)
            .cloned()
            .collect();
        records.sort_by(|a, b| a.account.as_str().cmp(b.account.as_str()));
        Ok(records)
    }
}

#[async_trait]
impl PinStore for MemoryStore {
    async fn set_pin(
        &self,
        channel: &ChannelName,
        msgid: &MsgId,
        pinned: bool,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        let set = inner.pins.entry(channel.clone()).or_default();
        if pinned {
            set.insert(msgid.ulid(), msgid.clone());
        } else {
            set.remove(&msgid.ulid());
        }
        Ok(())
    }

    async fn pins(&self, channel: &ChannelName) -> Result<Vec<MsgId>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .pins
            .get(channel)
            .map(|set| set.values().cloned().collect())
            .unwrap_or_default())
    }
}

#[async_trait]
impl EmojiStore for MemoryStore {
    async fn set_emoji(
        &self,
        namespace: &NamespaceName,
        name: &str,
        media: &str,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        inner
            .emoji
            .entry(namespace.clone())
            .or_default()
            .insert(name.to_string(), media.to_string());
        Ok(())
    }

    async fn remove_emoji(
        &self,
        namespace: &NamespaceName,
        name: &str,
    ) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        Ok(inner
            .emoji
            .get_mut(namespace)
            .map(|set| set.remove(name).is_some())
            .unwrap_or(false))
    }

    async fn list_emoji(
        &self,
        namespace: &NamespaceName,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .emoji
            .get(namespace)
            .map(|set| set.iter().map(|(n, m)| (n.clone(), m.clone())).collect())
            .unwrap_or_default())
    }

    async fn emoji_media(&self) -> Result<Vec<String>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .emoji
            .values()
            .flat_map(|set| set.values().cloned())
            .collect())
    }
}

#[async_trait]
impl MembershipStore for MemoryStore {
    async fn set_membership(
        &self,
        account: &Account,
        channel: &ChannelName,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        inner
            .memberships
            .entry(account.clone())
            .or_default()
            .insert(channel.clone());
        Ok(())
    }

    async fn clear_membership(
        &self,
        account: &Account,
        channel: &ChannelName,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        if let Some(set) = inner.memberships.get_mut(account) {
            set.remove(channel);
        }
        Ok(())
    }

    async fn memberships(&self, account: &Account) -> Result<Vec<ChannelName>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .memberships
            .get(account)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default())
    }

    async fn members(&self, channel: &ChannelName) -> Result<Vec<Account>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .memberships
            .iter()
            .filter(|(_, chans)| chans.contains(channel))
            .map(|(account, _)| account.clone())
            .collect())
    }
}

#[async_trait]
impl RoleStore for MemoryStore {
    async fn set_role(
        &self,
        scope: &str,
        name: &str,
        color: &str,
        caps: &[String],
        hoist: bool,
        position: i32,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        inner.roles.entry(scope.to_string()).or_default().insert(
            name.to_string(),
            (color.to_string(), caps.to_vec(), hoist, position),
        );
        Ok(())
    }

    async fn reorder_roles(&self, scope: &str, order: &[String]) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        if let Some(defs) = inner.roles.get_mut(scope) {
            for (i, name) in order.iter().enumerate() {
                if let Some(role) = defs.get_mut(name) {
                    role.3 = i as i32;
                }
            }
        }
        Ok(())
    }

    async fn delete_role(&self, scope: &str, name: &str) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        if let Some(defs) = inner.roles.get_mut(scope) {
            defs.remove(name);
        }
        inner
            .role_assignments
            .retain(|(s, n, _)| !(s == scope && n == name));
        Ok(())
    }

    async fn rename_role(&self, scope: &str, old: &str, new: &str) -> Result<(), StoreError> {
        if old == new {
            return Ok(());
        }
        let mut inner = self.inner.lock().expect("store lock");

        // Move the definition (absent ⇒ nothing to rename, and no assignments
        // should be touched either).
        let Some(def) = inner.roles.get_mut(scope).and_then(|d| d.remove(old)) else {
            return Ok(());
        };
        inner
            .roles
            .entry(scope.to_string())
            .or_default()
            .insert(new.to_string(), def);

        // Carry membership across, so nobody loses a role to a rename.
        let moved: Vec<String> = inner
            .role_assignments
            .iter()
            .filter(|(s, n, _)| s == scope && n == old)
            .map(|(_, _, a)| a.clone())
            .collect();
        inner
            .role_assignments
            .retain(|(s, n, _)| !(s == scope && n == old));
        for subject in moved {
            inner
                .role_assignments
                .insert((scope.to_string(), new.to_string(), subject));
        }
        Ok(())
    }

    async fn roles(&self, scope: &str) -> Result<Vec<RoleDef>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .roles
            .get(scope)
            .map(|defs| {
                let mut out: Vec<RoleDef> = defs
                    .iter()
                    .map(|(name, (color, caps, hoist, position))| RoleDef {
                        name: name.clone(),
                        color: color.clone(),
                        caps: caps.clone(),
                        hoist: *hoist,
                        position: *position,
                    })
                    .collect();
                out.sort_by(|a, b| a.position.cmp(&b.position).then(a.name.cmp(&b.name)));
                out
            })
            .unwrap_or_default())
    }

    async fn assign_role(&self, scope: &str, name: &str, subject: &str) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        inner
            .role_assignments
            .insert((scope.to_string(), name.to_string(), subject.to_string()));
        Ok(())
    }

    async fn unassign_role(
        &self,
        scope: &str,
        name: &str,
        subject: &str,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        inner
            .role_assignments
            .remove(&(scope.to_string(), name.to_string(), subject.to_string()));
        Ok(())
    }

    async fn roles_of(&self, scope: &str, subject: &str) -> Result<Vec<String>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .role_assignments
            .iter()
            .filter(|(s, _, a)| s == scope && a == subject)
            .map(|(_, n, _)| n.clone())
            .collect())
    }

    async fn role_members(&self, scope: &str, name: &str) -> Result<Vec<String>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(inner
            .role_assignments
            .iter()
            .filter(|(s, n, _)| s == scope && n == name)
            .map(|(_, _, a)| a.clone())
            .collect())
    }
}

#[async_trait]
impl NetblockStore for MemoryStore {
    async fn add_netblock(&self, record: NetblockRecord) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner.netblocks.insert(record.network.clone(), record);
        Ok(())
    }

    async fn remove_netblock(&self, network: &NetworkName) -> Result<bool, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .netblocks
            .remove(network)
            .is_some())
    }

    async fn is_netblocked(&self, network: &NetworkName) -> Result<bool, StoreError> {
        Ok(self.inner.lock().unwrap().netblocks.contains_key(network))
    }

    async fn list_netblocks(&self) -> Result<Vec<NetblockRecord>, StoreError> {
        let mut blocks: Vec<NetblockRecord> = self
            .inner
            .lock()
            .unwrap()
            .netblocks
            .values()
            .cloned()
            .collect();
        blocks.sort_by(|a, b| a.network.as_str().cmp(b.network.as_str()));
        Ok(blocks)
    }
}

#[async_trait]
impl MediaBlocklistStore for MemoryStore {
    async fn block_hash(&self, record: MediaBlockRecord) -> Result<(), StoreError> {
        self.inner
            .lock()
            .unwrap()
            .blocked_hashes
            .insert(record.hash.clone(), record);
        Ok(())
    }

    async fn unblock_hash(&self, hash: &str) -> Result<bool, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .blocked_hashes
            .remove(hash)
            .is_some())
    }

    async fn is_hash_blocked(&self, hash: &str) -> Result<bool, StoreError> {
        Ok(self.inner.lock().unwrap().blocked_hashes.contains_key(hash))
    }

    async fn list_blocked_hashes(&self) -> Result<Vec<MediaBlockRecord>, StoreError> {
        let mut blocks: Vec<MediaBlockRecord> = self
            .inner
            .lock()
            .unwrap()
            .blocked_hashes
            .values()
            .cloned()
            .collect();
        blocks.sort_by(|a, b| a.hash.cmp(&b.hash));
        Ok(blocks)
    }
}

#[async_trait]
impl AuditStore for MemoryStore {
    async fn append_audit(&self, entry: AuditEntry) -> Result<AuditRecord, StoreError> {
        let mut inner = self.inner.lock().unwrap();

        let seq = inner.audit.len() as u64 + 1;
        let prev_hash = inner
            .audit
            .last()
            .map(|r| r.hash.clone())
            .unwrap_or_else(|| AUDIT_GENESIS.to_string());
        let hash = audit_hash(
            seq,
            &entry.operator,
            &entry.action,
            &entry.target,
            entry.ts_ms,
            &entry.payload_digest,
            &prev_hash,
        );

        let record = AuditRecord {
            seq,
            operator: entry.operator,
            action: entry.action,
            target: entry.target,
            ts_ms: entry.ts_ms,
            payload_digest: entry.payload_digest,
            prev_hash,
            hash,
        };
        inner.audit.push(record.clone());
        Ok(record)
    }

    async fn list_audit(
        &self,
        operator: Option<&str>,
        action: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AuditRecord>, StoreError> {
        let rows = self
            .inner
            .lock()
            .unwrap()
            .audit
            .iter()
            .rev() // newest-first
            .filter(|r| operator.map_or(true, |o| r.operator == o))
            .filter(|r| action.map_or(true, |a| r.action == a))
            .take(limit)
            .cloned()
            .collect();
        Ok(rows)
    }
}

#[async_trait]
impl MediaStore for MemoryStore {
    async fn record_blob(&self, record: BlobRecord) -> Result<(), StoreError> {
        self.inner
            .lock()
            .expect("store lock")
            .blobs
            .entry(record.hash.clone())
            .or_insert(record);
        Ok(())
    }

    async fn blob_meta(&self, hash: &str) -> Result<Option<BlobRecord>, StoreError> {
        Ok(self
            .inner
            .lock()
            .expect("store lock")
            .blobs
            .get(hash)
            .cloned())
    }

    async fn add_refs(
        &self,
        scope: &Scope,
        msgid: &MsgId,
        hashes: &[String],
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("store lock");
        for hash in hashes {
            inner
                .media_refs
                .push((scope.clone(), msgid.clone(), hash.clone()));
        }
        Ok(())
    }

    async fn drop_refs(&self, msgid: &MsgId) -> Result<(), StoreError> {
        self.inner
            .lock()
            .expect("store lock")
            .media_refs
            .retain(|(_, m, _)| m != msgid);
        Ok(())
    }

    async fn drop_refs_before(&self, scope: &Scope, cutoff_ms: u64) -> Result<(), StoreError> {
        self.inner
            .lock()
            .expect("store lock")
            .media_refs
            .retain(|(s, m, _)| !(s == scope && m.ulid().timestamp_ms() < cutoff_ms));
        Ok(())
    }

    async fn blob_scopes(&self, hash: &str) -> Result<Vec<Scope>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        let scopes: HashSet<Scope> = inner
            .media_refs
            .iter()
            .filter(|(_, _, h)| h == hash)
            .map(|(s, _, _)| s.clone())
            .collect();
        Ok(scopes.into_iter().collect())
    }

    async fn orphans(&self, cutoff_ms: u64) -> Result<Vec<String>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        let referenced: HashSet<&String> = inner.media_refs.iter().map(|(_, _, h)| h).collect();
        Ok(inner
            .blobs
            .values()
            .filter(|rec| rec.created_ms < cutoff_ms && !referenced.contains(&rec.hash))
            .map(|rec| rec.hash.clone())
            .collect())
    }

    async fn forget_blob(&self, hash: &str) -> Result<(), StoreError> {
        self.inner.lock().expect("store lock").blobs.remove(hash);
        Ok(())
    }
}

#[async_trait]
impl ProfileStore for MemoryStore {
    async fn set_profile(&self, account: &str, profile: ProfileRecord) -> Result<(), StoreError> {
        self.inner
            .lock()
            .expect("store lock")
            .profiles
            .insert(account.to_string(), profile);
        Ok(())
    }

    async fn profile(&self, account: &str) -> Result<Option<ProfileRecord>, StoreError> {
        Ok(self
            .inner
            .lock()
            .expect("store lock")
            .profiles
            .get(account)
            .cloned())
    }

    async fn profiles(
        &self,
        accounts: &[String],
    ) -> Result<Vec<(String, ProfileRecord)>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        Ok(accounts
            .iter()
            .filter_map(|a| inner.profiles.get(a).map(|p| (a.clone(), p.clone())))
            .collect())
    }

    async fn avatar_exists(&self, hash: &str) -> Result<bool, StoreError> {
        Ok(self
            .inner
            .lock()
            .expect("store lock")
            .profiles
            .values()
            .any(|p| p.avatar.as_deref() == Some(hash)))
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
