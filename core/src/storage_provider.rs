use core::{cell::UnsafeCell, marker::PhantomData, ptr::NonNull};

/// Trait for a buffer provider.
/// The Buffer provider allows abstraction over the memory
/// The memory can be statically allocated, on the heap or on the stack
pub trait StorageProvider: PartialEq {
    /// Returns a reference to the provided buffer
    /// The buffer **HAS NO GARANTEE** on it's state or initialization
    fn storage(&self) -> NonNull<[u8]>;
}

/// A statically allocated buffer
#[derive(Debug)]
pub struct StaticBufferProvider<const N: usize> {
    buf: UnsafeCell<[u8; N]>,
}

impl<const N: usize> PartialEq for StaticBufferProvider<N> {
    fn eq(&self, other: &Self) -> bool {
        unsafe {
            let r = &*self.buf.get();
            let l = &*other.buf.get();
            r.eq(l)
        }
    }
}

impl<const N: usize> StaticBufferProvider<N> {
    /// A buffer with internal allocation
    pub const fn new() -> Self {
        Self {
            buf: UnsafeCell::new([0; N]),
        }
    }
}

impl<const N: usize> StorageProvider for StaticBufferProvider<N> {
    fn storage(&self) -> NonNull<[u8]> {
        NonNull::new(self.buf.get()).unwrap()
    }
}

/// A buffer allocated from userspace
#[derive(Debug, PartialEq)]
pub struct SliceBufferProvider<'a> {
    nn: NonNull<[u8]>,
    phantom: PhantomData<&'a mut [u8]>,
}

impl<'a> SliceBufferProvider<'a> {
    /// Creates a new BufferProvided from a userspace memory
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self {
            nn: buf.into(),
            phantom: PhantomData,
        }
    }
}

impl StorageProvider for SliceBufferProvider<'_> {
    fn storage(&self) -> NonNull<[u8]> {
        self.nn
    }
}
