//! FIFO implemented using the `nb` non-blocking I/O API.
//!
//! As this is meant to be used in a `no_std` environment, there are no heap allocations. This
//! requires extra lifetime parameters to explicitly keep references alive for the lifetime of
//! channel senders and receivers.

use nb;
use core::cell::UnsafeCell;
use core::mem::swap;
use bare_metal::CriticalSection;

/// Marker trait implemented on objects that represent a non-reentrant context for the function
/// being called.
pub unsafe trait NonReentrant {
}

unsafe impl NonReentrant for CriticalSection {
}

/// Token created for situations where the type has statically ensured non-reentrancy
struct NonReentrantToken {
    _0: (),
}

impl NonReentrantToken {
    /// Creates a new non-reentrant token
    unsafe fn new() -> Self {
        NonReentrantToken { _0: () }
    }
}

unsafe impl NonReentrant for NonReentrantToken {
}

/// Non-blocking FIFO
///
/// Alone, calling the `recv` and `send` methods on the channel requires that the caller guarantee
/// that those functions won't be called reentrantly. In order to remove this restriction, the
/// `split` function can be called to split out the channel into a separate `Sender` and `Receiver`
/// who use `&mut self` to guarantee non-reentrancy.
#[derive(Debug)]
pub struct Channel<'a, T: 'a> {
    buffer: UnsafeCell<&'a mut [Option<T>]>,
    send_index: UnsafeCell<usize>,
    recv_index: UnsafeCell<usize>,
}

impl<'a, T: 'a> Channel<'a, T> {
    /// Creates a new channel
    ///
    /// The passed buffer will be borrowed for the lifetime of this channel and will serve as the
    /// shared storage between the sender and receiver.
    ///
    /// To aid in optimization, the length of the slice should be a power of 2. But it doesn't have
    /// to be.
    pub fn new(buffer: &'a mut [Option<T>]) -> Self {
        // clear the buffer
        for el in buffer.iter_mut() {
            *el = None;
        }
        // create the channel
        Channel {
            buffer: UnsafeCell::new(buffer),
            send_index: UnsafeCell::new(0),
            recv_index: UnsafeCell::new(0),
        }
    }

    /// Returns the length of the channel buffer
    ///
    /// Note that the actual number of items that can be pending in the channel is 1 less than this
    /// value.
    pub fn len(&self) -> usize {
        // This is safe because our slice's len won't change
        unsafe { (*self.buffer.get()).len() }
    }

    /// Receives from the channel
    ///
    /// This returns a `T` if successful, otherwise it returns a `WouldBlock`. It is only
    /// unsuccessful if the Channel is empty.
    ///
    /// This requires a guarantee by the caller that this function will not be called reentrantly.
    pub fn recv(&self, _nr: &NonReentrant) -> nb::Result<T, !> {
        // This is safe because if recv is called, the fifo can only go from empty to non-empty.
        // Since we have a guarantee from the caller that this function won't be caled reentrantly,
        // we can depend on send_index not changing.
        //
        // From a data safety standpoint, this is safe because it is an atomic read with no side
        // effects.
        let empty = unsafe { *self.send_index.get() == *self.recv_index.get() };
        if empty {
            Err(nb::Error::WouldBlock)
        }
        else {
            let mut val: Option<T> = None;
            unsafe {
                let index = *self.recv_index.get();
                // This is safe because the recv_index and send_index don't reference the same item
                // in the array unless it is empty (in which case we already returned WouldBlock).
                // Between then and now, the buffer could not have become empty.
                swap(&mut val, &mut (*self.buffer.get())[index]);
                *self.recv_index.get() = (index + 1) % self.len();
            }
            match val {
                None => Err(nb::Error::WouldBlock),
                Some(v) => Ok(v),
            }
        }
    }

    /// Sends a value to the channel
    ///
    /// This returns a `()` if successful, otherwise it returns a `WouldBlock`. It is only
    /// unsuccessful if the Channel is full.
    ///
    /// This requires a guarantee by the caller that this function will not be called reentrantly.
    pub fn send(&self, value: T, _nr: &NonReentrant) -> nb::Result<(), !> {
        // This is safe because if send is called, the fifo can only go from full to non-full.
        // Since we have a guarantee from the caller that this function won't be called
        // reentrantly, we can depend on recv_index not changing.
        //
        // From a data safety standpoint, this is safe because it is an atomic read with no side
        // effects.
        let full = unsafe { ((*self.send_index.get()) + 1) % (self.len()) == *self.recv_index.get() };
        if full {
            Err(nb::Error::WouldBlock)
        }
        else {
            let mut val: Option<T> = Some(value);
            // This is all safe because this function is not called reentrantly.
            unsafe {
                let index = *self.send_index.get();
                swap(&mut val, &mut (*self.buffer.get())[index]);
                *self.send_index.get() = (index + 1) % self.len();
            }
            Ok(())
        }
    }
}

impl<'a, 'b: 'a, T: 'b> Channel<'b, T> {
    /// Builds a sender and receiver for this channel
    ///
    /// The mutable borrow of self in this function will be for as long as the lifetimes of the
    /// receiver and sender. This ensures that the Channel stays in one place in memory while the sender
    /// and receiver are doing their thing.
    ///
    /// The sender and receiver are not clonable. Due to this property they remove the requirement
    /// for the caller to provide a `NonReentrant` to the `send` and `recv` functions.
    pub fn split(&'a mut self) -> (Receiver<'a, 'b, T>, Sender<'a, 'b, T>) {
        (Receiver::new(self), Sender::new(self))
    }
}

unsafe impl<'a, T: 'a> Sync for Channel<'a, T> {
}

/// Channel Receiver removing the need for a `NonReentrant`
///
/// The Channel Receiver is not clonable. Since it uses a `&mut self` on the `recv` function, it
/// eliminates the need for the caller to explicitly guarantee non-reentrancy when calling the
/// `recv` function.
#[derive(Debug)]
pub struct Receiver<'a, 'b: 'a, T: 'b> {
    inner: &'a Channel<'b, T>,
}

impl<'a, 'b: 'a, T: 'b> Receiver<'a, 'b, T> {
    /// Creates a new receiver from a channel reference
    ///
    /// The caller should ensure that two `Receiver`s exist for the same channel at the same
    /// time.
    fn new(channel: &'a Channel<'b, T>) -> Self {
        Receiver { inner: channel }
    }

    /// Receives an item from the Channel
    ///
    /// This returns a `T` if successful, otherwise it returns a `WouldBlock`. It is only
    /// unsuccessful if the Channel is empty.
    ///
    /// This does not require a `NonReentrant` guarantee since the `&mut self` is guarantee enough.
    pub fn recv(&mut self) -> nb::Result<T, !> {
        // This is safe because only one receiver can exist at a time for each channel and due to
        // the &mut self this function can only be called once at a time, thus satisfying the
        // precondition for a NonReentrant.
        let nr = unsafe { NonReentrantToken::new() };
        self.inner.recv(&nr)
    }
}

unsafe impl<'a, 'b: 'a, T: 'b + Send> Send for Receiver<'a, 'b, T> {
}

impl<'a, 'b: 'a, T: 'b> !Sync for Receiver<'a, 'b, T> {
}

/// Channel Sender removing the need for a `NonReentrant`
///
/// The Channel Sender is not clonable. Since it uses a `&mut self` on the `send` function, it
/// eliminates the need for the caller to explicitly guarantee non-reentrancy when calling the
/// `send` function.
#[derive(Debug)]
pub struct Sender<'a, 'b: 'a, T: 'b> {
    inner: &'a Channel<'b, T>,
}

impl<'a, 'b: 'a, T: 'b> Sender<'a, 'b, T> {
    /// Creates a new sender from a channel reference
    ///
    /// The caller should ensure that no two `Sender`s exist for the same channel at the same time.
    fn new(channel: &'a Channel<'b, T>) -> Self {
        Sender { inner: channel }
    }

    /// Sends an item on the channel.
    ///
    /// This returns a `()` if successful, otherwise it returns a `WouldBlock`. It is only
    /// unsuccessful if the Channel is full.
    ///
    /// This does not require a `NonReentrant` guarantee since the `&mut self` is guarantee enough.
    pub fn send(&mut self, value: T) -> nb::Result<(), !> {
        // Since the sender is not Clone, no CriticalSection is required. We guarantee due to &mut
        // self that this function can only be called once at a time, thus satisfying the
        // precondition for a NonReentrant.
        let nr = unsafe { CriticalSection::new() };
        self.inner.send(value, &nr)
    }
}

unsafe impl<'a, 'b: 'a, T: 'b + Send> Send for Sender<'a, 'b, T> {
}

impl<'a, 'b: 'a, T: 'b> !Sync for Sender<'a, 'b, T> {
}

#[cfg(test)]
mod tests {
    use super::*;
    use nb;

    #[test]
    fn basic() {
        let mut arry: [Option<u8>; 4] = [None; 4];
        let len = arry.len();
        let mut channel = Channel::new(&mut arry);
        assert_eq!(channel.len(), len);
        let (mut receiver, mut sender) = channel.split();
        assert_eq!(receiver.recv(), Err(nb::Error::WouldBlock));
        assert_eq!(sender.send(4), Ok(()));
        assert_eq!(receiver.recv(), Ok(4));
        assert_eq!(receiver.recv(), Err(nb::Error::WouldBlock));
    }

    #[test]
    fn sync_blocking() {
        let mut arry: [Option<u8>; 4] = [None; 4];
        let len = arry.len();
        let mut channel = Channel::new(&mut arry);
        assert_eq!(channel.len(), len);
        let (mut receiver, mut sender) = channel.split();
        for _rep in 0..10 {
            for i in 0..(len - 1) {
                println!("Sending");
                assert_eq!(sender.send(i as u8), Ok(()));
            }
            assert_eq!(sender.send(255), Err(nb::Error::WouldBlock));
            assert_eq!(receiver.recv(), Ok(0));
            assert_eq!(sender.send(255), Ok(()));
            for i in 1..(len - 1) {
                assert_eq!(receiver.recv(), Ok(i as u8));
            }
            assert_eq!(receiver.recv(), Ok(255));
            assert_eq!(receiver.recv(), Err(nb::Error::WouldBlock));
        }
    }
}

