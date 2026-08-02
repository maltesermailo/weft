//! Reports domain (§6.7) — the moderation **report queue** (`report_id` → filed
//! report). The Rust mirror of `reportsHandlers`' queue bookkeeping: `REPORT-FILED`
//! adds, `REPORT-RESOLVED` removes. The queue is fetched on demand (open the modal
//! → clear + `MOD`/`REPORTS LIST`), so a `reports_clear` command backs that reset.
//! The two `sys(…)` confirmations (report filed / resolved) stay in TS — they're
//! channel system lines, not queue state — as do the modal's `open`/`target` UI.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::ClientEvent;

/// A filed report as shown in the queue. The event's `scope` isn't kept — the
/// queue is flat and refetched per open, matching the TS `ReportInfo`.
#[derive(Serialize, Clone)]
pub struct ReportInfo {
    pub report_id: String,
    pub msgid: String,
    pub category: String,
    pub state: String,
    pub reporter: Option<String>,
}

/// This domain's state diff — the mirror rebuilds `store.reports.queue`. The whole
/// queue (idempotent → a re-fetch / clear replaces cleanly). Keyed order is
/// `report_id` (a ULID → chronological).
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ReportDiff {
    Reports { reports: Vec<ReportInfo> },
}

/// The reports sub-model: report_id → filed report. Transient (fetched on demand).
#[derive(Default)]
pub struct Reports {
    queue: BTreeMap<String, ReportInfo>,
}

impl Reports {
    pub fn handle(&mut self, event: &ClientEvent) -> Vec<ReportDiff> {
        match event {
            ClientEvent::ReportFiled { report_id, msgid, category, state, reporter, .. } => {
                self.queue.insert(
                    report_id.clone(),
                    ReportInfo {
                        report_id: report_id.clone(),
                        msgid: msgid.clone(),
                        category: category.clone(),
                        state: state.clone(),
                        reporter: reporter.clone(),
                    },
                );
                self.snapshot()
            }
            ClientEvent::ReportResolved { report_id, .. } => {
                if self.queue.remove(report_id).is_some() {
                    self.snapshot()
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    /// Clear the queue ahead of an on-demand re-fetch (the modal's open reset).
    pub(super) fn clear(&mut self) -> Vec<ReportDiff> {
        self.queue.clear();
        self.snapshot()
    }

    fn snapshot(&self) -> Vec<ReportDiff> {
        vec![ReportDiff::Reports { reports: self.queue.values().cloned().collect() }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filed(report_id: &str, msgid: &str) -> ClientEvent {
        ClientEvent::ReportFiled {
            report_id: report_id.into(),
            msgid: msgid.into(),
            category: "spam".into(),
            state: "unverified".into(),
            scope: "ns:x".into(),
            reporter: None,
        }
    }
    fn resolved(report_id: &str) -> ClientEvent {
        ClientEvent::ReportResolved { report_id: report_id.into(), action: "dismissed".into(), note: None }
    }
    fn ids(d: &ReportDiff) -> Vec<&str> {
        let ReportDiff::Reports { reports } = d;
        reports.iter().map(|r| r.report_id.as_str()).collect()
    }

    #[test]
    fn filed_adds_and_resolved_removes() {
        let mut r = Reports::default();
        assert_eq!(ids(&r.handle(&filed("r1", "m1"))[0]), vec!["r1"]);
        assert_eq!(ids(&r.handle(&filed("r2", "m2"))[0]), vec!["r1", "r2"]);
        assert_eq!(ids(&r.handle(&resolved("r1"))[0]), vec!["r2"]);
        // Resolving an unknown report → no diff.
        assert!(r.handle(&resolved("gone")).is_empty());
    }

    #[test]
    fn clear_empties_the_queue() {
        let mut r = Reports::default();
        r.handle(&filed("r1", "m1"));
        assert!(ids(&r.clear()[0]).is_empty());
    }
}
