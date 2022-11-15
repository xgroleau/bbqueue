// use core::mem::MaybeUninit;

/// Trait for a buffer provider.
/// The Buffer provider allows abstraction over the memory
/// The memory can be statically allocated, on the heap or on the stack
pub trait BufferProvider: PartialEq {
    /// Returns a reference to the provided buffer
    /// The buffer **HAS NO GARANTEE** on it's state or initialization
    fn buf(&mut self) -> &mut [u8];

    /// Returns the capacity of the buffer
    fn capacity(&self) -> usize;
}

/// A statically allocated buffer
#[derive(Debug, PartialEq)]
pub struct StaticBP<const N: usize> {
    buf: [u8; N],
}

impl<const N: usize> StaticBP<N> {
    /// A buffer allocated from userspace
    pub const fn new() -> Self {
        // unsafe {
        Self {
            // buf: MaybeUninit::uninit().assume_init(),
            buf: [0; N],
        }
        // }
    }
}

impl<const N: usize> BufferProvider for StaticBP<N> {
    fn buf(&mut self) -> &mut [u8] {
        &mut self.buf
    }

    fn capacity(&self) -> usize {
        self.buf.len()
    }
}

/// A buffer allocated from userspace
#[derive(Debug, PartialEq)]
pub struct UserBP<'a> {
    buf: &'a mut [u8],
}

impl<'a> UserBP<'a> {
    /// Creates a new BufferProvided from a user buffer
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf }
    }
}

impl BufferProvider for UserBP<'_> {
    fn buf(&mut self) -> &mut [u8] {
        &mut self.buf
    }

    fn capacity(&self) -> usize {
        self.buf.len()
    }
}
