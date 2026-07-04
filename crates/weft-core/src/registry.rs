//! Channel registry. M1's channel set is fixed at startup (config), so
//! this is an immutable map — no locking anywhere. The architecture doc's
//! `DashMap` + lazy actor spawn arrives with dynamic `CHANNEL CREATE` (M4).

use std::collections::HashMap;

use weft_proto::{ChannelName, NetworkName};

use crate::channel::{self, ChannelHandle};

#[derive(Debug)]
pub struct Registry {
    channels: HashMap<ChannelName, ChannelHandle>,
}

impl Registry {
    pub(crate) fn spawn(
        channels: impl IntoIterator<Item = ChannelName>,
        network: NetworkName,
    ) -> Self {
        let channels = channels
            .into_iter()
            .map(|name| (name.clone(), channel::spawn(name, network.clone())))
            .collect();
        Self { channels }
    }

    pub fn get(&self, name: &ChannelName) -> Option<&ChannelHandle> {
        self.channels.get(name)
    }
}
