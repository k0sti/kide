//! A notification that a task can await: set by any caller, taken by one
//! waiting task.

use std::future::poll_fn;
use std::sync::{Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

#[derive(Default)]
pub struct Signal(Mutex<(bool, Option<Waker>)>);

impl Signal {
    fn lock(&self) -> MutexGuard<'_, (bool, Option<Waker>)> {
        // A poisoned lock only means a caller panicked; the flag is intact.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn notify(&self) {
        let waker = {
            let mut g = self.lock();
            g.0 = true;
            g.1.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
    }

    /// Takes the notification if set; otherwise wakes `cx` at the next.
    pub fn poll(&self, cx: &Context<'_>) -> bool {
        let mut g = self.lock();
        if std::mem::take(&mut g.0) {
            return true;
        }
        g.1 = Some(cx.waker().clone());
        false
    }

    pub async fn wait(&self) {
        poll_fn(|cx| match self.poll(cx) {
            true => Poll::Ready(()),
            false => Poll::Pending,
        })
        .await
    }
}
