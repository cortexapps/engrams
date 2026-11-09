//! Records the guest memory chunks a VM needs first, by position.

use std::fs::File;
use std::io::Write as _;
use std::path::Path;
use std::sync::Mutex;

use engram_chunk_store::FirstTouch;

/// Each chunk index once, in order, with the time the guest first faulted
/// on it.
///
/// The previous run's published prefix is *carried*: its chunks are
/// recorded like any other, when the guest touches them, so a chunk the
/// guest no longer needs leaves the next prefix. The eager sweep leaves
/// carried chunks to the guest's faults ([`Self::awaits_guest`]), so an
/// install does not hide a touch; the prefix fill makes them resident in
/// the chunk cache, so such a fault is a local read. The host-agent
/// decides where the published prefix ends (its first completed run).
///
/// With an output file, each entry is appended to the file as it is
/// recorded, so the file always holds the whole record (in
/// [`FirstTouch`]'s encoding).
pub struct FirstTouchRecorder {
    inner: Mutex<Inner>,
}

struct Inner {
    seen: Vec<bool>,
    carried: Vec<bool>,
    record: FirstTouch,
    output: Option<File>,
}

impl FirstTouchRecorder {
    pub fn new(chunks: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                seen: vec![false; chunks],
                carried: vec![false; chunks],
                record: FirstTouch::default(),
                output: None,
            }),
        }
    }

    /// Write the record to `path` (replacing what it holds) and append each
    /// later entry. A write failure is logged and ends the output; the file
    /// then holds whole entries only.
    pub fn set_output(&self, path: &Path) {
        let mut inner = self.inner.lock().expect("first-touch lock");
        let opened = File::create(path).and_then(|mut file| {
            file.write_all(&inner.record.encode())?;
            Ok(file)
        });
        match opened {
            Ok(file) => inner.output = Some(file),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "first-touch output");
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// Note a fault on chunk `idx` now. Later notes on the same chunk,
    /// and an index past the end, are ignored.
    pub fn touch(&self, idx: usize) {
        self.touch_at(idx, || chrono::Utc::now().timestamp_millis());
    }

    /// Carry the previous run's published prefix. An index past the end
    /// is ignored.
    pub fn carry(&self, indices: &[u32]) {
        let mut inner = self.inner.lock().expect("first-touch lock");
        for &idx in indices {
            if let Some(slot) = inner.carried.get_mut(idx as usize) {
                *slot = true;
            }
        }
    }

    /// True for a carried chunk the guest has not touched: a producer
    /// that installs it now hides the guest's fault.
    pub fn awaits_guest(&self, idx: usize) -> bool {
        let inner = self.inner.lock().expect("first-touch lock");
        inner.carried.get(idx).copied().unwrap_or(false) && !inner.seen[idx]
    }

    /// `(carried, touched)`: the chunks of the carried prefix, and how many
    /// of them the guest has touched.
    pub fn carried_use(&self) -> (usize, usize) {
        let inner = self.inner.lock().expect("first-touch lock");
        let carried = inner.carried.iter().filter(|c| **c);
        let touched = inner
            .carried
            .iter()
            .zip(&inner.seen)
            .filter(|(c, s)| **c && **s);
        (carried.count(), touched.count())
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
        let entry = (idx as u32, now_ms());
        inner.record.entries.push(entry);
        let Some(file) = inner.output.as_mut() else {
            return;
        };
        let bytes = FirstTouch {
            entries: vec![entry],
        }
        .encode();
        if let Err(e) = file.write_all(&bytes) {
            tracing::warn!(error = %e, "first-touch output write; output ends");
            let whole = (inner.record.entries.len() - 1) * bytes.len();
            if let Some(file) = inner.output.take() {
                let _ = file.set_len(whole as u64);
            }
        }
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

    #[test]
    fn a_carried_chunk_is_recorded_only_when_touched() {
        let r = FirstTouchRecorder::new(8);
        r.touch_at(5, || 1);
        r.carry(&[3, 5, 1, 99]);
        r.touch_at(1, || 2);
        r.touch_at(6, || 3);
        let indices: Vec<u32> = r.snapshot().entries.iter().map(|e| e.0).collect();
        assert_eq!(indices, vec![5, 1, 6], "carried chunk 3 was never touched");
        assert_eq!(r.carried_use(), (3, 2));
    }

    #[test]
    fn carried_chunks_await_the_guest_until_it_touches_them() {
        let r = FirstTouchRecorder::new(4);
        r.carry(&[1, 2]);
        r.touch_at(2, || 1);
        assert!(r.awaits_guest(1));
        assert!(!r.awaits_guest(2), "the guest touched it");
        assert!(!r.awaits_guest(3), "not carried");
        assert!(!r.awaits_guest(9));
        r.touch_at(1, || 2);
        assert!(!r.awaits_guest(1));
        assert_eq!(r.snapshot().entries, vec![(2, 1), (1, 2)]);
    }

    #[test]
    fn the_output_file_holds_every_entry_as_it_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FirstTouch::FILE);
        let r = FirstTouchRecorder::new(4);
        r.touch_at(2, || 10);
        r.set_output(&path);
        r.touch_at(0, || 11);
        let on_disk = FirstTouch::decode(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(on_disk, r.snapshot());
        r.touch_at(3, || 12);
        let on_disk = FirstTouch::decode(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(on_disk.entries, vec![(2, 10), (0, 11), (3, 12)]);
    }
}
