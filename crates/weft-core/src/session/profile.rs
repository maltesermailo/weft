//! §10.3 display profiles: `PROFILE SET` (own nick + avatar) and `PROFILES`
//! (query). Profiles are stored per account and broadcast to co-members; the
//! home-network signing + bridge portability (§11.8 avatar mirror) is M-prof-5.

use super::*;

impl<S: ControlStream> Session<S> {
    /// `PROFILE SET` — partial-update the caller's own display name + avatar. A
    /// provided field replaces (empty = clear); an omitted field is left as-is.
    pub(super) async fn on_profile_set(
        &mut self,
        label: Option<String>,
        display: Option<String>,
        avatar: Option<String>,
        account: Account,
    ) -> io::Result<Flow> {
        let current = self
            .ctx
            .profiles
            .profile(account.as_str())
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let display = match display {
            Some(d) => (!d.is_empty()).then_some(d),
            None => current.display,
        };
        let avatar = match avatar {
            Some(a) => (!a.is_empty()).then_some(a),
            None => current.avatar,
        };
        // §2.3 display names ≤128 B.
        if display.as_ref().is_some_and(|d| d.len() > 128) {
            self.send_err(
                label,
                ErrCode::Malformed,
                None,
                "display name too long (≤128 B)",
            )
            .await?;
            return Ok(Flow::Continue);
        }

        let record = weft_store::ProfileRecord {
            display: display.clone(),
            avatar: avatar.clone(),
            updated: unix_now_ms(),
        };
        if let Err(e) = self
            .ctx
            .profiles
            .set_profile(account.as_str(), record)
            .await
        {
            return self.internal(label, &e).await;
        }

        let event = Event::Profile {
            user: UserRef::new(account.clone(), self.ctx.info.network.clone()),
            display,
            avatar,
        };
        // Labeled ack to the setter; broadcast to co-members in every channel
        // they're in (attributed to this session so the setter's own copy is
        // skipped — the `VOICE STATE` / `SetPolicy` pattern).
        self.send_event(label, event.clone()).await?;
        for joined in self.joined.values() {
            joined.handle.announce_as(self.id, event.clone()).await;
        }
        Ok(Flow::Continue)
    }

    /// `PROFILES <account>...` — answer a `PROFILE` per known account. Absent
    /// accounts are silently omitted (a profile is not secret, but neither is
    /// its absence worth an error).
    pub(super) async fn on_profiles_query(
        &mut self,
        label: Option<String>,
        accounts: Vec<String>,
    ) -> io::Result<Flow> {
        let found = self
            .ctx
            .profiles
            .profiles(&accounts)
            .await
            .unwrap_or_default();
        for (handle, record) in found {
            // A federated handle (`user@network`) is a `UserRef`; a local profile
            // keys by bare account name and is qualified with our own network.
            let user = if let Ok(user) = handle.parse::<UserRef>() {
                user
            } else if let Ok(account) = handle.parse::<Account>() {
                UserRef::new(account, self.ctx.info.network.clone())
            } else {
                continue;
            };
            self.send_event(
                label.clone(),
                Event::Profile {
                    user,
                    display: record.display,
                    avatar: record.avatar,
                },
            )
            .await?;
        }
        Ok(Flow::Continue)
    }
}
