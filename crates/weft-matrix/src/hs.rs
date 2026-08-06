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

impl Hs {
    pub fn new(hs_url: &str, as_token: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: hs_url.trim_end_matches('/').to_string(),
            token: as_token.to_string(),
        }
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
        let v = self
            .call(
                reqwest::Method::POST,
                "/_matrix/client/v3/createRoom",
                Some(body),
                None,
                &[],
            )
            .await?;

        str_field(&v, "room_id")
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
            &format!(
                "/_matrix/client/v3/rooms/{}/state/{}/{}",
                enc(room_id),
                enc(event_type),
                enc(state_key)
            ),
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

        let res = req
            .send()
            .await
            .with_context(|| format!("{method} {path}"))?;
        let status = res.status();
        let v: Value = res.json().await.unwrap_or(Value::Null);

        if !status.is_success() {
            bail!(
                "{method} {path}: {status} {} {}",
                v["errcode"].as_str().unwrap_or(""),
                v["error"].as_str().unwrap_or("")
            );
        }

        Ok(v)
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
