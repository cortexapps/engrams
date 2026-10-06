//! Shared teardown tasks retain ownership when a caller is cancelled.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use futures::future::{BoxFuture, Shared};
use futures::FutureExt;

use crate::SandboxError;

pub type TeardownResult = Result<(), Arc<SandboxError>>;
pub type TeardownFuture = Shared<BoxFuture<'static, TeardownResult>>;

/// One task per resident sandbox until teardown completes.
/// The runtime drives each task even when all callers stop waiting.
pub struct TeardownRegistry<K> {
    tasks: Arc<Mutex<HashMap<K, TeardownFuture>>>,
}

impl<K> Default for TeardownRegistry<K> {
    fn default() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl<K: Clone + Eq + Hash + Send + 'static> TeardownRegistry<K> {
    pub fn run_or_join(
        &self,
        key: K,
        make: impl FnOnce() -> BoxFuture<'static, TeardownResult>,
    ) -> TeardownFuture {
        let mut tasks = self.tasks.lock().expect("teardown registry poisoned");
        if let Some(task) = tasks.get(&key) {
            return task.clone();
        }
        let work = make();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let result = async move {
            rx.await
                .unwrap_or_else(|_| Err(Arc::new(SandboxError::Vm("teardown task stopped".into()))))
        }
        .boxed()
        .shared();
        tasks.insert(key.clone(), result.clone());
        let registry = self.tasks.clone();
        tokio::spawn(async move {
            let outcome = std::panic::AssertUnwindSafe(work)
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    Err(Arc::new(SandboxError::Vm("teardown task panicked".into())))
                });
            // Publish completion and remove ownership under the same lock.
            let mut tasks = registry.lock().expect("teardown registry poisoned");
            let _ = tx.send(outcome);
            tasks.remove(&key);
        });
        result
    }

    pub fn join(&self, key: &K) -> Option<TeardownFuture> {
        self.tasks
            .lock()
            .expect("teardown registry poisoned")
            .get(key)
            .cloned()
    }

    pub fn contains(&self, key: &K) -> bool {
        self.tasks
            .lock()
            .expect("teardown registry poisoned")
            .contains_key(key)
    }

    pub fn keys(&self) -> Vec<K> {
        self.tasks
            .lock()
            .expect("teardown registry poisoned")
            .keys()
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_waiter_keeps_task_and_shared_error() {
        let registry = TeardownRegistry::default();
        let (release, parked) = tokio::sync::oneshot::channel();
        let first = registry.run_or_join(1, || {
            async move {
                parked.await.unwrap();
                Err(Arc::new(SandboxError::NotFound))
            }
            .boxed()
        });
        drop(first);
        assert!(registry.contains(&1));
        assert_eq!(registry.keys(), vec![1]);
        let second = registry.run_or_join(1, || panic!("duplicate teardown"));
        let third = registry.join(&1).unwrap();
        release.send(()).unwrap();
        let second = second.await.unwrap_err();
        let third = third.await.unwrap_err();
        assert!(Arc::ptr_eq(&second, &third));
        assert!(!registry.contains(&1));
    }
}
