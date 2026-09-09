// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::marker::PhantomData;
use core::sync::atomic::{AtomicPtr, Ordering};

/// This is essentially an `AtomicPtr` but is guaranteed to always be valid
pub struct AtomicRef<T: 'static>(AtomicPtr<T>, PhantomData<&'static T>);

impl<T: 'static> AtomicRef<T> {
    pub const fn new(initial: &'static T) -> AtomicRef<T> {
        AtomicRef(AtomicPtr::new(initial as *const T as *mut T), PhantomData)
    }

    pub fn swap(&self, new: &'static T) -> &'static T {
        // We never allow storing anything but a `'static` reference so it's safe to
        // return it for the same.
        unsafe { &*self.0.swap(new as *const T as *mut T, Ordering::SeqCst) }
    }
}

impl<T: 'static> core::ops::Deref for AtomicRef<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        // We never allow storing anything but a `'static` reference so it's safe to lend
        // it out for any amount of time.
        unsafe { &*self.0.load(Ordering::SeqCst) }
    }
}
