//! Records the guest memory chunks a VM needs first, by position.

use std::fs::File;
use std::io::Write as _;
use std::path::Path;
use std::sync::Mutex;

use engram_chunk_store::FirstTouch;

/// Each chunk index once, in order, with the time it was recorded: a chunk
/// the guest faults on, and a chunk the previous run's records name (its
/// published prefix, and its hash trace as the trace prefault installs
/// it), because a producer that installs a chunk early hides the guest's
/// fault on it.
///
/// With an output file, each entry is appended to the file as it is
/// recorded, so the file always holds the whole record (in
/// [`FirstTouch`]'s encoding).
pub struct FirstTouchRecorder {
    inner: Mutex<Inner>,
}

struct Inner {
    seen: Vec<bool>,
    record: FirstTouch,
    output: Option<File>,
}

impl FirstTouchRecorder {
    pub fn new(chunks: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                seen: vec![false; chunks],
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

    /// Note a fault (or a trace install) on chunk `idx` now. Later notes on
    /// the same chunk, and an index past the end, are ignored.
    pub fn touch(&self, idx: usize) {
        self.touch_at(idx, || chrono::Utc::now().timestamp_millis());
    }

    /// Note every chunk of the previous run's published prefix now, in its
    /// order.
    pub fn carry(&self, indices: &[u32]) {
        let now = chrono::Utc::now().timestamp_millis();
        for &idx in indices {
            self.touch_at(idx as usize, || now);
        }
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
    fn a_carried_prefix_keeps_its_order_and_skips_seen_chunks() {
        let r = FirstTouchRecorder::new(8);
        r.touch_at(5, || 1);
        r.carry(&[3, 5, 1, 99]);
        r.touch_at(1, || i64::MAX);
        r.touch_at(6, || i64::MAX);
        let indices: Vec<u32> = r.snapshot().entries.iter().map(|e| e.0).collect();
        assert_eq!(indices, vec![5, 3, 1, 6]);
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
