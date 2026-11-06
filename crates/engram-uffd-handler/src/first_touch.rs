//! Records the guest memory chunks a VM faults first, by position.

use std::sync::Mutex;

use engram_chunk_store::FirstTouch;

/// Each chunk index once, in first-fault order, with the time of the fault.
pub struct FirstTouchRecorder {
    inner: Mutex<Inner>,
}

struct Inner {
    seen: Vec<bool>,
    record: FirstTouch,
}

impl FirstTouchRecorder {
    pub fn new(chunks: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                seen: vec![false; chunks],
                record: FirstTouch::default(),
            }),
        }
    }

    /// Note a fault on chunk `idx` now. Later faults on the same chunk, and
    /// an index past the end, are ignored.
    pub fn touch(&self, idx: usize) {
        self.touch_at(idx, || chrono::Utc::now().timestamp_millis());
    }

    fn touch_at(&self, idx: usize, now_ms: impl FnOnce() -> i64) {
        let mut inner = self.inner.lock().expect("first-touch lock");
        let Some(seen) = inner.seen.get_mut(idx) else {
            return;
        };
        if std::mem::replace(seen, true) {
            return;
        }
        // The time is read under the lock, so the entries stay in order.
        let at = now_ms();
        inner.record.entries.push((idx as u32, at));
    }

    pub fn snapshot(&self) -> FirstTouch {
        self.inner.lock().expect("first-touch lock").record.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_each_chunk_once_in_first_touch_order() {
        let r = FirstTouchRecorder::new(4);
        r.touch_at(2, || 10);
        r.touch_at(0, || 11);
        r.touch_at(2, || 12);
        r.touch_at(9, || 13);
        assert_eq!(r.snapshot().entries, vec![(2, 10), (0, 11)]);
        assert_eq!(r.snapshot().until(10), vec![2]);
    }
}
