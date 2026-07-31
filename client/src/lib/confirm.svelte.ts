// Promise-based confirm dialog. `appConfirm(msg)` returns a Promise<boolean> that
// resolves when the user answers the ConfirmModal; the pending state lives here so
// any caller (and the modal) reaches it without the AppCtx bridge.
let current = $state<{ message: string; label: string; resolve: (v: boolean) => void } | null>(null);

export const confirmDialog = {
  get current() {
    return current;
  },
};

export function appConfirm(message: string, label = "Confirm"): Promise<boolean> {
  return new Promise((resolve) => (current = { message, label, resolve }));
}

export function resolveConfirm(ok: boolean): void {
  current?.resolve(ok);
  current = null;
}
