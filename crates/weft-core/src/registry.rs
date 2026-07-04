//! Channel registry. Channels are seeded at boot from the store and then
//! mutated at runtime by `CHANNEL CREATE`/`DELETE` (M4). An `RwLock` around
//! the map gives interior mutability; handles are cloned out under a brief
//! read lock (never held across an `.await`), so the actors keep running
//! lock-free on the hot path.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use weft_proto::{ChannelName, NetworkName, RetentionPolicy};
use weft_store::EventStore;

use crate::channel::{self, ChannelHandle};

pub struct Registry {
    channels: RwLock<HashMap<ChannelName, ChannelHandle>>,
    network: NetworkName,
    store: Arc<dyn EventStore>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field(
                "channels",
                &self.channels.read().expect("registry lock").len(),
            )
            .finish()
    }
}

impl Registry {
    pub(crate) fn spawn(
        channels: impl IntoIterator<Item = (ChannelName, RetentionPolicy)>,
        network: NetworkName,
        store: Arc<dyn EventStore>,
    ) -> Self {
        let channels = channels
            .into_iter()
            .map(|(name, policy)| {
                let handle =
                    channel::spawn(name.clone(), network.clone(), policy, Arc::clone(&store));
                (name, handle)
            })
            .collect();
        Self {
            channels: RwLock::new(channels),
            network,
            store,
        }
    }

    /// The handle for a channel, cloned out (cheap: two Arcs).
    pub fn get(&self, name: &ChannelName) -> Option<ChannelHandle> {
        self.channels
            .read()
            .expect("registry lock")
            .get(name)
            .cloned()
    }

    pub fn exists(&self, name: &ChannelName) -> bool {
        self.channels
            .read()
            .expect("registry lock")
            .contains_key(name)
    }

    /// Spawn a channel actor and register it (CHANNEL CREATE). Returns the
    /// handle, or `None` if the channel already exists.
    pub(crate) fn create(
        &self,
        name: ChannelName,
        policy: RetentionPolicy,
    ) -> Option<ChannelHandle> {
        let mut channels = self.channels.write().expect("registry lock");
        if channels.contains_key(&name) {
            return None;
        }
        let handle = channel::spawn(
            name.clone(),
            self.network.clone(),
            policy,
            Arc::clone(&self.store),
        );
        channels.insert(name, handle.clone());
        Some(handle)
    }

    /// Remove a channel (CHANNEL DELETE). Dropping the handle closes the
    /// actor's inbox, so the task winds down once its last message drains.
    pub(crate) fn remove(&self, name: &ChannelName) -> Option<ChannelHandle> {
        self.channels.write().expect("registry lock").remove(name)
    }
}
