// The plugin surface's client state: the action catalog, and whatever views are
// currently open (plugin-spec.md §11–§13).
//
// A flow is server-correlated by `view_id`, so that is the key for everything
// here. weftd only routes steps for a view you opened, so there is never more
// than one owner of a flow — this store is that owner.

import { SvelteMap } from "svelte/reactivity";
import type { HandlerMap } from "$lib/sync/handler-map";
import { toast } from "$lib/notifications/toasts.svelte";
import * as weft from "$lib/transport/weft";
import { store } from "$lib/store/store.svelte";
import {
  applyPatch,
  initialValues,
  type ActionDecl,
  type Catalog,
  type CatalogEntry,
  type Surface,
  type View,
  type ViewResult,
} from "./sdui";

/** One open view and the form state the user has built up in it. */
export class OpenView {
  view = $state<View>({ container: "modal" });
  values = $state<Record<string, unknown>>({});
  /** True between sending a step and the plugin answering, so the UI can wait. */
  busy = $state(false);

  constructor(
    readonly id: string,
    view: View,
  ) {
    this.view = view;
    this.values = initialValues(view.blocks);
  }

  get isPanel() {
    return this.view.container === "panel";
  }
}

class PluginStore {
  /** The catalog, by plugin id. Empty until `PLUGINS` answers. */
  catalog = new SvelteMap<string, CatalogEntry>();
  /** Open views by `view_id`. */
  views = new SvelteMap<string, OpenView>();

  /** The schemes a plugin serves, or `[]` if it governs no realm. */
  schemesOf(pluginId: string): string[] {
    return this.catalog.get(pluginId)?.schemes ?? [];
  }

  /**
   * Does `plugin` speak for namespace `ns`?
   *
   * A plugin declaring no schemes governs no realm, so its surfaces are generic
   * and apply anywhere. One that *does* declare schemes is a realm adapter, and
   * its surfaces belong only to that realm's replicas — a Matrix "Bridged room
   * settings" page on a native channel opens a flow the adapter refuses ("this
   * channel is not bridged") and leaves the panel spinning on `Loading…`.
   *
   * Filtering by surface + context alone cannot know this, which is why the check
   * lives here and every caller shares it.
   */
  governs(pluginId: string, ns: string | undefined): boolean {
    const schemes = this.schemesOf(pluginId);
    if (schemes.length === 0) return true;
    if (!ns) return false;

    const origin = store.servers.get(ns)?.origin;
    const scheme = origin?.match(/^([a-z][a-z0-9+.-]*):\/\//)?.[1];

    return !!scheme && schemes.includes(scheme);
  }

  /** Every declared action on a surface, flattened with its plugin id. */
  actionsFor(surface: Surface): { plugin: string; action: ActionDecl }[] {
    const out: { plugin: string; action: ActionDecl }[] = [];

    for (const entry of this.catalog.values()) {
      for (const action of entry.actions) {
        if (action.surface === surface) out.push({ plugin: entry.plugin_id, action });
      }
    }

    return out;
  }

  /** The modal to show, if any. Only one is drawn at a time — a plugin opening a
   *  second while the first is up would otherwise stack dialogs invisibly. */
  get activeModal(): OpenView | undefined {
    for (const v of this.views.values()) {
      if (!v.isPanel) return v;
    }
    return undefined;
  }

  panelsFor(key: string): OpenView[] {
    return [...this.views.values()].filter((v) => v.isPanel && v.view.panel_key === key);
  }

  refresh() {
    void weft.plugins();
  }

  /** Open a flow. `params` pre-fills the action's declared inputs — a slash
   *  command's arguments arrive this way (§13.4), so the plugin can act without
   *  a form step. */
  invoke(plugin: string, action: string, ctxRef?: string, params?: Record<string, unknown>) {
    void weft.pluginInvoke(plugin, action, ctxRef, params);
  }

  submit(view: OpenView) {
    view.busy = true;
    void weft.pluginSubmit(view.id, view.values);
  }

  press(view: OpenView, button: string) {
    view.busy = true;
    void weft.pluginAction(view.id, button, view.values);
  }

  /** Dismiss a view. Terminal server-side, so drop it here too rather than
   *  waiting for a confirmation that will not come. */
  close(viewId: string) {
    const view = this.views.get(viewId);
    if (view?.isPanel) void weft.pluginSubscribe(viewId, false);

    this.views.delete(viewId);
    void weft.pluginClose(viewId);
  }

  // ---- wire events ----

  onManifest(json: string) {
    const catalog = parse<Catalog>(json);
    if (!catalog) return;

    this.catalog.clear();
    for (const entry of catalog.plugins ?? []) this.catalog.set(entry.plugin_id, entry);
  }

  onView(viewId: string, json: string) {
    const view = parse<View>(json);
    if (!view) return;

    const existing = this.views.get(viewId);
    if (existing) {
      // A later step in a flow already open: swap the view but keep the form,
      // so a multi-step wizard does not lose what the user typed on step one.
      existing.view = view;
      existing.values = { ...initialValues(view.blocks), ...existing.values };
      existing.busy = false;
      return;
    }

    const opened = new OpenView(viewId, view);
    this.views.set(viewId, opened);

    // §11.3 a panel is only patched while subscribed, so say so as it opens.
    if (opened.isPanel) void weft.pluginSubscribe(viewId, true);
  }

  onPatch(viewId: string, json: string) {
    const ops = parse<Parameters<typeof applyPatch>[1][]>(json);
    const open = this.views.get(viewId) ?? this.byPanelKey(viewId);
    if (!ops || !open) return;

    for (const op of ops) open.view = applyPatch(open.view, op);
  }

  onResult(viewId: string, json: string) {
    const result = parse<ViewResult>(json);
    this.views.delete(viewId);
    if (!result) return;

    switch (result.type) {
      case "toast":
        toast(result.text, result.kind === "ok" ? "info" : result.kind);
        break;
      // `navigate`/`refresh` describe intent; the real state change arrives on
      // the ordinary event stream (§11.5), so there is nothing to apply here.
      default:
        break;
    }
  }

  /** weftd addresses a patch by view-id *or* panel key (§11.3); resolve the
   *  second form to whichever of our views is showing that panel. */
  private byPanelKey(key: string): OpenView | undefined {
    return this.panelsFor(key)[0];
  }
}

/** A payload that does not parse is dropped: a plugin cannot break the client
 *  by sending something malformed. weft-client-core already rejects anything the
 *  codec cannot read, so this is the second net, not the first. */
function parse<T>(json: string): T | undefined {
  try {
    return JSON.parse(json) as T;
  } catch {
    return undefined;
  }
}

export const plugins = new PluginStore();

/** The plugin domain's slice of the event reducer. */
export const pluginHandlers: HandlerMap = {
  "plugin-manifest": (e) => plugins.onManifest(e.catalog),
  "plugin-view": (e) => plugins.onView(e.view_id, e.view),
  "plugin-patch": (e) => plugins.onPatch(e.view_id, e.patch),
  "plugin-result": (e) => plugins.onResult(e.view_id, e.result),
};
