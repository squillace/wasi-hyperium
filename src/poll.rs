use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    task::{Context, Poll, Wake, Waker},
};

use crate::wasi::traits::{WasiPoll, WasiPollable};

/// A PollableRegistry manages the polling of Pollables in relation to some
/// Rust async executor. This must be a cheaply-`clone`able handle to its
/// underlying state.
pub trait PollableRegistry: Clone + Unpin {
    type Pollable: WasiPollable;
    type RegisteredPollable: Unpin;

    /// Registers the given pollable to be polled. When the pollable is ready
    /// the the given context's waker should be called. The pollable must be
    /// immediately dropped when the returned RegisteredPollable is dropped.
    fn register_pollable(
        &self,
        cx: &mut Context,
        pollable: Self::Pollable,
    ) -> Self::RegisteredPollable;

    /// Poll all pollables. Returns false if there are no active pollables.
    fn poll(&self) -> bool;

    /// Runs the given future to completion, polling any WASI pollables that
    /// are registered with this registry. Returns Err(Stalled) if there are no
    /// active pollables while the future is pending.
    fn block_on<T>(&self, fut: impl std::future::Future<Output = T>) -> Result<T, Stalled> {
        let mut fut = std::pin::pin!(fut);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        loop {
            if let Poll::Ready(val) = fut.as_mut().poll(&mut cx) {
                return Ok(val);
            }
            if !self.poll() {
                return Err(Stalled);
            }
        }
    }
}

pub struct Poller<Pollable: WasiPoll> {
    entries: Arc<Mutex<HashMap<u32, Entry<Pollable>>>>,
}

struct Entry<Pollable: WasiPollable> {
    pollable: Weak<Pollable>,
    waker: Waker,
}

impl<Pollable: WasiPoll> PollableRegistry for Poller<Pollable> {
    type Pollable = Pollable;
    type RegisteredPollable = Arc<Pollable>;

    fn register_pollable(
        &self,
        cx: &mut Context,
        pollable: Self::Pollable,
    ) -> Self::RegisteredPollable {
        let handle = pollable.handle();
        let pollable = Arc::new(pollable);
        let entry = Entry {
            pollable: Arc::downgrade(&pollable),
            waker: cx.waker().clone(),
        };
        self.entries.lock().unwrap().insert(handle, entry);
        pollable
    }

    fn poll(&self) -> bool {
        let mut entries = self.entries.lock().unwrap();

        // Remove any dropped pollables
        entries.retain(|_, entry| entry.pollable.strong_count() > 0);

        if entries.is_empty() {
            return false;
        }

        // Poll pollables
        let pollables = entries
            .values()
            .filter_map(|entry| entry.pollable.upgrade())
            .collect::<Vec<_>>();
        let pollable_refs = pollables.iter().map(|p| p.as_ref()).collect::<Vec<_>>();
        let ready_idxs = Pollable::poll(&pollable_refs);

        // Remove and wake any ready pollables
        for idx in ready_idxs {
            let idx: usize = idx.try_into().unwrap();
            let handle = pollables[idx].handle();
            let entry = entries.remove(&handle).unwrap();
            entry.waker.wake();
        }
        true
    }
}

impl<Pollable: WasiPoll> Clone for Poller<Pollable> {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
        }
    }
}

impl<Pollable: WasiPoll> Default for Poller<Pollable> {
    fn default() -> Self {
        Self {
            entries: Default::default(),
        }
    }
}

pub fn noop_waker() -> Waker {
    struct NoopWaker;
    impl Wake for NoopWaker {
        fn wake(self: Arc<Self>) {}
    }
    Arc::new(NoopWaker).into()
}

#[derive(Debug)]
pub struct Stalled;

impl std::fmt::Display for Stalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "future stalled with no remaining pollables")
    }
}

impl std::error::Error for Stalled {}
