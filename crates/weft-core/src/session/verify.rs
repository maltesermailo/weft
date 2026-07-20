//! §10.5 account verification: `VERIFY EMAIL/BIRTHDAY/CONFIRM/LIST`.
//!
//! Two claim kinds. **email** is server-proven: `VERIFY EMAIL` records a pending
//! claim and mails a one-time code (via the [`Mailer`](crate::mailer::Mailer)
//! port); `VERIFY CONFIRM email <code>` proves it. **birthday** is
//! self-attested: `VERIFY BIRTHDAY` records + confirms it on the spot (honest
//! that it's self-declared). Claims — and their subjects (email address / birth
//! date, both PII) — are returned **only to the owner's own session**, never
//! broadcast. This milestone is badge-only: claims don't gate access yet.

use super::*;

/// A verification code is valid for 15 minutes.
const VERIFY_CODE_TTL_MS: u64 = 15 * 60 * 1000;

impl<S: ControlStream> Session<S> {
    /// `VERIFY EMAIL <address>` — record a pending email claim and mail a code.
    pub(super) async fn on_verify_email(
        &mut self,
        label: Option<String>,
        address: String,
        account: Account,
    ) -> io::Result<Flow> {
        if !is_plausible_email(&address) {
            return self
                .send_err(label, ErrCode::Malformed, None, "invalid email address")
                .await
                .map(|_| Flow::Continue);
        }

        if let Err(e) = self
            .ctx
            .accounts
            .upsert_verification(&account, "email", &address)
            .await
        {
            return self.internal(label, &e).await;
        }

        // A fresh 6-digit code, held in memory + mailed. Replaces any prior one.
        let code = format!("{:06}", rand::random::<u32>() % 1_000_000);
        let expiry = unix_now_ms() + VERIFY_CODE_TTL_MS;
        self.ctx
            .verify_send_code(&account, "email", &address, code, expiry)
            .await;

        self.send_event(
            label,
            Event::Verified {
                kind: "email".to_string(),
                subject: address,
                state: VerifyState::Pending,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// `VERIFY BIRTHDAY <YYYY-MM-DD>` — self-attest a birth date (recorded +
    /// confirmed immediately; self-declared, not server-proven).
    pub(super) async fn on_verify_birthday(
        &mut self,
        label: Option<String>,
        date: String,
        account: Account,
    ) -> io::Result<Flow> {
        if !is_iso_date(&date) {
            return self
                .send_err(
                    label,
                    ErrCode::Malformed,
                    None,
                    "birthday must be YYYY-MM-DD",
                )
                .await
                .map(|_| Flow::Continue);
        }

        if let Err(e) = self
            .ctx
            .accounts
            .upsert_verification(&account, "birthday", &date)
            .await
        {
            return self.internal(label, &e).await;
        }
        if let Err(e) = self
            .ctx
            .accounts
            .confirm_verification(&account, "birthday", unix_now())
            .await
        {
            return self.internal(label, &e).await;
        }

        self.send_event(
            label,
            Event::Verified {
                kind: "birthday".to_string(),
                subject: date,
                state: VerifyState::Confirmed,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// `VERIFY CONFIRM <kind> <code>` — prove a pending claim with its code.
    pub(super) async fn on_verify_confirm(
        &mut self,
        label: Option<String>,
        kind: String,
        code: String,
        account: Account,
    ) -> io::Result<Flow> {
        if !self
            .ctx
            .verify_check_code(&account, &kind, &code, unix_now_ms())
        {
            return self
                .send_err(
                    label,
                    ErrCode::Forbidden,
                    Some("bad-code"),
                    "invalid or expired verification code",
                )
                .await
                .map(|_| Flow::Continue);
        }

        match self
            .ctx
            .accounts
            .confirm_verification(&account, &kind, unix_now())
            .await
        {
            Ok(true) => {}
            // The code matched but the claim is gone (cleared/expired) — treat as
            // no such pending claim.
            Ok(false) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        }

        // Echo the confirmed claim, with its subject read back from the store.
        let subject = self
            .ctx
            .accounts
            .verifications(&account)
            .await
            .ok()
            .and_then(|claims| claims.into_iter().find(|c| c.kind == kind))
            .map(|c| c.subject)
            .unwrap_or_default();

        self.send_event(
            label,
            Event::Verified {
                kind,
                subject,
                state: VerifyState::Confirmed,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// `VERIFY LIST` — the caller's own claims (subjects are PII → owner only).
    pub(super) async fn on_verify_list(
        &mut self,
        label: Option<String>,
        account: Account,
    ) -> io::Result<Flow> {
        let claims = match self.ctx.accounts.verifications(&account).await {
            Ok(claims) => claims,
            Err(e) => return self.internal(label, &e).await,
        };
        for claim in claims {
            let state = if claim.verified_at.is_some() {
                VerifyState::Confirmed
            } else {
                VerifyState::Pending
            };
            self.send_event(
                label.clone(),
                Event::Verified {
                    kind: claim.kind,
                    subject: claim.subject,
                    state,
                },
            )
            .await?;
        }
        Ok(Flow::Continue)
    }
}

/// A lenient syntactic email check (`local@domain.tld`, no spaces, bounded). Not
/// an RFC 5322 validator — the *real* proof is the mailed code round-trip; this
/// only rejects the obviously-malformed before we bother mailing.
fn is_plausible_email(address: &str) -> bool {
    if address.len() > 254 || address.chars().any(char::is_whitespace) {
        return false;
    }
    let Some((local, domain)) = address.split_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

/// A basic `YYYY-MM-DD` check with sane month/day ranges (not full calendar
/// validation — a self-attested birthday needs only to be well-formed).
fn is_iso_date(date: &str) -> bool {
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 {
        return false;
    }
    let (Ok(y), Ok(m), Ok(d)) = (
        parts[0].parse::<u32>(),
        parts[1].parse::<u32>(),
        parts[2].parse::<u32>(),
    ) else {
        return false;
    };
    parts[0].len() == 4
        && (1900..=2100).contains(&y)
        && (1..=12).contains(&m)
        && (1..=31).contains(&d)
}
