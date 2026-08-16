//! The companion-homeserver client: the ~10 client-server API calls the MVP
//! needs, spoken as an appservice (`Authorization: Bearer <as_token>`, puppet
//! impersonation via `?user_id=`).
//!
//! Deliberately thin — reqwest + serde_json bodies, ruma only for identifiers
//! and event payloads. matrix-sdk was considered and rejected (client-framework
//! mismatch, e2ee dead weight; see `docs/architecture/matrix.md` decision 1).

use anyhow::{anyhow, bail, Context as _};
use serde_json::{json, Value};

#[derive(Clone)]
pub struct Hs {
    http: reqwest::Client,
    base: String,
    token: String,
}

/// How long one homeserver call may take before it is a failure.
///
/// Every call here is awaited *inline in the dispatch loop*, so an untimed request
/// does not slow the bridge down — it stops it completely, and invisibly: the
/// appservice session lives in its own task and keeps answering weftd's liveness
/// PING, so weftd goes on advertising this provider as online and relaying DMs and
/// room renames into a loop that will never come back to read them. A generous
/// ceiling (`/messages` backfill and a first `join` are genuinely slow) that still
/// turns a hang into a logged error the loop survives.
const HS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

impl Hs {
    pub fn new(hs_url: &str, as_token: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(HS_TIMEOUT)
                .build()
                .expect("a reqwest client with only a timeout set always builds"),
            base: hs_url.trim_end_matches('/').to_string(),
            token: as_token.to_string(),
        }
    }

    /// Tombstone a room: it is closed, with **no successor**.
    ///
    /// `replacement_room` is empty on purpose. The spec has it name the room that
    /// continues the conversation, and there is none — the projection promise that
    /// justified this room existing is gone, so nothing should carry it forward.
    /// Clients read the tombstone and stop offering to post.
    pub async fn tombstone(&self, room_id: &str, body: &str) -> anyhow::Result<()> {
        self.put_state(
            room_id,
            "m.room.tombstone",
            "",
            json!({ "body": body, "replacement_room": "" }),
        )
        .await
    }

    /// Publish a room in this server's **public room directory**.
    ///
    /// Distinct from `createRoom`'s `visibility`, which only applies at creation:
    /// this repairs a room that already exists. Projection is meant to be found,
    /// and a Space created before the flag existed (or created unlisted) would
    /// otherwise stay invisible to anyone browsing the server forever.
    pub async fn publish_room(&self, room_id: &str) -> anyhow::Result<()> {
        self.call(
            reqwest::Method::PUT,
            &format!("/_matrix/client/v3/directory/list/room/{}", enc(room_id)),
            Some(json!({ "visibility": "public" })),
            None,
            &[],
        )
        .await?;

        Ok(())
    }

    /// Point a room alias at `room_id` in this server's directory.
    ///
    /// Separate from the alias `create_room` mints: that one is the room's stable
    /// identity, this publishes an *additional*, human-typeable name for it.
    pub async fn set_alias(&self, alias: &str, room_id: &str) -> anyhow::Result<()> {
        self.call(
            reqwest::Method::PUT,
            &format!("/_matrix/client/v3/directory/room/{}", enc(alias)),
            Some(json!({ "room_id": room_id })),
            None,
            &[],
        )
        .await?;

        Ok(())
    }

    /// Remove an alias mapping. Used when a vanity changes: the old name must stop
    /// resolving, or two aliases claim to be the same namespace.
    pub async fn delete_alias(&self, alias: &str) -> anyhow::Result<()> {
        self.call(
            reqwest::Method::DELETE,
            &format!("/_matrix/client/v3/directory/room/{}", enc(alias)),
            None,
            None,
            &[],
        )
        .await?;

        Ok(())
    }

    /// Resolve a room alias: `(room_id, servers to join via)`.
    pub async fn resolve_alias(&self, alias: &str) -> anyhow::Result<(String, Vec<String>)> {
        let v = self
            .call(
                reqwest::Method::GET,
                &format!("/_matrix/client/v3/directory/room/{}", enc(alias)),
                None,
                None,
                &[],
            )
            .await?;

        let room_id = str_field(&v, "room_id")?;
        let servers = v["servers"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        Ok((room_id, servers))
    }

    /// Join a room (by id or alias), optionally as a puppet. Returns the room id.
    pub async fn join(
        &self,
        room: &str,
        via: &[String],
        as_user: Option<&str>,
    ) -> anyhow::Result<String> {
        let query: Vec<(String, String)> = via
            .iter()
            .map(|s| ("server_name".to_string(), s.clone()))
            .collect();
        let v = self
            .call(
                reqwest::Method::POST,
                &format!("/_matrix/client/v3/join/{}", enc(room)),
                Some(json!({})),
                as_user,
                &query,
            )
            .await?;

        str_field(&v, "room_id")
    }

    /// A room's full current state.
    pub async fn state(&self, room_id: &str) -> anyhow::Result<Vec<Value>> {
        let v = self
            .call(
                reqwest::Method::GET,
                &format!("/_matrix/client/v3/rooms/{}/state", enc(room_id)),
                None,
                None,
                &[],
            )
            .await?;

        v.as_array()
            .cloned()
            .ok_or_else(|| anyhow!("state was not an array"))
    }

    /// Send a timeline event, optionally as a puppet. Returns the event id.
    ///
    /// `txn_id` is the idempotency key — derive it from the WEFT msgid so a
    /// crash-and-retry cannot double-post.
    pub async fn send(
        &self,
        room_id: &str,
        event_type: &str,
        content: Value,
        txn_id: &str,
        as_user: Option<&str>,
    ) -> anyhow::Result<String> {
        let v = self
            .call(
                reqwest::Method::PUT,
                &format!(
                    "/_matrix/client/v3/rooms/{}/send/{}/{}",
                    enc(room_id),
                    enc(event_type),
                    enc(txn_id)
                ),
                Some(content),
                as_user,
                &[],
            )
            .await?;

        str_field(&v, "event_id")
    }

    /// Redact an event, optionally as a puppet. Returns the redaction's id.
    pub async fn redact(
        &self,
        room_id: &str,
        event_id: &str,
        reason: Option<&str>,
        txn_id: &str,
        as_user: Option<&str>,
    ) -> anyhow::Result<String> {
        let body = match reason {
            Some(reason) => json!({ "reason": reason }),
            None => json!({}),
        };
        let v = self
            .call(
                reqwest::Method::PUT,
                &format!(
                    "/_matrix/client/v3/rooms/{}/redact/{}/{}",
                    enc(room_id),
                    enc(event_id),
                    enc(txn_id)
                ),
                Some(body),
                as_user,
                &[],
            )
            .await?;

        str_field(&v, "event_id")
    }

    /// Create a room as the bot. `body` is the raw createRoom payload — the
    /// projection engine owns the shape (space vs room, alias, presets).
    pub async fn create_room(&self, body: Value) -> anyhow::Result<String> {
        self.create_room_as(body, None).await
    }

    /// Create a room as a **puppet** — a DM belongs to the two people in it,
    /// not to the bridge bot.
    pub async fn create_room_as(
        &self,
        body: Value,
        as_user: Option<&str>,
    ) -> anyhow::Result<String> {
        let v = self
            .call(
                reqwest::Method::POST,
                "/_matrix/client/v3/createRoom",
                Some(body),
                as_user,
                &[],
            )
            .await?;

        str_field(&v, "room_id")
    }

    /// One state event's content, or `None` if absent (M_NOT_FOUND).
    pub async fn get_state(
        &self,
        room_id: &str,
        event_type: &str,
        state_key: &str,
    ) -> anyhow::Result<Option<Value>> {
        match self
            .call(
                reqwest::Method::GET,
                &state_path(room_id, event_type, state_key),
                None,
                None,
                &[],
            )
            .await
        {
            Ok(v) => Ok(Some(v)),
            // Absent state is a 404 (M_NOT_FOUND) — an answer, not a failure.
            Err(e) if e.to_string().contains(" 404 ") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Typing notification, as a puppet (§15). `timeout_ms` is how long the
    /// indicator lives; `0` with `typing: false` clears it.
    pub async fn typing(
        &self,
        room_id: &str,
        as_user: &str,
        typing: bool,
        timeout_ms: u64,
    ) -> anyhow::Result<()> {
        self.call(
            reqwest::Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/typing/{}",
                enc(room_id),
                enc(as_user)
            ),
            Some(json!({ "typing": typing, "timeout": timeout_ms })),
            Some(as_user),
            &[],
        )
        .await?;

        Ok(())
    }

    /// §6.1 set a puppet's presence, mirroring what its WEFT account announced.
    ///
    /// `status_msg` is left unset: WEFT's away/dnd carry no text, and inventing one
    /// would put words on the user's profile. Requires `presence: enabled: true` on
    /// the homeserver — with presence off Synapse answers 404/403 here and the call
    /// is a no-op, which is why the caller only logs a failure.
    pub async fn set_presence(&self, as_user: &str, presence: &str) -> anyhow::Result<()> {
        self.call(
            reqwest::Method::PUT,
            &format!("/_matrix/client/v3/presence/{}/status", enc(as_user)),
            Some(json!({ "presence": presence })),
            Some(as_user),
            &[],
        )
        .await?;

        Ok(())
    }

    /// §6.1 read a user's **current** presence.
    ///
    /// The counterpart to the `m.presence` EDU, which only ever reports a
    /// *change*. Someone who was already online when we connected never
    /// generates one, so the live stream alone can never answer "who is here".
    ///
    /// `None` when the homeserver has nothing to give: presence disabled (403),
    /// or a user it holds no status for (404). Neither is an error — it is the
    /// ordinary answer on a server that runs with presence off.
    pub async fn presence_of(&self, mxid: &str) -> anyhow::Result<Option<String>> {
        match self
            .call(
                reqwest::Method::GET,
                &format!("/_matrix/client/v3/presence/{}/status", enc(mxid)),
                None,
                None,
                &[],
            )
            .await
        {
            Ok(v) => Ok(v["presence"].as_str().map(String::from)),
            Err(e) if e.to_string().contains(" 404 ") || e.to_string().contains(" 403 ") => {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Set a state event as the bot (`m.space.child`, power levels, …).
    pub async fn put_state(
        &self,
        room_id: &str,
        event_type: &str,
        state_key: &str,
        content: Value,
    ) -> anyhow::Result<()> {
        self.call(
            reqwest::Method::PUT,
            &state_path(room_id, event_type, state_key),
            Some(content),
            None,
            &[],
        )
        .await?;

        Ok(())
    }

    /// Leave a room, optionally as a puppet.
    pub async fn leave(&self, room_id: &str, as_user: Option<&str>) -> anyhow::Result<()> {
        self.call(
            reqwest::Method::POST,
            &format!("/_matrix/client/v3/rooms/{}/leave", enc(room_id)),
            Some(json!({})),
            as_user,
            &[],
        )
        .await?;

        Ok(())
    }

    /// Download a blob by its `mxc://server/id`, through the companion
    /// homeserver's **authenticated** media endpoint. Returns the bytes and
    /// whatever content type the server reported.
    pub async fn download_mxc(&self, mxc: &str) -> anyhow::Result<(Vec<u8>, String)> {
        let rest = mxc
            .strip_prefix("mxc://")
            .ok_or_else(|| anyhow!("not an mxc uri: {mxc}"))?;
        let (server, id) = rest
            .split_once('/')
            .ok_or_else(|| anyhow!("malformed mxc uri: {mxc}"))?;

        let res = self
            .http
            .get(format!(
                "{}/_matrix/client/v1/media/download/{}/{}",
                self.base,
                enc(server),
                enc(id)
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("downloading media")?;
        let status = res.status();
        let mime = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = res.bytes().await.context("reading media body")?;

        anyhow::ensure!(status.is_success(), "media download failed: {status}");

        Ok((bytes.to_vec(), mime))
    }

    /// Upload a blob to the companion homeserver's media repo. Returns its
    /// `mxc://` — remote homeservers fetch from here, which is what makes a
    /// projected attachment work over ordinary Matrix media federation (§12).
    pub async fn upload_media(
        &self,
        bytes: Vec<u8>,
        mime: &str,
        filename: &str,
    ) -> anyhow::Result<String> {
        let res = self
            .http
            .post(format!("{}/_matrix/media/v3/upload", self.base))
            .bearer_auth(&self.token)
            .header(reqwest::header::CONTENT_TYPE, mime)
            .query(&[("filename", filename)])
            .body(bytes)
            .send()
            .await
            .context("uploading media")?;
        let status = res.status();
        let v: Value = res.json().await.unwrap_or(Value::Null);

        anyhow::ensure!(status.is_success(), "media upload failed: {status} {v}");

        str_field(&v, "content_uri")
    }

    /// Every room the bot has joined — the entry point for recovery: the
    /// homeserver knows what we bridge even when our own store does not.
    pub async fn joined_rooms(&self) -> anyhow::Result<Vec<String>> {
        let v = self
            .call(
                reqwest::Method::GET,
                "/_matrix/client/v3/joined_rooms",
                None,
                None,
                &[],
            )
            .await?;

        Ok(v["joined_rooms"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|r| r.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Read the bot's account data — where the adapter keeps the decisions it
    /// cannot rebuild from anywhere else.
    pub async fn account_data(&self, user: &str, kind: &str) -> anyhow::Result<Option<Value>> {
        match self
            .call(
                reqwest::Method::GET,
                &format!(
                    "/_matrix/client/v3/user/{}/account_data/{}",
                    enc(user),
                    enc(kind)
                ),
                None,
                None,
                &[],
            )
            .await
        {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.to_string().contains(" 404 ") => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn set_account_data(
        &self,
        user: &str,
        kind: &str,
        content: Value,
    ) -> anyhow::Result<()> {
        self.call(
            reqwest::Method::PUT,
            &format!(
                "/_matrix/client/v3/user/{}/account_data/{}",
                enc(user),
                enc(kind)
            ),
            Some(content),
            None,
            &[],
        )
        .await?;

        Ok(())
    }

    /// One event by id — the on-demand half of link recovery: a mutation that
    /// names an event we have no row for is resolved by reading that event,
    /// which carries the WEFT msgid we stamped on it.
    pub async fn event(&self, room_id: &str, event_id: &str) -> anyhow::Result<Option<Value>> {
        match self
            .call(
                reqwest::Method::GET,
                &format!(
                    "/_matrix/client/v3/rooms/{}/event/{}",
                    enc(room_id),
                    enc(event_id)
                ),
                None,
                None,
                &[],
            )
            .await
        {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.to_string().contains(" 404 ") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// A pagination token positioned **at** an event — Matrix's `/messages`
    /// pages from a token, not an event id, so a backfill anchored on a known
    /// message has to resolve one first.
    pub async fn token_at_event(
        &self,
        room_id: &str,
        event_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let v = self
            .call(
                reqwest::Method::GET,
                &format!(
                    "/_matrix/client/v3/rooms/{}/context/{}",
                    enc(room_id),
                    enc(event_id)
                ),
                None,
                None,
                &[("limit".to_string(), "0".to_string())],
            )
            .await?;

        Ok(v["start"].as_str().map(String::from))
    }

    /// One page of a room's timeline, walking **backwards**. `from` is a token
    /// ([`Self::token_at_event`]); `None` starts at the live end. Returns the
    /// events newest-first, as Matrix does.
    pub async fn messages_back(
        &self,
        room_id: &str,
        from: Option<&str>,
        limit: u32,
    ) -> anyhow::Result<Vec<Value>> {
        let mut query = vec![
            ("dir".to_string(), "b".to_string()),
            ("limit".to_string(), limit.to_string()),
        ];
        if let Some(from) = from {
            query.push(("from".to_string(), from.to_string()));
        }

        let v = self
            .call(
                reqwest::Method::GET,
                &format!("/_matrix/client/v3/rooms/{}/messages", enc(room_id)),
                None,
                None,
                &query,
            )
            .await?;

        Ok(v["chunk"].as_array().cloned().unwrap_or_default())
    }

    /// Kick or ban a user from a room, as the bot (§9: bridge-created rooms are
    /// bridge-controlled, and a **foreign** member's membership is the realm's
    /// to state — so removing them happens here, not over the WEFT wire).
    pub async fn remove_member(
        &self,
        room_id: &str,
        mxid: &str,
        reason: Option<&str>,
        ban: bool,
    ) -> anyhow::Result<()> {
        let mut body = serde_json::Map::new();
        body.insert("user_id".into(), json!(mxid));
        if let Some(reason) = reason {
            body.insert("reason".into(), json!(reason));
        }

        self.call(
            reqwest::Method::POST,
            &format!(
                "/_matrix/client/v3/rooms/{}/{}",
                enc(room_id),
                if ban { "ban" } else { "kick" }
            ),
            Some(Value::Object(body)),
            None,
            &[],
        )
        .await?;

        Ok(())
    }

    /// Unban a user (the §10 revert of a refused ban).
    pub async fn unban(&self, room_id: &str, mxid: &str) -> anyhow::Result<()> {
        self.call(
            reqwest::Method::POST,
            &format!("/_matrix/client/v3/rooms/{}/unban", enc(room_id)),
            Some(json!({ "user_id": mxid })),
            None,
            &[],
        )
        .await?;

        Ok(())
    }

    /// Invite a user to a room, optionally as a puppet.
    pub async fn invite(
        &self,
        room_id: &str,
        mxid: &str,
        as_user: Option<&str>,
    ) -> anyhow::Result<()> {
        self.call(
            reqwest::Method::POST,
            &format!("/_matrix/client/v3/rooms/{}/invite", enc(room_id)),
            Some(json!({ "user_id": mxid })),
            as_user,
            &[],
        )
        .await?;

        Ok(())
    }

    /// Set a puppet's display name.
    ///
    /// Two reasons this is not cosmetic: Matrix users would otherwise see the
    /// raw `@weft_<ulid>` as the sender, and it is where the account label
    /// lives on the Matrix side — the ULID in the localpart is the identity, the
    /// display name is what recovery reads the *name* back from.
    pub async fn set_display_name(&self, mxid: &str, name: &str) -> anyhow::Result<()> {
        self.call(
            reqwest::Method::PUT,
            &format!("/_matrix/client/v3/profile/{}/displayname", enc(mxid)),
            Some(json!({ "displayname": name })),
            Some(mxid),
            &[],
        )
        .await?;

        Ok(())
    }

    /// Register a puppet. Idempotent: `M_USER_IN_USE` means it already exists,
    /// which is success — the appservice namespace makes it ours either way.
    pub async fn ensure_registered(&self, localpart: &str) -> anyhow::Result<()> {
        let body = json!({
            "type": "m.login.application_service",
            "username": localpart,
            "inhibit_login": true,
        });

        match self
            .call(
                reqwest::Method::POST,
                "/_matrix/client/v3/register",
                Some(body),
                None,
                &[],
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("M_USER_IN_USE") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// One CS-API call. Non-2xx becomes an error carrying the Matrix `errcode`
    /// so callers can branch on it (`M_USER_IN_USE`, …).
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        as_user: Option<&str>,
        query: &[(String, String)],
    ) -> anyhow::Result<Value> {
        let mut req = self
            .http
            .request(method.clone(), format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .query(query);
        if let Some(user) = as_user {
            req = req.query(&[("user_id", user)]);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }

        // Name the host we called, not just the path. Without it a failure reads as
        // if the bridge had dialled whatever appears in the path — a remote server
        // named inside an alias, say — when in fact every request here goes to our
        // own companion homeserver, and it is the homeserver's federation that
        // failed on our behalf.
        let res = req
            .send()
            .await
            .with_context(|| format!("{method} {}{path}", self.base))?;
        let status = res.status();
        let v: Value = res.json().await.unwrap_or(Value::Null);

        if !status.is_success() {
            bail!(
                "{method} {base}{path}: {status} {} {}",
                v["errcode"].as_str().unwrap_or(""),
                v["error"].as_str().unwrap_or(""),
                base = self.base,
            );
        }

        Ok(v)
    }
}

/// A state event's path. An empty state key omits its segment — a trailing
/// slash is legal on real homeservers but trips strict routers.
fn state_path(room_id: &str, event_type: &str, state_key: &str) -> String {
    let base = format!(
        "/_matrix/client/v3/rooms/{}/state/{}",
        enc(room_id),
        enc(event_type)
    );

    if state_key.is_empty() {
        base
    } else {
        format!("{base}/{}", enc(state_key))
    }
}

fn enc(s: &str) -> String {
    // Path segments: Matrix ids carry `!#$:@` — everything but unreserved
    // characters is percent-encoded, byte by byte.
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn str_field(v: &Value, key: &str) -> anyhow::Result<String> {
    v[key]
        .as_str()
        .map(String::from)
        .ok_or_else(|| anyhow!("response missing `{key}`: {v}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_ids_survive_path_encoding() {
        assert_eq!(enc("#gaming:matrix.org"), "%23gaming%3Amatrix.org");
        assert_eq!(enc("!abc:kde.org"), "%21abc%3Akde.org");
        assert_eq!(enc("$ev/il:x"), "%24ev%2Fil%3Ax");
    }
}
