use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::SeqCst;
use std::error::Error;
use std::fmt;

use {Future, Poll, Async};
use lock::Lock;
use task::{self, Task};

/// A future representing the completion of a computation happening elsewhere in
/// memory.
///
/// This is created by the `oneshot` function.
#[must_use = "futures do nothing unless polled"]
pub struct Oneshot<T> {
    inner: Arc<Inner<T>>,
}

/// Represents the completion half of a oneshot through which the result of a
/// computation is signaled.
///
/// This is created by the `oneshot` function.
pub struct Complete<T> {
    inner: Arc<Inner<T>>,
}

// Internal state of the `Oneshot`/`Complete` pair above. This is all used as
// the internal synchronization between the two for send/recv operations.
//
// The `state` field is the primary state of the oneshot, with possible values
// in the constants below. Despite this, however, each field is also wrapped in
// a `Lock<T>` which each have a word for synchronization as well (the lock
// state). We're primarily using `Lock` as a "checked unsafe cell" for now while
// we vet the implementation (also means no `unsafe` below)`. Note that while
// `data` and `rx_task` can likely become `UnsafeCell` the `tx_task` field
// cannot, currently.
//
// The `data` holds the data that's going to be sent on this oneshot, the
// `rx_task` holds the receiver (`Oneshot`) task to wake up when data is sent or
// the `Complete` goes away. The `tx_task` is the transmitter (`Complete`) task
// to wake up when the `Oneshot` goes away.
//
// Also note that currently `tx_task
struct Inner<T> {
    complete: AtomicBool,
    data: Lock<Option<T>>,
    rx_task: Lock<Option<Task>>,
    tx_task: Lock<Option<Task>>,
}

/// Creates a new in-memory oneshot used to represent completing a computation.
///
/// A oneshot in this library is a concrete implementation of the `Future` trait
/// used to complete a computation from one location with a future representing
/// what to do in another.
///
/// This function is similar to Rust's channels found in the standard library.
/// Two halves are returned, the first of which is a `Complete` handle, used to
/// signal the end of a computation and provide its value. The second half is a
/// `Oneshot` which implements the `Future` trait, resolving to the value that
/// was given to the `Complete` handle.
///
/// Each half can be separately owned and sent across threads.
///
/// # Examples
///
/// ```
/// use std::thread;
/// use futures::*;
///
/// let (c, p) = oneshot::<i32>();
///
/// thread::spawn(|| {
///     p.map(|i| {
///         println!("got: {}", i);
///     }).wait();
/// });
///
/// c.complete(3);
/// ```
pub fn oneshot<T>() -> (Complete<T>, Oneshot<T>) {
    let inner = Arc::new(Inner {
        complete: AtomicBool::new(false),
        data: Lock::new(None),
        rx_task: Lock::new(None),
        tx_task: Lock::new(None),
    });
    let oneshot = Oneshot {
        inner: inner.clone(),
    };
    let complete = Complete {
        inner: inner,
    };
    (complete, oneshot)
}

impl<T> Complete<T> {
    /// Completes this oneshot with a successful result.
    ///
    /// This function will consume `self` and indicate to the other end, the
    /// `Oneshot`, that the error provided is the result of the computation this
    /// represents.
    pub fn complete(mut self, t: T) {
        // First up, flag that this method was called and then store the data.
        // Note that this lock acquisition should always succeed as it can only
        // interfere with `poll` in `Oneshot` which is only called when the
        // `complete` flag is true, which we're setting here.
        let mut slot = self.inner.data.try_lock().unwrap();
        assert!(slot.is_none());
        *slot = Some(t);
        drop(slot);
    }

    /// Polls this `Complete` half to detect whether the `Oneshot` this has
    /// paired with has gone away.
    ///
    /// This function can be used to learn about when the `Oneshot` (consumer)
    /// half has gone away and nothing will be able to receive a message sent
    /// from `complete`.
    ///
    /// Like `Future::poll`, this function will panic if it's not called from
    /// within the context of a task. In otherwords, this should only ever be
    /// called from inside another future.
    ///
    /// If `Ready` is returned then it means that the `Oneshot` has disappeared
    /// and the result this `Complete` would otherwise produce should no longer
    /// be produced.
    ///
    /// If `NotReady` is returned then the `Oneshot` is still alive and may be
    /// able to receive a message if sent. The current task, however, is
    /// scheduled to receive a notification if the corresponding `Oneshot` goes
    /// away.
    pub fn poll_cancel(&mut self) -> Poll<(), ()> {
        // Fast path up first, just read the flag and see if our other half is
        // gone. This flag is set both in our destructor and the oneshot
        // destructor, but our destructor hasn't run yet so if it's set then the
        // oneshot is gone.
        if self.inner.complete.load(SeqCst) {
            return Ok(Async::Ready(()))
        }

        // If our other half is not gone then we need to park our current task
        // and move it into the `notify_cancel` slot to get notified when it's
        // actually gone.
        //
        // If `try_lock` fails, then the `Oneshot` is in the process of using
        // it, so we can deduce that it's now in the process of going away and
        // hence we're canceled. If it succeeds then we just store our handle.
        //
        // Crucially we then check `oneshot_gone` *again* before we return.
        // While we were storing our handle inside `notify_cancel` the `Oneshot`
        // may have been dropped. The first thing it does is set the flag, and
        // if it fails to acquire the lock it assumes that we'll see the flag
        // later on. So... we then try to see the flag later on!
        let handle = task::park();
        match self.inner.tx_task.try_lock() {
            Some(mut p) => *p = Some(handle),
            None => return Ok(Async::Ready(())),
        }
        if self.inner.complete.load(SeqCst) {
            Ok(Async::Ready(()))
        } else {
            Ok(Async::NotReady)
        }
    }
}

impl<T> Drop for Complete<T> {
    fn drop(&mut self) {
        // Flag that we're a completed `Complete` and try to wake up a receiver.
        // Whether or not we actually stored any data will get picked up and
        // translated to either an item or cancellation.
        //
        // Note that if we fail to acquire the `rx_task` lock then that means
        // we're in one of two situations:
        //
        // 1. The receiver is trying to block in `poll`
        // 2. The receiver is being dropped
        //
        // In the first case it'll check the `complete` flag after it's done
        // blocking to see if it succeeded. In the latter case we don't need to
        // wake up anyone anyway. So in both cases it's ok to ignore the `None`
        // case of `try_lock` and bail out.
        self.inner.complete.store(true, SeqCst);
        if let Some(mut slot) = self.inner.rx_task.try_lock() {
            if let Some(task) = slot.take() {
                drop(slot);
                task.unpark();
            }
        }
    }
}

/// Error returned from a `Oneshot<T>` whenever the correponding `Complete<T>`
/// is dropped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Canceled;

impl fmt::Display for Canceled {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "oneshot canceled")
    }
}

impl Error for Canceled {
    fn description(&self) -> &str {
        "oneshot canceled"
    }
}


impl<T> Future for Oneshot<T> {
    type Item = T;
    type Error = Canceled;

    fn poll(&mut self) -> Poll<T, Canceled> {
        let mut done = false;

        // Check to see if some data has arrived. If it hasn't then we need to
        // block our task.
        //
        // Note that the acquisition of the `rx_task` lock might fail below, but
        // the only situation where this can happen is during `Complete::drop`
        // when we are indeed completed already. If that's happening then we
        // know we're completed so keep going.
        if self.inner.complete.load(SeqCst) {
            done = true;
        } else {
            let task = task::park();
            match self.inner.rx_task.try_lock() {
                Some(mut slot) => *slot = Some(task),
                None => done = true,
            }
        }

        // If we're `done` via one of the paths above, then look at the data and
        // figure out what the answer is. If, however, we stored `rx_task`
        // successfully above we need to check again if we're completed in case
        // a message was sent while `rx_task` was locked and couldn't notify us
        // otherwise.
        //
        // If we're not done, and we're not complete, though, then we've
        // successfully blocked our task and we return `NotReady`.
        if done || self.inner.complete.load(SeqCst) {
            match self.inner.data.try_lock().unwrap().take() {
                Some(data) => Ok(data.into()),
                None => Err(Canceled),
            }
        } else {
            Ok(Async::NotReady)
        }
    }
}

impl<T> Drop for Oneshot<T> {
    fn drop(&mut self) {
        // Indicate to the `Complete` that we're done, so any future calls to
        // `poll_cancel` are weeded out.
        self.inner.complete.store(true, SeqCst);

        // If we've blocked a task then there's no need for it to stick around,
        // so we need to drop it. If this lock acquisition fails, though, then
        // it's just because our `Complete` is trying to take the task, so we
        // let them take care of that.
        if let Some(mut slot) = self.inner.rx_task.try_lock() {
            let task = slot.take();
            drop(slot);
            drop(task);
        }

        // Finally, if our `Complete` wants to get notified of us going away, it
        // would have stored something in `tx_task`. Here we try to peel that
        // out and unpark it.
        //
        // Note that the `try_lock` here may fail, but only if the `Complete` is
        // in the process of filling in the task. If that happens then we
        // already flagged `complete` and they'll pick that up above.
        if let Some(mut handle) = self.inner.tx_task.try_lock() {
            if let Some(task) = handle.take() {
                drop(handle);
                task.unpark()
            }
        }
    }
}
