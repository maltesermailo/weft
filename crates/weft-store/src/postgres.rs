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
use weft_proto::{Account, ChannelName, MsgId, MsgMeta, RetentionPolicy, Ulid};

use crate::compact::compaction_plan;
use crate::traits::{AccountStore, ChannelStore, EventStore};
use crate::types::{EventKind, EventRecord, Page, Scope, Verification};
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
            let mut families: HashMap<Ulid, Vec<EventRecord>> = HashMap::new();
            for record in records {
                families.entry(record.root.ulid()).or_default().push(record);
            }
            let drops: Vec<String> = families
                .values()
                .flat_map(|family| compaction_plan(family, cutoff_ms))
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
}
