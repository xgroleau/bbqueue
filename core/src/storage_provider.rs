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
pub struct StaticStorageProvider<const N: usize> {
    buf: UnsafeCell<[u8; N]>,
}

impl<const N: usize> PartialEq for StaticStorageProvider<N> {
    fn eq(&self, other: &Self) -> bool {
        unsafe {
            let r = &*self.buf.get();
            let l = &*other.buf.get();
            r.eq(l)
        }
    }
}

impl<const N: usize> StaticStorageProvider<N> {
    /// A buffer with internal allocation
    pub const fn new() -> Self {
        Self {
            buf: UnsafeCell::new([0; N]),
        }
    }
}

impl<const N: usize> StorageProvider for StaticStorageProvider<N> {
    fn storage(&self) -> NonNull<[u8]> {
        NonNull::new(self.buf.get()).unwrap()
    }
}

/// A buffer allocated from userspace
#[derive(Debug, PartialEq)]
pub struct SliceStorageProvider<'a> {
    nn: NonNull<[u8]>,
    phantom: PhantomData<&'a mut [u8]>,
}

impl<'a> SliceStorageProvider<'a> {
    /// Creates a new BufferProvided from a userspace memory
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self {
            nn: buf.into(),
            phantom: PhantomData,
        }
    }
}

impl StorageProvider for SliceStorageProvider<'_> {
    fn storage(&self) -> NonNull<[u8]> {
        self.nn
    }
}
