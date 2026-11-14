use std::fmt;
use std::sync::Arc;

/// Wakes the UI loop when a background thread (Evernote worker, Vim PTY reader, Vim exit
/// watcher) has something new for it, so the loop can sleep instead of polling.
#[derive(Clone, Default)]
pub struct Waker(Option<Arc<dyn Fn() + Send + Sync>>);

impl Waker {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(wake)))
    }

    /// For callers that poll on their own, such as tests.
    #[must_use]
    pub fn noop() -> Self {
        Self(None)
    }

    pub fn wake(&self) {
        if let Some(wake) = &self.0 {
            wake();
        }
    }
}

impl fmt::Debug for Waker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_some() {
            "Waker"
        } else {
            "Waker(noop)"
        })
    }
}
