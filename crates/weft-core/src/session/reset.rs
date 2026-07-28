//! §6.1 password reset via a mailed one-time code — the "forgot my password"
//! path, so it runs while **UNAUTHED**.
//!
//! `RESET REQUEST <email>` mails a code (reusing the §10.5 code store + `Mailer`
//! port, keyed `(account, "reset")`) and answers with a **uniform** `RESET-SENT`
//! whether or not the email is known — the response never reveals which
//! (anti-enumeration, §2.2/§8). `RESET CONFIRM <email> <code> :<new-password>`
//! sets the new password; an unknown email and a wrong code fail identically
//! (`ERR FORBIDDEN bad-code`). Success leaves the session unauthenticated — the
//! client then `AUTH`s with the new password.

use super::*;

/// A reset code is valid for 15 minutes (same window as §10.5 verification).
const RESET_CODE_TTL_MS: u64 = 15 * 60 * 1000;

impl<S: ControlStream> Session<S> {
    /// `RESET REQUEST <email>` — mail a reset code iff the email is registered;
    /// always answer with the same `RESET-SENT` (anti-enumeration).
    pub(super) async fn on_reset_request(
        &mut self,
        label: Option<String>,
        email: String,
    ) -> io::Result<Flow> {
        // Look the email up; only a real account gets a code minted + mailed. A
        // malformed / unknown address simply matches nothing — the response is
        // identical, so no lookup outcome is observable to the caller.
        match self.ctx.accounts.account_by_email(&email).await {
            Ok(Some(account)) => {
                let code = format!("{:06}", rand::random::<u32>() % 1_000_000);
                let expiry = unix_now_ms() + RESET_CODE_TTL_MS;
                self.ctx
                    .verify_send_code(&account, "reset", &email, code, expiry, "password reset")
                    .await;
            }
            Ok(None) => {}
            Err(e) => return self.internal(label, &e).await,
        }

        self.send_event(label, Event::ResetSent { email }).await?;
        Ok(Flow::Continue)
    }

    /// `RESET CONFIRM <email> <code> :<new-password>` — verify the code and set
    /// the new password. Unknown email and wrong code fail alike (`bad-code`).
    pub(super) async fn on_reset_confirm(
        &mut self,
        label: Option<String>,
        email: String,
        code: String,
        password: &str,
    ) -> io::Result<Flow> {
        // Reject a too-short password *before* touching the code, so a valid
        // code isn't burned by a policy rejection (codes are single-use).
        if password.len() < 12 {
            self.send_err(
                label,
                ErrCode::Policy,
                None,
                "password must be at least 12 bytes",
            )
            .await?;
            return Ok(Flow::Continue);
        }

        // Resolve the email. An unknown email is indistinguishable from a wrong
        // code — same FORBIDDEN/bad-code, no account-existence oracle.
        let account = match self.ctx.accounts.account_by_email(&email).await {
            Ok(Some(account)) => account,
            Ok(None) => return self.reset_bad_code(label).await,
            Err(e) => return self.internal(label, &e).await,
        };

        if !self
            .ctx
            .verify_check_code(&account, "reset", &code, unix_now_ms())
        {
            return self.reset_bad_code(label).await;
        }

        match self.ctx.accounts.set_password(&account, password).await {
            // The account existed at lookup; a lost race (deleted meanwhile) reads
            // as a bad code rather than leaking that it's gone.
            Ok(false) => self.reset_bad_code(label).await,
            Ok(true) => {
                info!(%account, "password reset via email code");
                self.send_event(label, Event::ResetDone { email }).await?;
                Ok(Flow::Continue)
            }
            Err(e) => self.internal(label, &e).await,
        }
    }

    /// The single failure surface for `RESET CONFIRM`: a wrong/expired code and
    /// an unknown email are one code, one text (anti-enumeration).
    async fn reset_bad_code(&mut self, label: Option<String>) -> io::Result<Flow> {
        self.send_err(
            label,
            ErrCode::Forbidden,
            Some("bad-code"),
            "invalid or expired reset code",
        )
        .await?;
        Ok(Flow::Continue)
    }
}
