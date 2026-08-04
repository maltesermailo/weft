//! §6.9 `SYNC` (v0.12): one-shot client state sync — the skeleton + delta that
//! replace the startup DISCOVER/CHANNELS/HISTORY loop (docs/architecture/
//! namespace-membership-sync-v0.12.md Part 3).
//!
//! The **skeleton** (fresh, no cursor), the **delta** (`since=<cursor>`) over
//! the message + DM event feed, and the **metadata delta** (channel layout /
//! policy + namespace NS-META changes). Message *previews* are legally withheld
//! (a client fetches cold channels via `HISTORY`), so the separate data-plane
//! body stream is a later optimization; role/friend/group/pin metadata are
//! re-fetched by the client on connect, so they need no server-side delta.

use super::*;
use std::collections::HashMap;

impl<S: ControlStream> Session<S> {
    pub(super) async fn on_sync(
        &mut self,
        label: Option<String>,
        since: Option<String>,
        _preview: Option<u32>,
        account: Account,
    ) -> io::Result<Flow> {
        match since {
            Some(cursor) => self.on_sync_delta(label, cursor, account).await,
            None => self.on_sync_fresh(label, account).await,
        }
    }

    /// Fresh login: the inline **skeleton** — per namespace the account belongs
    /// to, `NS-META` + (`CHANNEL-LAYOUT`, `POLICY`, `MARKED`, `UNREAD-COUNTS`)
    /// per visible channel; plus top-level channels — terminated by
    /// `@cursor=<token> SYNC END`. Previews are withheld (client fetches via
    /// `HISTORY`), so there is no body stream in this increment.
    async fn on_sync_fresh(&mut self, label: Option<String>, account: Account) -> io::Result<Flow> {
        // Read markers indexed by channel — MARKED + UNREAD-COUNTS sources.
        let marks: HashMap<ChannelName, weft_proto::MsgId> = self
            .ctx
            .accounts
            .marks(&account)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(target, msgid)| target.parse::<ChannelName>().ok().map(|c| (c, msgid)))
            .collect();

        // Namespaces the account belongs to, each with its visible channels.
        let namespaces = self
            .ctx
            .memberships
            .ns_memberships(account.as_str())
            .await
            .unwrap_or_default();
        for ns in namespaces {
            if let Ok(Some(record)) = self.ctx.namespaces.namespace_by_id(&ns).await {
                self.send_event(label.clone(), self.ns_meta_event(&record))
                    .await?;
            }
            // Convey the membership itself (not just the ns-meta) so the client
            // shows a server I belong to even when it has **zero** channels — the
            // channel layout below can't carry that. Mirrors the live NS-MEMBER.
            if let Ok(id) = ns.parse::<weft_proto::NamespaceId>() {
                let me = UserRef::new(account.clone(), self.ctx.info.network.clone());
                self.send_event(
                    label.clone(),
                    Event::NsMember {
                        namespace: id,
                        user: me,
                        action: MemberAction::Join,
                        display: None,
                        count: None,
                    },
                )
                .await?;
            }
            let channels = self
                .ctx
                .channel_store
                .channels_in_namespace(ns.as_str())
                .await
                .unwrap_or_default();
            for (channel, record) in channels {
                // A view-gated channel the account can't see is invisible on
                // every surface (invariant 1, inside the membership boundary).
                if self.view_gated_denied(&channel, &account).await {
                    continue;
                }
                // Hidden channels stay out of the sidebar (Part 1.1).
                if self
                    .ctx
                    .memberships
                    .is_hidden(&account, &channel)
                    .await
                    .unwrap_or(false)
                {
                    continue;
                }
                // The layout row shows the channel (voice channels too, §16).
                self.send_event(
                    label.clone(),
                    Event::ChannelLayout {
                        channel: channel.clone(),
                        category: record.category.clone(),
                        position: record.position,
                        kind: record.kind,
                        vanity: record.vanity.clone(),
                        origin: record.origin.as_deref().and_then(|o| o.parse().ok()),
                    },
                )
                .await?;
                // Voice channels carry no policy/read-state — layout only.
                if record.kind != ChannelKind::Voice {
                    self.emit_channel_state(&label, &channel, record.policy, &account, &marks)
                        .await?;
                }
            }
        }

        // Top-level channels: no namespace, no layout — policy + read state.
        for channel in self
            .ctx
            .memberships
            .memberships(&account)
            .await
            .unwrap_or_default()
        {
            let policy = self
                .ctx
                .channel_store
                .channel(&channel)
                .await
                .ok()
                .flatten()
                .map(|r| r.policy)
                .unwrap_or(RetentionPolicy::Ephemeral);
            self.emit_channel_state(&label, &channel, policy, &account, &marks)
                .await?;
        }

        let cursor = self.ctx.events.sync_cursor().await.unwrap_or_default();
        self.send_event(label, Event::SyncEnd { cursor }).await?;
        Ok(Flow::Continue)
    }

    /// A channel's `POLICY` + (if a read marker exists) `MARKED` +
    /// `UNREAD-COUNTS`, shared by the namespaced and top-level skeleton paths.
    async fn emit_channel_state(
        &mut self,
        label: &Option<String>,
        channel: &ChannelName,
        policy: RetentionPolicy,
        account: &Account,
        marks: &HashMap<ChannelName, weft_proto::MsgId>,
    ) -> io::Result<()> {
        self.send_event(
            label.clone(),
            Event::Policy {
                channel: channel.clone(),
                policy,
            },
        )
        .await?;
        if let Some(msgid) = marks.get(channel) {
            self.send_event(
                label.clone(),
                Event::Marked {
                    channel: channel.clone(),
                    msgid: msgid.clone(),
                },
            )
            .await?;
            if let Ok((unread, mentions)) = self
                .ctx
                .events
                .unread_counts(&Scope::Channel(channel.clone()), account, msgid.ulid())
                .await
            {
                self.send_event(
                    label.clone(),
                    Event::UnreadCounts {
                        channel: channel.clone(),
                        unread,
                        mentions,
                    },
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Reconnect: serve every message event with `seq > cursor` in the account's
    /// visible channels, as **materialized** upsert rows (final `MESSAGE` +
    /// `REACTIONS` + `DELETED` — never event chains), then `SYNC END` with a
    /// fresh cursor. A cursor whose epoch no longer matches (restore-from-backup)
    /// falls back to a full fresh sync (Part 2.4).
    async fn on_sync_delta(
        &mut self,
        label: Option<String>,
        cursor: String,
        account: Account,
    ) -> io::Result<Flow> {
        let current = self.ctx.events.sync_cursor().await.unwrap_or_default();
        let current_epoch = current.rsplit_once(':').map(|(e, _)| e).unwrap_or("");
        let since_seq = match cursor.rsplit_once(':') {
            Some((epoch, seq)) if epoch == current_epoch => seq.parse::<i64>().unwrap_or(0),
            // Stale epoch or malformed → treat as a cursor-less fresh sync.
            _ => return self.on_sync_fresh(label, account).await,
        };

        // The visible scopes to scan, each with the wire `Target` it maps to:
        // the derived channels plus the account's DM conversations. (Channel +
        // namespace metadata deltas follow the message loop below.)
        let mut scopes: Vec<Scope> = Vec::new();
        let mut target_by_scope: HashMap<String, Target> = HashMap::new();
        for channel in self.derived_channels(&account).await {
            let scope = Scope::Channel(channel.clone());
            target_by_scope.insert(scope.as_key(), Target::Channel(channel));
            scopes.push(scope);
        }
        if let Ok(partners) = self.ctx.events.dm_partners(&account).await {
            for partner in partners {
                let scope = Scope::dm(account.clone(), partner.clone());
                target_by_scope.insert(
                    scope.as_key(),
                    Target::User {
                        account: partner,
                        network: None,
                    },
                );
                scopes.push(scope);
            }
        }

        let events = self
            .ctx
            .events
            .events_since(&scopes, since_seq)
            .await
            .unwrap_or_default();

        // Collect the roots touched since the cursor, grouped by scope, then
        // re-materialize each so the client sees current state (upsert).
        let mut touched: HashMap<String, std::collections::HashSet<weft_proto::Ulid>> =
            HashMap::new();
        for ev in &events {
            touched
                .entry(ev.scope.as_key())
                .or_default()
                .insert(ev.root.ulid());
        }

        for (key, roots) in touched {
            let (Some(target), Some(scope)) =
                (target_by_scope.get(&key).cloned(), Scope::from_key(&key))
            else {
                continue;
            };
            let root_ulids: Vec<_> = roots.into_iter().collect();
            let mut root_records = Vec::new();
            for ulid in &root_ulids {
                if let Ok(Some(record)) = self.ctx.events.find_root(*ulid).await {
                    root_records.push(record);
                }
            }
            let children = self
                .ctx
                .events
                .children(&scope, &root_ulids)
                .await
                .unwrap_or_default();
            let items = weft_store::materialize(root_records, children);
            self.emit_materialized(&label, &target, items).await?;
        }

        // Metadata delta: channels whose layout/policy changed since the cursor
        // and are still in the caller's visible set — so an offline
        // `CHANNEL-LAYOUT`/`POLICY` change (new channel, re-category, re-policy,
        // rename) reaches the sidebar on reconnect. (Ns/role/friend/group/pin
        // metadata deltas are further follow-ups.)
        if let Ok(changed) = self
            .ctx
            .channel_store
            .channels_changed_since(since_seq)
            .await
        {
            for (channel, record) in changed {
                if self.view_gated_denied(&channel, &account).await {
                    continue;
                }
                if !self.is_member(&account, &channel).await {
                    continue;
                }
                self.send_event(
                    label.clone(),
                    Event::ChannelLayout {
                        channel: channel.clone(),
                        category: record.category.clone(),
                        position: record.position,
                        kind: record.kind,
                        vanity: record.vanity.clone(),
                        origin: record.origin.as_deref().and_then(|o| o.parse().ok()),
                    },
                )
                .await?;
                if record.kind != ChannelKind::Voice {
                    self.send_event(
                        label.clone(),
                        Event::Policy {
                            channel,
                            policy: record.policy,
                        },
                    )
                    .await?;
                }
            }
        }

        // Namespace metadata delta: NS-META changes (title/icon/categories/
        // visibility/recovery) for namespaces the account belongs to — these are
        // not re-pushed on reconnect otherwise.
        if let Ok(changed) = self
            .ctx
            .namespaces
            .namespaces_changed_since(since_seq)
            .await
        {
            for record in changed {
                if self
                    .ctx
                    .memberships
                    .is_ns_member(account.as_str(), &record.id)
                    .await
                    .unwrap_or(false)
                {
                    self.send_event(label.clone(), self.ns_meta_event(&record))
                        .await?;
                }
            }
        }

        let end = self.ctx.events.sync_cursor().await.unwrap_or_default();
        self.send_event(label, Event::SyncEnd { cursor: end })
            .await?;
        Ok(Flow::Continue)
    }

    /// Emit materialized history items as direct upsert rows (no `BATCH` — SYNC
    /// never wraps, Part 4.1): final `MESSAGE` (+ `edited=`) with per-emoji
    /// `REACTIONS` summaries, or a `DELETED` tombstone.
    async fn emit_materialized(
        &mut self,
        label: &Option<String>,
        target: &Target,
        items: Vec<weft_store::HistoryItem>,
    ) -> io::Result<()> {
        for item in items {
            match item {
                weft_store::HistoryItem::Message {
                    msgid,
                    sender,
                    body,
                    meta,
                    edited,
                    reactions,
                } => {
                    self.send_event(
                        label.clone(),
                        Event::Message(Box::new(weft_proto::MessageEvent {
                            target: target.clone(),
                            sender,
                            msgid: msgid.clone(),
                            body,
                            meta,
                            edited: edited.map(|(count, _)| count),
                            edited_at: edited.map(|(_, at)| at),
                        })),
                    )
                    .await?;
                    for summary in reactions {
                        self.send_event(
                            label.clone(),
                            Event::Reactions {
                                target: target.clone(),
                                msgid: msgid.clone(),
                                emoji: summary.emoji,
                                count: summary.count,
                                by: summary.actors,
                            },
                        )
                        .await?;
                    }
                }
                weft_store::HistoryItem::Tombstone { msgid, by } => {
                    self.send_event(
                        label.clone(),
                        Event::Deleted {
                            target: target.clone(),
                            msgid,
                            by: Some(by),
                        },
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }
}
