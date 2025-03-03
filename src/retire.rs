use core::marker::PhantomData;
#[cfg(not(feature = "check-loom"))]
use core::sync::atomic::{Ordering, fence};

#[cfg(feature = "check-loom")]
use loom::sync::atomic::{Ordering, fence};

use super::{HAZARDS, HazardBag};

type Retired = (*mut (), unsafe fn(*mut ()));

/// Thread-local list of retired pointers.
#[derive(Debug)]
pub struct RetiredSet<'s> {
    hazards: &'s HazardBag,
    /// The first element of the pair is the machine representation of the pointer and the second
    /// is the function pointer to `free::<T>` where `T` is the type of the object.
    inner: Vec<Retired>,
    _marker: PhantomData<*const ()>, // !Send + !Sync
}

impl<'s> RetiredSet<'s> {
    /// The max length of retired pointer list. `collect` is triggered when `THRESHOLD` pointers
    /// are retired.
    const THRESHOLD: usize = 64;

    /// Create a new retired pointer list protected by the given `HazardBag`.
    pub fn new(hazards: &'s HazardBag) -> Self {
        Self {
            hazards,
            inner: Vec::new(),
            _marker: PhantomData,
        }
    }

    /// Retires a pointer.
    ///
    /// # Safety
    ///
    /// * `pointer` must be removed from shared memory before calling this function, and must be
    ///   valid.
    /// * The same `pointer` should only be retired once.
    ///
    /// # Note
    ///
    /// `T: Send` is not required because the retired pointers are not sent to other threads.
    pub unsafe fn retire<T>(&mut self, pointer: *mut T) {
        /// Frees a pointer. This function is defined here instead of `collect()` as we know about
        /// the type of `pointer` only at the time of retiring it.
        ///
        /// # Safety
        ///
        /// * Subsumes the safety requirements of [`Box::from_raw`]. In particular, one must have
        ///   unique ownership to `data`.
        ///
        /// [`Box::from_raw`]: https://doc.rust-lang.org/std/boxed/struct.Box.html#method.from_raw
        unsafe fn free<T>(data: *mut ()) {
            drop(unsafe { Box::from_raw(data.cast::<T>()) })
        }
        self.inner.push((pointer.cast(), free::<T>));
        if self.inner.len() >= Self::THRESHOLD {
            self.collect();
        }
    }

    /// Free the pointers that are `retire`d by the current thread and not `protect`ed by any other
    /// threads.
    pub fn collect(&mut self) {
        let hazerd_ptrs = self.hazards.all_hazards();
        let mut can_free = Vec::new();
        self.inner.retain(|(ptr, deleter)| {
            if hazerd_ptrs.contains(ptr) {
                true
            } else {
                can_free.push((*ptr, *deleter));
                false
            }
        });
        for (ptr, deleter) in can_free {
            unsafe { deleter(ptr) };
        }
    }
}

impl Default for RetiredSet<'static> {
    fn default() -> Self {
        Self::new(&HAZARDS)
    }
}

// this triggers loom internal bug
#[cfg(not(feature = "check-loom"))]
impl Drop for RetiredSet<'_> {
    fn drop(&mut self) {
        // In a production-quality implementation of hazard pointers, the remaining local retired
        // pointers will be moved to a global list of retired pointers, which are then reclaimed by
        // the other threads. For pedagogical purposes, here we simply wait for all retired pointers
        // are no longer protected.
        while !self.inner.is_empty() {
            self.collect();
        }
    }
}

#[cfg(all(test, not(feature = "check-loom")))]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::rc::Rc;

    use super::{HazardBag, RetiredSet};

    // retire `THRESHOLD` pointers to trigger collection
    #[test]
    fn retire_threshold_collect() {
        struct Tester(Rc<RefCell<HashSet<usize>>>, usize);
        impl Drop for Tester {
            fn drop(&mut self) {
                let _ = self.0.borrow_mut().insert(self.1);
            }
        }
        let hazards = HazardBag::new();
        let mut retires = RetiredSet::new(&hazards);
        let freed = Rc::new(RefCell::new(HashSet::new()));
        for i in 0..RetiredSet::THRESHOLD {
            unsafe { retires.retire(Box::leak(Box::new(Tester(freed.clone(), i)))) };
        }
        let freed = Rc::try_unwrap(freed).unwrap().into_inner();

        assert_eq!(freed, (0..RetiredSet::THRESHOLD).collect())
    }
}
