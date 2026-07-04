//! Background storage duties (§12): the retention purge (per-channel
//! policy) and the §12.1 compaction pass (after the audit window). One
//! periodic task; each tick is idempotent, so timing is soft.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{debug, error};
use weft_proto::{ChannelName, RetentionPolicy};
use weft_store::{EventStore, Scope};

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

/// Spawn the maintenance loop over the (static) channel set.
pub fn spawn_maintenance(
    store: Arc<dyn EventStore>,
    channels: Vec<(ChannelName, RetentionPolicy)>,
    dm_policy: RetentionPolicy,
    config: MaintenanceConfig,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(config.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await; // immediate first tick: skip, let traffic settle
        loop {
            interval.tick().await;
            run_pass(&store, &channels, dm_policy, config.compact_after).await;
        }
    })
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
