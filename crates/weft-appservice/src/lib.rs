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

/// §3.4 keepalive interval. weftd reaps an idle session at its own ceiling, and a
/// bridge is quiet whenever its realm is — so the PING is what distinguishes
/// "nothing is happening" from "this peer is gone".
const KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(10);

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
    Event {
        event: Event,
        /// The line's `label=` — on the projection path this is the echo-ack
        /// correlation: an injected line's label returns on the minted event
        /// (§3.5), which is how an adapter learns the home-minted id.
        label: Option<String>,
        /// The acting **local** user's account ULID (`ulid=` tag), when the
        /// event has one — the stable identity to key puppets by (names are
        /// mutable vanity labels). Absent on foreign-actor and system events.
        actor_ulid: Option<String>,
    },
    /// An invoke of an action declared with [`AppServiceBuilder::declare`] —
    /// no closure handler, so the adapter handles it inline. Answer on
    /// `Realm::ctx_for(&view_id)`.
    Invoke {
        view_id: String,
        action: String,
        /// What the action was invoked on (a namespace, channel, member id).
        ctx_ref: Option<String>,
        /// The declared inputs' values, decoded from the wire's CBOR.
        params: std::collections::BTreeMap<String, serde_json::Value>,
        /// The invoking WEFT user, and their stable id.
        invoker: Option<String>,
        invoker_ulid: Option<String>,
    },
    /// A later step of a flow we opened: the user submitted a view or clicked
    /// one of its controls (spec §12.1). `values` are the form's inputs,
    /// already decoded from the wire's CBOR.
    Step {
        view_id: String,
        /// The clicked control's id; `None` for a plain submit.
        button: Option<String>,
        values: std::collections::BTreeMap<String, serde_json::Value>,
        /// True when the user dismissed the view — terminal, nothing to answer.
        closed: bool,
    },
    Command {
        /// The `@as` attribution — the local user on whose behalf weftd asks.
        as_user: Option<String>,
        /// The actor's **account ULID** (`ulid=` tag) — the stable identity.
        /// Key puppets and any per-user state by this, never by the account
        /// name, which is a mutable vanity label.
        as_ulid: Option<String>,
        /// The `label=` correlation, when weftd is waiting on an answer.
        ///
        /// A relayed post carries one: the realm is the home for that channel, so
        /// weftd minted nothing and is waiting for the copy that comes back to be
        /// tagged with this — that is how the poster's client reconciles the message
        /// it is showing as pending. Dropping it here left weftd waiting forever.
        label: Option<String>,
        command: Command,
    },
}

/// What a handler is given: the invocation's correlation, who is asking, and
/// the way to answer.
pub struct Ctx {
    view_id: String,
    /// The invoking WEFT user (`as=` on the routed invoke) — attribute any
    /// resulting wire commands to them, never to the service itself.
    pub invoker: Option<String>,
    /// The invoker's account ULID (`ulid=`) — the stable identity.
    pub invoker_ulid: Option<String>,
    out: mpsc::Sender<String>,
}

impl Ctx {
    /// A bare context for a known view-id — [`Realm::ctx_for`]'s backing.
    /// Invoker fields are empty: a step is answered on the flow the adapter
    /// already parked, which is where it remembers who is acting.
    pub(crate) fn new(view_id: String, out: mpsc::Sender<String>) -> Self {
        Self {
            view_id,
            invoker: None,
            invoker_ulid: None,
            out,
        }
    }

    /// The flow's correlation id — park per-view state under it, and it is
    /// what a later [`Incoming::Step`] names.
    pub fn view_id(&self) -> &str {
        &self.view_id
    }

    /// Show an SDUI view (spec §11.2). Non-terminal: the flow stays open, and
    /// the user's submit/click arrives as an [`Incoming::Step`].
    pub async fn view(&self, view: &weft_proto::View) -> anyhow::Result<()> {
        let mut line = Reply::new(Event::PluginView {
            view_id: self.view_id.clone(),
            view: String::new(),
        })
        .to_line()?;
        line.tags
            .insert("view".to_string(), weft_proto::plugin_to_b64(view)?);

        self.send_line(line.serialize()?).await
    }

    /// Finish the flow with a toast — the ordinary "done" answer.
    pub async fn toast(&self, kind: weft_proto::ToastKind, text: &str) -> anyhow::Result<()> {
        self.result(weft_proto::plugin_to_b64(&weft_proto::ViewResult::Toast {
            kind,
            text: text.to_string(),
        })?)
        .await
    }

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

    /// Declare an action **without** a closure handler: its invokes arrive on
    /// the [`Incoming`] stream as [`Incoming::Invoke`].
    ///
    /// This is the shape a bridge wants. Closure handlers are spawned detached
    /// (a slow one must not stall the session), so they cannot touch the
    /// adapter's own state; a single-tasked adapter handles invokes inline
    /// instead, next to the maps the flow needs.
    pub fn declare(mut self, decl: weft_proto::ActionDecl) -> Self {
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
            // The bot request finally reaches weftd: the builder collected it
            // from the start, but the registration had no field for it, so
            // every `.bot(…)` was silently dropped.
            bot: self.bot.clone(),
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

            // §3.4: an authenticated peer PINGs every ~10 s, and answering is
            // mandatory. Without this the session is silent whenever the realm is
            // quiet, and weftd reaps it at its idle ceiling — a bridge that works
            // perfectly looks like one that keeps dropping. weftd answers PONG,
            // which arrives as an ordinary event and needs no handling.
            let mut keepalive = tokio::time::interval(KEEPALIVE);
            keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

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

                        let Some((action, view_id, params, invoker, invoker_ulid)) =
                            invoke_of(&line)
                        else {
                            // Not an invoke → weftd talking to us. Surface it.
                            // A full queue means the adapter stopped reading —
                            // its problem to notice, not a reason to stall the
                            // session. Events first: some lines parse as both
                            // (`SYNC END` is also a lenient `Command::Sync`),
                            // and the event reading is the meaningful one.
                            if let Ok(reply) = Reply::from_line(&line) {
                                let actor_ulid = line.tags.get("ulid").cloned();
                                let _ = events_tx.try_send(Incoming::Event {
                                    event: reply.event,
                                    label: reply.label,
                                    actor_ulid,
                                });
                            } else if let Ok(req) = Request::from_line(&line) {
                                // Flow steps are commands too, but an adapter
                                // wants them typed and decoded, not raw.
                                if let Some(step) = step_of(&req.command) {
                                    let _ = events_tx.try_send(step);
                                    continue;
                                }

                                let as_user = line.tags.get("as").cloned();
                                let as_ulid = line.tags.get("ulid").cloned();
                                let label = line.tags.get("label").cloned();
                                let _ = events_tx.try_send(Incoming::Command {
                                    as_user,
                                    as_ulid,
                                    label,
                                    command: req.command,
                                });
                            }
                            continue;
                        };
                        let Some(handler) = handlers.get(&action) else {
                            // Declared without a closure (`declare`): the
                            // adapter owns it. Dropping it here is what the
                            // first cut did, and a flow that never opens looks
                            // to the user like a dead button.
                            let ctx_ref = ctx_ref_of(&line);
                            let _ = events_tx.try_send(Incoming::Invoke {
                                view_id,
                                action,
                                ctx_ref,
                                params: params
                                    .as_deref()
                                    .and_then(|p| weft_proto::plugin_from_b64(p).ok())
                                    .unwrap_or_default(),
                                invoker,
                                invoker_ulid,
                            });
                            continue;
                        };

                        let ctx = Ctx {
                            view_id,
                            invoker,
                            invoker_ulid,
                            out: out_tx.clone(),
                        };
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
                    _ = keepalive.tick() => {
                        // Best effort: a failed write means the session is going
                        // anyway, and the next branch will see it.
                        let ping = Request::new(Command::Ping { token: None })
                            .serialize()
                            .expect("PING serializes");
                        if let Err(e) = stream.send_line(&ping).await {
                            break Err(e.into());
                        }
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

/// `PLUGIN-REGISTER` carries its payload in the trailing (§3) — a tag caps at 1024
/// bytes and an action catalog passes that with a handful of declarations.
fn register_line(registration: &Registration) -> anyhow::Result<String> {
    let line = Reply::new(Event::PluginRegister {
        registration: weft_proto::plugin_to_b64(registration)?,
    })
    .to_line()?;

    // The line length (§4) is the ceiling on how much a provider may declare. Say so
    // here: the serializer's own error names a byte count and not the cause.
    line.serialize().map_err(|e| {
        anyhow::anyhow!(
            "{e}: the registration for {} declares {} actions, too much for one line — \
             declare fewer, or shorten their labels",
            registration.id,
            registration.actions.len()
        )
    })
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

/// The invoke's `ctx_ref` — what the action was invoked on.
fn ctx_ref_of(line: &Line) -> Option<String> {
    match Request::from_line(line).ok()?.command {
        Command::PluginInvoke { ctx_ref, .. } => ctx_ref,
        _ => None,
    }
}

/// A flow step as an [`Incoming::Step`], if that is what this command is.
fn step_of(cmd: &Command) -> Option<Incoming> {
    let decode = |values: &Option<String>| {
        values
            .as_deref()
            .and_then(|v| weft_proto::plugin_from_b64(v).ok())
            .unwrap_or_default()
    };

    match cmd {
        Command::PluginSubmit { view_id, values } => Some(Incoming::Step {
            view_id: view_id.clone(),
            button: None,
            values: decode(values),
            closed: false,
        }),
        Command::PluginAction {
            view_id,
            button,
            values,
        } => Some(Incoming::Step {
            view_id: view_id.clone(),
            button: Some(button.clone()),
            values: decode(values),
            closed: false,
        }),
        Command::PluginClose { view_id } => Some(Incoming::Step {
            view_id: view_id.clone(),
            button: None,
            values: Default::default(),
            closed: true,
        }),
        _ => None,
    }
}

/// `(action, view-id, params, invoker, invoker-ulid)` if this line is a routed
/// `PLUGIN INVOKE`. weftd carries the correlation as the request's label and
/// the invoking user as `as=`/`ulid=` (slice 11 — management actions must know
/// who is asking).
#[allow(clippy::type_complexity)]
fn invoke_of(
    line: &Line,
) -> Option<(
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
)> {
    let req = Request::from_line(line).ok()?;
    match req.command {
        Command::PluginInvoke { action, params, .. } => Some((
            action,
            req.label?,
            params,
            line.tags.get("as").cloned(),
            line.tags.get("ulid").cloned(),
        )),
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
        let line = Line::parse(
            "@label=v1;as=ada@test.example;ulid=01abc PLUGIN INVOKE matrix power-levels",
        )
        .expect("a valid line");
        let (action, view_id, _, invoker, ulid) = invoke_of(&line).expect("an invoke");
        assert_eq!(action, "power-levels");
        assert_eq!(view_id, "v1");
        // Slice 11: a management action must know who is asking, so it can
        // attribute the wire commands it issues to them.
        assert_eq!(invoker.as_deref(), Some("ada@test.example"));
        assert_eq!(ulid.as_deref(), Some("01abc"));

        let other = Line::parse("PING probe").expect("a valid line");
        assert!(invoke_of(&other).is_none());
    }
}
