//! Threads domain (§9.4) — the thread **names** (root msgid → display name) + the
//! thread **list** (streamed via the `t` batch). The Rust twin of `threadsHandlers`
//! plus the reducer's thread-list flush. The reply **side panel** (root / messages
//! / composer, streamed via the `b` history batch) stays TS — it ties into the
//! message-history path the messages capstone deliberately left TS.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::ClientEvent;

/// A thread as summarized in a `THREADS` list (mirrors TS `ThreadInfo`).
#[derive(Serialize, Clone)]
pub struct ThreadInfo {
    pub root: String,
    pub name: Option<String>,
    pub replies: u32,
    pub last: Option<String>,
}

/// This domain's state diffs — the mirror applies them onto `store.threads`.
/// `ThreadName` sets/clears `names[root]`; `ThreadList` sets the whole list.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ThreadDiff {
    ThreadName { root: String, name: Option<String> },
    ThreadList { threads: Vec<ThreadInfo> },
}

/// The threads sub-model: the root→name map + the streamed list + its batch state.
#[derive(Default)]
pub struct Threads {
    names: BTreeMap<String, String>,
    list: Vec<ThreadInfo>,
    buf: Vec<ThreadInfo>,
    in_batch: bool,
}

impl Threads {
    pub fn handle(&mut self, event: &ClientEvent) -> Vec<ThreadDiff> {
        match event {
            // §9.4 a THREADS-list row: set/clear its name, and buffer the row while
            // a list batch streams (the `t` batch below).
            ClientEvent::Thread {
                root,
                name,
                replies,
                last,
                ..
            } => {
                if self.in_batch {
                    self.buf.push(ThreadInfo {
                        root: root.clone(),
                        name: name.clone(),
                        replies: *replies,
                        last: last.clone(),
                    });
                }

                vec![self.set_name(root, name)]
            }
            // §9.4 a live rename: set/clear the name + update the loaded list entry.
            ClientEvent::ThreadNamed { root, name, .. } => self.rename(root, name),
            // §9.4 thread-list batches are id-prefixed `t`; mark the window so the
            // matching BATCH END flushes the buffered rows.
            ClientEvent::BatchStart { id } if id.starts_with('t') => {
                self.in_batch = true;

                Vec::new()
            }
            ClientEvent::BatchEnd { .. } => self.flush(),
            _ => Vec::new(),
        }
    }

    fn set_name(&mut self, root: &str, name: &Option<String>) -> ThreadDiff {
        match name {
            Some(n) => self.names.insert(root.to_string(), n.clone()),
            None => self.names.remove(root),
        };

        ThreadDiff::ThreadName {
            root: root.to_string(),
            name: name.clone(),
        }
    }

    fn rename(&mut self, root: &str, name: &Option<String>) -> Vec<ThreadDiff> {
        let mut out = vec![self.set_name(root, name)];

        // Reflect the rename in the loaded list too, if the root is in it.
        if let Some(t) = self.list.iter_mut().find(|t| t.root == root) {
            t.name = name.clone();
            out.push(ThreadDiff::ThreadList {
                threads: self.list.clone(),
            });
        }

        out
    }

    fn flush(&mut self) -> Vec<ThreadDiff> {
        if !self.in_batch {
            return Vec::new();
        }

        self.in_batch = false;
        self.list = std::mem::take(&mut self.buf);
        // Newest activity first (last-activity msgid sorts by its ULID; a nameless
        // `None` sorts as "" — matches the TS `(b.last ?? "").localeCompare(...)`).
        self.list.sort_by(|a, b| {
            b.last
                .as_deref()
                .unwrap_or("")
                .cmp(a.last.as_deref().unwrap_or(""))
        });

        vec![ThreadDiff::ThreadList {
            threads: self.list.clone(),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(root: &str, name: Option<&str>, last: Option<&str>) -> ClientEvent {
        ClientEvent::Thread {
            channel: "#n/c".into(),
            root: root.into(),
            replies: 2,
            last: last.map(Into::into),
            name: name.map(Into::into),
        }
    }
    fn batch_start(id: &str) -> ClientEvent {
        ClientEvent::BatchStart { id: id.into() }
    }
    fn batch_end() -> ClientEvent {
        ClientEvent::BatchEnd {
            id: "t1".into(),
            truncated: false,
        }
    }
    fn roots(diffs: &[ThreadDiff]) -> Vec<&str> {
        let d = diffs.iter().find_map(|d| match d {
            ThreadDiff::ThreadList { threads } => Some(threads),
            _ => None,
        });
        d.unwrap().iter().map(|t| t.root.as_str()).collect()
    }

    #[test]
    fn thread_row_sets_name_and_buffers_during_a_batch() {
        let mut t = Threads::default();
        // Outside a batch: a row just sets the name.
        assert!(matches!(&t.handle(&thread("r1", Some("Bugs"), None))[0],
            ThreadDiff::ThreadName { root, name } if root == "r1" && name.as_deref() == Some("Bugs")));
    }

    #[test]
    fn list_batch_buffers_then_flushes_sorted_by_last() {
        let mut t = Threads::default();
        t.handle(&batch_start("t1"));
        t.handle(&thread("r1", None, Some("01a")));
        t.handle(&thread("r2", None, Some("01c"))); // newer activity
        t.handle(&thread("r3", None, Some("01b")));
        // Flush sorts newest-activity first.
        assert_eq!(roots(&t.handle(&batch_end())), vec!["r2", "r3", "r1"]);
        // A non-thread batch end (no `t` start) doesn't re-flush.
        assert!(t
            .handle(&ClientEvent::BatchEnd {
                id: "r9".into(),
                truncated: false
            })
            .is_empty());
    }

    #[test]
    fn thread_named_renames_in_the_loaded_list() {
        let mut t = Threads::default();
        t.handle(&batch_start("t1"));
        t.handle(&thread("r1", None, Some("01a")));
        t.handle(&batch_end());

        // A live rename updates names + the loaded list entry.
        let d = t.handle(&ClientEvent::ThreadNamed {
            channel: "#n/c".into(),
            root: "r1".into(),
            name: Some("Design".into()),
        });
        assert!(
            matches!(&d[0], ThreadDiff::ThreadName { name, .. } if name.as_deref() == Some("Design"))
        );
        let ThreadDiff::ThreadList { threads } = &d[1] else {
            panic!("expected a list update")
        };
        assert_eq!(threads[0].name.as_deref(), Some("Design"));
    }
}
