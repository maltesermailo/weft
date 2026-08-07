/// Turning a §8 error code into something a person can act on.
///
/// The codes are deliberately uninformative: `NO-SUCH-TARGET` is the *same* answer
/// for a namespace that doesn't exist, one that's private, one that's view-gated and
/// one whose invite expired (spec §8, invariant 1 — anti-enumeration, down to the
/// timing envelope). That's correct on the wire and useless in a toast.
///
/// So the explanation has to come from what the *client* knows: which request this
/// answers (correlated by the §3.5 label it sent) and, for a foreign join, whether a
/// provider for that scheme is even connected. None of that asks the server to
/// distinguish anything it deliberately won't.

/// A foreign join we are waiting on, keyed by the label we sent it with.
type PendingRealmJoin = { uri: string; scheme: string; realm: string; space: string };

const pending = new Map<string, PendingRealmJoin>();

/// Labels sent on requests the *client* made on its own initiative.
const background = new Set<string>();

/// Labels are only ever matched against our own maps, so a counter is enough — no
/// need for randomness, and a readable label helps when reading the raw wire.
let seq = 0;

export function trackRealmJoin(join: PendingRealmJoin): string {
  const label = `nsjoin${++seq}`;
  pending.set(label, join);

  // A join that neither succeeds nor fails leaks an entry; expire it rather than
  // grow forever in a long-lived session. Generous, because a provider may have to
  // resolve and enumerate a remote space before answering.
  setTimeout(() => pending.delete(label), 60_000);

  return label;
}

/// Label a request the user did not ask for, so its failure doesn't become a
/// toast in their face.
///
/// Speculative fetches (a roster on opening a channel, a layout on seeing a
/// namespace) fail for reasons that are ours to handle, not theirs to read: a
/// stale belief that we're joined answers `CAP-REQUIRED`, and a channel that
/// went away answers `NO-SUCH-TARGET`. Neither is a thing the user did.
export function trackBackground(): string {
  const label = `bg${++seq}`;
  background.add(label);
  setTimeout(() => background.delete(label), 60_000);

  return label;
}

/// The toast for an §8 error, or `null` when it should be swallowed.
///
/// One entry point so "is this the user's business at all?" is answered in a
/// single place: a background fetch's failure is logged and dropped, a tracked
/// request gets the context only we have, everything else gets friendly text.
export function toastFor(code: string, text: string, label: string | null): string | null {
  if (label && background.has(label)) {
    background.delete(label);
    // Kept in the console: silent to the user is not the same as invisible to
    // whoever is debugging why a roster is empty.
    console.debug(`background request ${label} failed: ${code} ${text}`);

    return null;
  }

  return explainJoinError(code, label) ?? friendlyError(code, text);
}

/// The message to show for an error, or `null` to fall back to the generic path.
///
/// TODO once `CatalogEntry.schemes` exists — it currently carries only
/// id/name/icon/actions: if no connected provider declares this scheme, say so
/// outright. "No matrix bridge is connected" is the commonest real cause while one is
/// being set up, and it is something the client can *assert* rather than offer as one
/// possibility among several.
function explainJoinError(code: string, label: string | null): string | null {
  const join = label ? pending.get(label) : undefined;
  if (!join || !label) return null;

  pending.delete(label);

  if (code === "NO-SUCH-TARGET") {
    // Every cause the server refuses to distinguish, in the order they actually
    // happen while setting a bridge up.
    return `Couldn't join ${join.space} on ${join.realm}. It may not exist, may not be public, or the bridge may not have been invited to it yet.`;
  }

  if (code === "FORBIDDEN") {
    return `Not allowed to join ${join.space} on ${join.realm}.`;
  }

  return `Couldn't join ${join.space} on ${join.realm} (${code}).`;
}

/// Human text for the codes a user can actually meet outside a tracked request.
/// Anything absent falls through to the raw code, which is the honest default —
/// inventing an explanation for an unexpected error is worse than showing it.
const FRIENDLY: Record<string, string> = {
  "NO-SUCH-TARGET": "That doesn't exist, or you don't have access to it.",
  FORBIDDEN: "You don't have permission to do that.",
  "AUTH-FAILED": "Sign-in failed.",
  THROTTLED: "Too fast — try again in a moment.",
  SLOW: "The connection fell behind and is resyncing.",
  POLICY: "The server's policy doesn't allow that.",
};

function friendlyError(code: string, text: string): string {
  const friendly = FRIENDLY[code];
  if (!friendly) return text ? `${code}: ${text}` : code;

  // Keep the server's own words when it bothered to send some: they are more
  // specific than anything canned here.
  return text ? `${friendly} (${text})` : friendly;
}
