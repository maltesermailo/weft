//! PostgreSQL backend (sqlx, runtime queries — no compile-time database).
//!
//! Must match [`crate::MemoryStore`] semantics exactly; the memory backend
//! is the reference and the shared suite in `tests/backends.rs` runs
//! against both. All §12.1 logic stays in the shared pure functions
//! ([`crate::materialize`], [`crate::compaction_plan`]) — this module only
//! moves rows.

use async_trait::async_trait;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::collections::HashMap;
use weft_proto::{
    Account, ChannelName, ContentState, MsgId, MsgMeta, NamespaceName, NetworkName, ReportStatus,
    RetentionPolicy, Ulid,
};

use crate::compact::compaction_plan;
use crate::traits::{
    AccountStore, CapabilityStore, ChannelStore, EventStore, InviteStore, MembershipStore,
    ModerationStore, NamespaceStore, NetblockStore, PeerStore, PinStore, ReportStore, RoleStore,
    HOLD_RADIUS,
};
use crate::types::{
    ChannelRecord, EventKind, EventRecord, GrantRecord, InviteRecord, ModKind, ModRecord,
    NamespaceRecord, NetblockRecord, Page, PeerRecord, PendingRecovery, RedeemOutcome,
    ReportRecord, ReportResolution, RoleDef, RootHistoryEntry, Scope, Verification,
};
use crate::StoreError;

pub struct PgStore {
    pool: PgPool,
}

fn backend_err(e: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(e.to_string())
}

const KIND_MESSAGE: i16 = 0;
const KIND_EDIT: i16 = 1;
const KIND_DELETE: i16 = 2;
const KIND_REACT: i16 = 3;

impl PgStore {
    /// Connect and run migrations (idempotent).
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            // Fail fast on an unreachable/firewalled DB instead of hanging on
            // TCP retries with no output.
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(url)
            .await
            .map_err(backend_err)?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(backend_err)?;
        Ok(Self { pool })
    }

    fn record_from_row(row: &sqlx::postgres::PgRow) -> Result<EventRecord, StoreError> {
        let corrupt = |what: &str| StoreError::Backend(format!("corrupt row: {what}"));
        let scope = Scope::from_key(row.get::<&str, _>("scope")).ok_or_else(|| corrupt("scope"))?;
        let ulid = Ulid::from_string(row.get("ulid")).map_err(|_| corrupt("ulid"))?;
        let origin = row
            .get::<&str, _>("origin")
            .parse()
            .map_err(|_| corrupt("origin"))?;
        let root_ulid =
            Ulid::from_string(row.get("root_ulid")).map_err(|_| corrupt("root_ulid"))?;
        let root_origin = row
            .get::<&str, _>("root_origin")
            .parse()
            .map_err(|_| corrupt("root_origin"))?;
        let sender = row
            .get::<&str, _>("sender")
            .parse()
            .map_err(|_| corrupt("sender"))?;
        let body = || row.get::<Option<String>, _>("body").unwrap_or_default();
        let kind = match row.get::<i16, _>("kind") {
            KIND_MESSAGE => EventKind::Message {
                body: body(),
                meta: MsgMeta {
                    fmt: row.get("fmt"),
                    reply_to: parse_opt_msgid(row.get("reply_to"))?,
                    thread: parse_opt_msgid(row.get("thread"))?,
                    attachments: Vec::new(), // rejected until media (M6)
                },
            },
            KIND_EDIT => EventKind::Edit { body: body() },
            KIND_DELETE => EventKind::Delete,
            KIND_REACT => EventKind::React {
                emoji: row.get::<Option<String>, _>("emoji").unwrap_or_default(),
                add: row.get::<Option<bool>, _>("react_add").unwrap_or(true),
            },
            _ => return Err(corrupt("kind")),
        };
        Ok(EventRecord {
            scope,
            msgid: MsgId::new(origin, ulid),
            root: MsgId::new(root_origin, root_ulid),
            sender,
            kind,
        })
    }

    async fn purge_scope(&self, scope_key: &str, cutoff_ms: u64) -> Result<u64, StoreError> {
        // Data-modifying CTE: whole messages expire by ROOT age (children,
        // tombstones included, never outlive their message).
        let purged: i64 = sqlx::query_scalar(
            r#"
            WITH expired AS (
                SELECT ulid FROM weft_events
                WHERE scope = $1 AND kind = 0 AND at_ms < $2
                  -- Retention hold: held roots survive purge (invariant 11).
                  AND ulid NOT IN (SELECT root_ulid FROM weft_holds WHERE scope = $1)
            ), gone AS (
                DELETE FROM weft_events
                WHERE scope = $1 AND root_ulid IN (SELECT ulid FROM expired)
            )
            SELECT count(*) FROM expired
            "#,
        )
        .bind(scope_key)
        .bind(cutoff_ms as i64)
        .fetch_one(&self.pool)
        .await
        .map_err(backend_err)?;
        sqlx::query(
            r#"
            INSERT INTO weft_watermarks (scope, purged_before_ms) VALUES ($1, $2)
            ON CONFLICT (scope) DO UPDATE
            SET purged_before_ms = GREATEST(weft_watermarks.purged_before_ms, EXCLUDED.purged_before_ms)
            "#,
        )
        .bind(scope_key)
        .bind(cutoff_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(purged as u64)
    }
}

fn parse_opt_msgid(text: Option<String>) -> Result<Option<MsgId>, StoreError> {
    text.map(|t| {
        t.parse()
            .map_err(|_| StoreError::Backend("corrupt row: msgid".to_string()))
    })
    .transpose()
}

#[async_trait]
impl EventStore for PgStore {
    async fn append(&self, record: EventRecord) -> Result<(), StoreError> {
        let (kind, body, fmt, reply_to, thread, emoji, react_add) = match &record.kind {
            EventKind::Message { body, meta } => (
                KIND_MESSAGE,
                Some(body.clone()),
                meta.fmt.clone(),
                meta.reply_to.as_ref().map(MsgId::to_string),
                meta.thread.as_ref().map(MsgId::to_string),
                None,
                None,
            ),
            EventKind::Edit { body } => {
                (KIND_EDIT, Some(body.clone()), None, None, None, None, None)
            }
            EventKind::Delete => (KIND_DELETE, None, None, None, None, None, None),
            EventKind::React { emoji, add } => (
                KIND_REACT,
                None,
                None,
                None,
                None,
                Some(emoji.clone()),
                Some(*add),
            ),
        };
        sqlx::query(
            r#"
            INSERT INTO weft_events
              (scope, ulid, origin, root_ulid, root_origin, kind, sender,
               body, fmt, reply_to, thread, emoji, react_add, at_ms)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
            ON CONFLICT (scope, ulid) DO NOTHING
            "#,
        )
        .bind(record.scope.as_key())
        .bind(record.msgid.ulid().to_string())
        .bind(record.msgid.origin().as_str())
        .bind(record.root.ulid().to_string())
        .bind(record.root.origin().as_str())
        .bind(kind)
        .bind(record.sender.to_string())
        .bind(body)
        .bind(fmt)
        .bind(reply_to)
        .bind(thread)
        .bind(emoji)
        .bind(react_add)
        .bind(record.at_ms() as i64)
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(())
    }

    async fn roots(&self, scope: &Scope, page: Page) -> Result<Vec<EventRecord>, StoreError> {
        // ULID text sorts in time order — `ulid < $2` IS the msgid cursor.
        let rows = sqlx::query(
            r#"
            SELECT * FROM weft_events
            WHERE scope = $1 AND kind = 0
              AND ($2::text IS NULL OR ulid < $2)
              AND ($3::text IS NULL OR ulid > $3)
            ORDER BY ulid DESC
            LIMIT $4
            "#,
        )
        .bind(scope.as_key())
        .bind(page.before.map(|u| u.to_string()))
        .bind(page.after.map(|u| u.to_string()))
        .bind(page.limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(backend_err)?;
        let mut records = rows
            .iter()
            .map(Self::record_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        records.reverse(); // newest-anchored, ascending — like MemoryStore
        Ok(records)
    }

    async fn children(
        &self,
        scope: &Scope,
        roots: &[Ulid],
    ) -> Result<Vec<EventRecord>, StoreError> {
        let keys: Vec<String> = roots.iter().map(Ulid::to_string).collect();
        let rows = sqlx::query(
            "SELECT * FROM weft_events WHERE scope = $1 AND kind <> 0 AND root_ulid = ANY($2)",
        )
        .bind(scope.as_key())
        .bind(&keys)
        .fetch_all(&self.pool)
        .await
        .map_err(backend_err)?;
        rows.iter().map(Self::record_from_row).collect()
    }

    async fn find_root(&self, ulid: Ulid) -> Result<Option<EventRecord>, StoreError> {
        let row = sqlx::query("SELECT * FROM weft_events WHERE ulid = $1 AND kind = 0 LIMIT 1")
            .bind(ulid.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(backend_err)?;
        row.as_ref().map(Self::record_from_row).transpose()
    }

    async fn is_deleted(&self, scope: &Scope, root: Ulid) -> Result<bool, StoreError> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM weft_events WHERE scope = $1 AND root_ulid = $2 AND kind = 2)",
        )
        .bind(scope.as_key())
        .bind(root.to_string())
        .fetch_one(&self.pool)
        .await
        .map_err(backend_err)
    }

    async fn purge_before(&self, scope: &Scope, cutoff_ms: u64) -> Result<u64, StoreError> {
        self.purge_scope(&scope.as_key(), cutoff_ms).await
    }

    async fn purged_before(&self, scope: &Scope) -> Result<Option<u64>, StoreError> {
        let watermark: Option<i64> =
            sqlx::query_scalar("SELECT purged_before_ms FROM weft_watermarks WHERE scope = $1")
                .bind(scope.as_key())
                .fetch_optional(&self.pool)
                .await
                .map_err(backend_err)?;
        Ok(watermark.map(|ms| ms as u64))
    }

    async fn purge_dms_before(&self, cutoff_ms: u64) -> Result<u64, StoreError> {
        let scopes: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT scope FROM weft_events WHERE scope LIKE 'dm:%' AND kind = 0 AND at_ms < $1",
        )
        .bind(cutoff_ms as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(backend_err)?;
        let mut purged = 0;
        for scope in scopes {
            purged += self.purge_scope(&scope, cutoff_ms).await?;
        }
        Ok(purged)
    }

    async fn compact_before(&self, cutoff_ms: u64) -> Result<u64, StoreError> {
        // Scope-at-a-time: load rows, run the shared plan, delete. Loads a
        // whole scope into memory — fine at current scale; page per root
        // family when channels grow past that.
        let scopes: Vec<String> = sqlx::query_scalar("SELECT DISTINCT scope FROM weft_events")
            .fetch_all(&self.pool)
            .await
            .map_err(backend_err)?;
        let mut dropped = 0;
        for scope in scopes {
            let rows = sqlx::query("SELECT * FROM weft_events WHERE scope = $1")
                .bind(&scope)
                .fetch_all(&self.pool)
                .await
                .map_err(backend_err)?;
            let records = rows
                .iter()
                .map(Self::record_from_row)
                .collect::<Result<Vec<_>, _>>()?;
            // Held roots are exempt from compaction (invariant 11).
            let held: std::collections::HashSet<String> =
                sqlx::query_scalar("SELECT root_ulid FROM weft_holds WHERE scope = $1")
                    .bind(&scope)
                    .fetch_all(&self.pool)
                    .await
                    .map_err(backend_err)?
                    .into_iter()
                    .collect();
            let mut families: HashMap<Ulid, Vec<EventRecord>> = HashMap::new();
            for record in records {
                families.entry(record.root.ulid()).or_default().push(record);
            }
            let drops: Vec<String> = families
                .iter()
                .filter(|(root, _)| !held.contains(&root.to_string()))
                .flat_map(|(_, family)| compaction_plan(family, cutoff_ms))
                .map(|ulid| ulid.to_string())
                .collect();
            if drops.is_empty() {
                continue;
            }
            let result = sqlx::query("DELETE FROM weft_events WHERE scope = $1 AND ulid = ANY($2)")
                .bind(&scope)
                .bind(&drops)
                .execute(&self.pool)
                .await
                .map_err(backend_err)?;
            dropped += result.rows_affected();
        }
        Ok(dropped)
    }
}

#[async_trait]
impl AccountStore for PgStore {
    async fn register(&self, account: &Account, password_phc: &str) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "INSERT INTO weft_accounts (name, password_phc) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(account.as_str())
        .bind(password_phc)
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(result.rows_affected() == 1)
    }

    async fn password_phc(&self, account: &Account) -> Result<Option<String>, StoreError> {
        sqlx::query_scalar("SELECT password_phc FROM weft_accounts WHERE name = $1")
            .bind(account.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(backend_err)
    }

    async fn list_accounts(&self) -> Result<Vec<Account>, StoreError> {
        let names: Vec<String> =
            sqlx::query_scalar("SELECT name FROM weft_accounts ORDER BY name")
                .fetch_all(&self.pool)
                .await
                .map_err(backend_err)?;
        Ok(names.into_iter().filter_map(|n| n.parse().ok()).collect())
    }

    async fn enroll_device(&self, account: &Account, device: [u8; 32]) -> Result<bool, StoreError> {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM weft_accounts WHERE name = $1)")
                .bind(account.as_str())
                .fetch_one(&self.pool)
                .await
                .map_err(backend_err)?;
        if !exists {
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO weft_devices (account, pubkey) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(account.as_str())
        .bind(device.as_slice())
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(true)
    }

    async fn device_enrolled(
        &self,
        account: &Account,
        device: &[u8; 32],
    ) -> Result<bool, StoreError> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM weft_devices WHERE account = $1 AND pubkey = $2)",
        )
        .bind(account.as_str())
        .bind(device.as_slice())
        .fetch_one(&self.pool)
        .await
        .map_err(backend_err)
    }

    async fn set_mark(
        &self,
        account: &Account,
        target: &str,
        msgid: &MsgId,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO weft_marks (account, target, msgid) VALUES ($1, $2, $3)
            ON CONFLICT (account, target) DO UPDATE SET msgid = EXCLUDED.msgid
            "#,
        )
        .bind(account.as_str())
        .bind(target)
        .bind(msgid.to_string())
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(())
    }

    async fn marks(&self, account: &Account) -> Result<Vec<(String, MsgId)>, StoreError> {
        let rows = sqlx::query("SELECT target, msgid FROM weft_marks WHERE account = $1")
            .bind(account.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(backend_err)?;
        rows.iter()
            .map(|row| {
                let msgid = row
                    .get::<&str, _>("msgid")
                    .parse()
                    .map_err(|_| StoreError::Backend("corrupt mark".to_string()))?;
                Ok((row.get::<String, _>("target"), msgid))
            })
            .collect()
    }

    async fn upsert_verification(
        &self,
        account: &Account,
        kind: &str,
        subject: &str,
    ) -> Result<(), StoreError> {
        // FK enforces account existence; a missing account is a no-op to
        // match the memory backend.
        let _ = sqlx::query(
            r#"
            INSERT INTO weft_verifications (account, kind, subject, verified_at)
            VALUES ($1, $2, $3, NULL)
            ON CONFLICT (account, kind)
            DO UPDATE SET subject = EXCLUDED.subject, verified_at = NULL
            "#,
        )
        .bind(account.as_str())
        .bind(kind)
        .bind(subject)
        .execute(&self.pool)
        .await;
        Ok(())
    }

    async fn confirm_verification(
        &self,
        account: &Account,
        kind: &str,
        verified_at: u64,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE weft_verifications SET verified_at = $3 WHERE account = $1 AND kind = $2",
        )
        .bind(account.as_str())
        .bind(kind)
        .bind(verified_at as i64)
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(result.rows_affected() == 1)
    }

    async fn verifications(&self, account: &Account) -> Result<Vec<Verification>, StoreError> {
        let rows = sqlx::query(
            "SELECT kind, subject, verified_at FROM weft_verifications WHERE account = $1",
        )
        .bind(account.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(rows
            .iter()
            .map(|row| Verification {
                kind: row.get("kind"),
                subject: row.get("subject"),
                verified_at: row.get::<Option<i64>, _>("verified_at").map(|v| v as u64),
            })
            .collect())
    }
}

#[async_trait]
impl ChannelStore for PgStore {
    async fn upsert_channel(
        &self,
        name: &ChannelName,
        policy: RetentionPolicy,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO weft_channels (name, policy) VALUES ($1, $2)
            ON CONFLICT (name) DO UPDATE SET policy = EXCLUDED.policy
            "#,
        )
        .bind(name.as_str())
        .bind(policy.to_string())
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(())
    }

    async fn list_channels(&self) -> Result<Vec<(ChannelName, RetentionPolicy)>, StoreError> {
        let rows = sqlx::query("SELECT name, policy FROM weft_channels ORDER BY name")
            .fetch_all(&self.pool)
            .await
            .map_err(backend_err)?;
        rows.iter()
            .map(|row| {
                let corrupt = || StoreError::Backend("corrupt channel row".to_string());
                let name = row.get::<&str, _>("name").parse().map_err(|_| corrupt())?;
                let policy = row
                    .get::<&str, _>("policy")
                    .parse()
                    .map_err(|_| corrupt())?;
                Ok((name, policy))
            })
            .collect()
    }

    async fn channel(&self, name: &ChannelName) -> Result<Option<ChannelRecord>, StoreError> {
        let row = sqlx::query("SELECT * FROM weft_channels WHERE name = $1")
            .bind(name.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(backend_err)?;
        row.map(|row| channel_from_row(&row)).transpose()
    }

    async fn set_channel_topic(&self, name: &ChannelName, topic: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE weft_channels SET topic = $2 WHERE name = $1")
            .bind(name.as_str())
            .bind(topic)
            .execute(&self.pool)
            .await
            .map_err(backend_err)?;
        Ok(())
    }

    async fn set_channel_view_gated(
        &self,
        name: &ChannelName,
        gated: bool,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE weft_channels SET view_gated = $2 WHERE name = $1")
            .bind(name.as_str())
            .bind(gated)
            .execute(&self.pool)
            .await
            .map_err(backend_err)?;
        Ok(())
    }

    async fn set_channel_restricted(
        &self,
        name: &ChannelName,
        restricted: bool,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE weft_channels SET restricted = $2 WHERE name = $1")
            .bind(name.as_str())
            .bind(restricted)
            .execute(&self.pool)
            .await
            .map_err(backend_err)?;
        Ok(())
    }

    async fn delete_channel(&self, name: &ChannelName) -> Result<bool, StoreError> {
        let result = sqlx::query("DELETE FROM weft_channels WHERE name = $1")
            .bind(name.as_str())
            .execute(&self.pool)
            .await
            .map_err(backend_err)?;
        Ok(result.rows_affected() == 1)
    }

    async fn rename_channel(
        &self,
        old: &ChannelName,
        new: &ChannelName,
    ) -> Result<bool, StoreError> {
        let ok = old.as_str();
        let nk = new.as_str();
        let mut tx = self.pool.begin().await.map_err(backend_err)?;

        // Old must exist; new must be free.
        let old_exists: Option<String> =
            sqlx::query_scalar("SELECT name FROM weft_channels WHERE name = $1")
                .bind(ok)
                .fetch_optional(&mut *tx)
                .await
                .map_err(backend_err)?;
        let new_exists: Option<String> =
            sqlx::query_scalar("SELECT name FROM weft_channels WHERE name = $1")
                .bind(nk)
                .fetch_optional(&mut *tx)
                .await
                .map_err(backend_err)?;
        if old_exists.is_none() || new_exists.is_some() {
            tx.rollback().await.map_err(backend_err)?;
            return Ok(false);
        }

        // Re-key every channel-scoped row atomically. `weft_events` also covers
        // root/tombstone rows (they're events, not separate tables).
        for sql in [
            "UPDATE weft_channels         SET name    = $2 WHERE name    = $1",
            "UPDATE weft_events           SET scope   = $2 WHERE scope   = $1",
            "UPDATE weft_watermarks       SET scope   = $2 WHERE scope   = $1",
            "UPDATE weft_marks            SET target  = $2 WHERE target  = $1",
            "UPDATE weft_grants           SET scope   = $2 WHERE scope   = $1",
            "UPDATE weft_epochs           SET scope   = $2 WHERE scope   = $1",
            "UPDATE weft_holds            SET scope   = $2 WHERE scope   = $1",
            "UPDATE weft_moderation       SET scope   = $2 WHERE scope   = $1",
            "UPDATE weft_pins             SET channel = $2 WHERE channel = $1",
            "UPDATE weft_memberships      SET channel = $2 WHERE channel = $1",
            "UPDATE weft_roles            SET scope   = $2 WHERE scope   = $1",
            "UPDATE weft_role_assignments SET scope   = $2 WHERE scope   = $1",
        ] {
            sqlx::query(sql)
                .bind(ok)
                .bind(nk)
                .execute(&mut *tx)
                .await
                .map_err(backend_err)?;
        }

        tx.commit().await.map_err(backend_err)?;
        Ok(true)
    }

    async fn set_channel_layout(
        &self,
        name: &ChannelName,
        category: Option<&str>,
        position: i64,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE weft_channels SET category = $2, position = $3 WHERE name = $1")
            .bind(name.as_str())
            .bind(category)
            .bind(position)
            .execute(&self.pool)
            .await
            .map_err(backend_err)?;
        Ok(())
    }

    async fn channels_in_namespace(
        &self,
        namespace: &str,
    ) -> Result<Vec<(ChannelName, ChannelRecord)>, StoreError> {
        let prefix = format!("#{namespace}/%");
        let rows = sqlx::query(
            r#"
            SELECT * FROM weft_channels WHERE name LIKE $1
            ORDER BY category NULLS FIRST, position, name
            "#,
        )
        .bind(prefix)
        .fetch_all(&self.pool)
        .await
        .map_err(backend_err)?;
        rows.iter()
            .map(|row| {
                let name = row
                    .get::<&str, _>("name")
                    .parse()
                    .map_err(|_| StoreError::Backend("corrupt channel name".to_string()))?;
                Ok((name, channel_from_row(row)?))
            })
            .collect()
    }
}

fn channel_from_row(row: &sqlx::postgres::PgRow) -> Result<ChannelRecord, StoreError> {
    Ok(ChannelRecord {
        policy: row
            .get::<&str, _>("policy")
            .parse()
            .map_err(|_| StoreError::Backend("corrupt channel policy".to_string()))?,
        topic: row.get("topic"),
        view_gated: row.get("view_gated"),
        restricted: row.get("restricted"),
        category: row.get("category"),
        position: row.get::<Option<i64>, _>("position").unwrap_or(0),
    })
}

#[async_trait]
impl CapabilityStore for PgStore {
    async fn record_grant(
        &self,
        subject: &str,
        scope: &str,
        caps: &[String],
        epoch: u64,
        expiry: Option<u64>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO weft_grants (subject, scope, caps, epoch, expiry)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (subject, scope)
            DO UPDATE SET caps = EXCLUDED.caps, epoch = EXCLUDED.epoch, expiry = EXCLUDED.expiry
            "#,
        )
        .bind(subject)
        .bind(scope)
        .bind(caps.join(","))
        .bind(epoch as i64)
        .bind(expiry.map(|e| e as i64))
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(())
    }

    async fn grants_for(&self, subject: &str) -> Result<Vec<GrantRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT subject, scope, caps, epoch, expiry FROM weft_grants WHERE subject = $1",
        )
        .bind(subject)
        .fetch_all(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(rows
            .iter()
            .map(|row| GrantRecord {
                subject: row.get("subject"),
                scope: row.get("scope"),
                caps: split_caps(row.get("caps")),
                epoch: row.get::<i64, _>("epoch") as u64,
                expiry: row.get::<Option<i64>, _>("expiry").map(|e| e as u64),
            })
            .collect())
    }

    async fn grants_at_scope(&self, scope: &str) -> Result<Vec<GrantRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT subject, scope, caps, epoch, expiry FROM weft_grants WHERE scope = $1",
        )
        .bind(scope)
        .fetch_all(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(rows
            .iter()
            .map(|row| GrantRecord {
                subject: row.get("subject"),
                scope: row.get("scope"),
                caps: split_caps(row.get("caps")),
                epoch: row.get::<i64, _>("epoch") as u64,
                expiry: row.get::<Option<i64>, _>("expiry").map(|e| e as u64),
            })
            .collect())
    }

    async fn revoke_grants(
        &self,
        subject: &str,
        scope: &str,
        caps: Option<&[String]>,
    ) -> Result<u64, StoreError> {
        match caps {
            None => {
                let result =
                    sqlx::query("DELETE FROM weft_grants WHERE subject = $1 AND scope = $2")
                        .bind(subject)
                        .bind(scope)
                        .execute(&self.pool)
                        .await
                        .map_err(backend_err)?;
                Ok(result.rows_affected())
            }
            Some(drop) => {
                // Read-modify-write the caps list; the whole grant goes if
                // nothing is left.
                let Some(row) =
                    sqlx::query("SELECT caps FROM weft_grants WHERE subject = $1 AND scope = $2")
                        .bind(subject)
                        .bind(scope)
                        .fetch_optional(&self.pool)
                        .await
                        .map_err(backend_err)?
                else {
                    return Ok(0);
                };
                let mut remaining = split_caps(row.get("caps"));
                let before = remaining.len();
                remaining.retain(|c| !drop.contains(c));
                let removed = (before - remaining.len()) as u64;
                if remaining.is_empty() {
                    sqlx::query("DELETE FROM weft_grants WHERE subject = $1 AND scope = $2")
                        .bind(subject)
                        .bind(scope)
                        .execute(&self.pool)
                        .await
                        .map_err(backend_err)?;
                } else {
                    sqlx::query(
                        "UPDATE weft_grants SET caps = $3 WHERE subject = $1 AND scope = $2",
                    )
                    .bind(subject)
                    .bind(scope)
                    .bind(remaining.join(","))
                    .execute(&self.pool)
                    .await
                    .map_err(backend_err)?;
                }
                Ok(removed)
            }
        }
    }

    async fn scope_epoch(&self, scope: &str) -> Result<u64, StoreError> {
        let epoch: Option<i64> =
            sqlx::query_scalar("SELECT epoch FROM weft_epochs WHERE scope = $1")
                .bind(scope)
                .fetch_optional(&self.pool)
                .await
                .map_err(backend_err)?;
        Ok(epoch.unwrap_or(0) as u64)
    }

    async fn bump_epoch(&self, scope: &str) -> Result<u64, StoreError> {
        let epoch: i64 = sqlx::query_scalar(
            r#"
            INSERT INTO weft_epochs (scope, epoch) VALUES ($1, 1)
            ON CONFLICT (scope) DO UPDATE SET epoch = weft_epochs.epoch + 1
            RETURNING epoch
            "#,
        )
        .bind(scope)
        .fetch_one(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(epoch as u64)
    }
}

#[async_trait]
impl InviteStore for PgStore {
    async fn create_invite(&self, invite: InviteRecord) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO weft_invites (id, scope, caps, uses_left, expiry) VALUES ($1,$2,$3,$4,$5)",
        )
        .bind(&invite.id)
        .bind(&invite.scope)
        .bind(invite.caps.join(","))
        .bind(invite.uses_left.map(|u| u as i32))
        .bind(invite.expiry.map(|e| e as i64))
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(())
    }

    async fn invite(&self, id: &str) -> Result<Option<InviteRecord>, StoreError> {
        let row = sqlx::query(
            "SELECT id, scope, caps, uses_left, expiry FROM weft_invites WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(row.map(|row| invite_from_row(&row)))
    }

    async fn redeem_invite(&self, id: &str, now: u64) -> Result<RedeemOutcome, StoreError> {
        // Atomic: decrement only when a use remains and not expired.
        // RETURNING lets us distinguish "counted down" from "no change".
        let updated = sqlx::query(
            r#"
            UPDATE weft_invites
            SET uses_left = uses_left - 1
            WHERE id = $1
              AND (expiry IS NULL OR expiry > $2)
              AND (uses_left IS NULL OR uses_left > 0)
            RETURNING id, scope, caps, uses_left, expiry
            "#,
        )
        .bind(id)
        .bind(now as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend_err)?;
        if let Some(row) = updated {
            return Ok(RedeemOutcome::Redeemed(invite_from_row(&row)));
        }
        // No row updated: either gone/expired, or exhausted. Distinguish.
        let existing: Option<(Option<i32>, Option<i64>)> =
            sqlx::query_as("SELECT uses_left, expiry FROM weft_invites WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .map_err(backend_err)?;
        match existing {
            None => Ok(RedeemOutcome::Gone),
            Some((_, Some(expiry))) if now as i64 >= expiry => Ok(RedeemOutcome::Gone),
            Some((Some(0), _)) => Ok(RedeemOutcome::Exhausted),
            _ => Ok(RedeemOutcome::Gone),
        }
    }

    async fn revoke_invite(&self, id: &str) -> Result<bool, StoreError> {
        let result = sqlx::query("DELETE FROM weft_invites WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(backend_err)?;
        Ok(result.rows_affected() == 1)
    }
}

#[async_trait]
impl NamespaceStore for PgStore {
    async fn create_namespace(&self, record: NamespaceRecord) -> Result<bool, StoreError> {
        let result = sqlx::query(
            r#"
            INSERT INTO weft_namespaces (name, owner, root_key, visibility, title, description, icon)
            VALUES ($1,$2,$3,$4,$5,$6,$7)
            ON CONFLICT (name) DO NOTHING
            "#,
        )
        .bind(record.name.as_str())
        .bind(record.owner.as_str())
        .bind(&record.root_key)
        .bind(&record.visibility)
        .bind(&record.title)
        .bind(&record.description)
        .bind(&record.icon)
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(result.rows_affected() == 1)
    }

    async fn namespace(&self, name: &NamespaceName) -> Result<Option<NamespaceRecord>, StoreError> {
        let row = sqlx::query("SELECT * FROM weft_namespaces WHERE name = $1")
            .bind(name.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(backend_err)?;
        row.map(|row| namespace_from_row(&row)).transpose()
    }

    async fn namespaces_owned(&self, owner: &str) -> Result<u64, StoreError> {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM weft_namespaces WHERE owner = $1")
            .bind(owner)
            .fetch_one(&self.pool)
            .await
            .map_err(backend_err)?;
        Ok(n as u64)
    }

    async fn list_public(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<NamespaceRecord>, StoreError> {
        let rows = sqlx::query(
            r#"
            SELECT * FROM weft_namespaces
            WHERE visibility = 'public' AND ($1::text IS NULL OR name > $1)
            ORDER BY name
            LIMIT $2
            "#,
        )
        .bind(after)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(backend_err)?;
        rows.iter().map(namespace_from_row).collect()
    }

    async fn set_namespace_meta(
        &self,
        name: &NamespaceName,
        key: &str,
        value: &str,
    ) -> Result<(), StoreError> {
        // Whitelist the column — never interpolate a key into SQL.
        let column = match key {
            "title" => "title",
            "description" => "description",
            "icon" => "icon",
            "categories" => "categories",
            _ => return Ok(()),
        };
        sqlx::query(&format!(
            "UPDATE weft_namespaces SET {column} = $2 WHERE name = $1"
        ))
        .bind(name.as_str())
        .bind(value)
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(())
    }

    async fn set_namespace_visibility(
        &self,
        name: &NamespaceName,
        visibility: &str,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE weft_namespaces SET visibility = $2 WHERE name = $1")
            .bind(name.as_str())
            .bind(visibility)
            .execute(&self.pool)
            .await
            .map_err(backend_err)?;
        Ok(())
    }

    async fn delete_namespace(&self, name: &NamespaceName) -> Result<bool, StoreError> {
        let result = sqlx::query("DELETE FROM weft_namespaces WHERE name = $1")
            .bind(name.as_str())
            .execute(&self.pool)
            .await
            .map_err(backend_err)?;
        Ok(result.rows_affected() == 1)
    }

    async fn rotate_root(
        &self,
        name: &NamespaceName,
        new_owner: &str,
        new_root_key: &str,
        operator_initiated: bool,
        at_ms: u64,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            UPDATE weft_namespaces
            SET owner = $2, root_key = $3,
                pending_root_key = NULL, pending_owner = NULL,
                pending_eta_ms = NULL, pending_rung = NULL
            WHERE name = $1
            "#,
        )
        .bind(name.as_str())
        .bind(new_owner)
        .bind(new_root_key)
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        sqlx::query(
            "INSERT INTO weft_root_history (namespace, at_ms, root_key, owner, operator_initiated) VALUES ($1,$2,$3,$4,$5)",
        )
        .bind(name.as_str())
        .bind(at_ms as i64)
        .bind(new_root_key)
        .bind(new_owner)
        .bind(operator_initiated)
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(())
    }

    async fn set_recovery_set(
        &self,
        name: &NamespaceName,
        m: u32,
        keys: &[String],
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE weft_namespaces SET recovery_m = $2, recovery_keys = $3 WHERE name = $1",
        )
        .bind(name.as_str())
        .bind(m as i32)
        .bind(keys.join(","))
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(())
    }

    async fn set_pending_recovery(
        &self,
        name: &NamespaceName,
        pending: PendingRecovery,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            UPDATE weft_namespaces
            SET pending_root_key = $2, pending_owner = $3, pending_eta_ms = $4, pending_rung = $5
            WHERE name = $1
            "#,
        )
        .bind(name.as_str())
        .bind(&pending.new_root_key)
        .bind(&pending.new_owner)
        .bind(pending.eta_ms as i64)
        .bind(pending.rung as i16)
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(())
    }

    async fn clear_pending_recovery(&self, name: &NamespaceName) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            UPDATE weft_namespaces
            SET pending_root_key = NULL, pending_owner = NULL, pending_eta_ms = NULL, pending_rung = NULL
            WHERE name = $1
            "#,
        )
        .bind(name.as_str())
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(())
    }

    async fn due_recoveries(&self, now_ms: u64) -> Result<Vec<NamespaceRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT * FROM weft_namespaces WHERE pending_eta_ms IS NOT NULL AND pending_eta_ms <= $1",
        )
        .bind(now_ms as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(backend_err)?;
        rows.iter().map(namespace_from_row).collect()
    }

    async fn root_history(
        &self,
        name: &NamespaceName,
    ) -> Result<Vec<RootHistoryEntry>, StoreError> {
        let rows = sqlx::query(
            "SELECT root_key, owner, at_ms, operator_initiated FROM weft_root_history WHERE namespace = $1 ORDER BY at_ms",
        )
        .bind(name.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(rows
            .iter()
            .map(|row| RootHistoryEntry {
                root_key: row.get("root_key"),
                owner: row.get("owner"),
                at_ms: row.get::<i64, _>("at_ms") as u64,
                operator_initiated: row.get("operator_initiated"),
            })
            .collect())
    }
}

fn namespace_from_row(row: &sqlx::postgres::PgRow) -> Result<NamespaceRecord, StoreError> {
    let corrupt = || StoreError::Backend("corrupt namespace row".to_string());
    let recovery_set = row.get::<Option<i32>, _>("recovery_m").map(|m| {
        let keys = row
            .get::<Option<String>, _>("recovery_keys")
            .unwrap_or_default();
        (
            m as u32,
            keys.split(',')
                .filter(|k| !k.is_empty())
                .map(str::to_string)
                .collect(),
        )
    });
    let pending_recovery = row
        .get::<Option<String>, _>("pending_root_key")
        .map(|root| PendingRecovery {
            new_root_key: root,
            new_owner: row
                .get::<Option<String>, _>("pending_owner")
                .unwrap_or_default(),
            eta_ms: row.get::<Option<i64>, _>("pending_eta_ms").unwrap_or(0) as u64,
            rung: row.get::<Option<i16>, _>("pending_rung").unwrap_or(0) as u8,
        });
    Ok(NamespaceRecord {
        name: row.get::<&str, _>("name").parse().map_err(|_| corrupt())?,
        owner: row.get::<&str, _>("owner").parse().map_err(|_| corrupt())?,
        root_key: row.get("root_key"),
        visibility: row.get("visibility"),
        title: row.get("title"),
        description: row.get("description"),
        icon: row.get("icon"),
        recovery_set,
        pending_recovery,
        categories: row
            .get::<Option<String>, _>("categories")
            .unwrap_or_default()
            .split(',')
            .filter(|c| !c.is_empty())
            .map(str::to_string)
            .collect(),
    })
}

fn split_caps(caps: String) -> Vec<String> {
    caps.split(',')
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect()
}

fn report_from_row(row: &sqlx::postgres::PgRow) -> Result<ReportRecord, StoreError> {
    let corrupt = |what: &str| StoreError::Backend(format!("corrupt report row: {what}"));
    let scope = Scope::from_key(row.get::<&str, _>("scope")).ok_or_else(|| corrupt("scope"))?;
    let msgid = format!(
        "{}/{}",
        row.get::<&str, _>("root_origin"),
        row.get::<&str, _>("root_ulid")
    )
    .parse()
    .map_err(|_| corrupt("msgid"))?;
    let held_roots = row
        .get::<&str, _>("held_roots")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| Ulid::from_string(s).map_err(|_| corrupt("held root")))
        .collect::<Result<Vec<_>, _>>()?;
    let resolution = row
        .get::<Option<String>, _>("res_action")
        .map(|action| -> Result<ReportResolution, StoreError> {
            Ok(ReportResolution {
                action: action.parse().map_err(|_| corrupt("res_action"))?,
                note: row.get("res_note"),
                resolved_by: row
                    .get::<&str, _>("res_by")
                    .parse()
                    .map_err(|_| corrupt("res_by"))?,
                at_ms: row.get::<Option<i64>, _>("res_at_ms").unwrap_or(0) as u64,
                hold_release_at: row.get::<Option<i64>, _>("hold_release_at").unwrap_or(0) as u64,
            })
        })
        .transpose()?;
    Ok(ReportRecord {
        id: row.get("id"),
        msgid,
        scope,
        category: row.get("category"),
        state: row
            .get::<&str, _>("state")
            .parse()
            .map_err(|_| corrupt("state"))?,
        reporter: row
            .get::<&str, _>("reporter")
            .parse()
            .map_err(|_| corrupt("reporter"))?,
        note: row.get("note"),
        queue_scopes: row
            .get::<&str, _>("queue_scopes")
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        status: row
            .get::<&str, _>("status")
            .parse()
            .map_err(|_| corrupt("status"))?,
        filed_at_ms: row.get::<i64, _>("filed_at_ms") as u64,
        held_roots,
        resolution,
        holds_released: row.get("holds_released"),
    })
}

#[async_trait]
impl ReportStore for PgStore {
    async fn file_report(&self, mut record: ReportRecord) -> Result<(), StoreError> {
        let scope_key = record.scope.as_key();
        // Verified reports hold the reported root + ±HOLD_RADIUS context.
        if record.state == ContentState::Verified {
            let target = record.msgid.ulid().to_string();
            // Target + up to HOLD_RADIUS older roots, and up to HOLD_RADIUS newer.
            let older: Vec<String> = sqlx::query_scalar(
                "SELECT ulid FROM weft_events WHERE scope = $1 AND kind = 0 AND ulid <= $2 \
                 ORDER BY ulid DESC LIMIT $3",
            )
            .bind(&scope_key)
            .bind(&target)
            .bind(HOLD_RADIUS as i64 + 1)
            .fetch_all(&self.pool)
            .await
            .map_err(backend_err)?;
            let newer: Vec<String> = sqlx::query_scalar(
                "SELECT ulid FROM weft_events WHERE scope = $1 AND kind = 0 AND ulid > $2 \
                 ORDER BY ulid ASC LIMIT $3",
            )
            .bind(&scope_key)
            .bind(&target)
            .bind(HOLD_RADIUS as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(backend_err)?;
            record.held_roots = older
                .iter()
                .chain(newer.iter())
                .map(|s| Ulid::from_string(s).map_err(|_| backend_err("held root")))
                .collect::<Result<Vec<_>, _>>()?;
            for root in &record.held_roots {
                sqlx::query(
                    "INSERT INTO weft_holds (scope, root_ulid, refcount) VALUES ($1, $2, 1) \
                     ON CONFLICT (scope, root_ulid) DO UPDATE \
                     SET refcount = weft_holds.refcount + 1",
                )
                .bind(&scope_key)
                .bind(root.to_string())
                .execute(&self.pool)
                .await
                .map_err(backend_err)?;
            }
        }
        let held_joined = record
            .held_roots
            .iter()
            .map(Ulid::to_string)
            .collect::<Vec<_>>()
            .join(",");
        sqlx::query(
            r#"
            INSERT INTO weft_reports
              (id, scope, root_ulid, root_origin, category, state, reporter, note,
               queue_scopes, status, filed_at_ms, held_roots, holds_released)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,FALSE)
            "#,
        )
        .bind(&record.id)
        .bind(&scope_key)
        .bind(record.msgid.ulid().to_string())
        .bind(record.msgid.origin().as_str())
        .bind(&record.category)
        .bind(record.state.as_str())
        .bind(record.reporter.as_str())
        .bind(&record.note)
        .bind(record.queue_scopes.join(","))
        .bind(record.status.as_str())
        .bind(record.filed_at_ms as i64)
        .bind(held_joined)
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(())
    }

    async fn report(&self, id: &str) -> Result<Option<ReportRecord>, StoreError> {
        let row = sqlx::query("SELECT * FROM weft_reports WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend_err)?;
        row.as_ref().map(report_from_row).transpose()
    }

    async fn list_reports(
        &self,
        scope: &str,
        status: Option<ReportStatus>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ReportRecord>, StoreError> {
        // queue_scopes is a comma-joined list; membership via a wrapped match
        // so `ns:g` never matches `ns:games`.
        let rows = sqlx::query(
            r#"
            SELECT * FROM weft_reports
            WHERE (',' || queue_scopes || ',') LIKE ('%,' || $1 || ',%')
              AND ($2::text IS NULL OR status = $2)
              AND ($3::text IS NULL OR id < $3)
            ORDER BY id DESC
            LIMIT $4
            "#,
        )
        .bind(scope)
        .bind(status.map(|s| s.as_str()))
        .bind(after)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(backend_err)?;
        rows.iter().map(report_from_row).collect()
    }

    async fn resolve_report(
        &self,
        id: &str,
        resolution: ReportResolution,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            r#"
            UPDATE weft_reports
            SET status = 'resolved', res_action = $2, res_note = $3, res_by = $4,
                res_at_ms = $5, hold_release_at = $6
            WHERE id = $1 AND status = 'open'
            "#,
        )
        .bind(id)
        .bind(resolution.action.as_str())
        .bind(&resolution.note)
        .bind(resolution.resolved_by.as_str())
        .bind(resolution.at_ms as i64)
        .bind(resolution.hold_release_at as i64)
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(result.rows_affected() == 1)
    }

    async fn escalate_report(&self, id: &str) -> Result<bool, StoreError> {
        // Append '*' to queue_scopes iff open and not already present.
        let result = sqlx::query(
            r#"
            UPDATE weft_reports
            SET queue_scopes = queue_scopes || ',*'
            WHERE id = $1 AND status = 'open'
              AND (',' || queue_scopes || ',') NOT LIKE '%,*,%'
            "#,
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        if result.rows_affected() == 1 {
            return Ok(true);
        }
        // Distinguish "already had *" (still success) from "no such open report".
        let open: Option<bool> =
            sqlx::query_scalar("SELECT status = 'open' FROM weft_reports WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .map_err(backend_err)?;
        Ok(open == Some(true))
    }

    async fn reports_by_since(&self, reporter: &Account, since_ms: u64) -> Result<u64, StoreError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM weft_reports WHERE reporter = $1 AND filed_at_ms >= $2",
        )
        .bind(reporter.as_str())
        .bind(since_ms as i64)
        .fetch_one(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(count as u64)
    }

    async fn release_due_holds(&self, now_ms: u64) -> Result<u64, StoreError> {
        let due = sqlx::query(
            "SELECT id, scope, held_roots FROM weft_reports \
             WHERE status = 'resolved' AND holds_released = FALSE AND hold_release_at <= $1",
        )
        .bind(now_ms as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(backend_err)?;
        let mut released = 0u64;
        for row in &due {
            let scope: &str = row.get("scope");
            let held: &str = row.get("held_roots");
            for root in held.split(',').filter(|s| !s.is_empty()) {
                sqlx::query(
                    "UPDATE weft_holds SET refcount = refcount - 1 WHERE scope = $1 AND root_ulid = $2",
                )
                .bind(scope)
                .bind(root)
                .execute(&self.pool)
                .await
                .map_err(backend_err)?;
            }
            sqlx::query("UPDATE weft_reports SET holds_released = TRUE WHERE id = $1")
                .bind(row.get::<&str, _>("id"))
                .execute(&self.pool)
                .await
                .map_err(backend_err)?;
            released += 1;
        }
        // Drop fully-released holds so purge/compact stop skipping them.
        sqlx::query("DELETE FROM weft_holds WHERE refcount <= 0")
            .execute(&self.pool)
            .await
            .map_err(backend_err)?;
        Ok(released)
    }
}

fn invite_from_row(row: &sqlx::postgres::PgRow) -> InviteRecord {
    InviteRecord {
        id: row.get("id"),
        scope: row.get("scope"),
        caps: split_caps(row.get("caps")),
        uses_left: row.get::<Option<i32>, _>("uses_left").map(|u| u as u32),
        expiry: row.get::<Option<i64>, _>("expiry").map(|e| e as u64),
    }
}

/// Parse a stored network name back into the validated type; a malformed row
/// is corruption, not a normal outcome.
fn network_from(value: &str) -> Result<NetworkName, StoreError> {
    value
        .parse()
        .map_err(|_| StoreError::Backend(format!("corrupt row: network name {value:?}")))
}

#[async_trait]
impl PeerStore for PgStore {
    async fn upsert_peer(&self, record: PeerRecord) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO weft_peers
                (peer, scope, manifest, version, acked_manifest, severed, created_ms, updated_ms)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
            ON CONFLICT (peer) DO UPDATE SET
                scope = EXCLUDED.scope,
                manifest = EXCLUDED.manifest,
                version = EXCLUDED.version,
                acked_manifest = EXCLUDED.acked_manifest,
                severed = EXCLUDED.severed,
                updated_ms = EXCLUDED.updated_ms
            "#,
        )
        .bind(record.peer.as_str())
        .bind(&record.scope)
        .bind(&record.manifest)
        .bind(record.version as i64)
        .bind(&record.acked_manifest)
        .bind(record.severed)
        .bind(record.created_ms as i64)
        .bind(record.updated_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(())
    }

    async fn peer(&self, peer: &NetworkName) -> Result<Option<PeerRecord>, StoreError> {
        let row = sqlx::query("SELECT * FROM weft_peers WHERE peer = $1")
            .bind(peer.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(backend_err)?;
        row.map(|row| peer_from_row(&row)).transpose()
    }

    async fn list_peers(&self) -> Result<Vec<PeerRecord>, StoreError> {
        let rows = sqlx::query("SELECT * FROM weft_peers ORDER BY peer")
            .fetch_all(&self.pool)
            .await
            .map_err(backend_err)?;
        rows.iter().map(peer_from_row).collect()
    }

    async fn remove_peer(&self, peer: &NetworkName) -> Result<bool, StoreError> {
        let result = sqlx::query("DELETE FROM weft_peers WHERE peer = $1")
            .bind(peer.as_str())
            .execute(&self.pool)
            .await
            .map_err(backend_err)?;
        Ok(result.rows_affected() > 0)
    }
}

fn peer_from_row(row: &sqlx::postgres::PgRow) -> Result<PeerRecord, StoreError> {
    Ok(PeerRecord {
        peer: network_from(row.get("peer"))?,
        scope: row.get("scope"),
        manifest: row.get("manifest"),
        version: row.get::<i64, _>("version") as u64,
        acked_manifest: row.get("acked_manifest"),
        severed: row.get("severed"),
        created_ms: row.get::<i64, _>("created_ms") as u64,
        updated_ms: row.get::<i64, _>("updated_ms") as u64,
    })
}

#[async_trait]
impl ModerationStore for PgStore {
    async fn set_moderation(&self, record: ModRecord) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO weft_moderation (scope, account, kind, actor, reason, at_ms)
            VALUES ($1,$2,$3,$4,$5,$6)
            ON CONFLICT (scope, account, kind) DO UPDATE SET
                actor = EXCLUDED.actor, reason = EXCLUDED.reason, at_ms = EXCLUDED.at_ms
            "#,
        )
        .bind(&record.scope)
        .bind(record.account.as_str())
        .bind(record.kind.as_str())
        .bind(&record.actor)
        .bind(&record.reason)
        .bind(record.at_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(())
    }

    async fn clear_moderation(
        &self,
        scope: &str,
        account: &Account,
        kind: ModKind,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "DELETE FROM weft_moderation WHERE scope = $1 AND account = $2 AND kind = $3",
        )
        .bind(scope)
        .bind(account.as_str())
        .bind(kind.as_str())
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(result.rows_affected() > 0)
    }

    async fn is_moderated(
        &self,
        account: &Account,
        scopes: &[String],
        kind: ModKind,
    ) -> Result<bool, StoreError> {
        if scopes.is_empty() {
            return Ok(false);
        }
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM weft_moderation WHERE account = $1 AND kind = $2 AND scope = ANY($3))",
        )
        .bind(account.as_str())
        .bind(kind.as_str())
        .bind(scopes)
        .fetch_one(&self.pool)
        .await
        .map_err(backend_err)
    }

    async fn list_moderation(&self, scope: &str) -> Result<Vec<ModRecord>, StoreError> {
        let rows = sqlx::query("SELECT * FROM weft_moderation WHERE scope = $1 ORDER BY account")
            .bind(scope)
            .fetch_all(&self.pool)
            .await
            .map_err(backend_err)?;
        rows.iter()
            .map(|row| {
                let kind = match row.get::<&str, _>("kind") {
                    "ban" => ModKind::Ban,
                    _ => ModKind::Mute,
                };
                Ok(ModRecord {
                    scope: row.get("scope"),
                    account: row
                        .get::<&str, _>("account")
                        .parse()
                        .map_err(|_| StoreError::Backend("corrupt row: account".to_string()))?,
                    kind,
                    actor: row.get("actor"),
                    reason: row.get("reason"),
                    at_ms: row.get::<i64, _>("at_ms") as u64,
                })
            })
            .collect()
    }
}

#[async_trait]
impl NetblockStore for PgStore {
    async fn add_netblock(&self, record: NetblockRecord) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO weft_netblocks (network, reason, added_ms, actor)
            VALUES ($1,$2,$3,$4)
            ON CONFLICT (network) DO UPDATE SET
                reason = EXCLUDED.reason,
                added_ms = EXCLUDED.added_ms,
                actor = EXCLUDED.actor
            "#,
        )
        .bind(record.network.as_str())
        .bind(&record.reason)
        .bind(record.added_ms as i64)
        .bind(&record.actor)
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(())
    }

    async fn remove_netblock(&self, network: &NetworkName) -> Result<bool, StoreError> {
        let result = sqlx::query("DELETE FROM weft_netblocks WHERE network = $1")
            .bind(network.as_str())
            .execute(&self.pool)
            .await
            .map_err(backend_err)?;
        Ok(result.rows_affected() > 0)
    }

    async fn is_netblocked(&self, network: &NetworkName) -> Result<bool, StoreError> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM weft_netblocks WHERE network = $1)")
            .bind(network.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(backend_err)
    }

    async fn list_netblocks(&self) -> Result<Vec<NetblockRecord>, StoreError> {
        let rows = sqlx::query("SELECT * FROM weft_netblocks ORDER BY network")
            .fetch_all(&self.pool)
            .await
            .map_err(backend_err)?;
        rows.iter()
            .map(|row| {
                Ok(NetblockRecord {
                    network: network_from(row.get("network"))?,
                    reason: row.get("reason"),
                    added_ms: row.get::<i64, _>("added_ms") as u64,
                    actor: row.get("actor"),
                })
            })
            .collect()
    }
}

#[async_trait]
impl PinStore for PgStore {
    async fn set_pin(
        &self,
        channel: &ChannelName,
        msgid: &MsgId,
        pinned: bool,
    ) -> Result<(), StoreError> {
        if pinned {
            sqlx::query(
                "INSERT INTO weft_pins (channel, msgid, ulid) VALUES ($1,$2,$3) \
                 ON CONFLICT (channel, msgid) DO NOTHING",
            )
            .bind(channel.as_str())
            .bind(msgid.to_string())
            .bind(msgid.ulid().to_string())
            .execute(&self.pool)
            .await
            .map_err(backend_err)?;
        } else {
            sqlx::query("DELETE FROM weft_pins WHERE channel = $1 AND msgid = $2")
                .bind(channel.as_str())
                .bind(msgid.to_string())
                .execute(&self.pool)
                .await
                .map_err(backend_err)?;
        }
        Ok(())
    }

    async fn pins(&self, channel: &ChannelName) -> Result<Vec<MsgId>, StoreError> {
        let rows = sqlx::query("SELECT msgid FROM weft_pins WHERE channel = $1 ORDER BY ulid ASC")
            .bind(channel.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(backend_err)?;
        rows.iter()
            .map(|r| {
                r.get::<&str, _>("msgid")
                    .parse()
                    .map_err(|_| StoreError::Backend("corrupt pin msgid".to_string()))
            })
            .collect()
    }
}

#[async_trait]
impl MembershipStore for PgStore {
    async fn set_membership(
        &self,
        account: &Account,
        channel: &ChannelName,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO weft_memberships (account, channel) VALUES ($1,$2) \
             ON CONFLICT (account, channel) DO NOTHING",
        )
        .bind(account.as_str())
        .bind(channel.as_str())
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(())
    }

    async fn clear_membership(
        &self,
        account: &Account,
        channel: &ChannelName,
    ) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM weft_memberships WHERE account = $1 AND channel = $2")
            .bind(account.as_str())
            .bind(channel.as_str())
            .execute(&self.pool)
            .await
            .map_err(backend_err)?;
        Ok(())
    }

    async fn memberships(&self, account: &Account) -> Result<Vec<ChannelName>, StoreError> {
        let rows = sqlx::query("SELECT channel FROM weft_memberships WHERE account = $1")
            .bind(account.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(backend_err)?;
        rows.iter()
            .map(|r| {
                r.get::<&str, _>("channel")
                    .parse()
                    .map_err(|_| StoreError::Backend("corrupt membership channel".to_string()))
            })
            .collect()
    }
}

#[async_trait]
impl RoleStore for PgStore {
    async fn set_role(
        &self,
        scope: &str,
        name: &str,
        color: &str,
        caps: &[String],
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO weft_roles (scope, name, color, caps) VALUES ($1,$2,$3,$4) \
             ON CONFLICT (scope, name) DO UPDATE SET color = EXCLUDED.color, caps = EXCLUDED.caps",
        )
        .bind(scope)
        .bind(name)
        .bind(color)
        .bind(caps.join(","))
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(())
    }

    async fn delete_role(&self, scope: &str, name: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM weft_roles WHERE scope = $1 AND name = $2")
            .bind(scope)
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(backend_err)?;
        sqlx::query("DELETE FROM weft_role_assignments WHERE scope = $1 AND name = $2")
            .bind(scope)
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(backend_err)?;
        Ok(())
    }

    async fn roles(&self, scope: &str) -> Result<Vec<RoleDef>, StoreError> {
        let rows =
            sqlx::query("SELECT name, color, caps FROM weft_roles WHERE scope = $1 ORDER BY name")
                .bind(scope)
                .fetch_all(&self.pool)
                .await
                .map_err(backend_err)?;
        Ok(rows
            .iter()
            .map(|r| {
                let caps: &str = r.get("caps");
                RoleDef {
                    name: r.get::<&str, _>("name").to_string(),
                    color: r.get::<&str, _>("color").to_string(),
                    caps: caps
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect(),
                }
            })
            .collect())
    }

    async fn assign_role(
        &self,
        scope: &str,
        name: &str,
        account: &Account,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO weft_role_assignments (scope, name, account) VALUES ($1,$2,$3) \
             ON CONFLICT (scope, name, account) DO NOTHING",
        )
        .bind(scope)
        .bind(name)
        .bind(account.as_str())
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(())
    }

    async fn unassign_role(
        &self,
        scope: &str,
        name: &str,
        account: &Account,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "DELETE FROM weft_role_assignments WHERE scope = $1 AND name = $2 AND account = $3",
        )
        .bind(scope)
        .bind(name)
        .bind(account.as_str())
        .execute(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(())
    }

    async fn roles_of(&self, scope: &str, account: &Account) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query(
            "SELECT name FROM weft_role_assignments WHERE scope = $1 AND account = $2 ORDER BY name",
        )
        .bind(scope)
        .bind(account.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(backend_err)?;
        Ok(rows
            .iter()
            .map(|r| r.get::<&str, _>("name").to_string())
            .collect())
    }

    async fn role_members(&self, scope: &str, name: &str) -> Result<Vec<Account>, StoreError> {
        let rows =
            sqlx::query("SELECT account FROM weft_role_assignments WHERE scope = $1 AND name = $2")
                .bind(scope)
                .bind(name)
                .fetch_all(&self.pool)
                .await
                .map_err(backend_err)?;
        rows.iter()
            .map(|r| {
                r.get::<&str, _>("account")
                    .parse()
                    .map_err(|_| StoreError::Backend("corrupt role member".to_string()))
            })
            .collect()
    }
}
