// Convenience wrapper: render message markdown with the ambient `MdContext`
// built from the current session/server state, so components call `renderMd(text)`
// with a single argument. The pure, unit-testable renderer stays in `$lib/markdown`
// (decoupled from container state) — this module supplies the ambient inputs.
import { renderMd as renderMdPure, type MdContext } from "$lib/rendering/markdown";
import { store } from "$lib/store/store.svelte";
import { roleStore } from "$lib/roles/roles.svelte";
import { view } from "$lib/navigation/view.svelte";

function mdContext(): MdContext {
  const activeServer = view.activeServer;
  const account = store.session.account;

  return {
    account,
    activeServer,
    pingable: roleStore.rolesAt(`ns:${activeServer}`).filter((r) => r.pingable),
    myRoleIds: new Set(roleStore.memberRoles[`${account}|ns:${activeServer}`] ?? []),
    emoji: (n) => (activeServer ? store.servers.get(activeServer)?.emoji.get(n) : undefined),
  };
}

export function renderMd(text: string): string {
  return renderMdPure(text, mdContext());
}
