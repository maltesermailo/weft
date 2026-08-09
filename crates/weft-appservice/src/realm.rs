//! Driving bridge traffic: the outbound half of a provider session.
//!
//! Everything here exists because getting it wrong is easy and silent. The rules
//! it encodes are in `docs/protocol/bridge-session-protocol.md`:
//!
//! - **The realm mints.** Namespace, channel and message ids are ours; weftd
//!   pins them. So we know an object's canonical name *before* asserting it, and
//!   the whole startup burst pipelines instead of assert-wait-remember per item.
//! - **`@as` names the actor, `@msgid` names the event.** A message-bearing line
//!   without `@msgid` is dropped by weftd with no error, so these methods take
//!   the id rather than letting a caller forget it.
//! - **Membership is stated, joins are requested.** We send `NS-MEMBER` as an
//!   authority; weftd sends us `NS JOIN` as a request. Not interchangeable.
//! - **Bans are ours to store and enforce.** weftd sends
//!   [`weft_proto::Event::Bridging`] once, when an operator bans a space in the
//!   admin panel, and keeps no record of it — so persist it, apply it on
//!   reconnect, and never expect a reminder. What "stop bridging" means is the
//!   adapter's to decide: leaving a Matrix room, ignoring a Discord guild,
//!   dropping a feed. That is why the instruction is generic.

use anyhow::anyhow;
use tokio::sync::mpsc;
use weft_proto::{Event, MemberAction, MsgId, Reply, Target};

/// What is optional about a replayed message (§9.3/§9.2): the reply root, the
/// media it carries, and the bridge label a relayed post is being answered with.
/// Private, because callers reach it through the named `message*` methods.
#[derive(Default)]
struct Post<'a> {
    /// A WEFT msgid this message replies to — the adapter resolves its own
    /// relation to it before calling.
    reply_to: Option<&'a str>,
    /// `weft-media://<hash>` references, already uploaded.
    attachments: Vec<String>,
    /// The label of the local post this is the realm's minted answer to.
    label: Option<&'a str>,
}

/// A handle for speaking as the realm on an authenticated provider session.
///
/// Cheap to clone — every clone writes to the same session.
#[derive(Clone)]
pub struct Realm {
    out: mpsc::Sender<String>,
    /// The **WEFT** network we are connected to, so [`Realm::is_ours`] can tell
    /// its users from the realm's.
    network: String,
}

impl Realm {
    pub(crate) fn new(out: mpsc::Sender<String>, network: String) -> Self {
        Self { out, network }
    }

    /// A detached realm whose lines land on the returned receiver instead of a
    /// session — for testing an adapter's logic without a weftd. Every adapter
    /// needs exactly this seam, so it lives here rather than being faked per
    /// adapter with a private-constructor workaround.
    pub fn capture(network: &str) -> (Self, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel(256);

        (Self::new(tx, network.to_string()), rx)
    }

    /// The WEFT network this session is connected to.
    pub fn network(&self) -> &str {
        &self.network
    }

    /// A [`crate::Ctx`] for answering a later step of an already-open flow —
    /// the first step arrives with one, the rest are correlated by view-id.
    pub fn ctx_for(&self, view_id: &str) -> crate::Ctx {
        crate::Ctx::new(view_id.to_string(), self.out.clone())
    }

    /// Is this user one of the connected network's, rather than one of ours?
    /// Membership statements cover both, so the distinction matters when
    /// deciding whether to mirror somebody into the foreign system.
    pub fn is_ours(&self, user: &weft_proto::UserRef) -> bool {
        user.network.as_str() != self.network
    }

    /// Bind this session to a realm (`REALM ASSERT`). Everything afterwards is
    /// scoped by it: the realm's name is the network its users live on and its
    /// events originate from, so it must be one weftd will accept — not its own
    /// name, not a peer's, and not a domain that publishes `/.well-known/weft`.
    pub async fn assert(&self, uri: &str) -> anyhow::Result<()> {
        self.send(
            weft_proto::Request::new(weft_proto::Command::RealmAssert {
                realm: uri.parse()?,
            })
            .serialize()?,
        )
        .await
    }

    /// Mint an id for something in this realm.
    ///
    /// A random ULID is fine, but deriving one deterministically from the foreign
    /// id is better: re-asserting after a weftd restore then reproduces the same
    /// namespace and channels instead of orphaning every stored reference.
    pub fn mint() -> String {
        weft_proto::Ulid::new().to_string().to_ascii_lowercase()
    }

    /// Assert a space (`NS-META`), or update one already asserted.
    ///
    /// Re-asserting is the **only** way to change a namespace you govern: weftd
    /// refuses local edits to it. Absent fields clear, so an assertion is the
    /// whole truth rather than a patch.
    pub async fn assert_namespace(&self, ns: &NamespaceAssertion<'_>) -> anyhow::Result<()> {
        let line = Reply::new(Event::NsMetaForeign {
            uri: ns.uri.parse()?,
            id: ns.id.parse()?,
            authority: ns.authority,
            settings_disabled: ns.settings_disabled.iter().map(|s| s.to_string()).collect(),
            visibility: ns.visibility,
            title: ns.title.map(str::to_string),
            description: ns.description.map(str::to_string),
            icon: ns.icon.map(str::to_string),
        })
        .to_line()?;

        self.send(line.serialize()?).await
    }

    /// Assert a room under an already-asserted space (`CHANNEL-LAYOUT`).
    ///
    /// Returns the canonical channel name — `#<ns-id>/<chan-id>` — which is how
    /// everything afterwards addresses it. It is computable here precisely
    /// because we minted both ids.
    pub async fn assert_channel(&self, chan: &ChannelAssertion<'_>) -> anyhow::Result<String> {
        let line = Reply::new(Event::ChannelLayoutForeign {
            uri: chan.uri.parse()?,
            id: chan.id.parse()?,
            position: chan.position,
            kind: chan.kind,
            vanity: chan.vanity.to_string(),
            category: chan.category.map(str::to_string),
        })
        .serialize()?;
        self.send(line).await?;

        Ok(format!("#{}/{}", chan.namespace_id, chan.id))
    }

    /// Replay a message from one of the realm's users.
    ///
    /// `msgid` is ours to mint (`<realm>/<ulid>`) and weftd pins it, so edits and
    /// reactions can reference it later. `sender` must live on this realm.
    pub async fn message(
        &self,
        sender: &str,
        msgid: &str,
        channel: &str,
        body: &str,
    ) -> anyhow::Result<()> {
        self.post(sender, msgid, channel, body, Post::default())
            .await
    }

    /// Replay a message, carrying the `label` of the relayed post it answers.
    ///
    /// The realm is the home for a replica channel, so a local user's post is
    /// relayed here and minted *there*; this is the copy that comes back and
    /// becomes canonical. Echoing the label is what lets weftd hand it to the
    /// waiting session as its own, so the poster's client reconciles the pending
    /// message instead of seeing a duplicate arrive from a stranger.
    pub async fn message_labeled(
        &self,
        sender: &str,
        msgid: &str,
        channel: &str,
        body: &str,
        label: Option<&str>,
    ) -> anyhow::Result<()> {
        self.post(
            sender,
            msgid,
            channel,
            body,
            Post {
                label,
                ..Post::default()
            },
        )
        .await
    }

    /// Replay a message that **replies to** one weftd already knows (§9.3). The
    /// root is a WEFT msgid: the adapter resolves the foreign relation to it, so
    /// the reply threads against the same message on both sides.
    pub async fn message_replying(
        &self,
        sender: &str,
        msgid: &str,
        channel: &str,
        body: &str,
        reply_to: &str,
    ) -> anyhow::Result<()> {
        self.post(
            sender,
            msgid,
            channel,
            body,
            Post {
                reply_to: Some(reply_to),
                ..Post::default()
            },
        )
        .await
    }

    /// Replay a **DM** from one of the realm's users to one of ours. Stored in
    /// the ordinary DM scope keyed by member keys — a bridged conversation is a
    /// first-class DM, not a second table.
    pub async fn dm(
        &self,
        sender: &str,
        msgid: &str,
        to_account: &str,
        body: &str,
    ) -> anyhow::Result<()> {
        let mut line = weft_proto::Request::new(weft_proto::Command::Msg {
            target: Target::User {
                account: to_account.parse()?,
                network: None,
            },
            body: Some(body.to_string()),
            meta: weft_proto::MsgMeta::default(),
        })
        .to_line()?;
        line.tags.insert("as".to_string(), sender.to_string());
        line.tags.insert("msgid".to_string(), msgid.to_string());

        self.send(line.serialize()?).await
    }

    /// Replay an edit. Carries its **own** minted id, since the edit is itself a
    /// stored event; `root` is the message being edited.
    pub async fn edit(
        &self,
        sender: &str,
        msgid: &str,
        root: &MsgId,
        body: &str,
    ) -> anyhow::Result<()> {
        let mut line = weft_proto::Request::new(weft_proto::Command::Edit {
            msgid: root.clone(),
            body: body.to_string(),
        })
        .to_line()?;
        line.tags.insert("as".to_string(), sender.to_string());
        line.tags.insert("msgid".to_string(), msgid.to_string());

        self.send(line.serialize()?).await
    }

    /// Replay a redaction. No id of its own — a tombstone is keyed on its root.
    pub async fn delete(&self, sender: &str, root: &MsgId) -> anyhow::Result<()> {
        let mut line = weft_proto::Request::new(weft_proto::Command::Delete {
            msgid: root.clone(),
        })
        .to_line()?;
        line.tags.insert("as".to_string(), sender.to_string());

        self.send(line.serialize()?).await
    }

    /// Replay a reaction (or its removal). No id of its own, as with a delete.
    /// §6.1 one of the realm's users changed presence.
    ///
    /// Attributed like everything else the realm replays (`@as`), and per-*user*:
    /// `PRESENCE` names no channel, so weftd fans it out to the channels this user
    /// shares with us. Ephemeral — nothing is stored, and a status for someone we
    /// share nothing with is simply dropped.
    pub async fn presence(
        &self,
        sender: &str,
        status: weft_proto::PresenceStatus,
    ) -> anyhow::Result<()> {
        let mut line =
            weft_proto::Request::new(weft_proto::Command::Presence { status }).to_line()?;
        line.tags.insert("as".to_string(), sender.to_string());

        self.send(line.serialize()?).await
    }

    pub async fn react(
        &self,
        sender: &str,
        root: &MsgId,
        emoji: &str,
        add: bool,
    ) -> anyhow::Result<()> {
        let cmd = if add {
            weft_proto::Command::React {
                msgid: root.clone(),
                emoji: emoji.to_string(),
            }
        } else {
            weft_proto::Command::Unreact {
                msgid: root.clone(),
                emoji: emoji.to_string(),
            }
        };
        let mut line = weft_proto::Request::new(cmd).to_line()?;
        line.tags.insert("as".to_string(), sender.to_string());

        self.send(line.serialize()?).await
    }

    /// Inject a foreign user's post into a **projected native** channel
    /// (protocol doc §5's outbound-projection door). The inversion from
    /// [`Realm::message`] is the point: **no msgid** — the home mints, and the
    /// minted `MESSAGE` returns on this session tagged with `label`, which is
    /// the ack (§3.5) and the only way to learn the id. Weftd refuses a
    /// carried msgid here, so this API cannot offer one.
    pub async fn inject_message(
        &self,
        sender: &str,
        channel: &str,
        body: &str,
        label: &str,
    ) -> anyhow::Result<()> {
        let mut line = weft_proto::Request::with_label(
            weft_proto::Command::Msg {
                target: Target::Channel(channel.parse()?),
                body: Some(body.to_string()),
                meta: weft_proto::MsgMeta::default(),
            },
            label,
        )
        .to_line()?;
        line.tags.insert("as".to_string(), sender.to_string());

        self.send(line.serialize()?).await
    }

    /// Inject a foreign user's edit of a **home-minted** root (projected
    /// path). As with [`Realm::inject_message`]: no own msgid — the home mints
    /// the edit row and echoes it back labeled.
    pub async fn inject_edit(
        &self,
        sender: &str,
        root: &MsgId,
        body: &str,
        label: &str,
    ) -> anyhow::Result<()> {
        let mut line = weft_proto::Request::with_label(
            weft_proto::Command::Edit {
                msgid: root.clone(),
                body: body.to_string(),
            },
            label,
        )
        .to_line()?;
        line.tags.insert("as".to_string(), sender.to_string());

        self.send(line.serialize()?).await
    }

    /// State that a user is (or is no longer) a member of a namespace we govern.
    ///
    /// This is an **authoritative statement**, and it covers the connected
    /// network's users too — say so once weftd's `NS JOIN` request has actually
    /// been honoured foreign-side, since that is what makes it true.
    pub async fn member(
        &self,
        namespace: &str,
        user: &str,
        action: MemberAction,
    ) -> anyhow::Result<()> {
        let line = Reply::new(Event::NsMember {
            namespace: namespace.parse()?,
            user: user.parse()?,
            action,
            display: None,
            count: None,
        })
        .serialize()?;

        self.send(line).await
    }

    /// Begin a **full-replace** membership statement.
    ///
    /// Between this and [`Realm::end_sync`], state the complete membership of
    /// every namespace you govern; at the end weftd drops anyone unnamed. This is
    /// how to correct drift after any gap — you already hold the whole set, so
    /// stating it beats diffing, and replaying it is idempotent.
    ///
    /// The opener is what makes it safe: without one, a stray end would name
    /// nobody and weftd ignores it rather than emptying your namespaces.
    pub async fn begin_sync(&self) -> anyhow::Result<()> {
        self.send(Reply::new(Event::SyncStart).serialize()?).await
    }

    /// Close a full-replace statement. Anyone not named since [`Realm::begin_sync`]
    /// stops being a member.
    pub async fn end_sync(&self, cursor: &str) -> anyhow::Result<()> {
        let mut line = Reply::new(Event::SyncEnd {
            cursor: cursor.to_string(),
        })
        .to_line()?;
        line.tags.insert("cursor".to_string(), cursor.to_string());

        self.send(line.serialize()?).await
    }

    /// A **foreign moderator's** grant (§10, slice 11): `actor` — a user of a
    /// foreign system whose power-level change this translates — wields the
    /// authority, and weftd honors it iff WEFT granted *them* `grant:<cap>`.
    /// Contrast [`Realm::grant`], which is the provider acting as the
    /// governing authority of its own replicas.
    pub async fn grant_as(
        &self,
        actor: &str,
        subject: &str,
        scope: &str,
        caps: &str,
        label: Option<&str>,
    ) -> anyhow::Result<()> {
        self.send_as(
            actor,
            label,
            weft_proto::Command::Grant {
                subject: subject.to_string(),
                scope: scope.to_string(),
                caps: caps.to_string(),
                expiry: None,
            },
        )
        .await
    }

    /// A foreign moderator's revoke — the demotion half of [`Realm::grant_as`].
    pub async fn revoke_as(
        &self,
        actor: &str,
        subject: &str,
        scope: &str,
        caps: Option<&str>,
        label: Option<&str>,
    ) -> anyhow::Result<()> {
        self.send_as(
            actor,
            label,
            weft_proto::Command::Revoke {
                subject: subject.to_string(),
                scope: scope.to_string(),
                caps: caps.map(str::to_string),
                epoch: None,
            },
        )
        .await
    }

    /// A foreign moderator's ban (or unban) at a scope — checked against the
    /// grants weftd holds for *them* (`Actor::Foreign`), like every attributed
    /// moderation act.
    ///
    /// `label` opts into §3.5 correlation: weftd echoes it on the direct
    /// response, **including `ERR`**, which is what lets an adapter revert the
    /// foreign-side change when the act is refused (§10). Pass `None` for
    /// fire-and-forget.
    pub async fn ban_as(
        &self,
        actor: &str,
        scope: &str,
        account: &str,
        reason: Option<&str>,
        ban: bool,
        label: Option<&str>,
    ) -> anyhow::Result<()> {
        let cmd = if ban {
            weft_proto::Command::Ban {
                scope: scope.to_string(),
                account: account.parse()?,
                reason: reason.map(str::to_string),
            }
        } else {
            weft_proto::Command::Unban {
                scope: scope.to_string(),
                account: account.parse()?,
            }
        };

        self.send_as(actor, label, cmd).await
    }

    /// A foreign moderator's kick from one channel.
    pub async fn kick_as(
        &self,
        actor: &str,
        channel: &str,
        account: &str,
        reason: Option<&str>,
        label: Option<&str>,
    ) -> anyhow::Result<()> {
        self.send_as(
            actor,
            label,
            weft_proto::Command::Kick {
                channel: channel.parse()?,
                account: account.parse()?,
                reason: reason.map(str::to_string),
            },
        )
        .await
    }

    /// §13 ask for a media upload grant. weftd answers `STREAM ACCEPT <token>`
    /// on the session (surfaced as an [`crate::Incoming::Event`]); the bytes
    /// then ride weftd's HTTP media plane, not this stream.
    pub async fn offer_media(&self, mime: &str, bytes: u64, label: &str) -> anyhow::Result<()> {
        self.send(
            weft_proto::Request::with_label(
                weft_proto::Command::StreamOffer {
                    mode: weft_proto::StreamMode::Media,
                    mime: mime.to_string(),
                    bytes,
                },
                label,
            )
            .serialize()?,
        )
        .await
    }

    /// Replay a message from one of the realm's users **with attachments** —
    /// `weft-media://<hash>` references obtained from an upload.
    pub async fn message_with_attachments(
        &self,
        sender: &str,
        msgid: &str,
        channel: &str,
        body: &str,
        attachments: Vec<String>,
    ) -> anyhow::Result<()> {
        self.post(
            sender,
            msgid,
            channel,
            body,
            Post {
                attachments,
                ..Post::default()
            },
        )
        .await
    }

    /// The one place a replayed `MSG` line is built: `@as` + `@msgid` are what make
    /// it an ingestion, and everything optional about it lives in [`Post`]. The
    /// public `message*` methods above are named entry points onto this — each
    /// exists because callers ask for one thing at a time, not because the line
    /// differs.
    async fn post(
        &self,
        sender: &str,
        msgid: &str,
        channel: &str,
        body: &str,
        opts: Post<'_>,
    ) -> anyhow::Result<()> {
        let mut line = weft_proto::Request::new(weft_proto::Command::Msg {
            target: Target::Channel(channel.parse()?),
            body: Some(body.to_string()),
            meta: weft_proto::MsgMeta {
                attachments: opts.attachments,
                reply_to: opts.reply_to.map(str::parse).transpose()?,
                ..weft_proto::MsgMeta::default()
            },
        })
        .to_line()?;
        line.tags.insert("as".to_string(), sender.to_string());
        line.tags.insert("msgid".to_string(), msgid.to_string());
        if let Some(label) = opts.label {
            line.tags.insert("label".to_string(), label.to_string());
        }

        self.send(line.serialize()?).await
    }

    /// Create a channel **as** a WEFT user (their `chan-create` capability is
    /// what weftd checks). For a *projected* namespace this is how a room is
    /// made: the channel is the real object, and the projection mirrors it.
    ///
    /// `policy` matters more than it looks: only a `permanent` channel projects
    /// (matrix.md §3, locked decision 2), so a create meant to appear on Matrix
    /// must say so — the namespace default is `retained:90d`.
    pub async fn create_channel_as(
        &self,
        actor: &str,
        namespace: &str,
        vanity: &str,
        policy: weft_proto::RetentionPolicy,
        label: Option<&str>,
    ) -> anyhow::Result<()> {
        self.send_as(
            actor,
            label,
            weft_proto::Command::ChannelCreate {
                channel: format!("#{namespace}/{vanity}").parse()?,
                policy: Some(policy),
                kind: weft_proto::ChannelKind::Text,
            },
        )
        .await
    }

    /// Set a namespace's metadata **as** a WEFT user (ns-admin is what weftd
    /// checks). The categories key is how a projected namespace's sub-spaces
    /// are created: weftd applies it and pushes the resulting `NS-META` back,
    /// which is what tells the adapter to build them.
    pub async fn set_ns_meta_as(
        &self,
        actor: &str,
        namespace: &str,
        key: &str,
        value: &str,
        label: Option<&str>,
    ) -> anyhow::Result<()> {
        self.send_as(
            actor,
            label,
            weft_proto::Command::NsMeta {
                ns: namespace.parse()?,
                key: key.to_string(),
                value: value.to_string(),
            },
        )
        .await
    }

    /// A foreign moderator's mute (or unmute) at a scope.
    pub async fn mute_as(
        &self,
        actor: &str,
        scope: &str,
        account: &str,
        reason: Option<&str>,
        mute: bool,
        label: Option<&str>,
    ) -> anyhow::Result<()> {
        let cmd = if mute {
            weft_proto::Command::Mute {
                scope: scope.to_string(),
                account: account.parse()?,
                reason: reason.map(str::to_string),
            }
        } else {
            weft_proto::Command::Unmute {
                scope: scope.to_string(),
                account: account.parse()?,
            }
        };

        self.send_as(actor, label, cmd).await
    }

    /// Grant capabilities in a namespace we govern — how a foreign moderator
    /// becomes one here. Translate the foreign model (a Matrix power level, a
    /// Discord role) into capabilities yourself: weftd has no notion of a level.
    /// Confirm a message of weftd's reached the foreign system (framework §7a).
    ///
    /// weftd's echo only acks its own storage; this is the half that says the realm
    /// has it. Answer **every** relayed message one way or the other — silence past
    /// weftd's grace window is reported to the author as a failure.
    pub async fn delivered(&self, msgid: &str) -> anyhow::Result<()> {
        self.send(
            weft_proto::Request::new(weft_proto::Command::Delivered {
                msgid: msgid.parse()?,
            })
            .serialize()?,
        )
        .await
    }

    /// The negative half: it could not be delivered and will not be retried.
    pub async fn undelivered(&self, msgid: &str, reason: &str) -> anyhow::Result<()> {
        self.send(
            weft_proto::Request::new(weft_proto::Command::Undelivered {
                msgid: msgid.parse()?,
                reason: Some(reason.to_string()),
            })
            .serialize()?,
        )
        .await
    }

    pub async fn grant(&self, subject: &str, scope: &str, caps: &str) -> anyhow::Result<()> {
        self.send(
            weft_proto::Request::new(weft_proto::Command::Grant {
                subject: subject.to_string(),
                scope: scope.to_string(),
                caps: caps.to_string(),
                expiry: None,
            })
            .serialize()?,
        )
        .await
    }

    /// Revoke capabilities. `caps = None` removes everything at the scope.
    pub async fn revoke(
        &self,
        subject: &str,
        scope: &str,
        caps: Option<&str>,
    ) -> anyhow::Result<()> {
        self.send(
            weft_proto::Request::new(weft_proto::Command::Revoke {
                subject: subject.to_string(),
                scope: scope.to_string(),
                caps: caps.map(str::to_string),
                epoch: None,
            })
            .serialize()?,
        )
        .await
    }

    /// Answer a `PROVISION` push: the space now exists (assert it first), or it
    /// cannot be provided. A waiting client is parked on `job` either way.
    pub async fn provisioned(&self, job: &str, ok: bool) -> anyhow::Result<()> {
        let cmd = if ok {
            weft_proto::Command::ProvisionOk {
                job: job.to_string(),
            }
        } else {
            weft_proto::Command::ProvisionErr {
                job: job.to_string(),
            }
        };

        self.send(weft_proto::Request::new(cmd).serialize()?).await
    }

    /// One attributed act, optionally labeled for §10 revert correlation.
    async fn send_as(
        &self,
        actor: &str,
        label: Option<&str>,
        cmd: weft_proto::Command,
    ) -> anyhow::Result<()> {
        let mut line = match label {
            Some(label) => weft_proto::Request::with_label(cmd, label).to_line()?,
            None => weft_proto::Request::new(cmd).to_line()?,
        };
        line.tags.insert("as".to_string(), actor.to_string());

        self.send(line.serialize()?).await
    }

    async fn send(&self, line: String) -> anyhow::Result<()> {
        self.out
            .send(line)
            .await
            .map_err(|_| anyhow!("connection closed"))
    }
}

/// A space to assert. Absent fields **clear** on re-assertion.
pub struct NamespaceAssertion<'a> {
    /// `<scheme>://<realm>/<space>`.
    pub uri: &'a str,
    /// The ULID we mint for it ([`Realm::mint`]).
    pub id: &'a str,
    pub visibility: weft_proto::Visibility,
    pub title: Option<&'a str>,
    pub description: Option<&'a str>,
    pub icon: Option<&'a str>,
    /// How a client should render authority here. `Levels` (Matrix) hides the
    /// native roles editor in favour of a surface you supply.
    pub authority: Option<weft_proto::Authority>,
    /// Native settings surfaces to hide: `roles`, `permissions`, `channels`,
    /// `invites`, `moderation`, `ns-edit`, `recovery`.
    pub settings_disabled: &'a [&'a str],
}

/// A room to assert under an already-asserted space.
pub struct ChannelAssertion<'a> {
    /// `<scheme>://<realm>/<space>/<room>` — its parent is this minus the last
    /// segment, so assert the space first.
    pub uri: &'a str,
    /// The ULID we mint for the channel.
    pub id: &'a str,
    /// The parent's id, so the canonical name can be returned.
    pub namespace_id: &'a str,
    pub vanity: &'a str,
    pub position: i64,
    pub kind: weft_proto::ChannelKind,
    pub category: Option<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn realm() -> (Realm, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel(16);
        (Realm::new(tx, "test.example".into()), rx)
    }

    #[tokio::test]
    async fn a_channel_name_is_known_before_the_reply_arrives() {
        // The point of minting: no assert-wait-remember round-trip.
        let (realm, mut rx) = realm();
        let ns = Realm::mint();
        let chan = Realm::mint();

        let name = realm
            .assert_channel(&ChannelAssertion {
                uri: "matrix://matrix.org/gaming/general",
                id: &chan,
                namespace_id: &ns,
                vanity: "general",
                position: 0,
                kind: weft_proto::ChannelKind::Text,
                category: None,
            })
            .await
            .expect("asserted");

        assert_eq!(name, format!("#{ns}/{chan}"));
        let line = rx.try_recv().expect("a line");
        assert!(line.contains(&format!("id={chan}")), "{line}");
        assert!(
            line.contains("CHANNEL-LAYOUT matrix://matrix.org/gaming/general 0"),
            "{line}"
        );
    }

    #[tokio::test]
    async fn a_replayed_message_carries_both_attribution_and_its_id() {
        // Missing `@msgid` is dropped by weftd with no error, so the API takes it.
        let (realm, mut rx) = realm();
        realm
            .message(
                "alice@matrix.org",
                "matrix.org/01arz3ndektsv4rrffq69g5fav",
                "#01arz3ndektsv4rrffq69g5fav/01arz3ndektsv4rrffq69g5faw",
                "hi",
            )
            .await
            .expect("sent");

        let line = rx.try_recv().expect("a line");
        assert!(line.contains("as=alice@matrix.org"), "{line}");
        assert!(
            line.contains("msgid=matrix.org/01arz3ndektsv4rrffq69g5fav"),
            "{line}"
        );
        // One tag group, semicolon-separated: a second `@` would parse as the
        // verb, which is the mistake this API exists to make impossible.
        assert!(line.starts_with('@'), "{line}");
        assert!(!line.contains(" @"), "tags must be one group: {line}");
    }

    #[tokio::test]
    async fn a_full_replace_window_is_framed() {
        let (realm, mut rx) = realm();
        realm.begin_sync().await.expect("began");
        realm
            .member(
                "01arz3ndektsv4rrffq69g5fav",
                "alice@matrix.org",
                MemberAction::Join,
            )
            .await
            .expect("stated");
        realm.end_sync("cursor-1").await.expect("ended");

        assert_eq!(rx.try_recv().unwrap(), "SYNC START");
        assert!(rx.try_recv().unwrap().contains("NS-MEMBER"));
        let end = rx.try_recv().unwrap();
        assert!(end.contains("cursor=cursor-1"), "{end}");
        assert!(end.contains("SYNC END"), "{end}");
    }

    #[test]
    fn our_users_are_told_apart_from_the_realms() {
        let (realm, _rx) = realm();
        assert!(realm.is_ours(&"alice@matrix.org".parse().unwrap()));
        assert!(!realm.is_ours(&"ada@test.example".parse().unwrap()));
    }
}
