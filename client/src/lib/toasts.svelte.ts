// Transient toast notifications + the server-confirmed-success toast system.
// A module singleton so any store/reducer/component can raise a toast without
// threading it through the AppCtx. `toasts` is mutated in place (never
// reassigned) so it can be a `const` export imported bare and stay reactive.

let seq = 0;
export const toasts = $state<{ id: number; text: string; kind: string }[]>([]);

export function toast(text: string, kind = "info"): void {
  const id = seq++;
  toasts.push({ id, text, kind });
  setTimeout(() => {
    const i = toasts.findIndex((t) => t.id === id);
    if (i >= 0) toasts.splice(i, 1);
  }, 4500);
}

// A weft call resolves on *send*, not on server confirmation, so success can't
// be toasted in `.then()` (a missing-cap failure arrives later as an ERR event).
// Instead an action registers an expected key here; when the matching confirming
// event lands, `confirmSuccess` fires the toast. Unmatched keys simply expire.
const pendingSuccess: Record<string, string> = {};

export function expectSuccess(key: string, message: string): void {
  pendingSuccess[key] = message;
  // Don't leave a stale expectation if the action silently fails.
  setTimeout(() => delete pendingSuccess[key], 6000);
}

export function confirmSuccess(key: string): void {
  const m = pendingSuccess[key];
  if (m) {
    delete pendingSuccess[key];
    toast(m, "success");
  }
}
