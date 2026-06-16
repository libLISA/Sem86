use std::fmt::Debug;
use std::mem::offset_of;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use log::error;

struct Inner {
    pointer_is_valid: AtomicBool,
    refcount: AtomicU32,
}

// Make sure the count is placed on its own cache line.
#[repr(align(128))]
pub struct Intr {
    /// Only public so codegeneration can compute the offset.
    pub(crate) count: AtomicU32,
    inner: Arc<Inner>,
}

impl Debug for Intr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Intr").field("pending", &self.any_pending()).finish()
    }
}

impl Default for Intr {
    fn default() -> Self {
        Self::new()
    }
}

impl Intr {
    pub fn new() -> Self {
        Self {
            count: AtomicU32::new(0),
            inner: Arc::new(Inner {
                pointer_is_valid: AtomicBool::new(true),
                refcount: AtomicU32::new(0),
            }),
        }
    }

    #[inline(always)]
    pub fn any_pending(&self) -> bool {
        self.count.load(Ordering::Relaxed) > 0
    }

    pub fn handle(me: Pin<&Self>) -> IntrHandle {
        IntrHandle {
            count_ptr: me.count.as_ptr(),
            inner: me.inner.clone(),
        }
    }
}

impl Drop for Intr {
    fn drop(&mut self) {
        self.inner.pointer_is_valid.store(false, Ordering::Release);

        // Wait until there are no references
        while self.inner.refcount.load(Ordering::Acquire) > 0 {
            std::hint::spin_loop()
        }
    }
}

pub struct EarlyIntr {
    ptr: *const Intr,
    inner: Arc<Inner>,
}

impl EarlyIntr {
    /// # Safety
    ///
    /// You must ensure `ptr` is allocated memory suitable for `Intr` (alignment, etc.).
    /// You must ensure `ptr` is the location you will later construct `Intr`.
    /// You must construct the `Intr` via the [`build`] function.
    pub unsafe fn from_ptr(ptr: *const Intr) -> Self {
        unsafe {
            // SAFETY: Computed pointer is within the bounds of Intr.
            // SAFETY: The caller of this function ensures that an Intr will be constructed later on in this location. We early-initialize it now to ensure IntrHandles do not write to uninitialized memory.
            (ptr.byte_add(offset_of!(Intr, count)) as *mut u32).write(0);
        }

        Self {
            ptr,
            inner: Arc::new(Inner {
                pointer_is_valid: AtomicBool::new(true),
                refcount: AtomicU32::new(0),
            }),
        }
    }

    /// # Safety
    ///
    /// See `from_ptr`.
    pub unsafe fn handle(&self) -> IntrHandle {
        IntrHandle {
            // SAFETY: Computed pointer is within the bounds of Intr.
            count_ptr: self.compute_count_ptr(),
            inner: self.inner.clone(),
        }
    }

    fn compute_count_ptr(&self) -> *mut u32 {
        unsafe { self.ptr.byte_add(offset_of!(Intr, count)) as *mut u32 }
    }

    /// Builds the Intr structure.
    ///
    /// Any interrupts that have already been requested via any created IntrHandles are ignored.
    pub fn build(&self) -> Intr {
        Intr {
            // This might miss some pending requests.
            // There might be a panic when a pending request is dropped if the count is incorrect. (see [`PendingRequest::drop`])
            count: AtomicU32::new(unsafe { (*(self.compute_count_ptr() as *mut AtomicU32)).load(Ordering::SeqCst) }),
            inner: self.inner.clone(),
        }
    }
}

#[derive(Clone)]

pub struct IntrHandle {
    count_ptr: *mut u32,
    inner: Arc<Inner>,
}

unsafe impl Send for IntrHandle {}
unsafe impl Sync for IntrHandle {}

impl Debug for IntrHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IntrHandle").field("pending", &self.any_pending()).finish()
    }
}

impl IntrHandle {
    fn with<T>(&self, f: impl FnOnce(&mut AtomicU32) -> T) -> Option<T> {
        if self.inner.pointer_is_valid.load(Ordering::Acquire) {
            self.inner.refcount.fetch_add(1, Ordering::Release);
            // SAFETY: We ensure that `Intr::drop` will hang if `refcount` > 0, so that we can safely perform this access.
            let result = f(unsafe { &mut *(self.count_ptr as *mut AtomicU32) });
            self.inner.refcount.fetch_sub(1, Ordering::Release);
            Some(result)
        } else {
            error!("Attempted to use INTR pointer after free");
            None
        }
    }

    pub fn request(&self) -> PendingRequest {
        self.with(|count| count.fetch_add(1, Ordering::Relaxed));
        PendingRequest(self.clone())
    }

    fn any_pending(&self) -> bool {
        self.with(|count| count.load(Ordering::Relaxed) > 0).unwrap_or(false)
    }

    /// Returns a pointer to the count.
    ///
    /// This is a utility method that is only used for an extra check in [`crate::emulator::EmulatorContext`].
    ///
    /// This pointer may, at any point, be deallocated.
    /// You cannot safely read from it or write to it.
    /// If you want to increment this pointer, use [`Self::request`] instead.
    pub fn count_ptr(&self) -> *mut u32 {
        self.count_ptr
    }
}

pub struct PendingRequest(IntrHandle);

impl Debug for PendingRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PendingRequest").finish()
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        assert_ne!(self.0.with(|count| count.fetch_sub(1, Ordering::Relaxed)).unwrap_or(1), 0);
    }
}
