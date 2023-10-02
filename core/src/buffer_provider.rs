use core::{marker::PhantomData, ptr::NonNull};

/// Trait for a buffer provider.
/// The Buffer provider allows abstraction over the memory
/// The memory can be statically allocated, on the heap or on the stack
pub trait StorageProvider: PartialEq {
    /// Returns a reference to the provided buffer
    /// The buffer **HAS NO GARANTEE** on it's state or initialization
    fn storage(&mut self) -> NonNull<[u8]>;
}

/// A statically allocated buffer
#[derive(Debug, PartialEq)]
pub struct StaticBufferProvider<const N: usize> {
    buf: [u8; N],
}

impl<const N: usize> StaticBufferProvider<N> {
    /// A buffer with internal allocation
    pub const fn new() -> Self {
        Self { buf: [0; N] }
    }
}

impl<const N: usize> StorageProvider for StaticBufferProvider<N> {
    fn storage(&mut self) -> NonNull<[u8]> {
        self.buf.as_mut_slice().into()
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
    fn storage(&mut self) -> NonNull<[u8]> {
        self.nn
    }
}
