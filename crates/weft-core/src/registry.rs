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
    /// §11.13 home-authoritative channels: the network that owns a channel's
    /// namespace is its sole ULID writer. This map holds only channels whose home
    /// is **another** network (a replica we mirror); a channel absent here is
    /// home-local (the common case — created or seeded here), so existing
    /// channels need no entry and mint locally exactly as before.
    homes: RwLock<HashMap<ChannelName, NetworkName>>,
    network: NetworkName,
    store: Arc<dyn EventStore>,
    /// §13 media reference index, handed to each channel actor so it records
    /// blob references as it mints msgids (the single-writer point).
    media: Arc<dyn weft_store::MediaStore>,
    /// §6.4 pins, handed to each actor so a delete can clear the pin of the
    /// message it tombstones (the same single-writer argument).
    pins: Arc<dyn weft_store::PinStore>,
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
        media: Arc<dyn weft_store::MediaStore>,
        pins: Arc<dyn weft_store::PinStore>,
    ) -> Self {
        let channels = channels
            .into_iter()
            .map(|(name, policy)| {
                let handle = channel::spawn(
                    name.clone(),
                    network.clone(),
                    policy,
                    Arc::clone(&store),
                    Arc::clone(&media),
                    Arc::clone(&pins),
                );
                (name, handle)
            })
            .collect();
        Self {
            channels: RwLock::new(channels),
            homes: RwLock::new(HashMap::new()),
            network,
            store,
            media,
            pins,
        }
    }

    /// The home network of a channel (§11.13) — the sole ULID writer for it.
    /// Defaults to this network unless the channel was provisioned as a replica
    /// of a foreign-owned namespace via [`set_home`](Self::set_home).
    pub fn home(&self, name: &ChannelName) -> NetworkName {
        self.homes
            .read()
            .expect("registry lock")
            .get(name)
            .cloned()
            .unwrap_or_else(|| self.network.clone())
    }

    /// True if this network is the home (sole writer) of the channel.
    pub fn is_home(&self, name: &ChannelName) -> bool {
        self.home(name) == self.network
    }

    /// Record a channel's home network. Passing this network clears the entry
    /// (the channel becomes home-local). Called when a spoke provisions a replica
    /// of a foreign namespace's channel (from an acked manifest, §11.1).
    pub fn set_home(&self, name: ChannelName, home: NetworkName) {
        let mut homes = self.homes.write().expect("registry lock");
        if home == self.network {
            homes.remove(&name);
        } else {
            homes.insert(name, home);
        }
    }

    /// §11.13 provision a **replica** of a foreign-owned channel: spawn a local
    /// actor (so mirrored events have somewhere to land and local members can join
    /// and relay) and record its `home`. No-op if the channel already exists. A
    /// replica never mints — a member's post is relayed to the home — so it is a
    /// local cache; it uses `Permanent` retention for now (per-channel retention
    /// sync from the manifest is a follow-up). Returns the handle, or `None` if the
    /// channel already existed.
    pub(crate) fn provision_replica(
        &self,
        name: ChannelName,
        home: NetworkName,
    ) -> Option<ChannelHandle> {
        let mut channels = self.channels.write().expect("registry lock");
        if channels.contains_key(&name) {
            return None;
        }
        let handle = channel::spawn(
            name.clone(),
            self.network.clone(),
            RetentionPolicy::Permanent,
            Arc::clone(&self.store),
            Arc::clone(&self.media),
            Arc::clone(&self.pins),
        );
        channels.insert(name.clone(), handle.clone());
        drop(channels);

        self.set_home(name, home);
        Some(handle)
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
            Arc::clone(&self.media),
            Arc::clone(&self.pins),
        );
        channels.insert(name, handle.clone());
        Some(handle)
    }

    /// Remove a channel (CHANNEL DELETE). Dropping the handle closes the
    /// actor's inbox, so the task winds down once its last message drains.
    pub(crate) fn remove(&self, name: &ChannelName) -> Option<ChannelHandle> {
        self.channels.write().expect("registry lock").remove(name)
    }

    /// Move a channel actor to a new name (CHANNEL RENAME). The old actor is
    /// dropped and a fresh one spawned under `new` — history is served from the
    /// store, which the caller re-keys first, so the new actor sees it all.
    /// Returns `false` if `old` is absent or `new` already taken (announce the
    /// rename to members via the OLD handle *before* calling this, so the
    /// broadcast reaches them before their forwarders close).
    pub(crate) fn rename(
        &self,
        old: &ChannelName,
        new: ChannelName,
        policy: RetentionPolicy,
    ) -> bool {
        let mut channels = self.channels.write().expect("registry lock");
        if !channels.contains_key(old) || channels.contains_key(&new) {
            return false;
        }
        channels.remove(old);
        let handle = channel::spawn(
            new.clone(),
            self.network.clone(),
            policy,
            Arc::clone(&self.store),
            Arc::clone(&self.media),
            Arc::clone(&self.pins),
        );
        channels.insert(new, handle);
        true
    }
}
