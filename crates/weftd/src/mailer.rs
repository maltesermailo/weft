//! §10.5 the weftd side of account verification: the `Mailer` port impls.
//!
//! weft-core generates + holds the one-time code and hands it here to deliver.
//! `SmtpMailer` sends it via `lettre` (STARTTLS submission by default, rustls
//! with the ring provider); `LogMailer` is the dev/unconfigured fallback that
//! logs the code so the flow is exercisable without an SMTP server.

use anyhow::Context;
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use tracing::{info, warn};

use weft_core::Mailer;

/// Dev / unconfigured fallback: logs the verification code instead of mailing it.
pub struct LogMailer;

#[async_trait::async_trait]
impl Mailer for LogMailer {
    async fn send_code(&self, address: &str, code: &str) {
        info!(%address, %code, "verification code (no SMTP configured — dev log only)");
    }
}

/// SMTP submission mailer (`lettre`), built once from `[smtp]` config.
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl SmtpMailer {
    pub fn new(cfg: &crate::config::Smtp) -> anyhow::Result<Self> {
        let from: Mailbox = cfg
            .from
            .parse()
            .with_context(|| format!("invalid [smtp] from address {:?}", cfg.from))?;

        // 465 = implicit TLS on connect; 587 = plaintext connect then STARTTLS.
        let builder = if cfg.implicit_tls {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.host)
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.host)
        }
        .with_context(|| format!("invalid [smtp] host {:?}", cfg.host))?
        .port(cfg.port);

        let builder = if cfg.username.is_empty() {
            builder
        } else {
            builder.credentials(Credentials::new(cfg.username.clone(), cfg.password.clone()))
        };

        Ok(Self {
            transport: builder.build(),
            from,
        })
    }
}

#[async_trait::async_trait]
impl Mailer for SmtpMailer {
    async fn send_code(&self, address: &str, code: &str) {
        let to: Mailbox = match address.parse() {
            Ok(mbox) => mbox,
            Err(e) => {
                warn!(%address, "verification email recipient rejected: {e}");
                return;
            }
        };

        let email = match Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject("Your verification code")
            .body(format!(
                "Your verification code is: {code}\n\nIt expires in 15 minutes."
            )) {
            Ok(email) => email,
            Err(e) => {
                warn!("building verification email failed: {e}");
                return;
            }
        };

        // Best effort (per the port contract): log a failure, keep the pending
        // claim so the user can re-request.
        if let Err(e) = self.transport.send(email).await {
            warn!(%address, "sending verification email failed: {e}");
        }
    }
}
