//! §6.2 / §2.4 namespace handlers: CREATE / META / VISIBILITY / DELETE /
//! DISCOVER / TRANSFER / RECOVERY / NS JOIN / CHANNELS layout.

use super::*;

impl<S: ControlStream> Session<S> {

    /// §6.2 `NS JOIN <name>`: join every channel in the namespace the caller
    /// can see, skipping view-gated and banned ones ("not hidden by
    /// permissions"). No visible channel — nonexistent, private, or fully
    /// gated — answers `NO-SUCH-TARGET` (one code, anti-enumeration).
    pub(super) async fn on_ns_join(
        &mut self,
        label: Option<String>,
        name: NamespaceName,
        account: Account,
    ) -> io::Result<Flow> {
        let channels = match self
            .ctx
            .channel_store
            .channels_in_namespace(name.as_str())
            .await
        {
            Ok(list) => list,
            Err(e) => return self.internal(label, &e).await,
        };
        let mut joined_any = false;
        for (channel, _record) in channels {
            // Per-channel joins are unlabeled (a bulk membership burst); the
            // client processes each MEMBER/POLICY as it arrives.
            if matches!(
                self.join_one(&channel, &account, None).await?,
                JoinResult::Joined
            ) {
                joined_any = true;
            }
        }
        if !joined_any {
            return self.no_such_target(label).await;
        }
        Ok(Flow::Continue)
    }

    pub(super) async fn namespace_exists(&self, name: &str) -> bool {
        let Ok(name) = name.parse::<weft_proto::NamespaceName>() else {
            return false;
        };
        matches!(self.ctx.namespaces.namespace(&name).await, Ok(Some(_)))
    }

    /// Build the NS-META reply for a namespace record, including the §2.4
    /// recovery announcement fields.
    pub(super) fn ns_meta_event(record: &weft_store::NamespaceRecord) -> Event {
        Event::NsMeta {
            name: record.name.clone(),
            visibility: record.visibility.parse().unwrap_or(Visibility::Unlisted),
            owner: Some(record.owner.to_string()),
            title: record.title.clone(),
            description: record.description.clone(),
            icon: record.icon.clone(),
            recovery_set: record.recovery_set.is_some(),
            recovery_pending: record.pending_recovery.as_ref().map(|p| (p.eta_ms, p.rung)),
            categories: record.categories.clone(),
            federation: record.federation,
        }
    }

    pub(super) async fn on_ns_create(
        &mut self,
        label: Option<String>,
        name: weft_proto::NamespaceName,
        visibility: Visibility,
        root_key: String,
        account: Account,
    ) -> io::Result<Flow> {
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
            federation: false, // §11.10 closed until the owner opts in
        };
        match self.ctx.namespaces.create_namespace(record.clone()).await {
            Ok(true) => {
                debug!(%name, %account, "namespace created");
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
        name: &weft_proto::NamespaceName,
        actor: &Actor,
    ) -> io::Result<Option<weft_store::NamespaceRecord>> {
        let record = match self.ctx.namespaces.namespace(name).await {
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
        let scope = TokenScope::Namespace(name.to_string());
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

    pub(super) async fn on_ns_meta(
        &mut self,
        label: Option<String>,
        name: weft_proto::NamespaceName,
        key: String,
        value: String,
        actor: Actor,
    ) -> io::Result<Flow> {
        if !matches!(
            key.as_str(),
            "title" | "description" | "icon" | "categories" | "federation"
        ) {
            self.send_err(
                label,
                ErrCode::Policy,
                None,
                "meta key must be title|description|icon|categories|federation",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        let Some(mut record) = self.ns_admin_gate(label.clone(), &name, &actor).await? else {
            return Ok(Flow::Continue);
        };
        // §11.10 auto-federation reachability lives on its own column, and
        // `open` requires `public` visibility (else it could never be offered).
        if key == "federation" {
            let open = value == "open";
            if open && record.visibility != "public" {
                self.send_err(
                    label,
                    ErrCode::Forbidden,
                    None,
                    "federation open requires public visibility",
                )
                .await?;
                return Ok(Flow::Continue);
            }
            if let Err(e) = self.ctx.namespaces.set_namespace_federation(&name, open).await {
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
        name: weft_proto::NamespaceName,
        visibility: Visibility,
        actor: Actor,
    ) -> io::Result<Flow> {
        let Some(mut record) = self.ns_admin_gate(label.clone(), &name, &actor).await? else {
            return Ok(Flow::Continue);
        };
        if let Err(e) = self
            .ctx
            .namespaces
            .set_namespace_visibility(&name, &visibility.to_string())
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
        name: weft_proto::NamespaceName,
        confirm: weft_proto::NamespaceName,
        actor: Actor,
    ) -> io::Result<Flow> {
        if name != confirm {
            self.send_err(
                label,
                ErrCode::Policy,
                None,
                "DELETE must repeat the namespace name",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        // Owner or operator (§6.2). ns_admin_gate covers both (owner holds
        // ns-admin, operators hold everything).
        if self
            .ns_admin_gate(label.clone(), &name, &actor)
            .await?
            .is_none()
        {
            return Ok(Flow::Continue);
        }
        if let Err(e) = self.ctx.namespaces.delete_namespace(&name).await {
            return self.internal(label, &e).await;
        }
        debug!(%name, "namespace deleted");
        // Reflect deletion as an NS-META marker (private + no owner).
        self.send_event(
            label,
            Event::NsMeta {
                name,
                visibility: Visibility::Private,
                owner: None,
                title: None,
                description: Some("deleted".to_string()),
                icon: None,
                recovery_set: false,
                recovery_pending: None,
                categories: Vec::new(),
                federation: false,
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
        name: &weft_proto::NamespaceName,
    ) -> io::Result<Option<weft_store::NamespaceRecord>> {
        match self.ctx.namespaces.namespace(name).await {
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
        name: weft_proto::NamespaceName,
        new_owner: Account,
        signature: String,
        _account: Account,
    ) -> io::Result<Flow> {
        let Some(record) = self.ns_or_absent(label.clone(), &name).await? else {
            return Ok(Flow::Continue);
        };
        let (Ok(root_key), Ok(sig)) = (
            weft_crypto::PublicKey::from_b64(&record.root_key),
            weft_crypto::signature_from_b64(&signature),
        ) else {
            return self.forbidden_sig(label).await;
        };
        // Authority is the root *key*, not the account — this is the one
        // place same-network namespaces are cryptographically enforced.
        if !weft_crypto::verify_transfer(&root_key, name.as_str(), new_owner.as_str(), &sig) {
            return self.forbidden_sig(label).await;
        }
        // Succession keeps the root key, changes the owner.
        if let Err(e) = self
            .ctx
            .namespaces
            .rotate_root(
                &name,
                new_owner.as_str(),
                &record.root_key,
                false,
                unix_now() * 1000,
            )
            .await
        {
            return self.internal(label, &e).await;
        }
        debug!(%name, %new_owner, "namespace transferred (rung 1)");
        let updated = self.ctx.namespaces.namespace(&name).await.ok().flatten();
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
        name: weft_proto::NamespaceName,
        m: u32,
        keys: String,
        account: Account,
    ) -> io::Result<Flow> {
        let Some(record) = self.ns_or_absent(label.clone(), &name).await? else {
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
            .set_recovery_set(&name, m, &key_list)
            .await
        {
            return self.internal(label, &e).await;
        }
        let updated = self.ctx.namespaces.namespace(&name).await.ok().flatten();
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

    /// NS RECOVER: submit a signed rotation; start the delay window. Rung 2
    /// = quorum-signed (7 d), rung 3 = operator-signed (30 d). No silent
    /// path — a rotation is only ever pending + announced here, or applied
    /// by the scheduler, or vetoed (invariant 9).
    pub(super) async fn on_ns_recover(
        &mut self,
        label: Option<String>,
        name: weft_proto::NamespaceName,
        rotation: String,
    ) -> io::Result<Flow> {
        let Some(record) = self.ns_or_absent(label.clone(), &name).await? else {
            return Ok(Flow::Continue);
        };
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
        name: weft_proto::NamespaceName,
        signature: String,
    ) -> io::Result<Flow> {
        let Some(record) = self.ns_or_absent(label.clone(), &name).await? else {
            return Ok(Flow::Continue);
        };
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
            for op in self.ctx.operator_accounts() {
                self.ctx.directory.notify(op, event.clone()).await;
            }
        } else if let Some(name) = queue.strip_prefix("ns:") {
            if let Ok(ns_name) = name.parse() {
                if let Ok(Some(ns)) = self.ctx.namespaces.namespace(&ns_name).await {
                    self.ctx.directory.notify(ns.owner, event).await;
                }
            }
        }
    }


    /// The ordered channel layout of a namespace (spec extension). A
    /// non-member of a `private` namespace can't observe it (invariant 1).
    pub(super) async fn on_channels(
        &mut self,
        label: Option<String>,
        namespace: weft_proto::NamespaceName,
    ) -> io::Result<Flow> {
        let record = match self.ctx.namespaces.namespace(&namespace).await {
            Ok(Some(record)) => record,
            Ok(None) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        };
        // Private namespaces are invisible unless you belong (view cap).
        if record.visibility == "private" {
            let State::Ready { account } = self.state.clone() else {
                unreachable!("on_channels only dispatched in READY");
            };
            let scope = TokenScope::Namespace(namespace.to_string());
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
        let channels = match self
            .ctx
            .channel_store
            .channels_in_namespace(namespace.as_str())
            .await
        {
            Ok(channels) => channels,
            Err(e) => return self.internal(label, &e).await,
        };
        for (name, record) in channels {
            self.send_event(
                label.clone(),
                Event::ChannelLayout {
                    channel: name,
                    category: record.category,
                    position: record.position,
                },
            )
            .await?;
        }
        Ok(Flow::Continue)
    }
}
