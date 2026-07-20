//! §10.5 the email-sending seam for account verification.
//!
//! weft-core owns the verification logic — it generates the one-time code, holds
//! it in memory with a short expiry, and records the claim — but it must not do
//! socket I/O (L2). So it hands the code to an installed `Mailer`; the real SMTP
//! sender lives in weftd (L3), and a mock drives the core tests. A server with no
//! mailer still records the pending claim (the code is simply never delivered) —
//! useful in dev, where weftd's log-mailer prints it instead.

use async_trait::async_trait;

/// Sends account-verification emails. Best effort: a delivery failure is the
/// impl's to log — the pending claim stands and the user can re-request.
#[async_trait]
pub trait Mailer: Send + Sync {
    /// Deliver the one-time `code` to `address` (a `VERIFY EMAIL` confirmation).
    async fn send_code(&self, address: &str, code: &str);
}
