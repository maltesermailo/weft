//! Label-keyed correlation: state parked until weftd's labelled answer arrives.
//!
//! Three flows in the bridge share one lifecycle. Each mints a label, parks the
//! state the answer will need, and resolves it exactly once when a line echoing
//! that label comes back (§3.5):
//!
//! | what waits | prefix | resolved by |
//! |---|---|---|
//! | the Matrix event a minted msgid must link to | `inj` | the injection echo |
//! | the undo for an act WEFT might refuse | `act` | an `ERR` (§10 revert) |
//! | a blob waiting for its upload grant | `up` | `STREAM ACCEPT` |
//!
//! They are unified because they share an **invariant and a lifecycle**, not
//! because they are all maps: the label is minted here and nowhere else, a label
//! resolves at most once, and a miss is a legitimate silent path — weftd labels
//! its answers with *our* label, so an unknown one simply is not ours.
//!
//! What is deliberately *not* in here: the DM transaction counter, which used to
//! share the upload counter. It mints a monotonic number with no parked value and
//! nothing to resolve, so it almost fits — which is the reason it gets its own
//! plain `u64` instead of bending this type around it.

use std::collections::HashMap;

/// Values parked under a locally-minted label.
///
/// The label prefix is fixed per registry, so labels from different flows can
/// never collide and a stray one is identifiable in a log.
pub struct PendingByLabel<T> {
    prefix: &'static str,
    seq: u64,
    parked: HashMap<String, T>,
}

impl<T> PendingByLabel<T> {
    pub fn new(prefix: &'static str) -> Self {
        Self {
            prefix,
            seq: 0,
            parked: HashMap::new(),
        }
    }

    /// Park `value` and return the label to send with the request.
    pub fn park(&mut self, value: T) -> String {
        self.seq += 1;
        let label = format!("{}-{}", self.prefix, self.seq);
        self.parked.insert(label.clone(), value);

        label
    }

    /// Resolve a label, or `None` when it is not ours (or already resolved).
    pub fn take(&mut self, label: &str) -> Option<T> {
        self.parked.remove(label)
    }

    /// Abandon a label without resolving it — for a request that failed to leave
    /// the building, where no answer will ever come back.
    pub fn forget(&mut self, label: &str) {
        self.parked.remove(label);
    }

    /// How many labels are outstanding. Test/diagnostic surface: the registry
    /// has no timeout, so "does this leak?" is a question worth being able to ask.
    pub fn outstanding(&self) -> usize {
        self.parked.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_unique_and_prefixed() {
        let mut pending: PendingByLabel<u8> = PendingByLabel::new("inj");

        let first = pending.park(1);
        let second = pending.park(2);

        assert_eq!(first, "inj-1");
        assert_eq!(second, "inj-2");
        assert_ne!(first, second);
    }

    #[test]
    fn a_label_resolves_exactly_once() {
        let mut pending: PendingByLabel<&str> = PendingByLabel::new("act");
        let label = pending.park("undo");

        assert_eq!(pending.take(&label), Some("undo"));
        // The second answer to the same label is not a second act to revert —
        // which is what makes a duplicate echo harmless rather than a double undo.
        assert_eq!(pending.take(&label), None);
    }

    #[test]
    fn an_unknown_label_is_not_ours() {
        let mut pending: PendingByLabel<u8> = PendingByLabel::new("up");
        pending.park(7);

        // weftd labels its answers with our label, so anything else belongs to
        // another flow (or another registry) and must not be claimed.
        assert_eq!(pending.take("up-99"), None);
        assert_eq!(pending.take("act-1"), None);
        assert_eq!(pending.outstanding(), 1, "a foreign label consumed nothing");
    }

    #[test]
    fn resolving_and_forgetting_both_release_the_entry() {
        // There is no timeout here, so every park needs a matching exit or the
        // entry is a leak for the life of the session.
        let mut pending: PendingByLabel<u8> = PendingByLabel::new("inj");

        let resolved = pending.park(1);
        let abandoned = pending.park(2);
        assert_eq!(pending.outstanding(), 2);

        pending.take(&resolved);
        pending.forget(&abandoned);

        assert_eq!(pending.outstanding(), 0);
    }

    #[test]
    fn a_never_answered_label_stays_outstanding() {
        // The honest statement of the lifecycle: parking without an answer keeps
        // the entry. Callers that can fail before the request leaves must
        // `forget`, and this is the test that would notice if that ever stopped
        // being true.
        let mut pending: PendingByLabel<u8> = PendingByLabel::new("up");
        pending.park(1);

        assert_eq!(pending.outstanding(), 1);
    }

    #[test]
    fn sequence_does_not_reuse_a_label_after_resolution() {
        // The counter never rewinds, so a late echo of an old label cannot be
        // mistaken for a fresh request that happens to sit in the same slot.
        let mut pending: PendingByLabel<u8> = PendingByLabel::new("act");

        let first = pending.park(1);
        pending.take(&first);
        let second = pending.park(2);

        assert_ne!(first, second);
    }
}
