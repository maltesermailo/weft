// The client domain model — see docs/architecture/client-model-refactor.md.
import { SvelteMap } from "svelte/reactivity";
import type { Msg } from "$lib/types";

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
