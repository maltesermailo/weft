//! The daemon's persisted state.
//!
//! What must survive a restart, and why:
//!
//! - **The structure maps** (space ↔ namespace, room ↔ channel): weftd pins
//!   the ids we minted, so losing these would orphan the replica.
//!   (Structure ids are *also* derivable — `ident::stable_ulid` — so this is
//!   a cache with a safety net, but the room↔channel direction is not.)
//! - **The event links** ([`Links`]): edits, reactions and redactions on
//!   either side address the other side's id through them.
//! - **The ban list**: weftd tells us once and never again
//!   (bridge-session-protocol §11) — an unpersisted ban silently resumes a
//!   banned space on restart.
//! - **Registered puppets**: registration is idempotent anyway; remembering
//!   just saves a round trip per sender.
//!
//! Storage is **Postgres** (owner directive 2026-08-06), `matrix_`-prefixed
//! tables so the daemon can share weftd's database or bring its own. The shape
//! is a write-through cache: the daemon is single-tasked with one writer, so
//! [`State`] in memory is the read path and every mutation goes through a
//! [`Store`] method that also writes its row. A failed write is a loud warning
//! rather than a dead bridge — traffic keeps flowing, and the worst case is a
//! link lost across a restart.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context as _;
use sqlx::Row as _;
use tracing::warn;
use weft_appservice::BanList;
use weft_proto::MemberAction;

/// A consumed Matrix space and the namespace it bridges to.
#[derive(Debug, Default, Clone)]
pub struct Space {
    pub ns_id: String,
    pub room_id: String,
    pub uri: String,
    /// The space's rooms: room_id → its channel entry.
    pub rooms: BTreeMap<String, Room>,
    /// Which mapped rooms each remote user has joined — the input to the §8
    /// membership mapping (see [`Space::member_joined`]).
    pub member_rooms: BTreeMap<String, BTreeSet<String>>,
}

impl Space {
    /// §8 membership mapping, join half: the user's **first** mapped-room join
    /// is the namespace join — every later one changes nothing weftd-side.
    pub fn member_joined(&mut self, user: &str, room_id: &str) -> Option<MemberAction> {
        let rooms = self.member_rooms.entry(user.to_string()).or_default();
        let first = rooms.is_empty();
        rooms.insert(room_id.to_string());

        first.then_some(MemberAction::Join)
    }

    /// §8, leave half: leaving the **last** joined room is the namespace leave.
    pub fn member_left(&mut self, user: &str, room_id: &str) -> Option<MemberAction> {
        let rooms = self.member_rooms.get_mut(user)?;
        rooms.remove(room_id);

        if rooms.is_empty() {
            self.member_rooms.remove(user);
            return Some(MemberAction::Part);
        }
        None
    }
}

#[derive(Debug, Clone)]
pub struct Room {
    pub chan_id: String,
    /// Canonical WEFT name: `#<ns-id>/<chan-id>`.
    pub channel: String,
    pub uri: String,
}

/// Where a WEFT msgid lives on the Matrix side.
#[derive(Debug, Clone)]
pub struct EventRef {
    pub room: String,
    pub event: String,
}

/// The bidirectional event_id ↔ msgid map, kept in step by construction:
/// the fields are private, so every write goes through [`Links::link`] and the
/// two directions cannot drift apart.
#[derive(Debug, Default)]
pub struct Links {
    /// Matrix event id → WEFT msgid.
    events: BTreeMap<String, String>,
    /// WEFT msgid → where it lives on the Matrix side.
    msgids: BTreeMap<String, EventRef>,
}

impl Links {
    pub fn link(&mut self, event_id: &str, msgid: &str, room_id: &str) {
        let msgid = canonical_msgid(msgid);
        self.events.insert(event_id.to_string(), msgid.clone());
        self.msgids.insert(
            msgid,
            EventRef {
                room: room_id.to_string(),
                event: event_id.to_string(),
            },
        );
    }

    pub fn msgid_of(&self, event_id: &str) -> Option<&str> {
        self.events.get(event_id).map(String::as_str)
    }

    /// How many links are held — the console's "how much do I remember".
    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn event_of(&self, msgid: &str) -> Option<&EventRef> {
        self.msgids.get(&canonical_msgid(msgid))
    }
}

/// A msgid in the one form the map is keyed by.
///
/// The same id reaches us in two spellings: the **wire** form we mint
/// (lowercase, `ident::msgid_for`) and the **canonical** form
/// `MsgId::to_string()` produces (uppercase ULID) when it arrives on an event.
/// Keying on whatever a caller happened to hold split one message across two
/// entries — so a WEFT reaction to an ingested Matrix message looked up the
/// canonical spelling and missed the lowercase one it was stored under, and
/// never reached Matrix. Normalizing here fixes every call site at once.
fn canonical_msgid(msgid: &str) -> String {
    msgid
        .parse::<weft_proto::MsgId>()
        .map(|id| id.to_string())
        .unwrap_or_else(|_| msgid.to_string())
}

/// One reaction, fully named — `(root, key, by)` as an anonymous tuple let a
/// swapped `key`/`by` compile silently.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Reaction {
    /// The reacted-to message's WEFT msgid.
    pub root: String,
    /// The annotation key (the emoji — an **arbitrary** string per Matrix).
    pub key: String,
    /// The reacting WEFT user.
    pub by: String,
}

/// The `m.reaction` events we sent for WEFT-side reactions, so an unreact can
/// redact exactly its own annotation.
///
/// Keyed by the full [`Reaction`] — the earlier `"root|key|by"` string key was
/// a latent bug: a Matrix annotation key may itself contain `|`, so two
/// distinct reactions could collide.
#[derive(Debug, Default, Clone)]
pub struct SentReactions(BTreeMap<Reaction, String>);

impl SentReactions {
    /// Remember the annotation event we just sent for this reaction.
    pub fn note(&mut self, reaction: Reaction, event_id: String) {
        self.0.insert(reaction, event_id);
    }

    /// The annotation event to redact for this reaction, forgetting it.
    pub fn take(&mut self, reaction: &Reaction) -> Option<String> {
        self.0.remove(reaction)
    }
}

/// One of our users, as the bridge knows them.
#[derive(Debug, Clone)]
pub struct LocalUser {
    /// The current account name — a **mutable vanity label**, display only.
    pub account: String,
    /// The registered puppet localpart, derived from the ULID — never from
    /// the name, so a rename cannot orphan the puppet.
    pub localpart: String,
}

/// Local users keyed by account ULID (the stable identity), with a name index
/// for the paths that only see the wire name (fan-out events). Kept in step by
/// construction: private maps, one writer ([`LocalUsers::note`]).
#[derive(Debug, Default)]
pub struct LocalUsers {
    by_ulid: BTreeMap<String, LocalUser>,
    by_account: BTreeMap<String, String>,
}

impl LocalUsers {
    /// Record (or re-record) a user. A changed account name moves the name
    /// index — the ULID row is the identity, the name just re-points.
    pub fn note(&mut self, ulid: &str, account: &str, localpart: &str) {
        if let Some(user) = self.by_ulid.get(ulid) {
            if user.account != account {
                self.by_account.remove(&user.account);
            }
        }

        self.by_account
            .insert(account.to_string(), ulid.to_string());
        self.by_ulid.insert(
            ulid.to_string(),
            LocalUser {
                account: account.to_string(),
                localpart: localpart.to_string(),
            },
        );
    }

    pub fn by_ulid(&self, ulid: &str) -> Option<&LocalUser> {
        self.by_ulid.get(ulid)
    }

    /// Resolve a wire name. Only for the paths with no ULID on the line —
    /// misses mean the user never reached this bridge via a membership relay.
    pub fn by_account(&self, account: &str) -> Option<(&str, &LocalUser)> {
        let ulid = self.by_account.get(account)?;
        self.by_ulid.get(ulid).map(|user| (ulid.as_str(), user))
    }

    /// Resolve a puppet localpart back to its user — for Matrix events whose
    /// *target* is one of our puppets (a mod banning a WEFT user). Linear:
    /// the map holds this bridge's own audience, never the world.
    pub fn by_localpart(&self, localpart: &str) -> Option<(&str, &LocalUser)> {
        self.by_ulid
            .iter()
            .find(|(_, u)| u.localpart == localpart)
            .map(|(ulid, u)| (ulid.as_str(), u))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &LocalUser)> {
        self.by_ulid.iter()
    }
}

/// A WEFT namespace mirrored as a Matrix Space (outbound projection) — the
/// inverse of [`Space`], which is consumed foreign structure.
#[derive(Debug, Default, Clone)]
pub struct Projection {
    pub space_room: String,
    /// WEFT channel name → the projected Matrix room.
    pub rooms: BTreeMap<String, String>,
    /// WEFT category → its sub-space room (matrix.md §6): a category holds its
    /// channels' rooms, so a channel's parent is its category's sub-space when
    /// it has one, and the top Space otherwise.
    pub categories: BTreeMap<String, String>,
    /// The namespace's category list **as weftd declares it**, in order — the
    /// authority for any edit, since `NS META categories` is a full replace.
    /// Refreshed by every `NS-META` push, so it needs no persistence: the push
    /// precedes any flow on a fresh session.
    pub declared_categories: Vec<String>,
    /// §8 in the outbound sense: which projected rooms each **Matrix** user
    /// has joined — first join ⇒ NS-MEMBER join statement, last leave ⇒ part.
    pub member_rooms: BTreeMap<String, BTreeSet<String>>,
}

impl Projection {
    pub fn member_joined(&mut self, user: &str, room_id: &str) -> Option<MemberAction> {
        let rooms = self.member_rooms.entry(user.to_string()).or_default();
        let first = rooms.is_empty();
        rooms.insert(room_id.to_string());

        first.then_some(MemberAction::Join)
    }

    pub fn member_left(&mut self, user: &str, room_id: &str) -> Option<MemberAction> {
        let rooms = self.member_rooms.get_mut(user)?;
        rooms.remove(room_id);

        if rooms.is_empty() {
            self.member_rooms.remove(user);
            return Some(MemberAction::Part);
        }
        None
    }
}

#[derive(Debug, Default)]
pub struct State {
    /// space URI (`matrix://realm/space`) → the bridged space.
    pub spaces: BTreeMap<String, Space>,
    /// WEFT ns-id → its outbound projection.
    pub projections: BTreeMap<String, Projection>,
    /// event_id ↔ msgid, both directions of traffic.
    pub links: Links,
    /// Our users with puppets, keyed by account ULID.
    pub users: LocalUsers,
    /// A remote `m.reaction`'s event id → the reaction it made, so its
    /// redaction can become the matching `UNREACT`.
    pub reactions: BTreeMap<String, Reaction>,
    /// Last-seen `m.room.power_levels` users map per room — the diff baseline.
    pub room_levels: BTreeMap<String, BTreeMap<String, i64>>,
    /// Bridged DMs: `(our account, their MXID)` → the Matrix DM room.
    pub dm_rooms: BTreeMap<(String, String), String>,
    pub sent_reactions: SentReactions,
    pub bans: BanList,
}

impl State {
    /// The bridged room behind a WEFT channel name, with its space.
    pub fn room_of_channel(&self, channel: &str) -> Option<(&str, &Space)> {
        for space in self.spaces.values() {
            for (room_id, room) in &space.rooms {
                if room.channel == channel {
                    return Some((room_id, space));
                }
            }
        }
        None
    }

    /// The channel behind a Matrix room id, with its space.
    pub fn channel_of_room(&self, room_id: &str) -> Option<(&Room, &Space)> {
        self.spaces
            .values()
            .find_map(|space| space.rooms.get(room_id).map(|room| (room, space)))
    }

    /// The DM pair behind a Matrix room, if it is a bridged conversation.
    pub fn dm_of_room(&self, room_id: &str) -> Option<(&str, &str)> {
        self.dm_rooms
            .iter()
            .find(|(_, room)| room.as_str() == room_id)
            .map(|((account, mxid), _)| (account.as_str(), mxid.as_str()))
    }

    pub fn space_of_ns(&self, ns_id: &str) -> Option<&Space> {
        self.spaces.values().find(|s| s.ns_id == ns_id)
    }

    /// The projected Matrix room behind a WEFT channel, with its ns-id.
    pub fn projected_room_of_channel(&self, channel: &str) -> Option<(&str, &str)> {
        self.projections.iter().find_map(|(ns_id, p)| {
            p.rooms
                .get(channel)
                .map(|room| (ns_id.as_str(), room.as_str()))
        })
    }

    /// The WEFT channel behind a projected Matrix room, with its ns-id.
    pub fn channel_of_projected_room(&self, room_id: &str) -> Option<(&str, &str)> {
        self.projections.iter().find_map(|(ns_id, p)| {
            p.rooms
                .iter()
                .find(|(_, r)| r.as_str() == room_id)
                .map(|(chan, _)| (chan.as_str(), ns_id.as_str()))
        })
    }
}

pub struct Store {
    /// `None` = ephemeral (tests): the in-memory state without durability.
    /// The production constructor is [`Store::connect`], which always has one.
    pool: Option<sqlx::PgPool>,
    pub state: State,
}

/// A write that failed only loses durability, not the running bridge: warn
/// loudly and keep the traffic flowing. The worst case across a restart is a
/// lost link, which degrades one message's edits — a dead bridge loses all.
async fn best_effort<T, E: std::fmt::Display>(
    what: &str,
    write: impl std::future::Future<Output = Result<T, E>>,
) {
    if let Err(e) = write.await {
        warn!("state write failed ({what}): {e:#}");
    }
}

impl Store {
    /// The live pool, when connected — for tests that need raw table access.
    pub fn pool(&self) -> Option<&sqlx::PgPool> {
        self.pool.as_ref()
    }

    /// Ephemeral store for tests — no pool, no durability.
    pub fn in_memory() -> Self {
        Self {
            pool: None,
            state: State::default(),
        }
    }

    /// Connect, migrate, and load the whole working set. The daemon is a
    /// single writer, so memory is the read path from here on.
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            // Fail fast on an unreachable DB instead of hanging silently.
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(url)
            .await
            .context("connecting to Postgres")?;
        // Idempotent DDL rather than `sqlx::migrate!`: the migrator's
        // `_sqlx_migrations` table is global per database, and weftd already
        // owns one — sharing its database (the supported deployment) would
        // make the two migration sets reject each other's checksums.
        sqlx::raw_sql(include_str!("../migrations/0001_init.sql"))
            .execute(&pool)
            .await
            .context("creating weft-matrix tables")?;

        let state = Self::load(&pool).await?;

        Ok(Self {
            pool: Some(pool),
            state,
        })
    }

    async fn load(pool: &sqlx::PgPool) -> anyhow::Result<State> {
        let mut state = State::default();

        for row in sqlx::query("SELECT uri, ns_id, room_id FROM matrix_spaces")
            .fetch_all(pool)
            .await?
        {
            let uri: String = row.get("uri");
            state.spaces.insert(
                uri.clone(),
                Space {
                    ns_id: row.get("ns_id"),
                    room_id: row.get("room_id"),
                    uri,
                    ..Space::default()
                },
            );
        }

        for row in sqlx::query("SELECT room_id, space_uri, chan_id, channel, uri FROM matrix_rooms")
            .fetch_all(pool)
            .await?
        {
            let space_uri: String = row.get("space_uri");
            if let Some(space) = state.spaces.get_mut(&space_uri) {
                space.rooms.insert(
                    row.get("room_id"),
                    Room {
                        chan_id: row.get("chan_id"),
                        channel: row.get("channel"),
                        uri: row.get("uri"),
                    },
                );
            }
        }

        for row in sqlx::query("SELECT space_uri, member, room_id FROM matrix_member_rooms")
            .fetch_all(pool)
            .await?
        {
            let space_uri: String = row.get("space_uri");
            if let Some(space) = state.spaces.get_mut(&space_uri) {
                space
                    .member_rooms
                    .entry(row.get("member"))
                    .or_default()
                    .insert(row.get("room_id"));
            }
        }

        for row in sqlx::query("SELECT event_id, msgid, room_id FROM matrix_links")
            .fetch_all(pool)
            .await?
        {
            let (event_id, msgid, room_id): (String, String, String) =
                (row.get("event_id"), row.get("msgid"), row.get("room_id"));
            state.links.link(&event_id, &msgid, &room_id);
        }

        for row in sqlx::query("SELECT ulid, account, localpart FROM matrix_users")
            .fetch_all(pool)
            .await?
        {
            let (ulid, account, localpart): (String, String, String) =
                (row.get("ulid"), row.get("account"), row.get("localpart"));
            state.users.note(&ulid, &account, &localpart);
        }

        for row in sqlx::query("SELECT event_id, root, key, sender FROM matrix_reactions")
            .fetch_all(pool)
            .await?
        {
            state.reactions.insert(
                row.get("event_id"),
                Reaction {
                    root: row.get("root"),
                    key: row.get("key"),
                    by: row.get("sender"),
                },
            );
        }

        for row in sqlx::query("SELECT root, key, sender, event_id FROM matrix_sent_reactions")
            .fetch_all(pool)
            .await?
        {
            state.sent_reactions.note(
                Reaction {
                    root: row.get("root"),
                    key: row.get("key"),
                    by: row.get("sender"),
                },
                row.get("event_id"),
            );
        }

        for row in sqlx::query("SELECT ns_id, space_room FROM matrix_projections")
            .fetch_all(pool)
            .await?
        {
            let ns_id: String = row.get("ns_id");
            state.projections.insert(
                ns_id,
                crate::store::Projection {
                    space_room: row.get("space_room"),
                    ..Projection::default()
                },
            );
        }

        for row in sqlx::query("SELECT channel, ns_id, room_id FROM matrix_projected_rooms")
            .fetch_all(pool)
            .await?
        {
            let ns_id: String = row.get("ns_id");
            if let Some(p) = state.projections.get_mut(&ns_id) {
                p.rooms.insert(row.get("channel"), row.get("room_id"));
            }
        }

        for row in
            sqlx::query("SELECT ns_id, category, space_room FROM matrix_projected_categories")
                .fetch_all(pool)
                .await?
        {
            let ns_id: String = row.get("ns_id");
            if let Some(p) = state.projections.get_mut(&ns_id) {
                p.categories
                    .insert(row.get("category"), row.get("space_room"));
            }
        }

        for row in sqlx::query("SELECT account, mxid, room_id FROM matrix_dm_rooms")
            .fetch_all(pool)
            .await?
        {
            state
                .dm_rooms
                .insert((row.get("account"), row.get("mxid")), row.get("room_id"));
        }

        for row in sqlx::query("SELECT room_id, mxid, level FROM matrix_room_levels")
            .fetch_all(pool)
            .await?
        {
            let room: String = row.get("room_id");
            state
                .room_levels
                .entry(room)
                .or_default()
                .insert(row.get("mxid"), row.get("level"));
        }

        let banned: Vec<String> = sqlx::query_scalar("SELECT ns_id FROM matrix_bans")
            .fetch_all(pool)
            .await?;
        state.bans = banned.into_iter().collect();

        Ok(state)
    }

    // ---- write-through mutators (memory first, then the row) ---------------

    /// Insert or wholesale-replace a provisioned space: its row, its rooms and
    /// its member set, transactionally (re-provisioning is a full re-statement).
    pub async fn save_space(&mut self, space: Space) {
        if let Some(pool) = self.pool.clone() {
            best_effort("save_space", async {
                let mut tx = pool.begin().await?;

                // DELETE + reinsert: the FK cascade clears rooms and members,
                // so the row set exactly mirrors the assertion.
                sqlx::query("DELETE FROM matrix_spaces WHERE uri = $1")
                    .bind(&space.uri)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("INSERT INTO matrix_spaces (uri, ns_id, room_id) VALUES ($1, $2, $3)")
                    .bind(&space.uri)
                    .bind(&space.ns_id)
                    .bind(&space.room_id)
                    .execute(&mut *tx)
                    .await?;

                for (room_id, room) in &space.rooms {
                    sqlx::query(
                        "INSERT INTO matrix_rooms (room_id, space_uri, chan_id, channel, uri) \
                         VALUES ($1, $2, $3, $4, $5)",
                    )
                    .bind(room_id)
                    .bind(&space.uri)
                    .bind(&room.chan_id)
                    .bind(&room.channel)
                    .bind(&room.uri)
                    .execute(&mut *tx)
                    .await?;
                }

                for (member, rooms) in &space.member_rooms {
                    for room_id in rooms {
                        sqlx::query(
                            "INSERT INTO matrix_member_rooms (space_uri, member, room_id) \
                             VALUES ($1, $2, $3)",
                        )
                        .bind(&space.uri)
                        .bind(member)
                        .bind(room_id)
                        .execute(&mut *tx)
                        .await?;
                    }
                }

                tx.commit().await.map_err(anyhow::Error::from)
            })
            .await;
        }

        self.state.spaces.insert(space.uri.clone(), space);
    }

    pub async fn link(&mut self, event_id: &str, msgid: &str, room_id: &str) {
        self.state.links.link(event_id, msgid, room_id);

        if let Some(pool) = &self.pool {
            best_effort(
                "link",
                sqlx::query(
                    "INSERT INTO matrix_links (event_id, msgid, room_id) VALUES ($1, $2, $3) \
                     ON CONFLICT (event_id) DO NOTHING",
                )
                .bind(event_id)
                .bind(msgid)
                .bind(room_id)
                .execute(pool),
            )
            .await;
        }
    }

    /// Record a local user (their puppet is registered by the caller first).
    pub async fn note_user(&mut self, ulid: &str, account: &str, localpart: &str) {
        self.state.users.note(ulid, account, localpart);

        if let Some(pool) = &self.pool {
            best_effort(
                "note_user",
                sqlx::query(
                    "INSERT INTO matrix_users (ulid, account, localpart) VALUES ($1, $2, $3) \
                     ON CONFLICT (ulid) DO UPDATE SET account = $2, localpart = $3",
                )
                .bind(ulid)
                .bind(account)
                .bind(localpart)
                .execute(pool),
            )
            .await;
        }
    }

    pub async fn reaction_add(&mut self, event_id: &str, reaction: Reaction) {
        if let Some(pool) = &self.pool {
            best_effort(
                "reaction_add",
                sqlx::query(
                    "INSERT INTO matrix_reactions (event_id, root, key, sender) \
                     VALUES ($1, $2, $3, $4) ON CONFLICT (event_id) DO NOTHING",
                )
                .bind(event_id)
                .bind(&reaction.root)
                .bind(&reaction.key)
                .bind(&reaction.by)
                .execute(pool),
            )
            .await;
        }

        self.state.reactions.insert(event_id.to_string(), reaction);
    }

    pub async fn reaction_take(&mut self, event_id: &str) -> Option<Reaction> {
        let taken = self.state.reactions.remove(event_id);

        if taken.is_some() {
            if let Some(pool) = &self.pool {
                best_effort(
                    "reaction_take",
                    sqlx::query("DELETE FROM matrix_reactions WHERE event_id = $1")
                        .bind(event_id)
                        .execute(pool),
                )
                .await;
            }
        }

        taken
    }

    pub async fn sent_note(&mut self, reaction: Reaction, event_id: String) {
        if let Some(pool) = &self.pool {
            best_effort(
                "sent_note",
                sqlx::query(
                    "INSERT INTO matrix_sent_reactions (root, key, sender, event_id) \
                     VALUES ($1, $2, $3, $4) \
                     ON CONFLICT (root, key, sender) DO UPDATE SET event_id = $4",
                )
                .bind(&reaction.root)
                .bind(&reaction.key)
                .bind(&reaction.by)
                .bind(&event_id)
                .execute(pool),
            )
            .await;
        }

        self.state.sent_reactions.note(reaction, event_id);
    }

    pub async fn sent_take(&mut self, reaction: &Reaction) -> Option<String> {
        let taken = self.state.sent_reactions.take(reaction);

        if taken.is_some() {
            if let Some(pool) = &self.pool {
                best_effort(
                    "sent_take",
                    sqlx::query(
                        "DELETE FROM matrix_sent_reactions \
                         WHERE root = $1 AND key = $2 AND sender = $3",
                    )
                    .bind(&reaction.root)
                    .bind(&reaction.key)
                    .bind(&reaction.by)
                    .execute(pool),
                )
                .await;
            }
        }

        taken
    }

    /// Persist one member-room delta (the in-memory transition already ran via
    /// [`Space::member_joined`]/[`Space::member_left`]).
    pub async fn persist_member_room(
        &mut self,
        space_uri: &str,
        member: &str,
        room_id: &str,
        joined: bool,
    ) {
        let Some(pool) = &self.pool else { return };

        if joined {
            best_effort(
                "member_room join",
                sqlx::query(
                    "INSERT INTO matrix_member_rooms (space_uri, member, room_id) \
                     VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
                )
                .bind(space_uri)
                .bind(member)
                .bind(room_id)
                .execute(pool),
            )
            .await;
        } else {
            best_effort(
                "member_room leave",
                sqlx::query(
                    "DELETE FROM matrix_member_rooms \
                     WHERE space_uri = $1 AND member = $2 AND room_id = $3",
                )
                .bind(space_uri)
                .bind(member)
                .bind(room_id)
                .execute(pool),
            )
            .await;
        }
    }

    /// Record a projected Space (in memory + its row).
    pub async fn save_projection(&mut self, ns_id: &str, space_room: &str) {
        self.state
            .projections
            .entry(ns_id.to_string())
            .or_default()
            .space_room = space_room.to_string();

        if let Some(pool) = &self.pool {
            best_effort(
                "save_projection",
                sqlx::query(
                    "INSERT INTO matrix_projections (ns_id, space_room) VALUES ($1, $2) \
                     ON CONFLICT (ns_id) DO UPDATE SET space_room = $2",
                )
                .bind(ns_id)
                .bind(space_room)
                .execute(pool),
            )
            .await;
        }
    }

    /// Record a projected room under an already-projected Space.
    pub async fn save_projected_room(&mut self, ns_id: &str, channel: &str, room_id: &str) {
        self.state
            .projections
            .entry(ns_id.to_string())
            .or_default()
            .rooms
            .insert(channel.to_string(), room_id.to_string());

        if let Some(pool) = &self.pool {
            best_effort(
                "save_projected_room",
                sqlx::query(
                    "INSERT INTO matrix_projected_rooms (channel, ns_id, room_id) \
                     VALUES ($1, $2, $3) \
                     ON CONFLICT (channel) DO UPDATE SET ns_id = $2, room_id = $3",
                )
                .bind(channel)
                .bind(ns_id)
                .bind(room_id)
                .execute(pool),
            )
            .await;
        }
    }

    /// Forget a projected room — its channel stopped qualifying (matrix.md §3:
    /// `permanent → anything else`). The Matrix room is tombstoned separately;
    /// dropping the mapping is what lets a later return to `permanent` create a
    /// *fresh* room rather than resurrect a dead one.
    pub async fn drop_projected_room(&mut self, ns_id: &str, channel: &str) {
        if let Some(p) = self.state.projections.get_mut(ns_id) {
            p.rooms.remove(channel);
        }

        if let Some(pool) = &self.pool {
            best_effort(
                "drop_projected_room",
                sqlx::query("DELETE FROM matrix_projected_rooms WHERE channel = $1")
                    .bind(channel)
                    .execute(pool),
            )
            .await;
        }
    }

    /// Record a category's sub-space.
    pub async fn save_projected_category(&mut self, ns_id: &str, category: &str, room: &str) {
        self.state
            .projections
            .entry(ns_id.to_string())
            .or_default()
            .categories
            .insert(category.to_string(), room.to_string());

        if let Some(pool) = &self.pool {
            best_effort(
                "save_projected_category",
                sqlx::query(
                    "INSERT INTO matrix_projected_categories (ns_id, category, space_room) \
                     VALUES ($1, $2, $3) \
                     ON CONFLICT (ns_id, category) DO UPDATE SET space_room = $3",
                )
                .bind(ns_id)
                .bind(category)
                .bind(room)
                .execute(pool),
            )
            .await;
        }
    }

    /// Remember a bridged DM's room.
    pub async fn save_dm_room(&mut self, account: &str, mxid: &str, room: &str) {
        self.state
            .dm_rooms
            .insert((account.to_string(), mxid.to_string()), room.to_string());

        if let Some(pool) = &self.pool {
            best_effort(
                "save_dm_room",
                sqlx::query(
                    "INSERT INTO matrix_dm_rooms (account, mxid, room_id) VALUES ($1, $2, $3) \
                     ON CONFLICT (account, mxid) DO UPDATE SET room_id = $3",
                )
                .bind(account)
                .bind(mxid)
                .bind(room)
                .execute(pool),
            )
            .await;
        }
    }

    /// Forget a bridged DM's room, because the peer left it.
    ///
    /// Reusing a room the other party walked out of is how a conversation dies
    /// silently: the puppet is still joined, so every relay succeeds and lands
    /// somewhere nobody is reading. Dropping the mapping makes the next DM open a
    /// fresh room and invite them again. The old room and its history stay on the
    /// Matrix side untouched — that is what leaving means.
    pub async fn forget_dm_room(&mut self, account: &str, mxid: &str) {
        self.state
            .dm_rooms
            .remove(&(account.to_string(), mxid.to_string()));

        if let Some(pool) = &self.pool {
            best_effort(
                "forget_dm_room",
                sqlx::query("DELETE FROM matrix_dm_rooms WHERE account = $1 AND mxid = $2")
                    .bind(account)
                    .bind(mxid)
                    .execute(pool),
            )
            .await;
        }
    }

    /// Replace a room's power-level baseline (after translating a PL event).
    pub async fn set_room_levels(&mut self, room_id: &str, users: BTreeMap<String, i64>) {
        if let Some(pool) = self.pool.clone() {
            let users = users.clone();
            let room = room_id.to_string();
            best_effort("set_room_levels", async move {
                let mut tx = pool.begin().await?;
                sqlx::query("DELETE FROM matrix_room_levels WHERE room_id = $1")
                    .bind(&room)
                    .execute(&mut *tx)
                    .await?;
                for (mxid, level) in &users {
                    sqlx::query(
                        "INSERT INTO matrix_room_levels (room_id, mxid, level) VALUES ($1, $2, $3)",
                    )
                    .bind(&room)
                    .bind(mxid)
                    .bind(level)
                    .execute(&mut *tx)
                    .await?;
                }
                tx.commit().await.map_err(anyhow::Error::from)
            })
            .await;
        }

        self.state.room_levels.insert(room_id.to_string(), users);
    }

    /// Apply a `BRIDGING` instruction: the ban list update plus its row —
    /// weftd never re-sends a ban, so the row IS the enforcement across
    /// restarts (bridge-session-protocol §11).
    pub async fn apply_bridging(&mut self, event: &weft_proto::Event) -> Option<(String, bool)> {
        let (ns, banned) = self.state.bans.apply(event)?;

        if let Some(pool) = &self.pool {
            if banned {
                best_effort(
                    "ban",
                    sqlx::query(
                        "INSERT INTO matrix_bans (ns_id) VALUES ($1) ON CONFLICT DO NOTHING",
                    )
                    .bind(&ns)
                    .execute(pool),
                )
                .await;
            } else {
                best_effort(
                    "unban",
                    sqlx::query("DELETE FROM matrix_bans WHERE ns_id = $1")
                        .bind(&ns)
                        .execute(pool),
                )
                .await;
            }
        }

        Some((ns, banned))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups_resolve_both_directions() {
        let mut state = State::default();
        let mut space = Space {
            ns_id: "nsid".into(),
            room_id: "!space:kde.org".into(),
            uri: "matrix://kde.org/community".into(),
            ..Space::default()
        };
        space.rooms.insert(
            "!gen:kde.org".into(),
            Room {
                chan_id: "chanid".into(),
                channel: "#nsid/chanid".into(),
                uri: "matrix://kde.org/community/general".into(),
            },
        );
        state.spaces.insert(space.uri.clone(), space);
        state.links.link("$ev1", "kde.org/01abc", "!gen:kde.org");

        let (room_id, space) = state.room_of_channel("#nsid/chanid").unwrap();
        assert_eq!(room_id, "!gen:kde.org");
        assert_eq!(space.ns_id, "nsid");
        let (room, _) = state.channel_of_room("!gen:kde.org").unwrap();
        assert_eq!(room.channel, "#nsid/chanid");

        let at = state.links.event_of("kde.org/01abc").unwrap();
        assert_eq!(
            (at.room.as_str(), at.event.as_str()),
            ("!gen:kde.org", "$ev1")
        );
        assert_eq!(state.links.msgid_of("$ev1"), Some("kde.org/01abc"));

        // The structured reaction key tolerates `|` inside the annotation key —
        // the string-composed key this replaced collided on it.
        let mut sent = SentReactions::default();
        let weird = Reaction {
            root: "kde.org/01abc".into(),
            key: "🔥|weird|key".into(),
            by: "ada@test.example".into(),
        };
        sent.note(weird.clone(), "$r1".into());
        assert_eq!(sent.take(&weird).as_deref(), Some("$r1"));
    }

    #[test]
    fn a_msgid_is_keyed_the_same_however_it_is_spelled() {
        // The wire form we mint is lowercase; `MsgId::to_string()` is uppercase.
        // Both must find the one entry, or a reaction to an ingested message
        // silently never reaches the foreign side.
        let mut links = Links::default();
        let lower = "kde.org/01arz3ndektsv4rrffq69g5fav";
        let upper = "kde.org/01ARZ3NDEKTSV4RRFFQ69G5FAV";

        links.link("$ev", lower, "!room:kde.org");
        assert!(
            links.event_of(lower).is_some(),
            "the spelling it was stored as"
        );
        assert!(links.event_of(upper).is_some(), "and the canonical one");
        assert_eq!(links.msgid_of("$ev"), Some(upper), "stored canonically");
    }

    #[test]
    fn membership_transitions_fire_on_first_join_and_last_leave() {
        // §8: the namespace join/leave is the *transition*, not every room op.
        let mut space = Space::default();

        assert_eq!(
            space.member_joined("carol@kde.org", "!a"),
            Some(MemberAction::Join),
            "first room join IS the namespace join"
        );
        assert_eq!(space.member_joined("carol@kde.org", "!b"), None);

        assert_eq!(space.member_left("carol@kde.org", "!a"), None);
        assert_eq!(
            space.member_left("carol@kde.org", "!b"),
            Some(MemberAction::Part),
            "leaving the last room IS the namespace leave"
        );

        // Unknown users and re-leaves are no-ops, not phantom parts.
        assert_eq!(space.member_left("carol@kde.org", "!b"), None);
        assert_eq!(space.member_left("nobody@kde.org", "!a"), None);
    }

    #[test]
    fn a_rename_moves_the_name_index_but_keeps_the_puppet() {
        // Account names are mutable vanity labels; the ULID is the identity.
        let mut users = LocalUsers::default();
        users.note("01hxulid", "ada", "weft_01hxulid");

        users.note("01hxulid", "adalovelace", "weft_01hxulid");
        let (ulid, user) = users.by_account("adalovelace").expect("new name resolves");
        assert_eq!(ulid, "01hxulid");
        assert_eq!(user.localpart, "weft_01hxulid", "the puppet never changes");
        assert!(
            users.by_account("ada").is_none(),
            "the old name no longer resolves"
        );
    }
}
