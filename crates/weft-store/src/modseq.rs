//! Server-global modification-sequence allocator + in-flight tracker (v0.12
//! SYNC, Part 2 — docs/architecture/namespace-membership-sync-v0.12.md).
//!
//! Every client-visible row is stamped with a monotonic `seq`; a client syncs
//! incrementally with `WHERE seq > cursor`. Seqs are reserved from the backend
//! counter in **batches** (amortizing the round-trip) and handed out one at a
//! time; each handed-out seq is tracked **in-flight** until its write finishes.
//!
//! The cursor a client receives is the **low-water mark**: `min(in-flight) − 1`,
//! or `max(completed)` when nothing is in flight. This is the one guarantee a
//! naive `max(seq)` can't give — under concurrent writes a lower seq can still
//! be uncommitted when a higher one commits, and a cursor past it would skip
//! that row forever. Biasing stale (a slightly-low cursor) is harmless: the
//! client re-receives a few rows and upsert absorbs them (Part 2.2).
//!
//! The wire cursor is opaque `epoch:seq` (Part 2.4): the epoch is bumped on any
//! restore-from-backup that could reuse seq values, so a stale-epoch cursor is
//! treated as cursor-less (full resync) instead of silently losing data.

use std::collections::{BTreeSet, VecDeque};
use std::sync::Mutex;

/// The allocator + in-flight tracker. Backend-agnostic: a backend refills the
/// reservation pool via [`stock`](Self::stock) (Postgres fetches a `nextval`
/// batch; memory hands out from an atomic counter) and brackets each write with
/// [`take`](Self::take) → [`complete`](Self::complete)/[`abort`](Self::abort).
pub struct ModSeq {
    inner: Mutex<Inner>,
}

struct Inner {
    /// Reserved from the counter, not yet handed to a write.
    pool: VecDeque<i64>,
    /// Handed out, write not yet finished — the in-flight set (ordered so the
    /// minimum is O(log n)).
    in_flight: BTreeSet<i64>,
    /// Highest **completed** (committed) seq; advances as writes finish.
    max_completed: i64,
    /// The sync epoch — the wire cursor is `epoch:seq`.
    epoch: String,
}

impl ModSeq {
    /// Create a tracker for a given sync epoch, with an empty reservation pool.
    pub fn new(epoch: String) -> Self {
        Self {
            inner: Mutex::new(Inner {
                pool: VecDeque::new(),
                in_flight: BTreeSet::new(),
                max_completed: 0,
                epoch,
            }),
        }
    }

    /// Refill the reservation pool with a freshly reserved batch (monotonic,
    /// disjoint from every prior batch — the backend counter guarantees this).
    pub fn stock(&self, seqs: impl IntoIterator<Item = i64>) {
        let mut inner = self.inner.lock().expect("modseq lock");
        inner.pool.extend(seqs);
    }

    /// True when the reservation pool is empty and the caller must `stock` a
    /// fresh batch before the next `take` can succeed.
    pub fn needs_stock(&self) -> bool {
        self.inner.lock().expect("modseq lock").pool.is_empty()
    }

    /// Take the next reserved seq and mark it in-flight. `None` when the pool is
    /// empty — the caller reserves a batch, `stock`s it, and retries.
    pub fn take(&self) -> Option<i64> {
        let mut inner = self.inner.lock().expect("modseq lock");
        let seq = inner.pool.pop_front()?;
        inner.in_flight.insert(seq);
        Some(seq)
    }

    /// A write committed: drop its seq from in-flight and advance the mark.
    pub fn complete(&self, seq: i64) {
        let mut inner = self.inner.lock().expect("modseq lock");
        inner.in_flight.remove(&seq);
        if seq > inner.max_completed {
            inner.max_completed = seq;
        }
    }

    /// A write failed/rolled back before committing: drop its seq from in-flight
    /// **without** advancing the mark. The seq becomes a permanent gap — safe to
    /// skip, since nothing was written at it.
    pub fn abort(&self, seq: i64) {
        let mut inner = self.inner.lock().expect("modseq lock");
        inner.in_flight.remove(&seq);
    }

    /// The low-water-mark cursor seq: every seq ≤ this is committed. Held back
    /// to `min(in-flight) − 1` so an uncommitted lower seq is never skipped.
    pub fn cursor_seq(&self) -> i64 {
        let inner = self.inner.lock().expect("modseq lock");
        match inner.in_flight.iter().next() {
            Some(&min) => min - 1,
            None => inner.max_completed,
        }
    }

    /// The opaque wire cursor `epoch:seq` at the current low-water mark. Clients
    /// store and echo it verbatim (Part 2.4).
    pub fn cursor_token(&self) -> String {
        let inner = self.inner.lock().expect("modseq lock");
        let seq = match inner.in_flight.iter().next() {
            Some(&min) => min - 1,
            None => inner.max_completed,
        };
        format!("{}:{}", inner.epoch, seq)
    }

    /// This server's current sync epoch.
    pub fn epoch(&self) -> String {
        self.inner.lock().expect("modseq lock").epoch.clone()
    }

    /// Split a wire cursor into `(epoch, seq)`. Returns `None` if malformed —
    /// callers treat that as a cursor-less fresh sync.
    pub fn parse_cursor(token: &str) -> Option<(&str, i64)> {
        let (epoch, seq) = token.rsplit_once(':')?;
        let seq = seq.parse().ok()?;
        Some((epoch, seq))
    }

    /// The seq floor a `since=<cursor>` delta should serve from: the parsed seq
    /// when the cursor's epoch matches ours, else `None` — a stale-epoch cursor
    /// (restore-from-backup) forces a full resync, never a silent empty delta.
    pub fn delta_floor(&self, token: &str) -> Option<i64> {
        let inner = self.inner.lock().expect("modseq lock");
        let (epoch, seq) = Self::parse_cursor(token)?;
        (epoch == inner.epoch).then_some(seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_holds_back_for_out_of_order_commits() {
        // Acceptance test #8: concurrent writes commit out of seq order → no
        // client ever misses the slow write's row.
        let ms = ModSeq::new("e1".into());
        ms.stock(1..=5);

        // Three writes start; nothing committed yet.
        let a = ms.take().unwrap(); // 1
        let b = ms.take().unwrap(); // 2
        let c = ms.take().unwrap(); // 3
        assert_eq!((a, b, c), (1, 2, 3));
        assert_eq!(ms.cursor_seq(), 0); // min in-flight (1) − 1

        // The highest commits first — the cursor must NOT advance past the
        // still-pending lower seqs.
        ms.complete(c);
        assert_eq!(ms.cursor_seq(), 0); // still held at 1 − 1

        ms.complete(a);
        assert_eq!(ms.cursor_seq(), 1); // 2 still in flight → 2 − 1

        ms.complete(b);
        assert_eq!(ms.cursor_seq(), 3); // nothing in flight → max completed
    }

    #[test]
    fn aborted_seq_is_a_harmless_gap() {
        let ms = ModSeq::new("e1".into());
        ms.stock(1..=3);
        let a = ms.take().unwrap(); // 1
        let b = ms.take().unwrap(); // 2
        ms.complete(b); // 2 committed, 1 still pending
        assert_eq!(ms.cursor_seq(), 0);

        // 1's write rolled back — it never commits. The cursor must be free to
        // advance past the gap to the highest committed seq.
        ms.abort(a);
        assert_eq!(ms.cursor_seq(), 2);
    }

    #[test]
    fn refills_when_pool_drains() {
        let ms = ModSeq::new("e1".into());
        assert!(ms.needs_stock());
        assert_eq!(ms.take(), None);
        ms.stock(10..=11);
        assert!(!ms.needs_stock());
        assert_eq!(ms.take(), Some(10));
        assert_eq!(ms.take(), Some(11));
        assert!(ms.needs_stock());
        assert_eq!(ms.take(), None);
    }

    #[test]
    fn cursor_token_and_epoch_gating() {
        let ms = ModSeq::new("epochABC".into());
        ms.stock(1..=2);
        let s = ms.take().unwrap();
        ms.complete(s);
        assert_eq!(ms.cursor_token(), "epochABC:1");

        // A matching-epoch cursor yields its seq floor.
        assert_eq!(ms.delta_floor("epochABC:1"), Some(1));
        // A stale epoch (restore-from-backup) → None = full resync.
        assert_eq!(ms.delta_floor("oldEpoch:99"), None);
        // Malformed → None.
        assert_eq!(ms.delta_floor("garbage"), None);

        assert_eq!(ModSeq::parse_cursor("e:42"), Some(("e", 42)));
        assert_eq!(ModSeq::parse_cursor("e:notnum"), None);
    }
}
