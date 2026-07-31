// The client domain model — see docs/architecture/client-model-refactor.md.
import { SvelteMap } from "svelte/reactivity";
import type { Msg } from "$lib/types";
import type { HandlerMap } from "$lib/sync/handler-map";
import { store } from "$lib/store/store.svelte";
import { sys } from "$lib/messages/messages.svelte";

/** A filed report as shown in the moderation queue (§6.7). */
export interface ReportInfo {
  report_id: string;
  msgid: string;
  category: string;
  state: string;
  reporter?: string | null;
}

/** §6.7 report-resolution actions offered in the queue UI. */
export const RESOLVE_ACTIONS = ["dismissed", "content-removed", "user-actioned", "escalated"];

/**
 * §6.7 moderation reports: the queue modal (`report_id` → filed report, kept
 * live from REPORT-FILED / REPORT-RESOLVED) plus the report-filing target (the
 * message `ReportModal` is filing). Replaces the `report*` `$state` in
 * `+page.svelte`.
 */
export class Reports {
  open = $state(false); // the reports queue modal
  target = $state<Msg | null>(null); // the message being reported (ReportModal)
  readonly queue = new SvelteMap<string, ReportInfo>();
}

/// §6.7 report wire-event handlers: filing confirmations + the moderation queue.
export const reportsHandlers: HandlerMap = {
  reported: (e) => sys(`✓ report filed (${e.report_id})`),
  "report-filed": (e) =>
    store.reports.queue.set(e.report_id, {
      report_id: e.report_id,
      msgid: e.msgid,
      category: e.category,
      state: e.state,
      reporter: e.reporter,
    }),
  "report-resolved": (e) => {
    store.reports.queue.delete(e.report_id);
    sys(`✓ report ${e.report_id} resolved: ${e.action}`);
  },
};
