//! Park/unpark executor for WinFsp callback threads.
//!
//! WinFsp calls back on its own dispatcher threads, which cannot host an
//! async runtime. Every LayerFS service future wakes its poller by unparking
//! it (the same contract the Linux receive loops rely on), so blocking here
//! never spins and never spawns.
use std::{
    future::Future,
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
    thread,
};

struct Parker(thread::Thread);

impl Wake for Parker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Runs `future` to completion on the calling thread.
pub(super) fn block_on<T>(future: impl Future<Output = T>) -> T {
    let mut future = Box::pin(future);
    let waker = Waker::from(Arc::new(Parker(thread::current())));
    let mut context = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => thread::park(),
        }
    }
}
