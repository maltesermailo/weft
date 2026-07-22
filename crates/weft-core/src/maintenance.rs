//! Background storage duties (§12): the retention purge (per-channel
//! policy) and the §12.1 compaction pass (after the audit window). One
//! periodic task; each tick is idempotent, so timing is soft.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{debug, error, info};
use weft_proto::{ChannelName, RetentionPolicy};
use weft_store::{
    AccountStore, BlobHash, BlobStore, EmojiStore, EventStore, MediaStore, NamespaceStore,
    ProfileStore, ReportStore, Scope,
};

/// §13 grace before an orphaned blob is GC'd — an uploaded-but-not-yet-posted
/// blob survives this window (it has no references until the MSG lands).
const MEDIA_GC_GRACE_MS: u64 = 60 * 60 * 1000; // 1 hour

#[derive(Debug, Clone)]
pub struct MaintenanceConfig {
    /// How often to run a pass.
    pub interval: Duration,
    /// §12.1 `compact-after` audit window (default 24 h, network config).
    pub compact_after: Duration,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(300),
            compact_after: Duration::from_secs(24 * 3600),
        }
    }
}

/// Spawn the maintenance loop over the (static) channel set. Also drives
/// the §2.4 recovery scheduler: pending recoveries whose delay window has
/// elapsed are applied on each tick.
#[allow(clippy::too_many_arguments)]
pub fn spawn_maintenance(
    store: Arc<dyn EventStore>,
    namespaces: Arc<dyn NamespaceStore>,
    reports: Arc<dyn ReportStore>,
    media_refs: Arc<dyn MediaStore>,
    blobs: Arc<dyn BlobStore>,
    profiles: Arc<dyn ProfileStore>,
    emoji: Arc<dyn EmojiStore>,
    accounts: Arc<dyn AccountStore>,
    channels: Vec<(ChannelName, RetentionPolicy)>,
    dm_policy: RetentionPolicy,
    config: MaintenanceConfig,
    shutdown: tokio_util::sync::CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(config.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await; // immediate first tick: skip, let traffic settle
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown.cancelled() => break, // exit promptly on shutdown
            }
            run_pass(
                &store,
                &media_refs,
                &channels,
                dm_policy,
                config.compact_after,
            )
            .await;
            // §13 collect blobs orphaned past the grace window.
            let cutoff = unix_now_ms().saturating_sub(MEDIA_GC_GRACE_MS);
            let gc = gc_orphan_blobs(&media_refs, &blobs, &profiles, &emoji, cutoff).await;
            if gc > 0 {
                info!(collected = gc, "orphaned media blobs GC'd (§13)");
            }
            let applied = apply_due_recoveries(&namespaces, unix_now_ms()).await;
            if applied > 0 {
                info!(applied, "namespace recoveries applied (§2.4)");
            }
            // WC3: finalize accounts whose soft-delete grace window has elapsed.
            let purged_accounts = purge_due_deletions(&accounts, unix_now_ms()).await;
            if purged_accounts > 0 {
                info!(
                    purged_accounts,
                    "scheduled account deletions finalized (WC3)"
                );
            }
            // §12.1: release retention holds whose report has resolved past
            // its grace window, so purge/compaction can resume on that
            // content (invariant 11).
            match reports.release_due_holds(unix_now_ms()).await {
                Ok(n) if n > 0 => info!(released = n, "retention holds released (§12.1)"),
                Ok(_) => {}
                Err(e) => error!("hold release failed: {e}"),
            }
        }
    })
}

/// §13 GC orphaned blobs: delete the bytes of every blob uploaded before
/// `cutoff_ms` with **zero** live references, and forget its tracking row.
/// Returns the count. `cutoff_ms` should be `now − grace` so an uploaded-but-
/// not-yet-referenced blob survives. Split out so it is unit/conformance-testable
/// without waiting for the maintenance interval.
pub async fn gc_orphan_blobs(
    media_refs: &Arc<dyn MediaStore>,
    blobs: &Arc<dyn BlobStore>,
    profiles: &Arc<dyn ProfileStore>,
    emoji: &Arc<dyn EmojiStore>,
    cutoff_ms: u64,
) -> usize {
    let orphans = match media_refs.orphans(cutoff_ms).await {
        Ok(orphans) => orphans,
        Err(e) => {
            error!("orphan scan failed: {e}");
            return 0;
        }
    };
    // §9.4 custom-emoji images are referenced by `weft_emoji`, not a message —
    // keep any blob a live emoji points at (the ref is `weft-media://<o>/<hash>`).
    let emoji_hashes: std::collections::HashSet<String> = emoji
        .emoji_media()
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|uri| crate::media::parse_media_uri(uri).map(|(_, hash)| hash.to_string()))
        .collect();
    let mut removed = 0;
    for hash in orphans {
        // §10.3 an avatar is referenced by a profile, not a message — keep it.
        if profiles.avatar_exists(&hash).await.unwrap_or(false) {
            continue;
        }
        if emoji_hashes.contains(&hash) {
            continue;
        }
        if let Some(parsed) = BlobHash::parse(&hash) {
            if let Err(e) = blobs.delete(&parsed).await {
                error!("blob delete failed: {e}");
                continue;
            }
        }
        let _ = media_refs.forget_blob(&hash).await;
        removed += 1;
    }
    removed
}

/// Apply every pending recovery whose delay window has elapsed: rotate the
/// root key + owner and record it in `root-history` (rung 3 marked
/// operator-initiated forever). Idempotent per tick; returns the count.
/// Split out so it is unit-testable without waiting real days.
pub async fn apply_due_recoveries(namespaces: &Arc<dyn NamespaceStore>, now_ms: u64) -> u64 {
    let due = match namespaces.due_recoveries(now_ms).await {
        Ok(due) => due,
        Err(e) => {
            error!("recovery scan failed: {e}");
            return 0;
        }
    };
    let mut applied = 0;
    for ns in due {
        let Some(pending) = ns.pending_recovery else {
            continue;
        };
        let operator_initiated = pending.rung == 3;
        match namespaces
            .rotate_root(
                &ns.name,
                &pending.new_owner,
                &pending.new_root_key,
                operator_initiated,
                now_ms,
            )
            .await
        {
            Ok(()) => {
                applied += 1;
                debug!(namespace = %ns.name, rung = pending.rung, "recovery applied");
            }
            Err(e) => error!(namespace = %ns.name, "recovery apply failed: {e}"),
        }
    }
    applied
}

/// WC3: hard-delete every account whose soft-delete grace window has elapsed
/// (`purge_at_ms <= now_ms`). Idempotent per tick; returns the count. Split out
/// so it is unit-testable without waiting the grace window.
pub async fn purge_due_deletions(accounts: &Arc<dyn AccountStore>, now_ms: u64) -> u64 {
    let due = match accounts.due_deletions(now_ms).await {
        Ok(due) => due,
        Err(e) => {
            error!("scheduled-deletion scan failed: {e}");
            return 0;
        }
    };
    let mut purged = 0;
    for account in due {
        match accounts.delete_account(&account).await {
            Ok(true) => purged += 1,
            Ok(false) => {} // already gone
            Err(e) => error!(%account, "scheduled account delete failed: {e}"),
        }
    }
    purged
}

async fn run_pass(
    store: &Arc<dyn EventStore>,
    media_refs: &Arc<dyn MediaStore>,
    channels: &[(ChannelName, RetentionPolicy)],
    dm_policy: RetentionPolicy,
    compact_after: Duration,
) {
    let now_ms = unix_now_ms();
    let mut purged = 0;

    // Retention purge: only `retained:<dur>` expires; `permanent` keeps,
    // `ephemeral` never stored anything (§5.2).
    for (channel, policy) in channels {
        let RetentionPolicy::Retained(duration) = policy else {
            continue;
        };
        let cutoff = now_ms.saturating_sub(duration.as_secs() * 1_000);
        let scope = Scope::Channel(channel.clone());
        match store.purge_before(&scope, cutoff).await {
            Ok(count) => purged += count,
            Err(e) => error!(%channel, "purge failed: {e}"),
        }
        // §13 purged messages release their blob references (refcount → retention).
        if let Err(e) = media_refs.drop_refs_before(&scope, cutoff).await {
            error!(%channel, "media ref purge failed: {e}");
        }
    }
    if let RetentionPolicy::Retained(duration) = dm_policy {
        let cutoff = now_ms.saturating_sub(duration.as_secs() * 1_000);
        match store.purge_dms_before(cutoff).await {
            Ok(count) => purged += count,
            Err(e) => error!("DM purge failed: {e}"),
        }
    }

    // §12.1 compaction after the audit window — policy-independent.
    let compacted = match store
        .compact_before(now_ms.saturating_sub(compact_after.as_millis() as u64))
        .await
    {
        Ok(count) => count,
        Err(e) => {
            error!("compaction failed: {e}");
            0
        }
    };

    if purged > 0 || compacted > 0 {
        debug!(purged, compacted, "maintenance pass");
    }
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use weft_store::{AccountStore, MemoryStore};

    #[tokio::test]
    async fn purge_due_deletions_finalizes_only_past_windows() {
        let store = Arc::new(MemoryStore::default());
        let past: weft_proto::Account = "past".parse().unwrap();
        let future: weft_proto::Account = "future".parse().unwrap();
        for a in [&past, &future] {
            store.register(a, "phc").await.unwrap();
        }
        // `past` is due at now=1_000; `future` isn't.
        store.schedule_deletion(&past, 500).await.unwrap();
        store.schedule_deletion(&future, 5_000).await.unwrap();

        let accounts: Arc<dyn AccountStore> = store.clone();
        let purged = purge_due_deletions(&accounts, 1_000).await;

        assert_eq!(purged, 1);
        assert!(!store.list_accounts().await.unwrap().contains(&past));
        assert!(store.list_accounts().await.unwrap().contains(&future));
        // Idempotent: nothing new to purge on the next tick at the same time.
        assert_eq!(purge_due_deletions(&accounts, 1_000).await, 0);
    }
}
