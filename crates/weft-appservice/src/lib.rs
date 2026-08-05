//! # weft-appservice — the WEFT App-Service SDK
//!
//! The base for building a `remote` plugin (an **App Service**, the Matrix-style
//! external process) against a WEFT server — and the foundation the Matrix bridge
//! is built on. It handles the pinned-key `AUTH ADAPTER` handshake, registration,
//! and the dispatch loop, so an author writes handlers and domain logic.
//!
//! The wire contract this implements is `docs/protocol/bridge-session-protocol.md`.
//!
//! This is a **client library** (a sibling of `weft-tui`): it depends on the wire
//! codec + transport + crypto, never on `weft-core`/`weftd`/`weft-store`. An App
//! Service is an external process, not the server.
//!
//! ## Shape
//!
//! ```no_run
//! # use weft_appservice::AppService;
//! # use weft_crypto::Keypair;
//! # async fn example() -> anyhow::Result<()> {
//! AppService::builder("127.0.0.1:9000", Keypair::generate(), "matrix")
//!     .name("Matrix Bridge")
//!     .on_action("ping", |ctx, _params| async move { ctx.result("pong").await })
//!     .run()
//!     .await
//! # }
//! ```
//!
//! ## What the SDK owns
//!
//! Deliberately more than a transport wrapper. A bridge behaves as a federated
//! weftd, and the fiddly parts of behaving *correctly* are the same for every
//! adapter — so they live here once rather than being reinvented, and subtly
//! mis-implemented, per realm:
//!
//! - **Minting.** The realm mints its own ULIDs for namespaces, channels and
//!   messages, and weftd pins them ([`Realm`]) — so an adapter never has to wait
//!   for a mapping reply before addressing what it just asserted.
//! - **Attribution.** Every ingested line carries `@as`; the message-bearing ones
//!   also need `@msgid`. Omitting the second silently drops the line, so the API
//!   does not offer the chance.
//! - **Line discipline.** Tags are one `;`-separated group — a second `@` parses
//!   as the verb. Nothing here hand-builds a line.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _};
use tokio::sync::mpsc;
use weft_crypto::Keypair;
use weft_proto::{Command, Event, Line, Registration, Reply, Request};

mod bans;
mod realm;
pub use bans::BanList;
pub use realm::{ChannelAssertion, NamespaceAssertion, Realm};

/// One line from weftd that is not a routed action invoke.
///
/// Two shapes because the wire has two: weftd *tells* the adapter things as
/// events (mapping acks, `PROVISION`/`BRIDGING` pushes, relayed
/// `MESSAGE`/`EDITED`/… traffic), and *asks* it things as commands (`@as NS
/// JOIN`, `@as EDIT`/`DELETE`/`REACT`, `HISTORY`, `GRANT`/`REVOKE` relays).
/// An adapter that saw only the events could not act on any request — which is
/// exactly what the first cut of this SDK got wrong.
#[derive(Debug)]
pub enum Incoming {
    Event(Event),
    Command {
        /// The `@as` attribution — the local user on whose behalf weftd asks.
        as_user: Option<String>,
        /// The actor's **account ULID** (`ulid=` tag) — the stable identity.
        /// Key puppets and any per-user state by this, never by the account
        /// name, which is a mutable vanity label.
        as_ulid: Option<String>,
        command: Command,
    },
}

/// What a handler is given: the invocation's correlation, and the way to answer.
pub struct Ctx {
    view_id: String,
    out: mpsc::Sender<String>,
}

impl Ctx {
    /// Answer the invocation. Terminal — the client's parked request completes.
    pub async fn result(&self, result: impl Into<String>) -> anyhow::Result<()> {
        let line = Reply::new(Event::PluginResult {
            view_id: self.view_id.clone(),
            result: result.into(),
        })
        .serialize()?;

        self.send_line(line).await
    }

    /// Put a raw line on the session — the escape hatch for anything the typed
    /// helpers do not cover yet. Prefer [`Realm`] for bridge traffic.
    pub async fn send_line(&self, line: String) -> anyhow::Result<()> {
        self.out
            .send(line)
            .await
            .map_err(|_| anyhow!("connection closed"))
    }
}

type BoxFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;
type Handler = Arc<dyn Fn(Ctx, Option<String>) -> BoxFuture + Send + Sync>;

/// Entry point: `AppService::builder(...)` starts configuring a service.
pub struct AppService;

impl AppService {
    /// Configure an App Service that authenticates to `endpoint` as plugin `id`,
    /// proving control of `keypair` — the key weftd pins in `[[plugin.remote]]`.
    pub fn builder(
        endpoint: impl Into<String>,
        keypair: Keypair,
        id: impl Into<String>,
    ) -> AppServiceBuilder {
        let id = id.into();
        AppServiceBuilder {
            endpoint: endpoint.into(),
            keypair,
            name: id.clone(),
            id,
            bot: None,
            schemes: Vec::new(),
            actions: Vec::new(),
            handlers: HashMap::new(),
        }
    }
}

pub struct AppServiceBuilder {
    endpoint: String,
    keypair: Keypair,
    id: String,
    name: String,
    bot: Option<String>,
    schemes: Vec<String>,
    actions: Vec<weft_proto::ActionDecl>,
    handlers: HashMap<String, Handler>,
}

impl AppServiceBuilder {
    /// Human-readable name for the plugin catalog. Defaults to the id.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Request that weftd provision + attribute a bot account for this service.
    pub fn bot(mut self, account: impl Into<String>) -> Self {
        self.bot = Some(account.into());
        self
    }

    /// A foreign-URI scheme this service handles (`matrix`, `discord`, …). weftd
    /// routes `PROVISION` for unknown spaces of that scheme here — and the pin in
    /// `[[plugin.remote]]` must authorize it, or every assertion is refused.
    pub fn scheme(mut self, scheme: impl Into<String>) -> Self {
        self.schemes.push(scheme.into());
        self
    }

    /// Declare an action and its handler. The declaration reaches clients via the
    /// plugin catalog; the handler runs when one is invoked.
    pub fn action<F, Fut>(mut self, decl: weft_proto::ActionDecl, handler: F) -> Self
    where
        F: Fn(Ctx, Option<String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        self.handlers.insert(
            decl.id.clone(),
            Arc::new(move |ctx, params| Box::pin(handler(ctx, params))),
        );
        self.actions.push(decl);
        self
    }

    /// [`Self::action`] with the default declaration shape — a context-menu entry
    /// on a message, labelled by its id.
    pub fn on_action<F, Fut>(self, id: &str, handler: F) -> Self
    where
        F: Fn(Ctx, Option<String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let decl = weft_proto::ActionDecl {
            id: id.to_string(),
            label: id.to_string(),
            icon: None,
            surface: weft_proto::Surface::ContextMenu,
            context: weft_proto::ContextType::Message,
            description: None,
            visibility: None,
            input: Vec::new(),
        };
        self.action(decl, handler)
    }

    /// Declare an **admin-panel** page: an action a provider exposes to operators
    /// rather than to members — its own ban list, a health view, whatever only an
    /// operator should see. Rendered in the panel, not the client.
    pub fn admin_action<F, Fut>(self, id: &str, label: &str, handler: F) -> Self
    where
        F: Fn(Ctx, Option<String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let decl = weft_proto::ActionDecl {
            id: id.to_string(),
            label: label.to_string(),
            icon: None,
            surface: weft_proto::Surface::Admin,
            context: weft_proto::ContextType::None,
            description: None,
            visibility: None,
            input: Vec::new(),
        };
        self.action(decl, handler)
    }

    /// Connect, authenticate, register, and pump the dispatch loop until the
    /// connection closes. For a service that also drives traffic of its own —
    /// which every bridge does — use [`Self::connect`] and keep the [`Realm`].
    pub async fn run(self) -> anyhow::Result<()> {
        let connected = self.connect().await?;
        connected.session.await
    }

    /// Connect and register, returning everything an adapter needs: the running
    /// session, a [`Realm`] for driving traffic, and the stream of everything
    /// weftd says. Await the session to run the dispatch loop; it ends when the
    /// connection does.
    pub async fn connect(self) -> anyhow::Result<Connected> {
        let addr = tokio::net::lookup_host(&self.endpoint)
            .await
            .with_context(|| format!("resolving {}", self.endpoint))?
            .next()
            .ok_or_else(|| anyhow!("{} resolved to nothing", self.endpoint))?;
        let server_name = self
            .endpoint
            .rsplit_once(':')
            .map(|(host, _)| host.to_string())
            .unwrap_or_else(|| self.endpoint.clone());

        let endpoint = weft_transport::insecure::client_endpoint(weft_transport::ALPN)?;
        let connection = endpoint.connect(addr, &server_name)?.await?;
        let mut stream = weft_transport::QuicControlStream::open(&connection).await?;

        let network = handshake(&mut stream, &self.keypair).await?;

        // One writer, many senders: handlers and the `Realm` both put lines on
        // the session, and neither may block the read loop. Everything outbound
        // funnels through this queue.
        let (out_tx, mut out_rx) = mpsc::channel::<String>(256);

        let registration = Registration {
            api: 1,
            id: self.id.clone(),
            name: self.name.clone(),
            icon: None,
            actions: self.actions.clone(),
            hooks: Vec::new(),
            schemes: self.schemes.clone(),
        };
        out_tx.send(register_line(&registration)?).await.ok();

        let realm = Realm::new(out_tx.clone(), network);
        let handlers = self.handlers;

        // Everything weftd says that isn't a routed invoke — mapping acks,
        // `PROVISION` pushes, `NS JOIN` requests, relayed events, backfill
        // requests. An adapter that ignored these could not bridge anything, so
        // the stream is part of the return rather than an opt-in.
        let (events_tx, events_rx) = mpsc::channel::<Incoming>(256);

        let session = async move {
            // One task owns the stream: a reader task plus a writer task would
            // need to share it, and sharing it behind a lock starves the writer
            // for as long as the reader is parked on the next line — which is
            // always. `recv_line` is cancel-safe (`Framed::next`), so selecting
            // over it loses nothing.
            let mut stream = stream;

            loop {
                tokio::select! {
                    incoming = stream.recv_line() => {
                        let raw = match incoming {
                            Ok(Some(raw)) => raw,
                            Ok(None) => break Ok(()), // peer closed
                            Err(e) => break Err(e.into()),
                        };
                        let Ok(line) = Line::parse(&raw) else {
                            continue; // tolerate noise (§4, lenient-in)
                        };

                        let Some((action, view_id, params)) = invoke_of(&line) else {
                            // Not an invoke → weftd talking to us. Surface it.
                            // A full queue means the adapter stopped reading —
                            // its problem to notice, not a reason to stall the
                            // session. Events first: some lines parse as both
                            // (`SYNC END` is also a lenient `Command::Sync`),
                            // and the event reading is the meaningful one.
                            if let Ok(reply) = Reply::from_line(&line) {
                                let _ = events_tx.try_send(Incoming::Event(reply.event));
                            } else if let Ok(req) = Request::from_line(&line) {
                                let as_user = line.tags.get("as").cloned();
                                let as_ulid = line.tags.get("ulid").cloned();
                                let _ = events_tx.try_send(Incoming::Command {
                                    as_user,
                                    as_ulid,
                                    command: req.command,
                                });
                            }
                            continue;
                        };
                        let Some(handler) = handlers.get(&action) else {
                            continue; // weftd only routes actions we declared
                        };

                        let ctx = Ctx { view_id, out: out_tx.clone() };
                        let handler = Arc::clone(handler);
                        // Detached: a slow handler must not stall the session,
                        // and weftd correlates the answer by view-id whenever it
                        // arrives.
                        tokio::spawn(async move {
                            if let Err(e) = handler(ctx, params).await {
                                tracing::warn!("action handler failed: {e}");
                            }
                        });
                    }
                    outgoing = out_rx.recv() => {
                        let Some(line) = outgoing else {
                            break Ok(()); // every sender dropped
                        };
                        if let Err(e) = stream.send_line(&line).await {
                            break Err(e.into());
                        }
                    }
                }
            }
        };

        Ok(Connected {
            session: Box::pin(session),
            realm,
            events: events_rx,
        })
    }
}

/// A connected, registered App Service.
pub struct Connected {
    /// The dispatch loop. Drive it (`tokio::spawn`, or await it) for the session
    /// to live; it ends when the connection does.
    pub session: Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>,
    /// Speak as the realm.
    pub realm: Realm,
    /// Everything weftd says that is not a routed action invoke — its
    /// statements as [`Incoming::Event`], its requests as [`Incoming::Command`].
    pub events: mpsc::Receiver<Incoming>,
}

/// `PLUGIN-REGISTER` carries its payload as the `reg=` tag (§3).
fn register_line(registration: &Registration) -> anyhow::Result<String> {
    let mut line = Reply::new(Event::PluginRegister {
        registration: weft_proto::plugin_to_b64(registration)?,
    })
    .to_line()?;
    line.tags
        .insert("reg".to_string(), weft_proto::plugin_to_b64(registration)?);

    Ok(line.serialize()?)
}

/// §4.2: `HELLO` → `AUTH ADAPTER <pubkey>` → sign the challenge → `WELCOME`.
///
/// Returns the network name. The proof signs `nonce ‖ network` (anti
/// cross-network replay), and the realm helpers need it to tell our own users
/// from the realm's.
async fn handshake(
    stream: &mut weft_transport::QuicControlStream,
    keypair: &Keypair,
) -> anyhow::Result<String> {
    stream.send_line("HELLO weft/1").await?;
    let network = match next_event(stream).await? {
        Event::Welcome { network, .. } => network.to_string(),
        other => bail!("expected WELCOME, got {other:?}"),
    };

    let line = Request::new(Command::AuthAdapter {
        pubkey: keypair.public().to_b64(),
    })
    .serialize()?;
    stream.send_line(&line).await?;

    let nonce = match next_event(stream).await? {
        Event::Challenge { nonce } => weft_crypto::b64::decode(&nonce)
            .map_err(|_| anyhow!("challenge nonce is not base64"))?,
        Event::Err(e) => bail!("adapter auth refused: {} {}", e.code, e.text),
        other => bail!("expected CHALLENGE, got {other:?}"),
    };
    let sig = weft_crypto::sign_challenge(keypair, &nonce, &network);
    let line = Request::new(Command::AuthProof {
        signature: weft_crypto::signature_to_b64(&sig),
    })
    .serialize()?;
    stream.send_line(&line).await?;

    match next_event(stream).await? {
        Event::Welcome { features, .. } if features.iter().any(|f| f == "plugin") => Ok(network),
        Event::Welcome { .. } => bail!("authenticated, but not as a plugin session"),
        Event::Err(e) => bail!("adapter auth refused: {} {}", e.code, e.text),
        other => bail!("expected WELCOME, got {other:?}"),
    }
}

async fn next_event(stream: &mut weft_transport::QuicControlStream) -> anyhow::Result<Event> {
    loop {
        let raw = stream
            .recv_line()
            .await?
            .ok_or_else(|| anyhow!("server closed during handshake"))?;
        if let Ok(reply) = Reply::parse(&raw) {
            return Ok(reply.event);
        }
        // Anything unreadable as an event during the handshake is noise.
    }
}

/// `(action, view-id, params)` if this line is a routed `PLUGIN INVOKE`. weftd
/// carries the correlation as the request's label and expects it echoed back.
fn invoke_of(line: &Line) -> Option<(String, String, Option<String>)> {
    let req = Request::from_line(line).ok()?;
    match req.command {
        Command::PluginInvoke { action, params, .. } => Some((action, req.label?, params)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_admin_page_is_declared_for_the_panel_not_the_client() {
        // A provider's operator-facing surfaces (its own ban list, a health view)
        // belong in the admin panel — which matches the permission model, since
        // operators act there rather than as wire capability in a namespace.
        let svc = AppService::builder("weft.example:9000", Keypair::generate(), "matrix")
            .admin_action("bans", "Bridged space bans", |ctx, _| async move {
                ctx.result("{}").await
            });

        let decl = svc.actions.first().expect("declared");
        assert_eq!(decl.surface, weft_proto::Surface::Admin);
        assert_eq!(decl.label, "Bridged space bans");
    }

    #[test]
    fn builder_collects_actions_and_schemes() {
        let svc = AppService::builder("weft.example:9000", Keypair::generate(), "matrix")
            .name("Matrix Bridge")
            .bot("matrix")
            .scheme("matrix")
            .on_action(
                "power-levels",
                |ctx, _| async move { ctx.result("{}").await },
            );

        assert_eq!(svc.id, "matrix");
        assert_eq!(svc.name, "Matrix Bridge");
        assert_eq!(svc.bot.as_deref(), Some("matrix"));
        assert_eq!(svc.schemes, vec!["matrix"]);
        assert_eq!(svc.actions.len(), 1);
        assert!(svc.handlers.contains_key("power-levels"));
    }

    #[test]
    fn a_routed_invoke_is_recognized_by_its_label() {
        // weftd correlates the whole flow by a minted view-id carried as the
        // invoke's label; anything without one is not an invocation.
        let line =
            Line::parse("@label=v1 PLUGIN INVOKE matrix power-levels").expect("a valid line");
        let (action, view_id, _) = invoke_of(&line).expect("an invoke");
        assert_eq!(action, "power-levels");
        assert_eq!(view_id, "v1");

        let other = Line::parse("PING probe").expect("a valid line");
        assert!(invoke_of(&other).is_none());
    }
}
