//! Nonblocking synchronization structures
//!
//! This crate is designed for `no_std` applications where heap allocation is not possible. As
//! such, there is no dependency on the standard library and all allocations are the responsibility
//! of the caller.
//!
//! # The Mutex
//!
//! The [`Mutex`] provided here can be used to provide exclusive access to a value. Because of this
//! library's non-blocking nature, care must be exercised to avoid resource starvation. The lock
//! method requires a [`bare_metal::CriticalSection`].
//!
//! # The Channel
//!
//! The [`fifo::Channel`] provides a single-producer single-consumer queue which is `Sync` and can
//! be optionally split into a [`fifo::Sender`] and [`fifo::Receiver`] which are both `Send`. A key
//! difference between using the `Channel` by itself vs the `Sender` and `Receiver` together is
//! that the `Channel` requires a [`bare_metal::CriticalSection`] for several of its methods in
//! order to provide safety. The `Sender` and `Receiver` can be used without this requirement.
//!
//! ## Channel Examples
//!
//! There are two ways a [`fifo::Channel`] can be used:
//!
//! ### Direct usage
//!
//! Direct usage requires passing an object that implements [`fifo::NonReentrant`].
//!
//!
//! ```
//! extern crate bare_metal;
//! extern crate nb_sync;
//!
//! use nb_sync::fifo::Channel;
//!
//! //In an actual program this would be obtained safely
//! let cs = unsafe { bare_metal::CriticalSection::new() };
//!
//! let mut buffer: [Option<u8>; 4] = [None; 4];
//! let channel = Channel::new(&mut buffer);
//!
//! channel.send(10, &cs).unwrap();
//! channel.recv(&cs).unwrap();
//! ```
//!
//! ### Split into a sender and receiver
//!
//! This uses similar `send` and `recv` methods to the previous example, but does not require
//! a [`bare_metal::CriticalSection`].
//!
//! #### Method 1: Basic "send" with a clonable
//!
//! For clonable types, the `fifo::Sender::send` method can be used inside an `await!` directly.
//!
//! ```
//! extern crate nb;
//! extern crate nb_sync;
//!
//! use nb_sync::fifo::Channel;
//!
//! let mut buffer: [Option<u8>; 4] = [None; 4];
//! let mut channel = Channel::new(&mut buffer);
//!
//! let (mut receiver, mut sender) = channel.split();
//!
//! let clonable = 5;
//! // this loop is "await!(sender.send(clonable)).unwrap()"
//! loop {
//!     match sender.send(clonable) {
//!         Ok(()) => break Ok(()),
//!         Err(nb::Error::WouldBlock) => {},
//!         Err(nb::Error::Other(e)) => break Err(e),
//!     }
//! }.unwrap();
//!
//! // recv is also compatible with nb's await! macro
//! receiver.recv().unwrap();
//! ```
//!
//! #### Method 2: Sending with a completion
//!
//! Non-clonable types can be sent using the [`fifo::Sender::send_with_completion`] method. This is
//! based on the [`fifo::Sender::send_lossless`] method. A [`fifo::SendCompletion`] is used to make
//! this more directly usable with the `await!` macro. It takes ownership of the `Sender` and the
//! passed value for the duration of the sending process. When [`fifo::SendCompletion::done`] is
//! called the `Sender` will be returned along with an `Option` which contains the original value
//! if it was not ultimately sent.
//!
//! ```
//! extern crate nb;
//! extern crate nb_sync;
//!
//! use nb_sync::fifo::Channel;
//!
//! struct NonClone {
//!     _0: (),
//! }
//! impl NonClone {
//!     fn new() -> Self { NonClone { _0: () } }
//! }
//!
//! let mut buffer: [Option<NonClone>; 4] = [None, None, None, None];
//! let mut channel = Channel::new(&mut buffer);
//!
//! let (mut receiver, mut sender) = channel.split();
//!
//! let value = NonClone::new();
//! let completion = sender.send_with_completion(value);
//! // Completions can be aborted.
//! let (s, v) = completion.done();
//! sender = s;
//! let value = v.unwrap(); //the original, unsent value is returned here
//!
//! let mut completion = sender.send_with_completion(value);
//! // This loop is "await!(completion.poll()).unwrap()"
//! loop {
//!     match completion.poll() {
//!         Ok(()) => break Ok(()),
//!         Err(nb::Error::WouldBlock) => {},
//!         Err(nb::Error::Other(e)) => break Err(e),
//!     }
//! }.unwrap();
//!
//! let (s, v) = completion.done();
//! sender = s;
//! assert!(v.is_none()); //the value has been sent.
//!
//! receiver.recv().unwrap();
//! ```
//!


#![cfg_attr(not(test), no_std)]
#![feature(const_fn)]
#![feature(optin_builtin_traits)]
#![feature(never_type)]

extern crate nb;
extern crate bare_metal;

pub mod fifo;

// In test mode, std is available since we are compiling for x86
#[cfg(test)]
use std as core;

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use bare_metal::CriticalSection;

/// Mutex with interior mutability
///
/// This mutex assues that `panic!` is unrecoverable and crashes the program. This is a safe
/// assumption for this embedded application since the `panic!` transforms into two `udf`
/// instructions which result in a hard fault. If the user program incorporates any measures to
/// recover from this sort of hard fault, this mutex is no longer safe since it does not implement
/// the concept of "poisoning".
///
/// Since this mutex is polled and does not block, it is easy to have resource starvation occur.
/// Care should be taken as to where the `lock` function is called to allow other tasks to have an
/// opportunity to also grab the mutex.
pub struct Mutex<T> {
    data: UnsafeCell<T>,
    count: UnsafeCell<i32>,
}

impl<T> Mutex<T> {
    /// Creates a new mutex in the unlocked state
    pub const fn new(val: T) -> Self {
        Mutex { data: UnsafeCell::new(val), count: UnsafeCell::new(1) }
    }

    /// Attempts to lock the mutex
    ///
    /// Once this function returns a `MutexGuard`, all other ongoing calls to `try_lock` will fail
    /// until the `MutexGuard` is `Drop`d.
    pub fn lock(&self, _cs: &CriticalSection) -> nb::Result<MutexGuard<T>, !> {
        // This critical section ensures that the references to our interior are safe.
        unsafe {
            if *self.count.get() > 0 {
                *self.count.get() -= 1;
                Ok(MutexGuard::new(&self))
            }
            else {
                Err(nb::Error::WouldBlock)
            }
        }
    }

    /// Consumes this mutex and returns the underlying data
    ///
    /// This is statically safe since we consume `self`.
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }

    /// Unlocks this mutex
    ///
    /// This is unsafe because it is not thread-safe. The caller must ensure that they are the
    /// exclusive owner of this mutex's inner value. A `MutexGuard` is an example of an object that
    /// can make such a guarantee.
    unsafe fn unlock(&self) {
        *self.count.get() += 1;
    }
}

impl<T> From<T> for Mutex<T> {
    /// Creates a new mutex from a value in an unlocked state
    fn from(v: T) -> Mutex<T> {
        Mutex::new(v)
    }
}

unsafe impl<T> Sync for Mutex<T> {
}

/// Scoped mutex access. This will unlock the mutex when dropped.
pub struct MutexGuard<'a, T:'a> {
    mutex: &'a Mutex<T>,
}

impl<'a, T: 'a> MutexGuard<'a, T> {
    fn new(mutex: &'a Mutex<T>) -> Self {
        MutexGuard { mutex: mutex }
    }
}

impl<'a, T: 'a> Drop for MutexGuard<'a, T> {
    /// Releases this mutex guard
    fn drop(&mut self) {
        // Being the exclusive owner of the mutex, we can call the unlock method. Since this is
        // inside drop, we ensure that this is only called once by us.
        unsafe { self.mutex.unlock() }
    }
}

impl<'a, T: 'a> Deref for MutexGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<'a, T: 'a> DerefMut for MutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

#[cfg(test)]
mod tests {

}

