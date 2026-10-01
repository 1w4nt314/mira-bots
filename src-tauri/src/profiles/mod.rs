//! Agent profiles (plan5 A.1): the model, the JSON store, the per-profile system prompt and the
//! shared context that emits `profiles-changed`.

pub mod model;
pub mod prompt;
pub mod store;

use std::sync::{Mutex, MutexGuard};

use crate::events::{EmitFn, PROFILES_CHANGED};
pub use model::{AgentProfile, Effort, ProfileError, ProfileKind, ProfileSnapshot, SpawnOverrides};
pub use store::ProfileStore;

/// The store behind a lock plus the emitter. The lock is taken alone and briefly (never together
/// with the manager or ticket lock) and never while emitting.
pub struct ProfilesCtx {
    store: Mutex<ProfileStore>,
    emit: EmitFn,
}

impl ProfilesCtx {
    pub fn new(store: ProfileStore, emit: EmitFn) -> Self {
        Self {
            store: Mutex::new(store),
            emit,
        }
    }

    fn lock(&self) -> MutexGuard<'_, ProfileStore> {
        self.store.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Runs `f` on the store under the lock (read-only use).
    pub fn read<T>(&self, f: impl FnOnce(&ProfileStore) -> T) -> T {
        f(&self.lock())
    }

    /// Runs `f` under the lock; on success emits `profiles-changed` with the full list after the
    /// lock is released.
    pub fn mutate<T, E>(&self, f: impl FnOnce(&mut ProfileStore) -> Result<T, E>) -> Result<T, E> {
        let (out, list) = {
            let mut s = self.lock();
            let out = f(&mut s)?;
            (out, s.list())
        };
        match serde_json::to_value(&list) {
            Ok(v) => (self.emit)(PROFILES_CHANGED, v),
            Err(e) => log::error!("serialize {PROFILES_CHANGED}: {e}"),
        }
        Ok(out)
    }

    pub fn get(&self, id: &str) -> Option<AgentProfile> {
        self.read(|s| s.get(id))
    }

    pub fn list(&self) -> Vec<AgentProfile> {
        self.read(|s| s.list())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::sync::Arc;

    #[test]
    fn mutate_emits_the_full_list_only_on_success() {
        let base = std::env::temp_dir().join(format!("mira-pctx-{}", uuid::Uuid::new_v4()));
        let events: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
        let e = Arc::clone(&events);
        let emit: EmitFn = Arc::new(move |n: &str, v: Value| {
            e.lock().unwrap().push((n.to_string(), v));
        });
        let ctx = ProfilesCtx::new(ProfileStore::load(base.join("p"), 1), emit);
        assert_eq!(ctx.list().len(), 7);
        let err: Result<(), ProfileError> = ctx.mutate(|s| s.delete("coder"));
        assert!(err.is_err());
        assert!(events.lock().unwrap().is_empty());
        ctx.mutate(|s| s.reset_builtin("coder", 2)).unwrap();
        let ev = events.lock().unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].0, "profiles-changed");
        assert_eq!(ev[0].1.as_array().unwrap().len(), 7);
        assert_eq!(ev[0].1[0]["id"], "coder");
        assert_eq!(ctx.get("coder").unwrap().updated_at, 2);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
