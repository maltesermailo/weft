// Convenience wrapper: render message markdown with the ambient `MdContext`
// built from the current session/server state, so components call `renderMd(text)`
// with a single argument. The pure, unit-testable renderer stays in `$lib/markdown`
// (decoupled from container state) — this module supplies the ambient inputs.
import { renderMd as renderMdPure, type MdContext } from "$lib/markdown";
import { store } from "$lib/models/store.svelte";
import { rolesAt, memberRoles } from "$lib/models/session.svelte";
import { view } from "$lib/view.svelte";

function mdContext(): MdContext {
  const activeServer = view.activeServer;
  const account = store.session.account;

  return {
    account,
    activeServer,
    pingable: rolesAt(`ns:${activeServer}`).filter((r) => r.pingable),
    myRoleIds: new Set(memberRoles[`${account}|ns:${activeServer}`] ?? []),
    emoji: (n) => (activeServer ? store.servers.get(activeServer)?.emoji.get(n) : undefined),
  };
}

export function renderMd(text: string): string {
  return renderMdPure(text, mdContext());
}
