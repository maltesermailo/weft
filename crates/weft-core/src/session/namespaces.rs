//! §6.2 / §2.4 namespace handlers: CREATE / META / VISIBILITY / DELETE /
//! DISCOVER / TRANSFER / RECOVERY / NS JOIN / CHANNELS layout.

use super::*;

impl<S: ControlStream> Session<S> {
    /// Resolve a §2.2 namespace ref (the immutable id **or** the vanity name) to
    /// its record: a stored id first, else the vanity via the name column.
    /// `Ok(None)` = no such namespace (the caller maps that to NO-SUCH-TARGET).
    pub(super) async fn resolve_ns_ref(
        &self,
        ns: &weft_proto::NamespaceRef,
    ) -> Result<Option<weft_store::NamespaceRecord>, weft_store::StoreError> {
        if let Some(record) = self.ctx.namespaces.namespace_by_id(ns.as_str()).await? {
            return Ok(Some(record));
        }
        let Ok(name) = ns.as_str().parse::<weft_proto::NamespaceName>() else {
            return Ok(None);
        };
        self.ctx.namespaces.namespace(&name).await
    }

    /// §6.2 `NS JOIN <name>` (v0.12): become a member of the namespace — one
    /// `(account, ns)` row; channel access is derived (Part 1.2). The caller
    /// subscribes to every channel it can see (view-gated ones stay hidden), and
    /// the join is announced once as `NS-MEMBER … join` rather than a per-channel
    /// `MEMBER` fan-out (Q1). No visible channel — nonexistent, private, or fully
    /// gated — answers `NO-SUCH-TARGET` (one code, anti-enumeration).
    pub(super) async fn on_ns_join(
        &mut self,
        label: Option<String>,
        ns: weft_proto::NamespaceRef,
        account: Account,
    ) -> io::Result<Flow> {
        // §2.2: the ref may be the id or the vanity name (unlisted-by-name) —
        // resolve either to the namespace record. Anti-enumeration (invariant 1):
        // an unresolvable ref, and a `private` namespace you don't belong to,
        // both collapse to NO-SUCH-TARGET. An existing, viewable namespace is
        // joinable even with **zero** channels (an empty server is real).
        let record = match self.resolve_ns_ref(&ns).await {
            Ok(Some(record)) => record,
            Ok(None) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        };
        // From here on the immutable id is authoritative (scopes/channels).
        let ns_str = record.id.clone();
        let ns: weft_proto::NamespaceId = match ns_str.parse() {
            Ok(id) => id,
            Err(_) => return self.no_such_target(label).await,
        };
        if record.visibility == "private" {
            let already = self
                .ctx
                .memberships
                .is_ns_member(&account, &ns_str)
                .await
                .unwrap_or(false);
            let scope = TokenScope::Namespace(ns_str.clone());
            let viewer = self
                .ctx
                .account_has_cap(&account, &Capability::View, &scope, unix_now())
                .await
                .unwrap_or(false);
            if !already && !viewer {
                return self.no_such_target(label).await;
            }
        }
        let channels = match self.ctx.channel_store.channels_in_namespace(&ns_str).await {
            Ok(list) => list,
            Err(e) => return self.internal(label, &e).await,
        };
        // The channels the caller can actually see. Voice channels are entered
        // separately (§16) and never text-subscribed here.
        let mut visible = Vec::new();
        for (channel, _record) in channels {
            if self.channel_kind(&channel).await == ChannelKind::Voice {
                continue;
            }
            if self.view_gated_denied(&channel, &account).await {
                continue;
            }
            visible.push(channel);
        }

        // A *new* member (not an auto-rejoin) triggers the welcome message —
        // checked before writing the membership row below.
        let first_join = !self
            .ctx
            .memberships
            .is_ns_member(&account, &ns.to_string())
            .await
            .unwrap_or(false);

        // Write the single ns membership row FIRST, so the per-channel
        // subscriptions below read as auto-rejoin — quiet, no "joined" system
        // spam for an ns-level action (the NS-MEMBER event is the signal).
        if let Err(e) = self
            .ctx
            .memberships
            .set_ns_membership(&account, &ns.to_string(), unix_now() as i64)
            .await
        {
            return self.internal(label, &e).await;
        }
        if first_join {
            self.post_ns_welcome(&ns, &account).await;
        }
        for channel in visible {
            // A channel the caller previously hid stays hidden — NS JOIN never
            // un-hides; that's a per-channel JOIN.
            if self
                .ctx
                .memberships
                .is_hidden(&account, &channel)
                .await
                .unwrap_or(false)
            {
                continue;
            }
            // Unlabeled: a bulk subscription burst; the client folds each
            // MEMBER/POLICY as it arrives.
            self.join_one(&channel, &account, None).await?;
        }

        // Announce the ns-level membership once (client expands to the derived
        // roster), carrying the distinct-account member count after the join.
        let count = self
            .ctx
            .memberships
            .ns_members(&ns.to_string())
            .await
            .map(|m| m.len() as u64)
            .ok();
        let me = UserRef::new(account, self.ctx.info.network.clone());
        self.send_event(
            label,
            Event::NsMember {
                namespace: ns,
                user: me,
                action: MemberAction::Join,
                display: None,
                count,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// §6.2 post the namespace's welcome line to its designated channel, if any.
    /// Called on a *new* namespace membership (first join, any path). No-op when
    /// no welcome channel is configured or it isn't a live registered channel.
    pub(super) async fn post_ns_welcome(
        &mut self,
        ns: &weft_proto::NamespaceId,
        account: &Account,
    ) {
        let Ok(Some(record)) = self.ctx.namespaces.namespace_by_id(&ns.to_string()).await else {
            return;
        };
        let Some(welcome) = record.welcome_channel else {
            return;
        };
        let Ok(channel) = welcome.parse::<ChannelName>() else {
            return;
        };
        let Some(handle) = self.ctx.registry.get(&channel) else {
            return;
        };
        let user = UserRef::new(account.clone(), self.ctx.info.network.clone());
        handle.announce_welcome(user).await;
    }

    /// §6.2 `NS LEAVE <name>` (v0.12): drop namespace membership — the
    /// `(account, ns)` row, every hide override for its channels (both in the
    /// store), and ns-scoped role assignments — then unsubscribe from its
    /// channels and announce `NS-MEMBER … part`. Also reachable as the
    /// `PART ns:<name>` alias. Not a member ⇒ `NO-SUCH-TARGET` (invariant 1).
    pub(super) async fn on_ns_leave(
        &mut self,
        label: Option<String>,
        ns: weft_proto::NamespaceId,
        account: Account,
    ) -> io::Result<Flow> {
        let ns_str = ns.to_string();
        if !self
            .ctx
            .memberships
            .is_ns_member(&account, &ns_str)
            .await
            .unwrap_or(false)
        {
            return self.no_such_target(label).await;
        }

        // The owner can't abandon their own namespace — leaving would orphan it.
        // They must TRANSFER ownership (§2.4 rung 1) or DELETE the namespace.
        if let Ok(Some(rec)) = self.ctx.namespaces.namespace_by_id(&ns_str).await {
            if rec.owner == account {
                self.send_err(
                    label,
                    ErrCode::Policy,
                    None,
                    "the owner can't leave; transfer or delete the namespace",
                )
                .await?;
                return Ok(Flow::Continue);
            }
        }

        // Unsubscribe from every joined channel in this namespace (runtime).
        // Silent part — an ns-level leave doesn't post per-channel "left" lines.
        // Channels embed the ns id as their first segment (v0.13).
        let leaving: Vec<ChannelName> = self
            .joined
            .keys()
            .filter(|c| c.namespace() == Some(ns_str.as_str()))
            .cloned()
            .collect();
        for channel in leaving {
            if let Some(joined) = self.joined.remove(&channel) {
                joined.forwarder.abort();
                joined.handle.part(self.id, false).await;
            }
        }

        // Drop the membership row + all hide overrides for the namespace.
        if let Err(e) = self
            .ctx
            .memberships
            .clear_ns_membership(&account, &ns_str)
            .await
        {
            return self.internal(label, &e).await;
        }
        // Clear ns-scoped role assignments (the store leaves these to us).
        let scope = format!("ns:{ns}");
        let subject = account.to_string();
        if let Ok(roles) = self.ctx.roles.roles_of(&scope, &subject).await {
            for role in roles {
                if let Err(e) = self.ctx.roles.unassign_role(&scope, &role, &subject).await {
                    error!("ns role unassign failed: {e}");
                }
            }
        }

        // Drop read markers for this namespace's channels — otherwise the login
        // snapshot (§6.3) would keep emitting UNREAD-COUNTS for channels the
        // account can no longer see. Non-fatal: membership is already gone.
        if let Err(e) = self
            .ctx
            .accounts
            .clear_marks_in_namespace(&account, &ns_str)
            .await
        {
            error!("mark prune on ns-leave failed: {e}");
        }

        let me = UserRef::new(account, self.ctx.info.network.clone());
        self.send_event(
            label,
            Event::NsMember {
                namespace: ns,
                user: me,
                action: MemberAction::Part,
                display: None,
                count: None,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// §6.2 `NS INFO MEMBERS <name>` — the moderator roster: every ns member
    /// with their join time and assigned ns-scoped roles, as a `BATCH` of
    /// `NS-MEMBER-INFO`. Cap-gated — the caller must hold a moderation
    /// capability at `ns:<name>` (`ns-admin`, which the owner holds implicitly,
    /// or `ban` / `kick` / `mute` / `reports`). Unknown namespace ⇒
    /// `NO-SUCH-TARGET` (invariant 1: no hidden-vs-absent branch before it).
    pub(super) async fn on_ns_info_members(
        &mut self,
        label: Option<String>,
        ns: weft_proto::NamespaceId,
        account: Account,
    ) -> io::Result<Flow> {
        if !matches!(
            self.ctx.namespaces.namespace_by_id(&ns.to_string()).await,
            Ok(Some(_))
        ) {
            return self.no_such_target(label).await;
        }

        // Any moderation cap at ns:<id> unlocks the roster. Owner passes via the
        // implicit ns-admin it holds at its own scope.
        let scope = TokenScope::Namespace(ns.to_string());
        let now = unix_now();
        let mut authorized = false;
        for cap in [
            Capability::NsAdmin,
            Capability::Ban,
            Capability::Kick,
            Capability::Mute,
            Capability::Reports,
        ] {
            match self.ctx.account_has_cap(&account, &cap, &scope, now).await {
                Ok(true) => {
                    authorized = true;
                    break;
                }
                Ok(false) => {}
                Err(e) => return self.internal(label, &e).await,
            }
        }
        if !authorized {
            return self.cap_required(label, "ns-admin").await;
        }

        let members = match self
            .ctx
            .memberships
            .ns_members_joined(&ns.to_string())
            .await
        {
            Ok(m) => m,
            Err(e) => return self.internal(label, &e).await,
        };
        let scope_str = format!("ns:{ns}");

        self.batches += 1;
        let id = format!("ni{}", self.batches);
        self.send_event(label.clone(), Event::BatchStart { id: id.clone() })
            .await?;
        for (member, joined_ms) in members {
            // v0.13: the roster carries role **ids** so the client addresses them
            // unambiguously (names aren't unique).
            let roles = self.assigned_role_ids(&scope_str, member.as_str()).await;
            let user = UserRef::new(member, self.ctx.info.network.clone());
            self.send_event(
                None,
                Event::NsMemberInfo {
                    namespace: ns,
                    user,
                    joined_ms: joined_ms.max(0) as u64,
                    roles,
                },
            )
            .await?;
        }
        self.send_event(
            label,
            Event::BatchEnd {
                id,
                truncated: false,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// Does a namespace with this **id** exist? (v0.13 — scopes are id-keyed, so
    /// callers pass the ns id extracted from an `ns:<id>` / `#<ns-id>/…` scope.)
    pub(super) async fn namespace_exists(&self, id: &str) -> bool {
        matches!(self.ctx.namespaces.namespace_by_id(id).await, Ok(Some(_)))
    }

    /// Build the NS-META reply for a namespace record, including the §2.4
    /// recovery announcement fields.
    pub(super) fn ns_meta_event(record: &weft_store::NamespaceRecord) -> Event {
        Event::NsMeta {
            // Records always carry a valid id post-migration (0047 backfills legacy
            // rows; new namespaces mint at create).
            id: record
                .id
                .parse()
                .expect("namespace record carries a valid ULID id"),
            vanity: record
                .name
                .as_str()
                .parse()
                .expect("namespace name is a valid vanity label"),
            visibility: record.visibility.parse().unwrap_or(Visibility::Unlisted),
            owner: Some(record.owner.to_string()),
            title: record.title.clone(),
            description: record.description.clone(),
            icon: record.icon.clone(),
            recovery_set: record.recovery_set.is_some(),
            recovery_pending: record.pending_recovery.as_ref().map(|p| (p.eta_ms, p.rung)),
            categories: record.categories.clone(),
            federation: record.federation,
            welcome: record.welcome_channel.clone(),
            // §7a.2: a provider-managed replica advertises its foreign origin so
            // clients badge it. Stored as a URI string; a malformed value (never
            // produced by our own writers) simply drops the badge.
            origin: record.origin.as_deref().and_then(|o| o.parse().ok()),
        }
    }

    pub(super) async fn on_ns_create(
        &mut self,
        label: Option<String>,
        vanity: weft_proto::VanityName,
        visibility: Visibility,
        root_key: String,
        account: Account,
    ) -> io::Result<Flow> {
        // The vanity is the mutable display label; identity is a freshly minted
        // ULID that scopes/channels/federation pin (§2.3, v0.13).
        let name: weft_proto::NamespaceName = vanity
            .as_str()
            .parse()
            .expect("a valid vanity is a valid namespace name");
        // §2.3 keep the id-space and vanity-space provably disjoint: a vanity that
        // is itself a well-formed ULID could never be *reached* by name (NS JOIN
        // resolves ids first), and would let a namespace masquerade as another's
        // id in links/UI. Reject it — real vanities are human labels, never ULIDs.
        if vanity.as_str().parse::<weft_proto::NamespaceId>().is_ok() {
            self.send_err(
                label,
                ErrCode::Policy,
                None,
                "vanity name can't be a ULID (reserved for ids)",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        // §6.7 banned-word filter: refuse a namespace vanity with a barred word.
        if self.ctx.name_is_banned(vanity.as_str()) {
            self.send_err(label, ErrCode::Policy, None, "that name isn't allowed")
                .await?;
            return Ok(Flow::Continue);
        }
        let ns_id = weft_proto::Ulid::new().to_string().to_ascii_lowercase();
        // The submitted root key must be a real Ed25519 pubkey (§2.1).
        if weft_crypto::PublicKey::from_b64(&root_key).is_err() {
            self.send_err(
                label,
                ErrCode::Malformed,
                None,
                "root must be a b64 ed25519 pubkey",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        // §2.3 an admin-reserved vanity can't be (re-)registered until an
        // operator lifts the lock in the web admin panel — uniform Conflict.
        match self.ctx.namespaces.vanity_locked(&name).await {
            Ok(true) => {
                self.send_err(
                    label,
                    ErrCode::Conflict,
                    None,
                    "that name is reserved by the operator",
                )
                .await?;
                return Ok(Flow::Continue);
            }
            Ok(false) => {}
            Err(e) => return self.internal(label, &e).await,
        }
        // §2.2 creation policy: gated needs `ns-create`; open enforces a
        // per-account quota.
        if self.ctx.ns_creation_open {
            let owned = match self.ctx.namespaces.namespaces_owned(account.as_str()).await {
                Ok(n) => n,
                Err(e) => return self.internal(label, &e).await,
            };
            if owned >= self.ctx.ns_quota {
                let mut err = ErrEvent::new(ErrCode::Quota, "namespace quota reached");
                err.max = Some(self.ctx.ns_quota);
                self.send_event(label, Event::Err(err)).await?;
                return Ok(Flow::Continue);
            }
        } else {
            let scope = TokenScope::Wildcard;
            match self
                .ctx
                .account_has_cap(&account, &Capability::NsCreate, &scope, unix_now())
                .await
            {
                Ok(true) => {}
                Ok(false) => return self.cap_required(label, "ns-create").await,
                Err(e) => return self.internal(label, &e).await,
            }
        }
        let record = weft_store::NamespaceRecord {
            id: ns_id.clone(),
            name: name.clone(),
            owner: account.clone(),
            root_key,
            visibility: visibility.to_string(),
            title: None,
            description: None,
            icon: None,
            recovery_set: None,
            pending_recovery: None,
            categories: Vec::new(),
            federation: false,     // §11.10 closed until the owner opts in
            frozen: false,         // WC7 full freeze — an operator action, never default
            welcome_channel: None, // §6.2 set later via NS META welcome
            origin: None,          // a native namespace (foreign replicas set the URI)
        };
        match self.ctx.namespaces.create_namespace(record.clone()).await {
            Ok(true) => {
                debug!(%ns_id, %account, "namespace created");
                // §6.5 baseline: seed the implicit @everyone role with the caps
                // every member is expected to hold out of the box — post
                // messages + mint invites. Editable afterward like any role.
                let ns_scope = format!("ns:{ns_id}");
                if let Err(e) = self
                    .ctx
                    .roles
                    .set_role(
                        &ns_scope,
                        crate::context::EVERYONE_ROLE,
                        "#99aab5",
                        &["send".to_string(), "invite".to_string()],
                        false,
                        false,
                        0,
                    )
                    .await
                {
                    error!("seed @everyone role failed: {e}");
                }
                // §6.2 seed a default `#<ns-id>/general` channel so a brand-new
                // namespace is immediately usable — the server owns this, not the
                // client (every namespace has somewhere to talk out of the box).
                let chan_id = weft_proto::Ulid::new().to_string().to_ascii_lowercase();
                if let Ok(canonical) =
                    format!("#{ns_id}/{chan_id}").parse::<weft_proto::ChannelName>()
                {
                    let policy = "retained:90d".parse().expect("valid default policy");
                    self.ctx.registry.create(canonical.clone(), policy);
                    if let Err(e) = self
                        .ctx
                        .channel_store
                        .upsert_channel(&canonical, "general", policy, ChannelKind::Text)
                        .await
                    {
                        error!("seed default channel failed: {e}");
                    }
                }

                // The creator is the namespace's first member. Persist that
                // membership row so the server shows in their rail (the client
                // keys it off ns-membership + ownership) AND survives a restart —
                // on reconnect SYNC re-emits the membership + the ns's visible
                // channels (incl. the seeded `#general`), no Discover round-trip.
                if let Err(e) = self
                    .ctx
                    .memberships
                    .set_ns_membership(&account, &ns_id, unix_now() as i64)
                    .await
                {
                    error!("seed creator ns membership failed: {e}");
                }

                self.send_event(label, Self::ns_meta_event(&record)).await?;
                Ok(Flow::Continue)
            }
            Ok(false) => {
                self.send_err(label, ErrCode::Conflict, None, "namespace name is taken")
                    .await?;
                Ok(Flow::Continue)
            }
            Err(e) => self.internal(label, &e).await,
        }
    }

    /// Shared owner/ns-admin gate for NS META/VISIBILITY/DELETE.
    /// `Ok(Some(record))` = authorized; `Ok(None)` = refused/answered.
    pub(super) async fn ns_admin_gate(
        &mut self,
        label: Option<String>,
        ns: &weft_proto::NamespaceId,
        actor: &Actor,
    ) -> io::Result<Option<weft_store::NamespaceRecord>> {
        let record = match self.ctx.namespaces.namespace_by_id(&ns.to_string()).await {
            Ok(Some(record)) => record,
            Ok(None) => {
                self.no_such_target(label).await?;
                return Ok(None);
            }
            Err(e) => {
                self.internal(label, &e).await?;
                return Ok(None);
            }
        };
        let scope = TokenScope::Namespace(ns.to_string());
        match self
            .ctx
            .actor_has_cap(actor, &Capability::NsAdmin, &scope, unix_now())
            .await
        {
            Ok(true) => Ok(Some(record)),
            Ok(false) => {
                self.cap_required(label, "ns-admin").await?;
                Ok(None)
            }
            Err(e) => {
                self.internal(label, &e).await?;
                Ok(None)
            }
        }
    }

    /// `EMOJI ADD <ns> <name> <media>` (§9.4): add/replace a namespace emoji.
    /// Cap-gated (`ns-admin`). Echoes the `EMOJI` event to the caller.
    pub(super) async fn on_emoji_add(
        &mut self,
        label: Option<String>,
        namespace: weft_proto::NamespaceId,
        name: String,
        media: String,
        actor: Actor,
    ) -> io::Result<Flow> {
        if !valid_emoji_name(&name) {
            self.send_err(
                label,
                ErrCode::Policy,
                None,
                "emoji name must be 1–32 chars of a-z A-Z 0-9 _",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        if self
            .ns_admin_gate(label.clone(), &namespace, &actor)
            .await?
            .is_none()
        {
            return Ok(Flow::Continue);
        }
        if let Err(e) = self
            .ctx
            .emoji
            .set_emoji(&namespace.to_string(), &name, &media)
            .await
        {
            return self.internal(label, &e).await;
        }
        self.send_event(
            label,
            Event::Emoji {
                namespace,
                name,
                media,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// `EMOJI REMOVE <ns> <name>` (§9.4). Cap-gated (`ns-admin`).
    pub(super) async fn on_emoji_remove(
        &mut self,
        label: Option<String>,
        namespace: weft_proto::NamespaceId,
        name: String,
        actor: Actor,
    ) -> io::Result<Flow> {
        if self
            .ns_admin_gate(label.clone(), &namespace, &actor)
            .await?
            .is_none()
        {
            return Ok(Flow::Continue);
        }
        if let Err(e) = self
            .ctx
            .emoji
            .remove_emoji(&namespace.to_string(), &name)
            .await
        {
            return self.internal(label, &e).await;
        }
        self.send_event(label, Event::EmojiRemoved { namespace, name })
            .await?;
        Ok(Flow::Continue)
    }

    /// `EMOJI LIST <ns>` (§9.4): a `BATCH` of `EMOJI` events. Any authed session
    /// may list — emoji aren't secret and clients need them to render.
    pub(super) async fn on_emoji_list(
        &mut self,
        label: Option<String>,
        namespace: weft_proto::NamespaceId,
    ) -> io::Result<Flow> {
        let emoji = match self.ctx.emoji.list_emoji(&namespace.to_string()).await {
            Ok(emoji) => emoji,
            Err(e) => return self.internal(label, &e).await,
        };
        self.batches += 1;
        let id = format!("e{}", self.batches);
        self.send_event(label.clone(), Event::BatchStart { id: id.clone() })
            .await?;
        for (name, media) in emoji {
            self.send_event(
                None,
                Event::Emoji {
                    namespace,
                    name,
                    media,
                },
            )
            .await?;
        }
        self.send_event(
            label,
            Event::BatchEnd {
                id,
                truncated: false,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    pub(super) async fn on_ns_meta(
        &mut self,
        label: Option<String>,
        ns: weft_proto::NamespaceId,
        key: String,
        value: String,
        actor: Actor,
    ) -> io::Result<Flow> {
        if !matches!(
            key.as_str(),
            "title" | "description" | "icon" | "categories" | "federation" | "welcome"
        ) {
            self.send_err(
                label,
                ErrCode::Policy,
                None,
                "meta key must be title|description|icon|categories|federation|welcome",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        let Some(mut record) = self.ns_admin_gate(label.clone(), &ns, &actor).await? else {
            return Ok(Flow::Continue);
        };
        // The namespace table is keyed by the (mutable) vanity name; the record
        // found by id carries the current one for these name-keyed mutations.
        let name = record.name.clone();
        // §6.2 welcome channel lives on its own column. Empty clears it; a value
        // must be a real channel *in this namespace* (else the message would go
        // nowhere) — validated leniently: we store what's given, and the join
        // path no-ops if the channel isn't registered.
        if key == "welcome" {
            let channel = (!value.is_empty()).then_some(value.as_str());
            if let Err(e) = self
                .ctx
                .namespaces
                .set_namespace_welcome(&name, channel)
                .await
            {
                return self.internal(label, &e).await;
            }
            record.welcome_channel = channel.map(str::to_string);
            self.send_event(label, Self::ns_meta_event(&record)).await?;
            return Ok(Flow::Continue);
        }
        // §11.10 auto-federation reachability lives on its own column. It is off
        // by default and is an explicit opt-in for *any* visibility: a `public`
        // namespace is then reachable to anyone, while an `unlisted`/`private`
        // one is reachable only to a peer presenting a valid invite (the invite,
        // not the visibility, is the access control — see `on_bridge_request_in`).
        if key == "federation" {
            let open = value == "open";
            if let Err(e) = self
                .ctx
                .namespaces
                .set_namespace_federation(&name, open)
                .await
            {
                return self.internal(label, &e).await;
            }
            record.federation = open;
            self.send_event(label, Self::ns_meta_event(&record)).await?;
            return Ok(Flow::Continue);
        }
        if let Err(e) = self
            .ctx
            .namespaces
            .set_namespace_meta(&name, &key, &value)
            .await
        {
            return self.internal(label, &e).await;
        }
        match key.as_str() {
            "title" => record.title = Some(value),
            "description" => record.description = Some(value),
            "icon" => record.icon = Some(value),
            "categories" => {
                record.categories = value
                    .split(',')
                    .filter(|c| !c.is_empty())
                    .map(str::to_string)
                    .collect()
            }
            _ => {}
        }
        self.send_event(label, Self::ns_meta_event(&record)).await?;
        Ok(Flow::Continue)
    }

    pub(super) async fn on_ns_visibility(
        &mut self,
        label: Option<String>,
        ns: weft_proto::NamespaceId,
        visibility: Visibility,
        actor: Actor,
    ) -> io::Result<Flow> {
        let Some(mut record) = self.ns_admin_gate(label.clone(), &ns, &actor).await? else {
            return Ok(Flow::Continue);
        };
        if let Err(e) = self
            .ctx
            .namespaces
            .set_namespace_visibility(&record.name, &visibility.to_string())
            .await
        {
            return self.internal(label, &e).await;
        }
        record.visibility = visibility.to_string();
        self.send_event(label, Self::ns_meta_event(&record)).await?;
        Ok(Flow::Continue)
    }

    pub(super) async fn on_ns_delete(
        &mut self,
        label: Option<String>,
        ns: weft_proto::NamespaceId,
        confirm: weft_proto::NamespaceId,
        actor: Actor,
    ) -> io::Result<Flow> {
        if ns != confirm {
            self.send_err(
                label,
                ErrCode::Policy,
                None,
                "DELETE must repeat the namespace id",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        // Owner or operator (§6.2). ns_admin_gate covers both (owner holds
        // ns-admin, operators hold everything). The record carries the current
        // vanity for the name-keyed cascade ops below.
        let Some(record) = self.ns_admin_gate(label.clone(), &ns, &actor).await? else {
            return Ok(Flow::Continue);
        };
        let name = record.name.clone();
        // Channels + ns-membership are keyed by the immutable id (v0.13); the
        // record carries the current vanity only for name-keyed table ops.
        let ns_str = ns.to_string();
        // Cascade: a namespace owns its channels, memberships, roles and pending
        // invites. Deleting only the record orphans them — the channels stay live
        // and in the store, so clients auto-rejoin and keep posting, DISCOVER and
        // the admin panel still surface them, and a namespace later recreated
        // under the same name would inherit ghost members/roles. Tear the whole
        // subtree down first, then drop the record.
        let channels = match self.ctx.channel_store.channels_in_namespace(&ns_str).await {
            Ok(channels) => channels,
            Err(e) => return self.internal(label, &e).await,
        };
        for (channel, _) in &channels {
            self.ctx.registry.remove(channel); // stop the live actor
            if let Err(e) = self.ctx.channel_store.delete_channel(channel).await {
                return self.internal(label, &e).await;
            }
            // A channel's own roles live at its channel scope — drop them too.
            let chan_scope = channel.to_string();
            if let Ok(roles) = self.ctx.roles.roles(&chan_scope).await {
                for role in roles {
                    let _ = self.ctx.roles.delete_role(&chan_scope, &role.name).await;
                }
            }
        }

        // Clear namespace memberships (also drops hide overrides) so a same-name
        // namespace can't auto-rejoin ghost members, then the ns-scope roles and
        // any pending invites.
        if let Ok(members) = self.ctx.memberships.ns_members(&ns_str).await {
            for member in members {
                let _ = self
                    .ctx
                    .memberships
                    .clear_ns_membership(&member, &ns_str)
                    .await;
            }
        }
        // ns-scope roles are keyed by the id (matches on_ns_create's id-scope).
        let ns_scope = format!("ns:{ns}");
        if let Ok(roles) = self.ctx.roles.roles(&ns_scope).await {
            for role in roles {
                let _ = self.ctx.roles.delete_role(&ns_scope, &role.name).await;
            }
        }
        // v0.13: invites + grants are scoped by the **id** (`ns:<id>` /
        // `#<id>/…`), so the cascade must pass the id, not the vanity.
        let _ = self.ctx.invites.revoke_invites_for_namespace(&ns_str).await;
        // Grant records are the enforcement fast path (§10.4) and key by subject,
        // not scope — so a role/direct grant at `ns:<id>` or `#<id>/<chan>`
        // survives the role-definition delete above. Purge them by namespace so a
        // recreated same-name namespace can't resurrect a former admin's caps.
        let _ = self.ctx.caps.revoke_grants_for_namespace(&ns_str).await;

        if let Err(e) = self.ctx.namespaces.delete_namespace(&name).await {
            return self.internal(label, &e).await;
        }
        debug!(%name, channels = channels.len(), "namespace deleted (cascade)");
        // Reflect deletion as an NS-META marker (private + no owner).
        self.send_event(
            label,
            Event::NsMeta {
                id: ns,
                vanity: name
                    .as_str()
                    .parse()
                    .expect("namespace name is a valid vanity label"),
                visibility: Visibility::Private,
                owner: None,
                title: None,
                description: Some("deleted".to_string()),
                icon: None,
                recovery_set: false,
                recovery_pending: None,
                categories: Vec::new(),
                federation: false,
                welcome: None,
                origin: None,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    pub(super) async fn on_discover(
        &mut self,
        label: Option<String>,
        cursor: Option<String>,
    ) -> io::Result<Flow> {
        const PAGE: usize = 50;
        let public = match self
            .ctx
            .namespaces
            .list_public(cursor.as_deref(), PAGE)
            .await
        {
            Ok(public) => public,
            Err(e) => return self.internal(label, &e).await,
        };
        let next_cursor = (public.len() == PAGE)
            .then(|| public.last().map(|ns| ns.name.to_string()))
            .flatten();
        for record in &public {
            self.send_event(label.clone(), Self::ns_meta_event(record))
                .await?;
        }
        if let Some(cursor) = next_cursor {
            self.send_event(label, Event::More { cursor }).await?;
        }
        Ok(Flow::Continue)
    }

    // ---- namespace recovery ladder (§2.4, invariant 9) ----

    /// Load a namespace or answer NO-SUCH-TARGET.
    pub(super) async fn ns_or_absent(
        &mut self,
        label: Option<String>,
        ns: &weft_proto::NamespaceId,
    ) -> io::Result<Option<weft_store::NamespaceRecord>> {
        match self.ctx.namespaces.namespace_by_id(&ns.to_string()).await {
            Ok(Some(record)) => Ok(Some(record)),
            Ok(None) => {
                self.no_such_target(label).await?;
                Ok(None)
            }
            Err(e) => {
                self.internal(label, &e).await?;
                Ok(None)
            }
        }
    }

    /// NS TRANSFER (rung 1): hand ownership to `new_owner`, proven by a
    /// signature from the current root key. No delay (§2.4).
    pub(super) async fn on_ns_transfer(
        &mut self,
        label: Option<String>,
        ns: weft_proto::NamespaceId,
        new_owner: Account,
        signature: String,
        _account: Account,
    ) -> io::Result<Flow> {
        let Some(record) = self.ns_or_absent(label.clone(), &ns).await? else {
            return Ok(Flow::Continue);
        };
        let (Ok(root_key), Ok(sig)) = (
            weft_crypto::PublicKey::from_b64(&record.root_key),
            weft_crypto::signature_from_b64(&signature),
        ) else {
            return self.forbidden_sig(label).await;
        };
        // Authority is the root *key*, not the account — this is the one place
        // same-network namespaces are cryptographically enforced. The signature
        // still covers the current vanity name (transfer is a live-root op).
        if !weft_crypto::verify_transfer(&root_key, record.name.as_str(), new_owner.as_str(), &sig)
        {
            return self.forbidden_sig(label).await;
        }
        // Succession keeps the root key, changes the owner.
        if let Err(e) = self
            .ctx
            .namespaces
            .rotate_root(
                &record.name,
                new_owner.as_str(),
                &record.root_key,
                false,
                unix_now() * 1000,
            )
            .await
        {
            return self.internal(label, &e).await;
        }
        debug!(%ns, %new_owner, "namespace transferred (rung 1)");
        let updated = self
            .ctx
            .namespaces
            .namespace_by_id(&ns.to_string())
            .await
            .ok()
            .flatten();
        let event = updated
            .as_ref()
            .map(Self::ns_meta_event)
            .unwrap_or_else(|| Self::ns_meta_event(&record));
        self.send_event(label, event).await?;
        Ok(Flow::Continue)
    }

    /// NS RECOVERY SET: designate the M-of-N quorum. Owner (root) only.
    pub(super) async fn on_ns_recovery_set(
        &mut self,
        label: Option<String>,
        ns: weft_proto::NamespaceId,
        m: u32,
        keys: String,
        account: Account,
    ) -> io::Result<Flow> {
        let Some(record) = self.ns_or_absent(label.clone(), &ns).await? else {
            return Ok(Flow::Continue);
        };
        if record.owner != account {
            return self.cap_required(label, "ns-admin").await;
        }
        let key_list: Vec<String> = keys
            .split(',')
            .filter(|k| !k.is_empty())
            .map(str::to_string)
            .collect();
        // Every quorum key must be a real pubkey, and m sane.
        if m == 0
            || m as usize > key_list.len()
            || key_list
                .iter()
                .any(|k| weft_crypto::PublicKey::from_b64(k).is_err())
        {
            self.send_err(
                label,
                ErrCode::Malformed,
                None,
                "bad quorum: m of valid keys required",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        if let Err(e) = self
            .ctx
            .namespaces
            .set_recovery_set(&record.name, m, &key_list)
            .await
        {
            return self.internal(label, &e).await;
        }
        let updated = self
            .ctx
            .namespaces
            .namespace_by_id(&ns.to_string())
            .await
            .ok()
            .flatten();
        let event = updated
            .as_ref()
            .map(Self::ns_meta_event)
            .unwrap_or_else(|| {
                let mut r = record.clone();
                r.recovery_set = Some((m, key_list));
                Self::ns_meta_event(&r)
            });
        self.send_event(label, event).await?;
        Ok(Flow::Continue)
    }

    /// NS RECOVER: submit a signed rotation. The rung follows from *whose*
    /// signatures verify:
    ///
    /// - **Rung 2** — quorum-signed (§2.4 social recovery): starts a 7-day
    ///   delay window, announced, and cancellable by a live root.
    /// - **Rung 3** — signed by the **network key** (operator takeover): applies
    ///   **immediately**, no window and nothing to cancel. This is the
    ///   moderation seizure path, so waiting out a delay the abusing owner could
    ///   veto would defeat it (Appendix A amends the spec's original 30 days).
    ///
    /// Still no *silent* path (invariant 9): every rotation is either announced
    /// and left pending here, applied by the scheduler, vetoed — or, for rung 3,
    /// applied and announced at once, marked operator-initiated forever.
    pub(super) async fn on_ns_recover(
        &mut self,
        label: Option<String>,
        ns: weft_proto::NamespaceId,
        rotation: String,
    ) -> io::Result<Flow> {
        let Some(record) = self.ns_or_absent(label.clone(), &ns).await? else {
            return Ok(Flow::Continue);
        };
        // The current vanity name for the name-keyed record ops + the rotation
        // record's namespace check (transitional — the record still names the
        // vanity; id-signing is a later crypto refinement).
        let name = record.name.clone();
        if record.pending_recovery.is_some() {
            self.send_err(
                label,
                ErrCode::Conflict,
                None,
                "a recovery is already pending",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        let Ok(signed) = weft_crypto::SignedRotation::from_b64(&rotation) else {
            return self.forbidden_sig(label).await;
        };
        // The record must actually be for this namespace.
        if signed.record.namespace != name.as_str() {
            return self.forbidden_sig(label).await;
        }
        // Decide the rung by whose signatures verify.
        let quorum: Vec<weft_crypto::PublicKey> = record
            .recovery_set
            .as_ref()
            .map(|(_, keys)| {
                keys.iter()
                    .filter_map(|k| weft_crypto::PublicKey::from_b64(k).ok())
                    .collect()
            })
            .unwrap_or_default();
        let m = record
            .recovery_set
            .as_ref()
            .map(|(m, _)| *m as usize)
            .unwrap_or(0);
        let rung = if m > 0 && signed.quorum_signers(&quorum) >= m {
            2u8
        } else if signed.signed_by(&self.ctx.identity_public()) {
            3u8
        } else {
            return self.forbidden_sig(label).await;
        };
        let delay_secs = if rung == 2 {
            RECOVERY_DELAY_RUNG2_SECS
        } else {
            RECOVERY_DELAY_RUNG3_SECS
        };
        // A zero-delay rung (§2.4 rung 3, operator takeover) applies *now*.
        // Parking it as "pending" with an elapsed ETA would leave the namespace
        // in the abuser's hands until the next maintenance tick — the opposite
        // of what a moderation seizure is for. There is no window, so there is
        // no pending state and nothing to cancel; the accountability that
        // survives is the announcement + the permanent `root-history` mark.
        if delay_secs == 0 {
            let now_ms = unix_now() * 1000;
            if let Err(e) = self
                .ctx
                .namespaces
                .rotate_root(
                    &name,
                    &signed.record.new_owner,
                    &signed.record.new_root_key.to_b64(),
                    rung == 3, // operator_initiated — marked forever
                    now_ms,
                )
                .await
            {
                return self.internal(label, &e).await;
            }
            info!(%name, rung, new_owner = %signed.record.new_owner, "namespace seized (§2.4 rung 3, immediate)");
            let updated = self.ctx.namespaces.namespace(&name).await.ok().flatten();
            if let Some(record) = updated {
                self.send_event(label, Self::ns_meta_event(&record)).await?;
            }
            return Ok(Flow::Continue);
        }
        let eta_ms = unix_now() * 1000 + delay_secs * 1000;
        let pending = weft_store::PendingRecovery {
            new_root_key: signed.record.new_root_key.to_b64(),
            new_owner: signed.record.new_owner.clone(),
            eta_ms,
            rung,
        };
        if let Err(e) = self
            .ctx
            .namespaces
            .set_pending_recovery(&name, pending)
            .await
        {
            return self.internal(label, &e).await;
        }
        debug!(%name, rung, "recovery pending (§2.4)");
        // §2.4 announcement: NS-META with recovery=pending. (Same-network,
        // it's reflected on any NS query; a push to all members needs an
        // ns-member broadcast, a follow-up.)
        let updated = self.ctx.namespaces.namespace(&name).await.ok().flatten();
        if let Some(record) = updated {
            self.send_event(label, Self::ns_meta_event(&record)).await?;
        }
        Ok(Flow::Continue)
    }

    /// NS RECOVERY CANCEL: the current root vetoes a pending recovery — a
    /// live root always wins (§2.4). Root signature only.
    pub(super) async fn on_ns_recovery_cancel(
        &mut self,
        label: Option<String>,
        ns: weft_proto::NamespaceId,
        signature: String,
    ) -> io::Result<Flow> {
        let Some(record) = self.ns_or_absent(label.clone(), &ns).await? else {
            return Ok(Flow::Continue);
        };
        let name = record.name.clone();
        let (Ok(root_key), Ok(sig)) = (
            weft_crypto::PublicKey::from_b64(&record.root_key),
            weft_crypto::signature_from_b64(&signature),
        ) else {
            return self.forbidden_sig(label).await;
        };
        if !weft_crypto::verify_cancel(&root_key, name.as_str(), &sig) {
            return self.forbidden_sig(label).await;
        }
        if let Err(e) = self.ctx.namespaces.clear_pending_recovery(&name).await {
            return self.internal(label, &e).await;
        }
        debug!(%name, "recovery cancelled by root veto");
        let updated = self.ctx.namespaces.namespace(&name).await.ok().flatten();
        if let Some(record) = updated {
            self.send_event(label, Self::ns_meta_event(&record)).await?;
        }
        Ok(Flow::Continue)
    }

    /// §2.4 / §11.4: bad signatures on a recovery/transfer are FORBIDDEN.
    pub(super) async fn forbidden_sig(&mut self, label: Option<String>) -> io::Result<Flow> {
        self.send_err(
            label,
            ErrCode::Forbidden,
            Some("signature"),
            "invalid signature",
        )
        .await?;
        Ok(Flow::Continue)
    }

    // ---- §6.7 moderation & reporting ----

    /// The honest content state of a reported message (§6.7). Reaching this
    /// with a stored root means the content exists: `Verified` (a hold is
    /// placed) unless the channel is `e2ee`, where the server holds only
    /// ciphertext → `reporter-attested`. `unverified` is unreachable on the
    /// same-network path — anything the server can't find is
    /// indistinguishable from nonexistent (invariant 1) and already answered
    /// NO-SUCH-TARGET; the state exists for bridged replicas (M5).
    pub(super) async fn content_state(&self, scope: &Scope) -> ContentState {
        if let Scope::Channel(channel) = scope {
            if let Ok(Some(record)) = self.ctx.channel_store.channel(channel).await {
                if record.policy == RetentionPolicy::E2ee {
                    return ContentState::ReporterAttested;
                }
            }
        }
        ContentState::Verified
    }

    /// Deliver a filed/resolved report event to a queue's live default
    /// handlers: the namespace owner for `ns:<name>`, every operator for `*`
    /// (§6.7). Delegated `reports` holders fetch via REPORTS LIST — there is
    /// no reverse index from cap to account for a live fan-out (same
    /// pull-not-push limit as the §2.4 recovery announcement).
    pub(super) async fn notify_queue_handlers(&self, queue: &str, event: Event) {
        if queue == "*" {
            for op in self.ctx.operator_accounts().await {
                self.ctx.directory.notify(op, event.clone()).await;
            }
        } else if let Some(id) = queue.strip_prefix("ns:") {
            // ns-scope queues are keyed by the immutable id (v0.13).
            if let Ok(Some(ns)) = self.ctx.namespaces.namespace_by_id(id).await {
                self.ctx.directory.notify(ns.owner, event).await;
            }
        }
    }

    /// The ordered channel layout of a namespace (spec extension). A
    /// non-member of a `private` namespace can't observe it (invariant 1).
    pub(super) async fn on_channels(
        &mut self,
        label: Option<String>,
        namespace: weft_proto::NamespaceId,
    ) -> io::Result<Flow> {
        let ns_str = namespace.to_string();
        let record = match self.ctx.namespaces.namespace_by_id(&ns_str).await {
            Ok(Some(record)) => record,
            Ok(None) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        };
        let State::Ready { account } = self.state.clone() else {
            unreachable!("on_channels only dispatched in READY");
        };
        // Private namespaces are invisible unless you belong (view cap).
        if record.visibility == "private" {
            let scope = TokenScope::Namespace(ns_str.clone());
            let member = self
                .ctx
                .account_has_cap(&account, &Capability::View, &scope, unix_now())
                .await
                .unwrap_or(false);
            if !member {
                return self.no_such_target(label).await;
            }
        }
        // The layout fetch also carries the namespace meta (categories, title,
        // …) so the client renders category groups purely from server state.
        self.send_event(label.clone(), Self::ns_meta_event(&record))
            .await?;
        let channels = match self.ctx.channel_store.channels_in_namespace(&ns_str).await {
            Ok(channels) => channels,
            Err(e) => return self.internal(label, &e).await,
        };
        for (name, record) in channels {
            let kind = record.kind;
            self.send_event(
                label.clone(),
                Event::ChannelLayout {
                    channel: name.clone(),
                    category: record.category,
                    position: record.position,
                    kind,
                    vanity: record.vanity,
                    origin: None, // channel-level origin lands with materialization
                },
            )
            .await?;
            // §16 voice channels: auto-subscribe this session to their live
            // presence so the roster appears + updates without a request.
            if kind == ChannelKind::Voice {
                self.auto_watch_voice(&name, &account).await?;
            }
        }
        Ok(Flow::Continue)
    }
}

/// A valid `:shortcode:` emoji name: 1–32 chars of `[A-Za-z0-9_]`.
fn valid_emoji_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}
