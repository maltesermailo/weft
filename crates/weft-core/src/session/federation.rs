//! §11 federation handlers: bridge auth/sessions, ingest/forward, netblock,
//! FEDERATE, and the federated (tunnelled) session dispatch.

use super::*;

impl<S: ControlStream> Session<S> {
    /// §11.2 AUTH BRIDGE: resolve the key the peer must prove control of —
    /// the pinned key (which the asserted key must match), or, in accept-any
    /// mode, the asserted key itself. A blocked network, an unknown network in
    /// pinned-only mode, or a pin mismatch all funnel to the uniform
    /// AUTH-FAILED (no peer-existence oracle, invariant 1 discipline).
    pub(super) async fn on_auth_bridge(
        &mut self,
        label: Option<String>,
        network: NetworkName,
        token: String,
    ) -> io::Result<Flow> {
        let asserted = PublicKey::from_b64(&token).ok();
        let blocked = self
            .ctx
            .netblocks
            .is_netblocked(&network)
            .await
            .unwrap_or(false);
        let device = if blocked {
            None
        } else if let Some(pinned) = self.ctx.peer_key(&network).copied() {
            (asserted == Some(pinned)).then_some(pinned)
        } else if self.ctx.bridge_accept_any() {
            asserted
        } else {
            None
        };
        let Some(device) = device else {
            return self.auth_failed(label).await;
        };
        let nonce: [u8; weft_crypto::CHALLENGE_NONCE_LEN] = rand::random();
        self.send_event(
            label,
            Event::Challenge {
                nonce: weft_crypto::b64::encode(nonce),
            },
        )
        .await?;
        self.state = State::Unauthed {
            challenge: Some(PendingChallenge {
                device,
                nonce,
                subject: ChallengeSubject::Bridge { peer: network },
            }),
        };
        Ok(Flow::Continue)
    }

    /// Bridge PROOF verified: enter the bridge state and resume forwarding any
    /// previously-acked channels.
    pub(super) async fn welcome_bridge(
        &mut self,
        label: Option<String>,
        peer: NetworkName,
        key: PublicKey,
    ) -> io::Result<Flow> {
        self.send_event(
            label,
            Event::Welcome {
                network: self.ctx.info.network.clone(),
                features: vec!["bridge".to_string()],
                attestation: None,
                motd: None,
            },
        )
        .await?;
        self.state = State::Bridge {
            peer: peer.clone(),
            key,
        };
        if let Ok(Some(record)) = self.ctx.peers.peer(&peer).await {
            self.sync_bridge_forwarders(&record).await;
        }
        Ok(Flow::Continue)
    }

    /// Route a line arriving on a bridge session: peer *events* ingest; peer
    /// *commands* drive the manifest state machine (§11.1) + backfill (§11.7).
    pub(super) async fn on_bridge_line(
        &mut self,
        peer: NetworkName,
        key: PublicKey,
        line: &Line,
    ) -> io::Result<Flow> {
        match line.verb.as_str() {
            // §10.3 PROFILE is account-scoped (not channel-scoped) but still
            // ingested — `ingest_bridged` verifies + stores the signed profile.
            "MESSAGE" | "EDITED" | "DELETED" | "REACTION" | "PROFILE" => {
                self.on_ingest(&peer, line).await
            }
            // Remote membership / typing / presence / marks are informational;
            // not stored, not re-broadcast in M5b.
            "MEMBER" | "TYPING" | "PRESENCE" | "MARKED" | "POLICY" => Ok(Flow::Continue),
            // §11.7 the peer answered our backfill HISTORY with a stream offer
            // (large page) → hand weftd the pull; it drains the data plane and
            // feeds each line back through `ingest_bridged`.
            "STREAM" => {
                if let Ok(Reply {
                    event: Event::StreamAccept { token },
                    ..
                }) = Reply::from_line(line)
                {
                    self.ctx.request_backfill_pull(crate::BackfillPull {
                        peer: peer.clone(),
                        token,
                    });
                }
                Ok(Flow::Continue)
            }
            _ => match Request::from_line(line) {
                Ok(req) => self.on_bridge_cmd(peer, key, req.label, req.command).await,
                Err(_) => Ok(Flow::Continue), // tolerate noise on a bridge
            },
        }
    }

    pub(super) async fn on_bridge_cmd(
        &mut self,
        peer: NetworkName,
        key: PublicKey,
        label: Option<String>,
        cmd: Command,
    ) -> io::Result<Flow> {
        match cmd {
            Command::BridgePropose {
                scope,
                history,
                media,
                typing,
                voice,
                manifest,
                ..
            } => {
                self.on_bridge_propose_in(peer, key, scope, history, media, typing, voice, manifest)
                    .await
            }
            Command::BridgeAccept { version, .. } => self.on_bridge_accept_in(peer, version).await,
            Command::BridgeSever { .. } => self.on_bridge_sever_in(peer).await,
            Command::BridgeRequest { ns } => self.on_bridge_request_in(peer, label, ns).await,
            Command::VoiceRequest { scope, channel } => {
                self.on_voice_request_in(peer, label, scope, channel).await
            }
            Command::FSession { fsid, op } => self.on_fsession(&peer, fsid, op).await,
            // §11.7 federated backfill: the peer pulls history over the bridge.
            Command::History {
                target,
                before,
                after,
                limit,
                ..
            } => {
                self.on_bridge_backfill(peer, label, target, before, after, limit)
                    .await
            }
            // §11.9 a forwarded report from the reporter's home network.
            Command::ReportForward {
                report_id,
                msgid,
                category,
                note,
            } => {
                self.on_report_forward_in(peer, report_id, msgid, category, note)
                    .await
            }
            Command::Ping { token } => {
                self.send_event(label, Event::Pong { token }).await?;
                Ok(Flow::Continue)
            }
            Command::Quit { .. } => Ok(Flow::Close),
            _ => Ok(Flow::Continue),
        }
    }

    /// §11.10 Demux a federation-session frame on a bridge (homeserver
    /// authority): `OPEN` spawns a tunnelled session for `<account>@<peer>`
    /// (`F` proved its network key, so it speaks for its users); `CMD` feeds
    /// that sub-session; `CLOSE` ends it. `REPLY` is H→F only and never arrives
    /// here. The spawned session's writes funnel back through this session's
    /// `fed_out` queue, so the one socket is written by one task.
    pub(super) async fn on_fsession(
        &mut self,
        peer: &NetworkName,
        fsid: String,
        op: FSessionOp,
    ) -> io::Result<Flow> {
        match op {
            FSessionOp::Open { account } => {
                let user = format!("{account}@{peer}");
                let (in_tx, in_rx) = mpsc::channel(EVENT_QUEUE);
                self.tunnels.insert(fsid.clone(), in_tx);
                let stream = TunnelStream {
                    fsid,
                    inbound: in_rx,
                    outbound: self.fed_out_tx.clone(),
                };
                spawn_federated_session(stream, Arc::clone(&self.ctx), user);
            }
            FSessionOp::Cmd { line } => {
                if let Some(tx) = self.tunnels.get(&fsid) {
                    let _ = tx.send(line).await; // dropped if the sub-session is gone
                }
            }
            FSessionOp::Close => {
                // Dropping the inbound sender ends the sub-session's recv loop.
                self.tunnels.remove(&fsid);
            }
            FSessionOp::Reply { .. } => {} // H→F only; ignore any inbound REPLY
        }
        Ok(Flow::Continue)
    }

    /// A peer sent us a signed manifest (§11.1). Verify it against the peer's
    /// pinned key, store it, and (auto-accept path) ack + start forwarding.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn on_bridge_propose_in(
        &mut self,
        peer: NetworkName,
        key: PublicKey,
        scope: String,
        _history: HistoryMode,
        _media: MediaMode,
        _typing: bool,
        _voice: bool,
        manifest: Option<String>,
    ) -> io::Result<Flow> {
        let Some(blob) = manifest else {
            return Ok(Flow::Continue);
        };
        let Ok(signed) = SignedManifest::from_b64(&blob) else {
            return Ok(Flow::Continue);
        };
        // Verify against the key this session authenticated with (pinned or
        // accept-any) — not a fresh config lookup, so open federation works.
        if !bridge::verify_incoming(&signed, &key, self.ctx.network()) {
            debug!(%peer, "rejected bridge proposal: bad manifest signature/peer");
            return Ok(Flow::Continue);
        }
        let now = unix_now_ms();
        let version = signed.manifest.version;
        // Auto-accept if configured, or if *we* requested this bridge (§11.10).
        let auto = self.ctx.bridge_auto_accept() || self.request_accept;
        let record = PeerRecord {
            peer: peer.clone(),
            scope,
            manifest: blob.clone(),
            version,
            acked_manifest: auto.then(|| blob.clone()),
            severed: false,
            created_ms: now,
            updated_ms: now,
        };
        if let Err(e) = self.ctx.peers.upsert_peer(record.clone()).await {
            return self.internal(None, &e).await;
        }
        if auto {
            let ack = Request::new(Command::BridgeAccept {
                peer: self.ctx.network().clone(),
                version,
            });
            if let Ok(line) = ack.serialize() {
                self.stream.send_line(&line).await?;
            }
            self.sync_bridge_forwarders(&record).await;
            self.announce_manifest(&record, BridgeState::Live).await;
        }
        Ok(Flow::Continue)
    }

    /// The peer acked our manifest at `version` → live. Mark it and forward.
    pub(super) async fn on_bridge_accept_in(
        &mut self,
        peer: NetworkName,
        version: u64,
    ) -> io::Result<Flow> {
        let Ok(Some(mut record)) = self.ctx.peers.peer(&peer).await else {
            return Ok(Flow::Continue);
        };
        if record.version != version {
            debug!(%peer, record.version, version, "bridge ack version mismatch");
            return Ok(Flow::Continue);
        }
        record.acked_manifest = Some(record.manifest.clone());
        record.updated_ms = unix_now_ms();
        if let Err(e) = self.ctx.peers.upsert_peer(record.clone()).await {
            return self.internal(None, &e).await;
        }
        self.sync_bridge_forwarders(&record).await;
        self.announce_manifest(&record, BridgeState::Live).await;
        Ok(Flow::Continue)
    }

    /// The peer tore the bridge down (§11.6/§6.6). Stop forwarding.
    pub(super) async fn on_bridge_sever_in(&mut self, peer: NetworkName) -> io::Result<Flow> {
        if let Ok(Some(mut record)) = self.ctx.peers.peer(&peer).await {
            record.severed = true;
            record.updated_ms = unix_now_ms();
            let _ = self.ctx.peers.upsert_peer(record.clone()).await;
            self.announce_manifest(&record, BridgeState::Severed).await;
        }
        for (_, forwarder) in self.bridged.drain() {
            forwarder.abort();
        }
        // §16 M-lk-3b: a severed bridge stops the peer's federated voice too.
        self.ctx.relay_drop_peer(&peer).await;
        Ok(Flow::Continue)
    }

    /// §11.10 A peer asked us to offer a manifest for one of *our* namespaces.
    /// We offer (a signed `BRIDGE PROPOSE`) iff it is auto-federation-reachable
    /// (`public` + `federation` open) and the peer isn't netblocked; otherwise
    /// `NO-SUCH-TARGET` — uniform with private/absent (anti-enumeration,
    /// invariant 1). The peer verifies + auto-accepts on its side.
    pub(super) async fn on_bridge_request_in(
        &mut self,
        peer: NetworkName,
        label: Option<String>,
        ns: NamespaceName,
    ) -> io::Result<Flow> {
        let reachable = match self.ctx.namespaces.namespace(&ns).await {
            Ok(Some(rec)) => rec.visibility == "public" && rec.federation,
            Ok(None) => false,
            Err(e) => return self.internal(label, &e).await,
        };
        let blocked = self
            .ctx
            .netblocks
            .is_netblocked(&peer)
            .await
            .unwrap_or(false);
        if !reachable || blocked {
            return self.no_such_target(label).await;
        }

        // Compile + sign a v1 manifest for the namespace's channels and offer it.
        let scope = format!("ns:{ns}");
        let Some(tscope) = TokenScope::parse(&scope) else {
            return self.no_such_target(label).await;
        };
        let channels = self.scope_channels(&tscope).await;
        // §11.10 auto-federation offers `history=full`: a user joining a foreign
        // public namespace wants its existing scrollback (§11.7 backfill), not
        // just messages from the moment they federated. `from-epoch` would floor
        // backfill at the manifest's creation and hide everything already posted.
        // §16 a public, federating namespace shares its voice channels too, so the
        // auto-federation offer sets `voice=on` (a foreign user who joins the ns can
        // then relay its voice rooms). `typing` stays off (noisy/low-value).
        let (history, media, typing, voice) = (
            weft_proto::HistoryMode::Full,
            weft_proto::MediaMode::None,
            false,
            true,
        );
        let record = match self
            .store_bridge_proposal(&peer, scope, &channels, history, media, typing, voice)
            .await
        {
            Ok(record) => record,
            Err(e) => return self.internal(label, &e).await,
        };
        let cmd = Command::BridgePropose {
            scope: record.scope.clone(),
            peer,
            history,
            media,
            typing,
            voice,
            manifest: Some(record.manifest.clone()),
        };
        if let Ok(line) = Request::new(cmd).serialize() {
            self.stream.send_line(&line).await?;
        }
        Ok(Flow::Continue)
    }

    /// §16 a peer asks us to relay one of *our* voice channels (`VOICE REQUEST`).
    /// Gate on invariant 3 (the channel is in the acked+current manifest) + the
    /// manifest `voice` flag + the peer not being netblocked (invariant 7), then
    /// mint the relay credentials and answer `VOICE GRANT`. Every refusal is the
    /// uniform NO-SUCH-TARGET (invariant 1) — the requester can't distinguish
    /// "no such channel" from "voice not offered" from "you're blocked".
    pub(super) async fn on_voice_request_in(
        &mut self,
        peer: NetworkName,
        label: Option<String>,
        _scope: String,
        channel: ChannelName,
    ) -> io::Result<Flow> {
        // A netblocked peer gets nothing (invariant 7, uniform-1 timing).
        if self
            .ctx
            .netblocks
            .is_netblocked(&peer)
            .await
            .unwrap_or(false)
        {
            return self.no_such_target(label).await;
        }

        // The channel must be forwardable to this peer (acked ∩ current,
        // invariant 3) AND the manifest must opt voice channels in (`voice=on`).
        let Ok(Some(record)) = self.ctx.peers.peer(&peer).await else {
            return self.no_such_target(label).await;
        };
        if !bridge::is_forwardable(&record, channel.as_str()) {
            return self.no_such_target(label).await;
        }
        let voice_on = SignedManifest::from_b64(&record.manifest)
            .map(|s| s.manifest.voice)
            .unwrap_or(false);
        if !voice_on {
            return self.no_such_target(label).await;
        }

        // Mint the media credential — needs a cascadable backend (LiveKit); the
        // embedded SFU can't be relayed to, so a request against it is refused.
        let Some(backend) = self.ctx.voice_backend().cloned() else {
            return self.no_such_target(label).await;
        };
        let Some((grant, ttl_secs)) = backend.relay_grant(&channel, peer.as_str()).await else {
            return self.no_such_target(label).await;
        };
        let room = grant.room.clone().unwrap_or_default();
        let url = grant.endpoint.clone().unwrap_or_default();

        // Sign the durable, offline-verifiable WEFT-level authorization.
        let now_ms = unix_now_ms();
        let signed = self.ctx.sign_voice_relay(&weft_crypto::VoiceRelayGrant {
            issuer: self.ctx.network().to_string(),
            grantee: peer.to_string(),
            channel: channel.to_string(),
            room: room.clone(),
            expiry: now_ms + ttl_secs.saturating_mul(1000),
        });

        self.send_event(
            label,
            Event::VoiceGrant {
                channel,
                url,
                room,
                token: grant.token,
                grant: signed.to_b64(),
                ttl: ttl_secs,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// §11.7 federated backfill: serve a bridged channel's history to the peer
    /// over the bridge session. Gated on the acked manifest (invariant 3) and
    /// the manifest `history` flag (`from-epoch` = nothing before the
    /// manifest's `created` ULID timestamp); origin retention is enforced by
    /// the store (purged rows never return, `truncated` is set honestly).
    pub(super) async fn on_bridge_backfill(
        &mut self,
        peer: NetworkName,
        label: Option<String>,
        target: Target,
        before: Option<MsgId>,
        after: Option<MsgId>,
        limit: Option<u32>,
    ) -> io::Result<Flow> {
        let Target::Channel(channel) = target.clone() else {
            return Ok(Flow::Continue); // DMs never bridge (§9.5)
        };
        let Some(record) = self.ctx.peers.peer(&peer).await.ok().flatten() else {
            return Ok(Flow::Continue);
        };
        if !bridge::is_forwardable(&record, channel.as_str()) {
            debug!(%peer, %channel, "backfill refused: channel not in acked manifest");
            return self
                .emit_batch(label, &target, Vec::new(), false)
                .await
                .map(|_| Flow::Continue);
        }
        // `from-epoch` lower bound = the manifest's `created` timestamp.
        let (history, created) = SignedManifest::from_b64(&record.manifest)
            .map(|s| (s.manifest.history, s.manifest.created))
            .unwrap_or_default();
        let manifest_floor = if history == "full" { 0 } else { created };
        let after_floor = after.as_ref().map(|m| m.timestamp_ms()).unwrap_or(0);
        // Respect an explicit `after` exclusivity when it's already past the
        // manifest floor; otherwise clamp up to the floor.
        let after_ulid = if after_floor >= manifest_floor {
            after.as_ref().map(|m| m.ulid())
        } else if manifest_floor > 0 {
            Some(Ulid::from_parts(manifest_floor, 0))
        } else {
            None
        };
        let scope = Scope::Channel(channel);
        let policy = self
            .ctx
            .channel_store
            .channel(match &scope {
                Scope::Channel(c) => c,
                _ => unreachable!(),
            })
            .await
            .ok()
            .flatten()
            .map(|c| c.policy)
            .unwrap_or(RetentionPolicy::Permanent);
        let limit = limit.unwrap_or(100).clamp(1, weft_proto::MAX_HISTORY_LIMIT) as usize;

        let (items, truncated) = if policy == RetentionPolicy::Ephemeral {
            (Vec::new(), true)
        } else {
            let page = weft_store::Page {
                before: before.as_ref().map(|m| m.ulid()),
                after: after_ulid,
                limit,
            };
            let roots = match self.ctx.events.roots(&scope, page).await {
                Ok(roots) => roots,
                Err(e) => return self.internal(label, &e).await,
            };
            let root_ulids: Vec<_> = roots.iter().map(|r| r.msgid.ulid()).collect();
            let children = match self.ctx.events.children(&scope, &root_ulids).await {
                Ok(children) => children,
                Err(e) => return self.internal(label, &e).await,
            };
            let watermark = self.ctx.events.purged_before(&scope).await.ok().flatten();
            let items = weft_store::materialize(roots, children);
            let floor_ms = manifest_floor.max(after_floor);
            let truncated = items.len() < limit && watermark.is_some_and(|w| floor_ms < w);
            (items, truncated)
        };
        self.emit_batch(label, &target, items, truncated).await?;
        Ok(Flow::Continue)
    }

    /// §11.9 a forwarded report arriving over the bridge from a reporter's home
    /// network. We're the origin of the reported msgid; treat it as a
    /// net-scope, `unverified` signal with the reporter stripped, and drop it
    /// into the operator queue. Report queues/holds never replicate — a fresh
    /// local id, no hold (unverified places none).
    pub(super) async fn on_report_forward_in(
        &mut self,
        _peer: NetworkName,
        _report_id: String,
        msgid: MsgId,
        category: String,
        note: Option<String>,
    ) -> io::Result<Flow> {
        if msgid.origin().as_str() != self.ctx.network_name() {
            return Ok(Flow::Continue); // not ours to act on
        }
        let Ok(Some(root)) = self.ctx.events.find_root(msgid.ulid()).await else {
            return Ok(Flow::Continue); // content gone — nothing to file against
        };
        let report_id = Ulid::new().to_string();
        let record = ReportRecord {
            id: report_id.clone(),
            msgid: msgid.clone(),
            scope: root.scope.clone(),
            category: category.clone(),
            state: ContentState::Unverified, // §11.9 unverified-at-minimum
            reporter: forwarded_reporter(),
            note,
            queue_scopes: vec!["*".to_string()], // net scope → operator
            status: ReportStatus::Open,
            filed_at_ms: unix_now_ms(),
            held_roots: vec![],
            resolution: None,
            holds_released: false,
        };
        if let Err(e) = self.ctx.reports.file_report(record).await {
            error!("forwarded report not filed: {e}");
            return Ok(Flow::Continue);
        }
        // Notify operators — reporter stripped (§11.9, invariant 12).
        self.notify_queue_handlers(
            "*",
            Event::ReportFiled {
                report_id,
                msgid,
                category,
                state: ContentState::Unverified,
                scope: ReportScope::Net,
                reporter: None,
            },
        )
        .await;
        Ok(Flow::Continue)
    }

    /// Ingest a bridged event (§11.4): the origin must be the authenticated
    /// peer (invariant 2), and the channel must be in the acked manifest
    /// (invariant 3). Persisted with its origin msgid intact.
    pub(super) async fn on_ingest(&mut self, peer: &NetworkName, line: &Line) -> io::Result<Flow> {
        self.ctx.ingest_bridged(peer, line).await;
        Ok(Flow::Continue)
    }

    /// Subscribe the bridge session to exactly the forwardable channels
    /// (invariant 3); tear down forwarders for channels no longer bridged.
    pub(super) async fn sync_bridge_forwarders(&mut self, record: &PeerRecord) {
        let want: Vec<ChannelName> = bridge::forwardable_channels(record)
            .iter()
            .filter_map(|c| c.parse().ok())
            .collect();
        let stale: Vec<ChannelName> = self
            .bridged
            .keys()
            .filter(|c| !want.contains(c))
            .cloned()
            .collect();
        for channel in stale {
            if let Some(forwarder) = self.bridged.remove(&channel) {
                forwarder.abort();
            }
        }
        for channel in &want {
            if self.bridged.contains_key(channel) {
                continue;
            }
            if let Some(handle) = self.ctx.registry.get(channel) {
                if let Some(rx) = handle.subscribe().await {
                    let forwarder = spawn_forwarder(channel.clone(), rx, self.events_tx.clone());
                    self.bridged.insert(channel.clone(), forwarder);
                }
            }
        }
    }

    /// §11.7 on-demand backfill: a local client's HISTORY ran out of local
    /// scrollback for a channel this bridge forwards, so pull that window from
    /// the peer (the peer serves the compacted view, bounded by its manifest
    /// `history` flag + retention, and streams it if large). We fetch **only what
    /// a client asked to see** — never a whole federated scrollback eagerly.
    /// Gated on forwardability (invariant 3) and deduped per `(channel, before)`
    /// window so repeated scrolls hit the peer once. Origin authority (invariant
    /// 2) keeps us ingesting only the peer's own events.
    pub(super) async fn on_backfill_demand(&mut self, req: crate::BackfillReq) {
        let State::Bridge { peer, .. } = self.state.clone() else {
            return; // only a bridge session speaks to a peer
        };
        let forwardable = self
            .ctx
            .peers
            .peer(&peer)
            .await
            .ok()
            .flatten()
            .map(|p| bridge::is_forwardable(&p, req.channel.as_str()))
            .unwrap_or(false);
        if !forwardable {
            return; // not our peer's channel — another bridge may serve it
        }
        let window = (
            req.channel.clone(),
            req.before.as_ref().map(|m| m.to_string()),
        );
        if !self.backfilled.insert(window) {
            return; // already asked the peer for this window
        }
        let cmd = Command::History {
            target: Target::Channel(req.channel),
            before: req.before,
            after: None,
            limit: Some(weft_proto::MAX_HISTORY_LIMIT),
            thread: None,
        };
        if let Ok(line) = Request::new(cmd).serialize() {
            let _ = self.stream.send_line(&line).await;
        }
    }

    /// Outbound bridge startup: transmit the proposal the operator compiled +
    /// stored (`BRIDGE PROPOSE`) for `peer` — M5d's job — and, if a prior
    /// session already got it acked, resume forwarding immediately. The peer
    /// ingests the manifest and (auto-accept) replies `BRIDGE ACCEPT`, handled
    /// by the ordinary bridge loop.
    pub(super) async fn begin_outbound_bridge(&mut self, peer: &NetworkName) {
        let Ok(Some(record)) = self.ctx.peers.peer(peer).await else {
            return;
        };
        if record.severed {
            return;
        }
        if let Ok(signed) = SignedManifest::from_b64(&record.manifest) {
            let cmd = Command::BridgePropose {
                scope: record.scope.clone(),
                peer: peer.clone(),
                // The peer authoritatively reads the `@manifest` blob; these
                // flags are informational, parsed back from it (defaults if not).
                history: signed
                    .manifest
                    .history
                    .parse()
                    .unwrap_or(weft_proto::HistoryMode::FromEpoch),
                media: signed
                    .manifest
                    .media
                    .parse()
                    .unwrap_or(weft_proto::MediaMode::None),
                typing: signed.manifest.typing,
                voice: signed.manifest.voice,
                manifest: Some(record.manifest.clone()),
            };
            if let Ok(line) = Request::new(cmd).serialize() {
                let _ = self.stream.send_line(&line).await;
            }
        }
        if record.acked_manifest.is_some() {
            self.sync_bridge_forwarders(&record).await;
        }
    }

    /// §11.10 requester startup: ask the peer to offer a manifest for its
    /// namespace `ns`. The peer answers `BRIDGE PROPOSE` (if reachable), handled
    /// by the ordinary bridge loop and auto-accepted (`request_accept`).
    pub(super) async fn begin_outbound_request(&mut self, ns: &NamespaceName) {
        let cmd = Command::BridgeRequest { ns: ns.clone() };
        if let Ok(line) = Request::new(cmd).serialize() {
            let _ = self.stream.send_line(&line).await;
        }
    }

    /// §6.6 MANIFEST-to-members: broadcast the change into each affected
    /// channel so local members learn of the audience change (mandatory).
    pub(super) async fn announce_manifest(&self, record: &PeerRecord, state: BridgeState) {
        let channels = bridge::forwardable_channels(record);
        for channel in &channels {
            if let Ok(chan) = channel.parse::<ChannelName>() {
                if let Some(handle) = self.ctx.registry.get(&chan) {
                    handle
                        .announce(manifest_event(record, state, &channels))
                        .await;
                }
            }
        }
    }

    /// A bridge session's channel events: forward only *local-origin*
    /// message-plane events to the peer (one hop, §11.4 — received events are
    /// never re-forwarded because their origin != our network).
    pub(super) async fn on_bridge_event(
        &mut self,
        _peer: NetworkName,
        event: SessionEvent,
    ) -> io::Result<()> {
        let SessionEvent::Channel { event, .. } = event else {
            return Ok(()); // Lagged: a real bridge would resync (M5c)
        };
        // §10.3 a *local* user's display profile forwards to the peer, signed
        // with our network key (avatar bound by hash) so the peer can verify it.
        if let Event::Profile {
            user,
            display,
            avatar,
        } = &event.event
        {
            if user.network.as_str() == self.ctx.network_name() {
                let profile = weft_crypto::Profile {
                    account: user.to_string(),
                    display: display.clone(),
                    avatar: avatar.clone(),
                    updated: unix_now_ms(),
                };
                let sig = self.ctx.sign_profile(&profile).to_b64();
                if let Ok(mut line) = Reply::new(event.event.clone()).to_line() {
                    line.tags.insert("sig".to_string(), sig);
                    if let Ok(serialized) = line.serialize() {
                        self.stream.send_line(&serialized).await?;
                    }
                }
            }
            return Ok(());
        }
        let ours = |id: &MsgId| id.origin().as_str() == self.ctx.network_name();
        let forward = match &event.event {
            // System messages (join/part lines) are local channel noise — not
            // re-broadcast across the bridge (like remote MEMBER, §11).
            Event::Message(m) if m.meta.system.is_some() => false,
            Event::Message(m) => ours(&m.msgid),
            Event::Edited { msgid, .. } => ours(msgid),
            Event::Deleted { msgid, .. } => ours(msgid),
            Event::Reaction { msgid, .. } => ours(msgid),
            _ => false, // MEMBER/TYPING/POLICY/MANIFEST not forwarded in M5b
        };
        if forward {
            if let Ok(line) = Reply::new(event.event).serialize() {
                self.stream.send_line(&line).await?;
            }
        }
        Ok(())
    }

    /// §11.10 FEDERATE: a local user asks to reach a foreign namespace on
    /// demand. Gate on the open policy (the dialer sink is installed only when
    /// `auto_bridge = open`), NETBLOCK, a per-account cooldown, and self-dial;
    /// then hand the dial to weftd. The bridge establishes asynchronously — the
    /// client learns it went live via `MANIFEST`.
    pub(super) async fn on_federate(
        &mut self,
        label: Option<String>,
        network: NetworkName,
        namespace: NamespaceName,
        account: Account,
    ) -> io::Result<Flow> {
        if network.as_str() == self.ctx.network_name() {
            return self
                .unsupported(label, "that namespace is on this network already")
                .await;
        }
        if self
            .ctx
            .netblocks
            .is_netblocked(&network)
            .await
            .unwrap_or(false)
        {
            self.send_err(label, ErrCode::Blocked, None, "network is blocked")
                .await?;
            return Ok(Flow::Continue);
        }
        if !self.ctx.federate_allowed(&account) {
            self.send_err(
                label,
                ErrCode::Throttled,
                None,
                "one federation request at a time",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        let req = crate::context::AutoBridgeRequest { network, namespace };
        if !self.ctx.request_auto_bridge(req) {
            return self
                .unsupported(label, "auto-federation is off on this network")
                .await;
        }
        Ok(Flow::Continue)
    }

    // ---- §11 federation: operator-facing management (§6.6) ----

    /// Compile + sign a v1 manifest for `scope`'s `channels` and store it as a
    /// pending (un-acked) proposal to `peer`. Shared by the operator
    /// `BRIDGE PROPOSE` (§6.6) and the §11.10 auto-offer.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn store_bridge_proposal(
        &self,
        peer: &NetworkName,
        scope: String,
        channels: &[ChannelName],
        history: HistoryMode,
        media: MediaMode,
        typing: bool,
        voice: bool,
    ) -> Result<PeerRecord, weft_store::StoreError> {
        let now = unix_now_ms();
        let manifest =
            bridge::build_manifest(peer, 1, channels, history, media, typing, voice, now, now);
        let record = PeerRecord {
            peer: peer.clone(),
            scope,
            manifest: self.ctx.sign_manifest(&manifest),
            version: 1,
            acked_manifest: None,
            severed: false,
            created_ms: now,
            updated_ms: now,
        };
        self.ctx.peers.upsert_peer(record.clone()).await?;
        Ok(record)
    }

    /// §6.6/§11.3 BRIDGE PROPOSE from an operator: check the scope authority,
    /// compile + sign a v1 manifest, and store it. Transmission to the peer
    /// over the bridge session is the dialer's job (M5d).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn on_bridge_propose(
        &mut self,
        label: Option<String>,
        scope: String,
        peer: NetworkName,
        history: HistoryMode,
        media: MediaMode,
        typing: bool,
        voice: bool,
        account: Account,
    ) -> io::Result<Flow> {
        if self
            .ctx
            .netblocks
            .is_netblocked(&peer)
            .await
            .unwrap_or(false)
        {
            self.send_err(label, ErrCode::Blocked, None, "peer network is blocked")
                .await?;
            return Ok(Flow::Continue);
        }
        let Some(tscope) = TokenScope::parse(&scope) else {
            return self.no_such_target(label).await;
        };
        // §11.3 ladder: `bridge` cap at the scope (operators/ns-owners implied).
        match self
            .ctx
            .account_has_cap(&account, &Capability::Bridge, &tscope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "bridge").await,
            Err(e) => return self.internal(label, &e).await,
        }
        let channels = self.scope_channels(&tscope).await;
        let record = match self
            .store_bridge_proposal(&peer, scope, &channels, history, media, typing, voice)
            .await
        {
            Ok(record) => record,
            Err(e) => return self.internal(label, &e).await,
        };
        let channel_strs = bridge::forwardable_channels(&record);
        self.send_event(
            label,
            manifest_event(&record, BridgeState::Added, &channel_strs),
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// §6.6 BRIDGE ACCEPT from an operator: mark a stored proposal live.
    pub(super) async fn on_bridge_accept_op(
        &mut self,
        label: Option<String>,
        peer: NetworkName,
        version: u64,
        account: Account,
    ) -> io::Result<Flow> {
        let Some(mut record) = self.ctx.peers.peer(&peer).await.ok().flatten() else {
            return self.no_such_target(label).await;
        };
        let tscope = TokenScope::parse(&record.scope).unwrap_or(TokenScope::Wildcard);
        match self
            .ctx
            .account_has_cap(&account, &Capability::Bridge, &tscope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "bridge").await,
            Err(e) => return self.internal(label, &e).await,
        }
        if record.version != version {
            self.send_err(label, ErrCode::Conflict, None, "manifest version race")
                .await?;
            return Ok(Flow::Continue);
        }
        record.acked_manifest = Some(record.manifest.clone());
        record.updated_ms = unix_now_ms();
        if let Err(e) = self.ctx.peers.upsert_peer(record.clone()).await {
            return self.internal(label, &e).await;
        }
        let channel_strs = bridge::forwardable_channels(&record);
        self.send_event(
            label,
            manifest_event(&record, BridgeState::Live, &channel_strs),
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// §6.6 BRIDGE SEVER from an operator: unilateral teardown.
    pub(super) async fn on_bridge_sever_op(
        &mut self,
        label: Option<String>,
        peer: NetworkName,
        account: Account,
    ) -> io::Result<Flow> {
        let Some(mut record) = self.ctx.peers.peer(&peer).await.ok().flatten() else {
            return self.no_such_target(label).await;
        };
        let tscope = TokenScope::parse(&record.scope).unwrap_or(TokenScope::Wildcard);
        match self
            .ctx
            .account_has_cap(&account, &Capability::Bridge, &tscope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "bridge").await,
            Err(e) => return self.internal(label, &e).await,
        }
        record.severed = true;
        record.updated_ms = unix_now_ms();
        if let Err(e) = self.ctx.peers.upsert_peer(record.clone()).await {
            return self.internal(label, &e).await;
        }
        self.send_event(label, manifest_event(&record, BridgeState::Severed, &[]))
            .await?;
        Ok(Flow::Continue)
    }

    /// Channels covered by a bridge scope, snapshotted at propose time (§11.1).
    pub(super) async fn scope_channels(&self, scope: &TokenScope) -> Vec<ChannelName> {
        match scope {
            TokenScope::Channel(c) => c.parse().ok().into_iter().collect(),
            TokenScope::Namespace(n) => self
                .ctx
                .channel_store
                .channels_in_namespace(n)
                .await
                .map(|v| v.into_iter().map(|(name, _)| name).collect())
                .unwrap_or_default(),
            TokenScope::Wildcard => self
                .ctx
                .channel_store
                .list_channels()
                .await
                .map(|v| v.into_iter().map(|(name, _)| name).collect())
                .unwrap_or_default(),
        }
    }

    // ---- §11.6 NETBLOCK ----

    pub(super) async fn on_netblock_add(
        &mut self,
        label: Option<String>,
        network: NetworkName,
        reason: Option<String>,
        account: Account,
    ) -> io::Result<Flow> {
        // `netblock` cap is `*`-scope only (§10.4).
        match self
            .ctx
            .account_has_cap(
                &account,
                &Capability::Netblock,
                &TokenScope::Wildcard,
                unix_now(),
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "netblock").await,
            Err(e) => return self.internal(label, &e).await,
        }
        let record = NetblockRecord {
            network: network.clone(),
            reason,
            added_ms: unix_now_ms(),
            actor: account.to_string(),
        };
        if let Err(e) = self.ctx.netblocks.add_netblock(record).await {
            return self.internal(label, &e).await;
        }
        // Effect 2 (§11.6): sever any existing manifest with this network.
        if let Ok(Some(mut peer)) = self.ctx.peers.peer(&network).await {
            peer.severed = true;
            peer.updated_ms = unix_now_ms();
            let _ = self.ctx.peers.upsert_peer(peer).await;
        }
        // §16 M-lk-3b (invariant 7 "stop media"): drop the network's voice relays.
        self.ctx.relay_drop_peer(&network).await;
        self.send_event(
            label,
            Event::Netblocked {
                network,
                reason: None,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    pub(super) async fn on_netblock_remove(
        &mut self,
        label: Option<String>,
        network: NetworkName,
        account: Account,
    ) -> io::Result<Flow> {
        match self
            .ctx
            .account_has_cap(
                &account,
                &Capability::Netblock,
                &TokenScope::Wildcard,
                unix_now(),
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "netblock").await,
            Err(e) => return self.internal(label, &e).await,
        }
        match self.ctx.netblocks.remove_netblock(&network).await {
            Ok(true) => {
                self.send_event(
                    label,
                    Event::Netblocked {
                        network,
                        reason: None,
                    },
                )
                .await?
            }
            Ok(false) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        }
        Ok(Flow::Continue)
    }

    pub(super) async fn on_netblock_list(
        &mut self,
        label: Option<String>,
        account: Account,
    ) -> io::Result<Flow> {
        match self
            .ctx
            .account_has_cap(
                &account,
                &Capability::Netblock,
                &TokenScope::Wildcard,
                unix_now(),
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "netblock").await,
            Err(e) => return self.internal(label, &e).await,
        }
        let blocks = self
            .ctx
            .netblocks
            .list_netblocks()
            .await
            .unwrap_or_default();
        for (i, block) in blocks.iter().enumerate() {
            // Echo the request label on the first line only (§3.5).
            let lbl = (i == 0).then(|| label.clone()).flatten();
            self.send_event(
                lbl,
                Event::Netblocked {
                    network: block.network.clone(),
                    reason: block.reason.clone(),
                },
            )
            .await?;
        }
        Ok(Flow::Continue)
    }

    // ---- §6.7 moderation ----

    /// The §6.7 posting gate: `None` = may post; `Some((code, context))` = the
    /// refusal. Checks ban → mute → (restricted ⇒ `send` cap), against the
    /// channel's covering scopes.
    pub(super) async fn can_post(
        &self,
        channel: &ChannelName,
        account: &Account,
    ) -> Result<Option<(ErrCode, &'static str)>, weft_store::StoreError> {
        let scopes = covering_scopes(channel);
        if self
            .ctx
            .moderation
            .is_moderated(account, &scopes, ModKind::Ban)
            .await?
        {
            return Ok(Some((ErrCode::Forbidden, "banned")));
        }
        if self
            .ctx
            .moderation
            .is_moderated(account, &scopes, ModKind::Mute)
            .await?
        {
            return Ok(Some((ErrCode::Forbidden, "muted")));
        }
        let restricted = self
            .ctx
            .channel_store
            .channel(channel)
            .await?
            .map(|c| c.restricted)
            .unwrap_or(false);
        if restricted {
            let scope = TokenScope::Channel(channel.to_string());
            if !self
                .ctx
                .account_has_cap(account, &Capability::Send, &scope, unix_now())
                .await?
            {
                return Ok(Some((ErrCode::CapRequired, "send")));
            }
        }
        Ok(None)
    }

    /// §13 may `account` post *attachments* to `channel`? Attachments to a
    /// **restricted** channel additionally require the `attach` capability
    /// (open channels allow them freely, mirroring the posting gate). Operators /
    /// ns-owners hold `attach` implicitly via `account_has_cap`.
    pub(super) async fn can_attach(
        &self,
        channel: &ChannelName,
        account: &Account,
    ) -> Result<bool, weft_store::StoreError> {
        let restricted = self
            .ctx
            .channel_store
            .channel(channel)
            .await?
            .map(|c| c.restricted)
            .unwrap_or(false);
        if !restricted {
            return Ok(true);
        }
        let scope = TokenScope::Channel(channel.to_string());
        self.ctx
            .account_has_cap(account, &Capability::Attach, &scope, unix_now())
            .await
    }

    /// §11.10 dispatch for a **federated** (tunnelled) session: F's user `user`
    /// (`account@F`) acts on H under homeserver authority, enforced against H's
    /// grant store as `Actor::Foreign(user)` (§10.4). A pure command conduit —
    /// no JOIN / no subscribe, so broadcast events ride the mirror not the
    /// tunnel (§10.3). Only the actor-aware verbs are wired so far (moderation,
    /// P5 step 4); the rest answer `UNSUPPORTED` until step 5 generalizes them.
    pub(super) async fn on_federated(
        &mut self,
        label: Option<String>,
        cmd: Command,
        user: String,
    ) -> io::Result<Flow> {
        let actor = Actor::Foreign(user);
        match cmd {
            Command::Mute {
                scope,
                account: target,
                reason,
            } => {
                self.on_moderate(label, scope, target, ModKind::Mute, true, reason, actor)
                    .await
            }
            Command::Unmute {
                scope,
                account: target,
            } => {
                self.on_moderate(label, scope, target, ModKind::Mute, false, None, actor)
                    .await
            }
            Command::Ban {
                scope,
                account: target,
                reason,
            } => {
                self.on_moderate(label, scope, target, ModKind::Ban, true, reason, actor)
                    .await
            }
            Command::Unban {
                scope,
                account: target,
            } => {
                self.on_moderate(label, scope, target, ModKind::Ban, false, None, actor)
                    .await
            }
            Command::Kick {
                channel,
                account: target,
                reason,
            } => self.on_kick(label, channel, target, reason, actor).await,
            // §6.5 delegation: a federated admin re-delegates caps she holds
            // (`grant:<cap>`) to others — enforced as `Actor::Foreign` (§10.4).
            Command::Grant {
                subject,
                scope,
                caps,
                expiry,
            } => {
                self.on_grant(label, subject, scope, caps, expiry, actor)
                    .await
            }
            Command::Revoke {
                subject,
                scope,
                caps,
                epoch,
            } => {
                self.on_revoke(label, subject, scope, caps, epoch, actor)
                    .await
            }
            // §6.3 channel administration.
            Command::ChannelCreate {
                channel,
                policy,
                kind,
            } => {
                self.on_channel_create(label, channel, policy, kind, actor)
                    .await
            }
            Command::ChannelPolicy {
                channel,
                policy,
                purge,
            } => {
                self.on_channel_policy(label, channel, policy, purge, actor)
                    .await
            }
            Command::ChannelMeta {
                channel,
                key,
                value,
            } => {
                self.on_channel_meta(label, channel, key, value, actor)
                    .await
            }
            Command::ChannelDelete { channel, confirm } => {
                self.on_channel_delete(label, channel, confirm, actor).await
            }
            // §6.5 invites.
            Command::InviteMint {
                scope,
                max_uses,
                expiry,
            } => {
                self.on_invite_mint(label, scope, max_uses, expiry, actor)
                    .await
            }
            Command::InviteRevoke { invite_id } => {
                self.on_invite_revoke(label, invite_id, actor).await
            }
            // §6.5 role membership.
            Command::RoleAssign {
                scope,
                account: subject,
                name,
            } => {
                self.on_role_assign(label, scope, subject, name, actor)
                    .await
            }
            Command::RoleUnassign {
                scope,
                account: subject,
                name,
            } => {
                self.on_role_unassign(label, scope, subject, name, actor)
                    .await
            }
            // §6.2 namespace administration.
            Command::NsMeta { name, key, value } => {
                self.on_ns_meta(label, name, key, value, actor).await
            }
            Command::NsVisibility { name, visibility } => {
                self.on_ns_visibility(label, name, visibility, actor).await
            }
            Command::NsDelete { name, confirm } => {
                self.on_ns_delete(label, name, confirm, actor).await
            }
            Command::NsDelegate {
                name,
                subject,
                caps,
            } => {
                self.on_grant(label, subject, format!("ns:{name}"), caps, None, actor)
                    .await
            }
            // §6.7 report handling.
            Command::ReportsList {
                scope,
                status,
                cursor,
            } => {
                self.on_reports_list(label, scope, status, cursor, actor)
                    .await
            }
            Command::ReportsResolve {
                report_id,
                action,
                note,
            } => {
                self.on_reports_resolve(label, report_id, action, note, actor)
                    .await
            }
            Command::Ping { token } => {
                self.send_event(None, Event::Pong { token }).await?;
                Ok(Flow::Continue)
            }
            _ => {
                self.unsupported(label, "not yet available over a federation session")
                    .await
            }
        }
    }
}

// ---- ctx-level bridged ingestion (shared by the live bridge session and
// weftd's §11.7 backfill puller, which has no Session) ----

/// Map a bridged event to its storage record, enforcing origin authority
/// (invariant 2): the event and its root must originate on `peer`.
fn ingest_record(peer: &NetworkName, event: &Event) -> Option<(ChannelName, EventRecord)> {
    let channel_of = |t: &Target| match t {
        Target::Channel(c) => Some(c.clone()),
        _ => None, // DMs never bridge (§9.5)
    };
    let from_peer = |id: &MsgId| id.origin().as_str() == peer.as_str();
    match event {
        Event::Message(m) => {
            let channel = channel_of(&m.target)?;
            if !from_peer(&m.msgid) || m.sender.network.as_str() != peer.as_str() {
                return None;
            }
            let record = EventRecord {
                scope: Scope::Channel(channel.clone()),
                msgid: m.msgid.clone(),
                root: m.msgid.clone(),
                sender: m.sender.clone(),
                kind: EventKind::Message {
                    body: m.body.clone(),
                    meta: m.meta.clone(),
                },
            };
            Some((channel, record))
        }
        Event::Edited {
            target,
            user,
            msgid,
            edit_of,
            body,
        } => {
            let channel = channel_of(target)?;
            // The edit and the message it edits both belong to the origin.
            if !from_peer(msgid) || !from_peer(edit_of) {
                return None;
            }
            let record = EventRecord {
                scope: Scope::Channel(channel.clone()),
                msgid: msgid.clone(),
                root: edit_of.clone(),
                sender: user.clone(),
                kind: EventKind::Edit { body: body.clone() },
            };
            Some((channel, record))
        }
        Event::Deleted { target, msgid, by } => {
            let channel = channel_of(target)?;
            if !from_peer(msgid) {
                return None;
            }
            let sender = by
                .clone()
                .unwrap_or_else(|| UserRef::new(deleted_placeholder(), peer.clone()));
            let record = EventRecord {
                // A replica delete row needs its own id; the tombstone is keyed
                // on the root (`msgid`), which is what materialize uses — this
                // id is local bookkeeping only.
                scope: Scope::Channel(channel.clone()),
                msgid: MsgId::new(peer.clone(), Ulid::new()),
                root: msgid.clone(),
                sender,
                kind: EventKind::Delete,
            };
            Some((channel, record))
        }
        Event::Reaction {
            target,
            msgid,
            emoji,
            op,
            by,
        } => {
            let channel = channel_of(target)?;
            if !from_peer(msgid) {
                return None;
            }
            let record = EventRecord {
                scope: Scope::Channel(channel.clone()),
                msgid: MsgId::new(peer.clone(), Ulid::new()),
                root: msgid.clone(),
                sender: by.clone(),
                kind: EventKind::React {
                    emoji: emoji.clone(),
                    add: matches!(op, weft_proto::ReactionOp::Add),
                },
            };
            Some((channel, record))
        }
        _ => None,
    }
}

impl ServerCtx {
    /// §11.4 ingest one verified line from `peer` (a live bridge event, or a
    /// line pulled from a §11.7 backfill stream). Netblock-guarded (invariant
    /// 7), origin-authority-checked (invariant 2), manifest-gated (invariant 3),
    /// with foreign attachments mirrored (§11.8). A non-ingestible line (a batch
    /// frame, a non-origin event) is silently skipped, so feeding a whole backfill
    /// body through it is safe. `SessionId::MAX` origin ⇒ every member gets the
    /// broadcast copy (the puller is no session).
    pub async fn ingest_bridged(&self, peer: &NetworkName, line: &Line) {
        // §11.6 effect 3: a blocked network's events are rejected at ingestion
        // (a mid-session block takes effect at once, not just at auth).
        if self.netblocks.is_netblocked(peer).await.unwrap_or(false) {
            return;
        }
        let Ok(reply) = Reply::from_line(line) else {
            return;
        };
        // §10.3 a federated user's signed display profile (account-scoped, not
        // channel-scoped) — verify + store + mirror the avatar, then done.
        if let Event::Profile {
            user,
            display,
            avatar,
        } = &reply.event
        {
            self.ingest_profile(peer, line, user, display, avatar).await;
            return;
        }
        let Some((channel, record)) = ingest_record(peer, &reply.event) else {
            return;
        };
        let gated = self
            .peers
            .peer(peer)
            .await
            .ok()
            .flatten()
            .map(|p| bridge::is_forwardable(&p, channel.as_str()))
            .unwrap_or(false);
        if !gated {
            debug!(%peer, %channel, "dropped ingest: channel not in acked manifest");
            return;
        }
        // §11.8 mirror any foreign blob attachments the bridged message carries:
        // record the reference (so local members are gated + can fetch it once
        // present) and ask weftd to pull + verify + store the bytes.
        if let Event::Message(m) = &reply.event {
            self.mirror_attachments(peer, &channel, m).await;
        }
        if let Some(handle) = self.registry.get(&channel) {
            handle.ingest(u64::MAX, record, reply.event).await;
        }
    }

    /// §10.3 ingest a federated user's signed profile forwarded over the bridge:
    /// verify the `SignedProfile` against the peer's network key + that it covers
    /// exactly these fields, store it keyed by the `user@network` handle, and
    /// mirror the avatar blob (§11.8, BLAKE3-verified by weftd's fetcher). No
    /// local re-broadcast — the receiver's clients pull it via `PROFILES`.
    async fn ingest_profile(
        &self,
        peer: &NetworkName,
        line: &Line,
        user: &UserRef,
        display: &Option<String>,
        avatar: &Option<String>,
    ) {
        // The handle must belong to the forwarding peer (origin authority).
        if user.network.as_str() != peer.as_str() {
            return;
        }
        let Some(sig) = line.tags.get("sig") else {
            return;
        };
        let Ok(signed) = weft_crypto::SignedProfile::from_b64(sig) else {
            return;
        };
        let Some(peer_key) = self.peer_key(peer) else {
            return;
        };
        // Signed by the peer's network key AND covering exactly this profile.
        if !signed.signed_by(peer_key)
            || signed.profile.account != user.to_string()
            || signed.profile.display.as_deref() != display.as_deref()
            || signed.profile.avatar.as_deref() != avatar.as_deref()
        {
            return;
        }

        let handle = user.to_string();
        self.store_federated_profile(
            &handle,
            weft_store::ProfileRecord {
                display: display.clone(),
                avatar: avatar.clone(),
                updated: signed.profile.updated,
            },
        )
        .await;
        // Pull the avatar bytes so we can serve them locally (content-addressed,
        // BLAKE3-verified). The mirror consumer ignores the channel field.
        if let Some(hash) = avatar {
            self.request_mirror(crate::MirrorRequest {
                peer: peer.clone(),
                hash: hash.clone(),
                channel: "#avatars".parse().expect("valid placeholder channel"),
            });
        }
    }

    /// §11.8 for each **foreign** `weft-media://` attachment on a bridged message,
    /// record its reference in the mirrored channel and hand weftd a mirror pull.
    /// The peer's manifest `media` policy (+ `mirror-max` + the receiver
    /// blocklist) is enforced in weftd's mirror fetcher.
    async fn mirror_attachments(
        &self,
        peer: &NetworkName,
        channel: &ChannelName,
        msg: &MessageEvent,
    ) {
        let our = self.network_name();
        for uri in &msg.meta.attachments {
            let Some((origin, hash)) = crate::media::parse_media_uri(uri) else {
                continue;
            };
            // Local-origin blobs are already home; only foreign ones mirror.
            if origin == our {
                continue;
            }
            let hash = hash.to_string();
            let _ = self
                .media_refs
                .add_refs(
                    &weft_store::Scope::Channel(channel.clone()),
                    &msg.msgid,
                    std::slice::from_ref(&hash),
                )
                .await;
            self.request_mirror(crate::MirrorRequest {
                peer: peer.clone(),
                hash,
                channel: channel.clone(),
            });
        }
    }
}
