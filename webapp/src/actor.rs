//! A minimal actor: state owned by one tokio task, driven by a mailbox.
//!
//! There are no locks in this server. Every piece of state — each symbol's
//! exchange and book, the market that clears the trades, the stream's
//! sequence counter, the rate limiter — belongs to exactly one task, and
//! anyone who wants to read or change it sends that task a job to run. Jobs
//! are closures over the state, sent through an unbounded channel and run
//! one after another in the order they arrived, so the state is only ever
//! touched from one thread at a time and never waits on a mutex.
//!
//! Two ways to talk to an actor:
//!
//! * [`Actor::send`] posts a job and does not wait. It is synchronous — it
//!   can be called from inside another actor's job — and it never blocks.
//! * [`Actor::call`] posts a job and waits for what it returns.
//!   [`Actor::call_async`] is the same for a job that itself has to wait on
//!   something, such as another actor.
//!
//! The one rule that keeps this deadlock-free is that **calls only go one
//! way**. The market actor calls the symbol actors; the symbol actors call
//! nobody. See [`crate::market`] for the full picture.

use std::future::Future;
use std::pin::Pin;

use tokio::sync::{mpsc, oneshot};

/// A job for the actor: borrows the state, may await, returns nothing to the
/// mailbox (a reply, if any, travels on a `oneshot` the job captured).
type Job<S> = Box<dyn for<'a> FnOnce(&'a mut S) -> BoxFuture<'a, ()> + Send>;

/// A boxed future that borrows for `'a`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The actor stopped before it could answer: its task has ended, which
/// happens only if it panicked or every handle to it was dropped while a
/// call was in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gone;

impl std::fmt::Display for Gone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the actor has stopped")
    }
}

impl std::error::Error for Gone {}

/// The answer to a [`Actor::request`], still on its way.
pub struct Reply<R> {
    answer: Option<oneshot::Receiver<R>>,
}

impl<R> Future for Reply<R> {
    type Output = Result<R, Gone>;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match self.answer.as_mut() {
            None => std::task::Poll::Ready(Err(Gone)),
            Some(rx) => Pin::new(rx).poll(cx).map(|r| r.map_err(|_| Gone)),
        }
    }
}

/// A handle to a running actor. Cloning it does not clone the state: every
/// handle addresses the same task. The task ends when the last handle is
/// dropped and its mailbox is empty.
pub struct Actor<S> {
    tx: mpsc::UnboundedSender<Job<S>>,
}

impl<S> Clone for Actor<S> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

impl<S: Send + 'static> Actor<S> {
    /// Move `state` into a new task and hand back its address. Needs a tokio
    /// runtime to spawn on.
    pub fn spawn(state: S) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Job<S>>();
        tokio::spawn(async move {
            let mut state = state;
            while let Some(job) = rx.recv().await {
                job(&mut state).await;
            }
        });
        Self { tx }
    }

    /// Post a job and carry on. Never blocks and never waits; the job runs
    /// after everything posted before it.
    pub fn send(&self, f: impl FnOnce(&mut S) + Send + 'static) {
        // A closed mailbox means the actor is gone; there is nobody left to
        // tell, and a fire-and-forget job has nowhere to report to.
        let _ = self.tx.send(Box::new(move |s| {
            f(s);
            Box::pin(async {})
        }));
    }

    /// Post a job now and hand back the reply to wait for later. Posting
    /// several before waiting on any is how work is fanned out to several
    /// actors at once.
    pub fn request<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut S) -> R + Send + 'static,
    ) -> Reply<R> {
        let (reply, answer) = oneshot::channel();
        let posted = self
            .tx
            .send(Box::new(move |s| {
                let r = f(s);
                let _ = reply.send(r);
                Box::pin(async {})
            }))
            .is_ok();
        Reply {
            answer: posted.then_some(answer),
        }
    }

    /// Post a job and wait for its result.
    pub async fn call<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut S) -> R + Send + 'static,
    ) -> Result<R, Gone> {
        self.request(f).await
    }

    /// Post a job that has to wait on something itself — another actor,
    /// typically — and wait for its result. The actor runs nothing else
    /// until the job is done.
    pub async fn call_async<R: Send + 'static>(
        &self,
        f: impl for<'a> FnOnce(&'a mut S) -> BoxFuture<'a, R> + Send + 'static,
    ) -> Result<R, Gone> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(Box::new(move |s| {
                Box::pin(async move {
                    let r = f(s).await;
                    let _ = reply.send(r);
                })
            }))
            .map_err(|_| Gone)?;
        answer.await.map_err(|_| Gone)
    }

    /// Wait until every job posted before this one has run. A way to know a
    /// [`send`](Self::send) has taken effect.
    pub async fn flush(&self) -> Result<(), Gone> {
        self.call(|_| ()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn jobs_run_in_order_and_calls_answer() {
        let actor = Actor::spawn(Vec::<u32>::new());
        actor.send(|v| v.push(1));
        actor.send(|v| v.push(2));
        let seen = actor.call(|v| v.clone()).await.unwrap();
        assert_eq!(seen, [1, 2]);
        let doubled = actor
            .call_async(|v| {
                Box::pin(async move {
                    v.push(3);
                    v.iter().map(|x| x * 2).collect::<Vec<_>>()
                })
            })
            .await
            .unwrap();
        assert_eq!(doubled, [2, 4, 6]);
    }

    #[tokio::test]
    async fn an_actor_can_call_another_from_inside_a_job() {
        let inner = Actor::spawn(10u32);
        let outer = Actor::spawn(inner.clone());
        let sum = outer
            .call_async(|inner| Box::pin(async move { inner.call(|n| *n + 5).await.unwrap() }))
            .await
            .unwrap();
        assert_eq!(sum, 15);
    }

    #[tokio::test]
    async fn a_dropped_actor_reports_gone() {
        let actor = Actor::spawn(0u8);
        let handle = actor.clone();
        drop(actor);
        assert_eq!(handle.call(|n| *n).await, Ok(0));
        let stopped = {
            let (tx, _rx) = mpsc::unbounded_channel::<Job<u8>>();
            drop(_rx);
            Actor { tx }
        };
        assert_eq!(stopped.call(|n| *n).await, Err(Gone));
    }
}
