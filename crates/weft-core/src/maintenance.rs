//! Background storage duties (§12): the retention purge (per-channel
//! policy) and the §12.1 compaction pass (after the audit window). One
//! periodic task; each tick is idempotent, so timing is soft.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{debug, error, info};
use weft_proto::{ChannelName, RetentionPolicy};
use weft_store::{EventStore, NamespaceStore, ReportStore, Scope};

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
            run_pass(&store, &channels, dm_policy, config.compact_after).await;
            let applied = apply_due_recoveries(&namespaces, unix_now_ms()).await;
            if applied > 0 {
                info!(applied, "namespace recoveries applied (§2.4)");
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

async fn run_pass(
    store: &Arc<dyn EventStore>,
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
        match store
            .purge_before(&Scope::Channel(channel.clone()), cutoff)
            .await
        {
            Ok(count) => purged += count,
            Err(e) => error!(%channel, "purge failed: {e}"),
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
