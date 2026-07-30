// The client domain model — see docs/architecture/client-model-refactor.md.
import type { Mode } from "$lib/weft";
import { store } from "./store.svelte";

/**
 * The connect / login screen state (§3.6 homeserver pick, §6.1 auth): the
 * homeserver + auth inputs + probe results, grouped so `ConnectScreen` takes a
 * single `form` object (mutated by reference) instead of a dozen bindable props.
 *
 * Ephemeral pre-auth UI — held by `+page.svelte`, not the shared store (nothing
 * else reads it). The authenticated identity lives on `store.session`.
 */
export class ConnectForm {
  mode = $state<Mode>("login");
  /// The homeserver (QUIC host on desktop; page origin on web, display-only).
  host = $state("");
  account = $state("");
  password = $state("");
  /// §6.1 register email — shown/required only when the homeserver asks (§3.6).
  email = $state("");
  /// Two-step: pick the homeserver ("server") → log in / register ("auth").
  serverStep = $state<"server" | "auth">("server");
  /// This homeserver requires an email at REGISTER (from its WELCOME, §3.6).
  emailRequired = $state(false);
  /// A homeserver probe (HELLO→WELCOME only) is in flight.
  probing = $state(false);
  /// TLS verification disabled (client.toml).
  insecure = $state(false);
  /// The last auth error (AUTH-FAILED etc.), shown on the form.
  authError = $state("");
  /// AUTH-FAILED closes the stream; this keeps the specific reason from being
  /// clobbered by a generic "connection closed" in the `closed` handler.
  authFailed = $state(false);
  /// A device key exists for (host, account) → offer passwordless login.
  deviceKeyAvailable = $state(false);
}

/// The single connect-form instance (module singleton — imported by the layout,
/// the reducer's auth handlers, and ConnectScreen).
export const cf = new ConnectForm();

/// Per-account localStorage key for the email-nudge banner dismissal.
export const emailNudgeKey = (): string =>
  `weft:email-nudge-dismissed:${cf.host}:${store.session.account}`;
