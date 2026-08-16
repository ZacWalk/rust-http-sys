//! Pooled receive buffers.
//!
//! Every `HttpReceiveHttpRequest` needs an `HTTP_REQUEST_V2` followed by
//! enough slack for the kernel to write the raw URL, the header values and —
//! with `HTTP_RECEIVE_REQUEST_FLAG_COPY_BODY` — the entity body. Allocating
//! and zeroing that block per request is pure overhead at high request rates,
//! so buffers are recycled through a bounded lock-free queue and only the
//! fixed-size header block is cleared on reuse.

use std::sync::Arc;

use crossbeam_queue::ArrayQueue;
use windows::Win32::Networking::HttpServer::{HTTP_REQUEST_V2, HttpHeaderContentLength};

/// Room reserved for the request line and headers when deciding whether a
/// body can share the receive buffer with them.
const HEADER_SLACK: u64 = 8 * 1024;

/// Backing store for one in-flight request.
///
/// `Box<[u64]>` gives the 8-byte alignment `HTTP_REQUEST_V2` needs and is
/// allocated zeroed, so a fresh buffer costs one `alloc_zeroed` and a recycled
/// one costs a `memset` of just the header block.
pub(crate) struct RequestBuffer {
    words: Box<[u64]>,
}

impl RequestBuffer {
    fn new(bytes: usize) -> Self {
        let min_words = size_of::<HTTP_REQUEST_V2>().div_ceil(8) + 1;
        let words = bytes.div_ceil(8).max(min_words);
        Self {
            words: vec![0u64; words].into_boxed_slice(),
        }
    }

    /// Total size handed to `HttpReceiveHttpRequest` as `RequestBufferLength`.
    pub(crate) fn capacity(&self) -> u32 {
        // Buffers are kilobytes, so the cast can never truncate in practice.
        (self.words.len() * size_of::<u64>()) as u32
    }

    /// Grow to at least `bytes`, discarding the old contents.
    fn grow_to(&mut self, bytes: usize) {
        if bytes as u64 > u64::from(self.capacity()) {
            *self = Self::new(bytes);
        }
    }

    /// The body length this request declared, or `None` when it declared no
    /// usable `Content-Length` (no body at all, or a chunked one).
    fn declared_body_len(&self) -> Option<u64> {
        // SAFETY: only called after a successful receive.
        let base = &unsafe { self.request() }.Base;
        let header = &base.Headers.KnownHeaders[HttpHeaderContentLength.0 as usize];
        if header.RawValueLength == 0 || header.pRawValue.is_null() {
            return None;
        }
        // SAFETY: header values point into this buffer after a successful receive.
        let bytes = unsafe {
            std::slice::from_raw_parts(header.pRawValue.0, header.RawValueLength as usize)
        };
        std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| text.parse::<u64>().ok())
            // An unparseable length is not worth gambling a syscall on.
            .or(Some(u64::MAX))
    }

    /// Clear the header block so no field can be read stale if the kernel
    /// leaves it untouched, then return the pointer to fill.
    pub(crate) fn prepare(&mut self) -> *mut HTTP_REQUEST_V2 {
        let ptr = self.words.as_mut_ptr().cast::<HTTP_REQUEST_V2>();
        // SAFETY: `new` guarantees the allocation is at least one
        // `HTTP_REQUEST_V2` long and correctly aligned. The type is a plain
        // C struct, so an all-zero bit pattern is a valid value for it.
        unsafe { ptr.write_bytes(0, 1) };
        ptr
    }

    /// The kernel request id.
    ///
    /// Safe to read at any point after [`RequestBuffer::prepare`]: the field
    /// is a scalar in the zeroed header block, and http.sys fills it in even
    /// when the receive fails with `ERROR_MORE_DATA`. Reads `0` if no receive
    /// has landed yet.
    pub(crate) fn request_id(&self) -> u64 {
        // SAFETY: `prepare` zeroed the header block, so this scalar is always
        // initialized; unlike the pointer fields it is never left half-written.
        unsafe { self.request().Base.RequestId }
    }

    /// The request the kernel wrote.
    ///
    /// # Safety
    ///
    /// Only valid after a `HttpReceiveHttpRequest` against this buffer has
    /// completed successfully.
    pub(crate) unsafe fn request(&self) -> &HTTP_REQUEST_V2 {
        // SAFETY: forwarded from this function's contract; the allocation is
        // aligned and large enough, and outlives the borrow.
        unsafe { &*self.words.as_ptr().cast::<HTTP_REQUEST_V2>() }
    }
}

/// Bounded free list of receive buffers.
pub(crate) struct BufferPool {
    free: ArrayQueue<RequestBuffer>,
    bytes: usize,
    max_bytes: usize,
}

impl BufferPool {
    pub(crate) fn new(slots: usize, bytes: usize, max_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            free: ArrayQueue::new(slots.max(1)),
            bytes,
            max_bytes: max_bytes.max(bytes),
        })
    }

    /// Take a buffer, allocating one if the pool is empty.
    pub(crate) fn take(self: &Arc<Self>) -> PooledBuffer {
        let buffer = self
            .free
            .pop()
            .unwrap_or_else(|| RequestBuffer::new(self.bytes));
        PooledBuffer {
            buffer: Some(buffer),
            pool: Arc::clone(self),
        }
    }
}

/// A buffer checked out of a [`BufferPool`], returned on drop.
pub(crate) struct PooledBuffer {
    buffer: Option<RequestBuffer>,
    pool: Arc<BufferPool>,
}

impl PooledBuffer {
    /// Resize this buffer so the body just described could arrive inline with
    /// the next request's headers, and report whether that is now possible.
    ///
    /// A slot that keeps receiving 64 KiB uploads settles on a 72 KiB buffer
    /// and goes back to one syscall per request, while a slot that only ever
    /// sees small requests keeps its small buffer.
    pub(crate) fn fit_body_inline(&mut self) -> bool {
        let Some(declared) = self.declared_body_len() else {
            return true;
        };
        let needed = declared.saturating_add(HEADER_SLACK);
        if needed > self.pool.max_bytes as u64 {
            return false;
        }
        self.grow_to(needed as usize);
        true
    }
}

impl std::ops::Deref for PooledBuffer {
    type Target = RequestBuffer;
    fn deref(&self) -> &RequestBuffer {
        self.buffer.as_ref().expect("buffer taken only on drop")
    }
}

impl std::ops::DerefMut for PooledBuffer {
    fn deref_mut(&mut self) -> &mut RequestBuffer {
        self.buffer.as_mut().expect("buffer taken only on drop")
    }
}

impl Drop for PooledBuffer {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            // A full pool means we are over the configured concurrency; just
            // let the surplus buffer go.
            let _ = self.pool.free.push(buffer);
        }
    }
}

/// Recycled heap buffers for request bodies that did not arrive inline.
///
/// Without this a large upload costs a multi-hundred-kilobyte allocation and
/// free per request, which at tens of thousands of requests a second is enough
/// to dominate the copy it exists to hold.
pub(crate) struct BodyPool {
    free: ArrayQueue<Vec<u8>>,
    /// Buffers larger than this are freed instead of retained, so one outsized
    /// upload cannot pin memory for the life of the process.
    max_retained: usize,
}

impl BodyPool {
    pub(crate) fn new(slots: usize, max_retained: usize) -> Arc<Self> {
        Arc::new(Self {
            free: ArrayQueue::new(slots.max(1)),
            max_retained,
        })
    }

    /// Take an empty buffer with room for at least `capacity` bytes.
    pub(crate) fn take(self: &Arc<Self>, capacity: usize) -> PooledVec {
        let mut vec = self.free.pop().unwrap_or_default();
        vec.reserve_exact(capacity.saturating_sub(vec.capacity()));
        PooledVec {
            vec: Some(vec),
            pool: Arc::clone(self),
        }
    }
}

/// A body buffer checked out of a [`BodyPool`], returned on drop.
pub(crate) struct PooledVec {
    vec: Option<Vec<u8>>,
    pool: Arc<BodyPool>,
}

impl std::ops::Deref for PooledVec {
    type Target = Vec<u8>;
    fn deref(&self) -> &Vec<u8> {
        self.vec.as_ref().expect("buffer taken only on drop")
    }
}

impl std::ops::DerefMut for PooledVec {
    fn deref_mut(&mut self) -> &mut Vec<u8> {
        self.vec.as_mut().expect("buffer taken only on drop")
    }
}

impl Drop for PooledVec {
    fn drop(&mut self) {
        if let Some(mut vec) = self.vec.take()
            && vec.capacity() <= self.pool.max_retained
        {
            vec.clear();
            let _ = self.pool.free.push(vec);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_is_at_least_the_header_block() {
        let buf = RequestBuffer::new(0);
        assert!(buf.capacity() as usize > size_of::<HTTP_REQUEST_V2>());
    }

    #[test]
    fn capacity_rounds_up_to_whole_words() {
        assert_eq!(RequestBuffer::new(16 * 1024).capacity(), 16 * 1024);
    }

    #[test]
    fn buffers_are_recycled() {
        let pool = BufferPool::new(2, 4096, 4096);
        let ptr = {
            let mut b = pool.take();
            b.prepare() as usize
        };
        let mut again = pool.take();
        assert_eq!(again.prepare() as usize, ptr, "buffer should be reused");
    }

    #[test]
    fn surplus_buffers_are_dropped_not_queued() {
        let pool = BufferPool::new(1, 4096, 4096);
        let a = pool.take();
        let b = pool.take();
        drop(a);
        drop(b);
        assert_eq!(pool.free.len(), 1);
    }

    #[test]
    fn growing_replaces_the_allocation_only_when_needed() {
        let mut buffer = RequestBuffer::new(16 * 1024);
        let before = buffer.capacity();
        buffer.grow_to(1024);
        assert_eq!(buffer.capacity(), before, "must not shrink");
        buffer.grow_to(64 * 1024);
        assert_eq!(buffer.capacity(), 64 * 1024);
    }

    #[test]
    fn body_pool_retains_small_buffers_and_drops_oversized_ones() {
        let pool = BodyPool::new(4, 1024);
        drop(pool.take(512));
        assert_eq!(pool.free.len(), 1);
        // Taking it back out and growing it past the cap retires the buffer
        // instead of parking a large allocation in the pool forever.
        drop(pool.take(8192));
        assert_eq!(pool.free.len(), 0, "oversized buffer must not be retained");
    }

    #[test]
    fn body_pool_hands_back_empty_buffers_with_enough_room() {
        let pool = BodyPool::new(2, 1 << 20);
        {
            let mut first = pool.take(4096);
            first.extend_from_slice(&[1u8; 4096]);
        }
        let second = pool.take(1024);
        assert!(second.is_empty());
        assert!(second.capacity() >= 4096, "capacity should be reused");
    }
}
