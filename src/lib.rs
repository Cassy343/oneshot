//! Oneshot spsc channel working both in and between sync and async environments.

#![deny(rust_2018_idioms)]

use core::fmt;
use core::mem;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

pub fn sync_channel<T>() -> (SyncSender<T>, SyncReceiver<T>) {
    // Allocate the state on the heap and initialize it with `states::init()` and get the pointer.
    // The last endpoint of the channel to be alive is responsible for freeing the state.
    let state = Box::into_raw(Box::new(AtomicUsize::new(states::init())));
    (
        SyncSender {
            state,
            _marker: std::marker::PhantomData,
        },
        SyncReceiver {
            state,
            _marker: std::marker::PhantomData,
        },
    )
}

#[derive(Debug, Eq, PartialEq, Hash)]
pub struct SyncSender<T> {
    state: *mut AtomicUsize,
    _marker: std::marker::PhantomData<T>,
}

#[derive(Debug, Eq, PartialEq, Hash)]
pub struct SyncReceiver<T> {
    state: *mut AtomicUsize,
    _marker: std::marker::PhantomData<T>,
}

unsafe impl<T: Send> Send for SyncSender<T> {}
unsafe impl<T: Send> Send for SyncReceiver<T> {}

impl<T> SyncSender<T> {
    /// Sends `value` over the channel to the [`Receiver`].
    /// Returns an error if the receiver was dropped before the send took place. The value can
    /// be extracted from the error again.
    pub fn send(self, value: T) -> Result<(), DroppedReceiverError<T>> {
        let state_ptr = self.state;
        // Don't run our Drop implementation if send was called, any cleanup now happens here
        mem::forget(self);

        // Put the value on the heap and get the pointer. If sending succeeds the receiver is
        // responsible for freeing it, otherwise we do that
        let value_ptr = Box::into_raw(Box::new(value));

        // Store the address to the value in the state and read out what state the receiver is in
        let state = unsafe { &*state_ptr }.swap(value_ptr as usize, Ordering::SeqCst);
        if state == states::init() {
            // The receiver is alive and has not started waiting. Send done
            // Receiver frees state and value from heap
            Ok(())
        } else if state == states::dropped() {
            // The receiver was already dropped. We are responsible for freeing the state and value
            unsafe { Box::from_raw(state_ptr) };
            Err(DroppedReceiverError(unsafe { Box::from_raw(value_ptr) }))
        } else {
            // The receiver is waiting. Wake it up so it can return the value. The receiver frees
            // the state, the value and the thread instance in the state
            unsafe { &*(state as *const thread::Thread) }.unpark();
            Ok(())
        }
    }
}

impl<T> Drop for SyncSender<T> {
    fn drop(&mut self) {
        let state = unsafe { &*self.state }.swap(states::dropped(), Ordering::SeqCst);
        if state == states::init() {
            // The receiver has not started waiting, nor is it dropped. Nothing to do
        } else if state == states::dropped() {
            // The receiver was already dropped. We are responsible for freeing the state
            unsafe { Box::from_raw(self.state) };
        } else {
            // The receiver started waiting. Wake it up so it can detect it has been cancelled
            unsafe { &*(state as *const thread::Thread) }.unpark();
        }
    }
}

impl<T> SyncReceiver<T> {
    pub fn recv(self) -> Result<T, DroppedSenderError> {
        let state_ptr = self.state;
        // Don't run our Drop implementation if we are receiving
        mem::forget(self);

        let state = unsafe { &*state_ptr }.load(Ordering::SeqCst);
        if state == states::init() {
            // The sender is alive but has not sent anything yet.
            // Put our thread object on the heap and in the state so the sender can unpark us.
            // We are always responsible for freeing the heap allocated thread object.
            let thread_ptr = Box::into_raw(Box::new(thread::current()));
            let state = unsafe { &*state_ptr }.compare_and_swap(
                states::init(),
                thread_ptr as usize,
                Ordering::SeqCst,
            );
            if state == states::init() {
                // We stored our thread, now we park until the sender has changed the state
                loop {
                    thread::park();
                    // Check if the sender updated the state
                    let state = unsafe { &*state_ptr }.load(Ordering::SeqCst);
                    if state != thread_ptr as usize {
                        // The sender updated the state. It was either dropped or sent something
                        unsafe { Box::from_raw(thread_ptr) };
                        unsafe { Box::from_raw(state_ptr) };
                        if state == states::dropped() {
                            break Err(DroppedSenderError(()));
                        } else {
                            break Ok(unsafe { *Box::from_raw(state as *mut T) });
                        }
                    }
                }
            } else if state == states::dropped() {
                // The sender was dropped while we prepared to park
                unsafe { Box::from_raw(thread_ptr) };
                unsafe { Box::from_raw(state_ptr) };
                Err(DroppedSenderError(()))
            } else {
                // The sender sent data while we prepared to park. We free everything
                unsafe { Box::from_raw(thread_ptr) };
                unsafe { Box::from_raw(state_ptr) };
                Ok(unsafe { *Box::from_raw(state as *mut T) })
            }
        } else if state == states::dropped() {
            // The sender was already dropped before sending anything. We free the state
            unsafe { Box::from_raw(state_ptr) };
            Err(DroppedSenderError(()))
        } else {
            // The sender already sent data. We free the state and the value
            unsafe { Box::from_raw(state_ptr) };
            Ok(unsafe { *Box::from_raw(state as *mut T) })
        }
    }
}

impl<T> Drop for SyncReceiver<T> {
    fn drop(&mut self) {
        let state = unsafe { &*self.state }.swap(states::dropped(), Ordering::SeqCst);
        if state == states::init() {
            // The sender has not sent anything, nor is it dropped. The sender is responsible for
            // freeing the state
        } else if state == states::dropped() {
            // The sender was already dropped. We are responsible for freeing the state
            unsafe { Box::from_raw(self.state) };
        } else {
            // The sender already sent something. We must free it, and our state
            unsafe { Box::from_raw(self.state) };
            unsafe { Box::from_raw(state as *mut T) };
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct DroppedSenderError(());

impl fmt::Display for DroppedSenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "Oneshot sender dropped without sending anything".fmt(f)
    }
}

impl std::error::Error for DroppedSenderError {}

pub struct DroppedReceiverError<T>(pub Box<T>);

impl<T: Eq> Eq for DroppedReceiverError<T> {}
impl<T: PartialEq> PartialEq for DroppedReceiverError<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T> DroppedReceiverError<T> {
    #[inline]
    pub fn into_value(self) -> T {
        *self.0
    }
}

impl<T> fmt::Display for DroppedReceiverError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "Oneshot receiver has already been dropped".fmt(f)
    }
}

impl<T> fmt::Debug for DroppedReceiverError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DroppedReceiverError<{}>(_)", stringify!(T))
    }
}

impl<T> std::error::Error for DroppedReceiverError<T> {}

mod states {
    static INIT: u8 = 1u8;
    static DROPPED: u8 = 2u8;

    /// Returns a memory address in integer form representing the initial state of a channel.
    /// This state is active while both the sender and receiver are still alive, no value
    /// has yet been sent and the receiver has not started receiving.
    ///
    /// The value is guaranteed to:
    /// * be the same for every call in the same process
    /// * be different from what `states::dropped` returns
    /// * and never equal a pointer returned from `Box::into_raw`.
    #[inline(always)]
    pub fn init() -> usize {
        &INIT as *const u8 as usize
    }

    /// Returns a memory address in integer form representing a channel where one or both ends
    /// have been dropped.
    ///
    /// The value is guaranteed to:
    /// * be the same for every call in the same process
    /// * be different from what `states::init` returns
    /// * and never equal a pointer returned from `Box::into_raw`.
    #[inline(always)]
    pub fn dropped() -> usize {
        &DROPPED as *const u8 as usize
    }
}

#[cfg(test)]
mod tests {
    use std::{mem, thread, time::Duration};

    #[test]
    fn send_with_dropped_receiver() {
        let (sender, receiver) = crate::sync_channel();
        mem::drop(receiver);
        let send_error = sender.send(5u128).unwrap_err();
        assert_eq!(send_error, crate::DroppedReceiverError(Box::new(5)));
        assert_eq!(send_error.into_value(), 5);
    }

    #[test]
    fn recv_with_dropped_sender() {
        let (sender, receiver) = crate::sync_channel::<u128>();
        mem::drop(sender);
        receiver.recv().unwrap_err();
    }

    #[test]
    fn send_before_recv() {
        let (sender, receiver) = crate::sync_channel();
        assert!(sender.send(19i128).is_ok());
        assert_eq!(receiver.recv(), Ok(19i128));
    }

    #[test]
    fn recv_before_send() {
        let (sender, receiver) = crate::sync_channel();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(1));
            sender.send(9u128).unwrap();
        });
        assert_eq!(receiver.recv(), Ok(9));
    }

    #[test]
    fn recv_before_send_then_drop_sender() {
        let (sender, receiver) = crate::sync_channel::<u128>();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(1));
            mem::drop(sender);
        });
        assert!(receiver.recv().is_err());
    }

    #[test]
    fn send_then_drop_receiver() {
        let (sender, receiver) = crate::sync_channel();
        assert!(sender.send(19i128).is_ok());
        mem::drop(receiver);
    }
}
