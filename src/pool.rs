//! Lock-free-ish buffer pool that eliminates per-frame heap allocations.
//!
//! Typical usage:
//!
//! ```ignore
//! let pool = BufPool::<u32>::new();
//!
//! // Hot path (zero alloc after warmup):
//! let mut buf = pool.checkout(width * height);  // reuses a pooled Vec
//! fill_pixels(&mut buf);                        // write via DerefMut
//! let shared = buf.share();                     // freeze; now Arc-wrapped
//! let clone = shared.clone();                   // refcount bump only
//! drop(shared);
//! drop(clone);                                  // last ref → Vec returns to pool
//! ```

use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Pool
// ---------------------------------------------------------------------------

/// A pool of reusable `Vec<T>` buffers.
///
/// `Clone` is cheap (shared inner state). Safe to pass to multiple threads.
pub struct BufPool<T> {
    inner: Arc<Mutex<Vec<Vec<T>>>>,
}

impl<T> Clone for BufPool<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> BufPool<T> {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Check out an empty buffer with recycled capacity.
    ///
    /// The returned [`PoolBuf`] has length 0 but retains the heap
    /// allocation from a previous use.  Use [`PoolBuf::push`] to fill it,
    /// then [`.share()`](PoolBuf::share) to freeze.  Requires no trait
    /// bounds on `T`.
    pub fn checkout_empty(&self) -> PoolBuf<T> {
        let mut v = self
            .inner
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_default();
        v.clear();
        PoolBuf {
            inner: Some(v),
            pool: self.inner.clone(),
        }
    }
}

impl<T> SharedBuf<T> {
    /// Create a `SharedBuf` from a `Vec` without a pool.
    ///
    /// When dropped, the `Vec` is simply freed (there is no pool to return
    /// to). Useful for tests, sentinel values, and other non-hot-path code.
    pub fn from_vec(v: Vec<T>) -> Self {
        // Use an empty pool — the buffer will be "returned" to it, which
        // just means it sits in a pool nobody reads from and gets dropped
        // when the pool itself is dropped.
        let pool = Arc::new(Mutex::new(Vec::new()));
        Self(Arc::new(SharedInner { data: v, pool }))
    }
}

impl<T: Default + Clone> BufPool<T> {
    /// Check out a buffer of exactly `len` elements.
    ///
    /// If the pool has a buffer available it is reused (the `Vec`'s heap
    /// allocation is kept); otherwise a new `Vec` is allocated.  The
    /// returned buffer's contents are unspecified — callers must overwrite
    /// all elements before reading.
    pub fn checkout(&self, len: usize) -> PoolBuf<T> {
        let mut v = self
            .inner
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_default();
        // resize_with keeps existing capacity when len <= capacity.
        v.resize_with(len, T::default);
        PoolBuf {
            inner: Some(v),
            pool: self.inner.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Exclusively-owned pooled buffer
// ---------------------------------------------------------------------------

/// An exclusively-owned buffer checked out from a [`BufPool`].
///
/// Dereferences to `&[T]` / `&mut [T]`.  On drop, the underlying `Vec` is
/// returned to the pool.  Call [`.share()`](PoolBuf::share) to convert to a
/// cheaply-cloneable [`SharedBuf`].
pub struct PoolBuf<T> {
    inner: Option<Vec<T>>,
    pool: Arc<Mutex<Vec<Vec<T>>>>,
}

impl<T> Deref for PoolBuf<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.inner.as_ref().unwrap()
    }
}

impl<T> DerefMut for PoolBuf<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.inner.as_mut().unwrap()
    }
}

impl<T> Drop for PoolBuf<T> {
    fn drop(&mut self) {
        if let Some(mut v) = self.inner.take() {
            v.clear();
            self.pool.lock().unwrap().push(v);
        }
    }
}

impl<T> PoolBuf<T> {
    /// Append an element to the buffer.
    ///
    /// Intended for use with buffers obtained via
    /// [`BufPool::checkout_empty`].  Amortised O(1), reusing capacity
    /// from a previous pool cycle.
    pub fn push(&mut self, value: T) {
        self.inner.as_mut().unwrap().push(value);
    }

    /// Append a slice of elements to the buffer.
    ///
    /// Intended for use with buffers obtained via
    /// [`BufPool::checkout_empty`].  Reuses capacity from a previous
    /// pool cycle.
    pub fn push_slice(&mut self, values: &[T])
    where
        T: Copy,
    {
        self.inner.as_mut().unwrap().extend_from_slice(values);
    }

    /// Returns the number of elements in the buffer.
    pub fn len(&self) -> usize {
        self.inner.as_ref().unwrap().len()
    }

    /// Freeze this buffer into a shared, cheaply-cloneable handle.
    ///
    /// The returned [`SharedBuf`] can be cloned with only a reference-count
    /// bump (no data copy).  When the **last** clone is dropped the `Vec` is
    /// returned to the originating pool — not freed.
    pub fn share(mut self) -> SharedBuf<T> {
        let v = self.inner.take().unwrap();
        SharedBuf(Arc::new(SharedInner {
            data: v,
            pool: self.pool.clone(),
        }))
    }

    /// Truncate to `len` elements and freeze into a shared handle.
    ///
    /// Useful when the buffer was checked out at an estimated size and
    /// only partially filled.  The excess capacity is retained for reuse
    /// when the buffer is eventually returned to the pool.
    pub fn truncate_and_share(mut self, len: usize) -> SharedBuf<T> {
        let mut v = self.inner.take().unwrap();
        v.truncate(len);
        SharedBuf(Arc::new(SharedInner {
            data: v,
            pool: self.pool.clone(),
        }))
    }
}

// ---------------------------------------------------------------------------
// Shared (Arc-wrapped) pooled buffer
// ---------------------------------------------------------------------------

/// A reference-counted handle to a pooled buffer.
///
/// Cloning is a refcount bump.  When the last clone is dropped the
/// underlying `Vec<T>` is returned to the pool.
pub struct SharedBuf<T>(Arc<SharedInner<T>>);

impl<T: PartialEq> PartialEq<[T]> for SharedBuf<T> {
    fn eq(&self, other: &[T]) -> bool {
        &**self == other
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for SharedBuf<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

impl<T> Clone for SharedBuf<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}


impl<T> Deref for SharedBuf<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.0.data
    }
}

impl SharedBuf<u8> {
    /// Convert into a `bytes::Bytes` with zero copy.
    ///
    /// The `Bytes` takes ownership of this `SharedBuf`.  When the `Bytes`
    /// (and all its slices/clones) is dropped, the underlying `Vec<u8>` is
    /// returned to the originating pool.
    pub fn into_bytes(self) -> bytes::Bytes {
        bytes::Bytes::from_owner(self)
    }
}

impl AsRef<[u8]> for SharedBuf<u8> {
    fn as_ref(&self) -> &[u8] {
        &self.0.data
    }
}

struct SharedInner<T> {
    data: Vec<T>,
    pool: Arc<Mutex<Vec<Vec<T>>>>,
}

impl<T> Drop for SharedInner<T> {
    fn drop(&mut self) {
        let mut v = std::mem::take(&mut self.data);
        v.clear();
        self.pool.lock().unwrap().push(v);
    }
}

// Safety: SharedBuf is safe to send/share across threads because
// the inner Mutex handles synchronisation and Vec<T> is Send when T: Send.
unsafe impl<T: Send> Send for SharedBuf<T> {}
unsafe impl<T: Send + Sync> Sync for SharedBuf<T> {}
unsafe impl<T: Send> Send for PoolBuf<T> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkout_share_return() {
        let pool = BufPool::<u32>::new();

        // First checkout allocates.
        let mut buf = pool.checkout(100);
        buf[0] = 42;
        let shared = buf.share();
        assert_eq!(shared[0], 42);
        assert_eq!(shared.len(), 100);

        let shared2 = shared.clone();
        drop(shared);
        // Vec not yet returned (shared2 still alive).
        assert_eq!(pool.inner.lock().unwrap().len(), 0);

        drop(shared2);
        // Vec returned to pool.
        assert_eq!(pool.inner.lock().unwrap().len(), 1);

        // Second checkout reuses the buffer — no allocation.
        let buf2 = pool.checkout(100);
        assert_eq!(pool.inner.lock().unwrap().len(), 0);
        drop(buf2);
        assert_eq!(pool.inner.lock().unwrap().len(), 1);
    }

    #[test]
    fn pool_buf_drop_returns() {
        let pool = BufPool::<u8>::new();
        let buf = pool.checkout(64);
        drop(buf); // should return without share()
        assert_eq!(pool.inner.lock().unwrap().len(), 1);
    }

    #[test]
    fn resize_reuses_capacity() {
        let pool = BufPool::<u32>::new();

        let buf = pool.checkout(1000);
        let shared = buf.share();
        drop(shared); // returns Vec with capacity >= 1000

        // Checkout smaller size — should reuse the large-capacity Vec.
        let buf2 = pool.checkout(10);
        assert_eq!(buf2.len(), 10);
        // The underlying Vec still has capacity from the first allocation.
        drop(buf2);
    }
}
