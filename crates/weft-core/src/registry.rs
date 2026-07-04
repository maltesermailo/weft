//! Channel registry. The channel set is fixed at startup (config), so
//! this is an immutable map — no locking anywhere. The architecture doc's
//! `DashMap` + lazy actor spawn arrives with dynamic `CHANNEL CREATE` (M4).

use std::collections::HashMap;
use std::sync::Arc;

use weft_proto::{ChannelName, NetworkName, RetentionPolicy};
use weft_store::EventStore;

use crate::channel::{self, ChannelHandle};

#[derive(Debug)]
pub struct Registry {
    channels: HashMap<ChannelName, ChannelHandle>,
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
        Self { channels }
    }

    pub fn get(&self, name: &ChannelName) -> Option<&ChannelHandle> {
        self.channels.get(name)
    }
}
