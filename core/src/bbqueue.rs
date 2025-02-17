use atomic_waker::AtomicWaker;

use crate::{
    framed::{FrameConsumer, FrameProducer},
    Error, Result, SliceStorageProvider, StaticStorageProvider, StorageProvider,
};
use core::{
    cell::UnsafeCell,
    cmp::min,
    future::Future,
    marker::PhantomData,
    mem::{forget, transmute},
    ops::{Deref, DerefMut},
    pin::Pin,
    ptr::NonNull,
    result::Result as CoreResult,
    slice::{from_raw_parts, from_raw_parts_mut},
    sync::atomic::{
        AtomicBool, AtomicUsize,
        Ordering::{AcqRel, Acquire, Release},
    },
    task::{Context, Poll},
};

#[derive(Debug)]
/// A backing structure for a BBQueue. Can be used to create either
/// a BBQueue or a split Producer/Consumer pair
pub struct BBQueue<B>
where
    B: StorageProvider,
{
    // The buffer provider
    buf: UnsafeCell<B>,

    // Max capacity of the buffer
    capacity: usize,

    // Where the next byte will be written
    write: AtomicUsize,

    // Where the next byte will be read from
    read: AtomicUsize,

    // Used in the inverted case to mark the end of the
    // readable streak. Otherwise will == sizeof::<self.buf>().
    // Writer is responsible for placing this at the correct
    // place when entering an inverted condition, and Reader
    // is responsible for moving it back to sizeof::<self.buf>()
    // when exiting the inverted condition
    last: AtomicUsize,

    // Used by the Writer to remember what bytes are currently
    // allowed to be written to, but are not yet ready to be
    // read from
    reserve: AtomicUsize,

    // Is there an active read grant?
    read_in_progress: AtomicBool,

    // Is there an active write grant?
    write_in_progress: AtomicBool,

    // Have we already split?
    already_split: AtomicBool,

    // Read waker for async support
    // Woken up when a commit is done
    read_waker: AtomicWaker,

    // Write waker for async support
    // Woken up when a release is done
    write_waker: AtomicWaker,
}

unsafe impl<B> Sync for BBQueue<B> where B: StorageProvider {}

impl<'a, B> BBQueue<B>
where
    B: StorageProvider,
{
    /// Attempt to split the `BBQueue` into `Consumer` and `Producer` halves to gain access to the
    /// buffer. If buffer has already been split, an error will be returned.
    ///
    /// NOTE: When splitting, the underlying buffer will be explicitly initialized
    /// to zero. This may take a measurable amount of time, depending on the size
    /// of the buffer. This is necessary to prevent undefined behavior. If the buffer
    /// is placed at `static` scope within the `.bss` region, the explicit initialization
    /// will be elided (as it is already performed as part of memory initialization)
    ///
    /// NOTE:  If the `thumbv6` feature is selected, this function takes a short critical section
    /// while splitting.
    ///
    /// ```rust
    /// # // bbqueue test shim!
    /// # fn bbqtest() {
    /// use bbqueue::{BBQueue, StaticBufferProvider};
    ///
    /// // Create and split a new buffer
    /// let mut buffer: BBQueue<StaticBufferProvider<6>> = BBQueue::new_static();
    /// let (prod, cons) = buffer.try_split().unwrap();
    ///
    /// // Not possible to split twice
    /// assert!(buffer.try_split().is_err());
    /// # // bbqueue test shim!
    /// # }
    /// #
    /// # fn main() {
    /// # #[cfg(not(feature = "thumbv6"))]
    /// # bbqtest();
    /// # }
    /// ```
    pub fn try_split(&'a self) -> Result<(Producer<'a, B>, Consumer<'a, B>)> {
        if atomic::swap(&self.already_split, true, AcqRel) {
            return Err(Error::AlreadySplit);
        }

        unsafe {
            // Explicitly zero the data to avoid undefined behavior.
            // This is required, because we hand out references to the buffers,
            // which mean that creating them as references is technically UB for now
            let mu_ptr = (&mut *self.buf.get()).storage().as_mut();
            (*mu_ptr).as_mut_ptr().write_bytes(0u8, 1);

            let nn1 = NonNull::new_unchecked(self as *const _ as *mut _);
            let nn2 = NonNull::new_unchecked(self as *const _ as *mut _);
            Ok((
                Producer {
                    bbq: nn1,
                    pd: PhantomData,
                },
                Consumer {
                    bbq: nn2,
                    pd: PhantomData,
                },
            ))
        }
    }

    /// Attempt to split the `BBQueue` into `FrameConsumer` and `FrameProducer` halves
    /// to gain access to the buffer. If buffer has already been split, an error
    /// will be returned.
    ///
    /// NOTE: When splitting, the underlying buffer will be explicitly initialized
    /// to zero. This may take a measurable amount of time, depending on the size
    /// of the buffer. This is necessary to prevent undefined behavior. If the buffer
    /// is placed at `static` scope within the `.bss` region, the explicit initialization
    /// will be elided (as it is already performed as part of memory initialization)
    ///
    /// NOTE:  If the `thumbv6` feature is selected, this function takes a short critical
    /// section while splitting.
    pub fn try_split_framed(&'a self) -> Result<(FrameProducer<'a, B>, FrameConsumer<'a, B>)> {
        let (producer, consumer) = self.try_split()?;
        Ok((FrameProducer { producer }, FrameConsumer { consumer }))
    }

    /// Attempt to release the Producer and Consumer
    ///
    /// This re-initializes the buffer so it may be split in a different mode at a later
    /// time. There must be no read or write grants active, or an error will be returned.
    ///
    /// The `Producer` and `Consumer` must be from THIS `BBQueue`, or an error will
    /// be returned.
    ///
    /// ```rust
    /// # // bbqueue test shim!
    /// # fn bbqtest() {
    /// use bbqueue::{BBQueue, StaticBufferProvider};
    ///
    /// // Create and split a new buffer
    /// let mut buffer: BBQueue<StaticBufferProvider<6>> = BBQueue::new_static();
    /// let (prod, cons) = buffer.try_split().unwrap();
    ///
    /// // Not possible to split twice
    /// assert!(buffer.try_split().is_err());
    ///
    /// // Release the producer and consumer
    /// assert!(buffer.try_release(prod, cons).is_ok());
    ///
    /// // Split the buffer in framed mode
    /// let (fprod, fcons) = buffer.try_split_framed().unwrap();
    /// # // bbqueue test shim!
    /// # }
    /// #
    /// # fn main() {
    /// # #[cfg(not(feature = "thumbv6"))]
    /// # bbqtest();
    /// # }
    /// ```
    pub fn try_release(
        &'a self,
        prod: Producer<'a, B>,
        cons: Consumer<'a, B>,
    ) -> CoreResult<(), (Producer<'a, B>, Consumer<'a, B>)> {
        // Note: Re-entrancy is not possible because we require ownership
        // of the producer and consumer, which are not cloneable. We also
        // can assume the buffer has been split, because

        // Are these our producers and consumers?
        let our_prod = prod.bbq.as_ptr() as *const Self == self;
        let our_cons = cons.bbq.as_ptr() as *const Self == self;

        if !(our_prod && our_cons) {
            // Can't release, not our producer and consumer
            return Err((prod, cons));
        }

        let wr_in_progress = self.write_in_progress.load(Acquire);
        let rd_in_progress = self.read_in_progress.load(Acquire);

        if wr_in_progress || rd_in_progress {
            // Can't release, active grant(s) in progress
            return Err((prod, cons));
        }

        // Drop the producer and consumer halves
        drop(prod);
        drop(cons);

        // Re-initialize the buffer (not totally needed, but nice to do)
        self.write.store(0, Release);
        self.read.store(0, Release);
        self.reserve.store(0, Release);
        self.last.store(0, Release);

        // Mark the buffer as ready to be split
        self.already_split.store(false, Release);

        Ok(())
    }

    /// Attempt to release the Producer and Consumer in Framed mode
    ///
    /// This re-initializes the buffer so it may be split in a different mode at a later
    /// time. There must be no read or write grants active, or an error will be returned.
    ///
    /// The `FrameProducer` and `FrameConsumer` must be from THIS `BBQueue`, or an error
    /// will be returned.
    pub fn try_release_framed(
        &'a self,
        prod: FrameProducer<'a, B>,
        cons: FrameConsumer<'a, B>,
    ) -> CoreResult<(), (FrameProducer<'a, B>, FrameConsumer<'a, B>)> {
        self.try_release(prod.producer, cons.consumer)
            .map_err(|(producer, consumer)| {
                // Restore the wrapper types
                (FrameProducer { producer }, FrameConsumer { consumer })
            })
    }
}

impl<B> BBQueue<B>
where
    B: StorageProvider,
{
    /// Create a new BBQueue with abstraction over the memory provider
    ///
    /// ```rust,no_run
    /// use bbqueue::{BBQueue, StaticBufferProvider};
    ///
    ///
    /// fn main() {
    ///    let provider = StaticBufferProvider::<6>::new();
    ///    let mut buf = BBQueue::new(provider);
    ///    let (prod, cons) = buf.try_split().unwrap();
    /// }
    /// ```
    pub fn new(buf: B) -> Self {
        Self {
            capacity: unsafe { buf.storage().as_ref().len() },

            // This will not be initialized until we split the buffer
            buf: UnsafeCell::new(buf),

            // Owned by the writer
            write: AtomicUsize::new(0),

            // Owned by the reader
            read: AtomicUsize::new(0),

            // Cooperatively owned
            //
            // NOTE: This should generally be initialized as size_of::<self.buf>(), however
            // this would prevent the structure from being entirely zero-initialized,
            // and can cause the .data section to be much larger than necessary. By
            // forcing the `last` pointer to be zero initially, we place the structure
            // in an "inverted" condition, which will be resolved on the first commited
            // bytes that are written to the structure.
            //
            // When read == last == write, no bytes will be allowed to be read (good), but
            // write grants can be given out (also good).
            last: AtomicUsize::new(0),

            // Owned by the Writer, "private"
            reserve: AtomicUsize::new(0),

            // Owned by the Reader, "private"
            read_in_progress: AtomicBool::new(false),

            // Owned by the Writer, "private"
            write_in_progress: AtomicBool::new(false),

            // We haven't split at the start
            already_split: AtomicBool::new(false),

            // Shared between reader and writer.
            read_waker: AtomicWaker::new(),

            // Shared between reader and writer
            write_waker: AtomicWaker::new(),
        }
    }
}

impl<const N: usize> BBQueue<StaticStorageProvider<N>> {
    /// Create a new constant static BBQ, using staic memory allocation
    /// ```rust,no_run
    /// use bbqueue::{BBQueue, StaticBufferProvider};
    ///
    /// static BUF: BBQueue<StaticBufferProvider<6>> = BBQueue::new_static();
    ///
    /// fn main() {
    ///    let (prod, cons) = BUF.try_split().unwrap();
    /// }
    /// ```
    pub const fn new_static() -> Self {
        Self {
            capacity: N,

            // This will not be initialized until we split the buffer
            buf: UnsafeCell::new(StaticStorageProvider::new()),

            // Owned by the writer
            write: AtomicUsize::new(0),

            // Owned by the reader
            read: AtomicUsize::new(0),

            // Cooperatively owned
            //
            // NOTE: This should generally be initialized as size_of::<self.buf>(), however
            // this would prevent the structure from being entirely zero-initialized,
            // and can cause the .data section to be much larger than necessary. By
            // forcing the `last` pointer to be zero initially, we place the structure
            // in an "inverted" condition, which will be resolved on the first commited
            // bytes that are written to the structure.
            //
            // When read == last == write, no bytes will be allowed to be read (good), but
            // write grants can be given out (also good).
            last: AtomicUsize::new(0),

            // Owned by the Writer, "private"
            reserve: AtomicUsize::new(0),

            // Owned by the Reader, "private"
            read_in_progress: AtomicBool::new(false),

            // Owned by the Writer, "private"
            write_in_progress: AtomicBool::new(false),

            // We haven't split at the start
            already_split: AtomicBool::new(false),

            // Shared between reader and writer.
            read_waker: AtomicWaker::new(),

            // Shared between reader and writer
            write_waker: AtomicWaker::new(),
        }
    }
}

impl<'a> BBQueue<SliceStorageProvider<'a>> {
    /// Create a new BBQueue using userspace provided memory in the form of a slice.
    /// ```rust,no_run
    /// use bbqueue::{BBQueue, StaticBufferProvider};
    ///
    /// fn main() {
    ///    let mut bb_memory = [0; 6];
    ///    let mut buf = BBQueue::new_from_slice(&mut bb_memory);
    ///    let (prod, cons) = buf.try_split().unwrap();
    /// }
    /// ```
    pub fn new_from_slice(buf: &'a mut [u8]) -> Self {
        Self::new(SliceStorageProvider::new(buf))
    }
}

/// `Producer` is the primary interface for pushing data into a `BBQueue`.
/// There are various methods for obtaining a grant to write to the buffer, with
/// different potential tradeoffs. As all grants are required to be a contiguous
/// range of data, different strategies are sometimes useful when making the decision
/// between maximizing usage of the buffer, and ensuring a given grant is successful.
///
/// As a short summary of currently possible grants:
///
/// * `grant_exact(N)`
///   * User will receive a grant `sz == N` (or receive an error)
///   * This may cause a wraparound if a grant of size N is not available
///       at the end of the ring.
///   * If this grant caused a wraparound, the bytes that were "skipped" at the
///       end of the ring will not be available until the reader reaches them,
///       regardless of whether the grant commited any data or not.
///   * Maximum possible waste due to skipping: `N - 1` bytes
/// * `grant_max_remaining(N)`
///   * User will receive a grant `0 < sz <= N` (or receive an error)
///   * This will only cause a wrap to the beginning of the ring if exactly
///       zero bytes are available at the end of the ring.
///   * Maximum possible waste due to skipping: 0 bytes
///
/// See [this github issue](https://github.com/jamesmunns/bbqueue/issues/38) for a
/// discussion of grant methods that could be added in the future.
pub struct Producer<'a, B>
where
    B: StorageProvider,
{
    bbq: NonNull<BBQueue<B>>,
    pd: PhantomData<&'a ()>,
}

unsafe impl<'a, B> Send for Producer<'a, B> where B: StorageProvider {}

impl<'a, B> Producer<'a, B>
where
    B: StorageProvider,
{
    /// Request a writable, contiguous section of memory of exactly
    /// `sz` bytes. If the buffer size requested is not available,
    /// an error will be returned.
    ///
    /// This method may cause the buffer to wrap around early if the
    /// requested space is not available at the end of the buffer, but
    /// is available at the beginning
    ///
    /// ```rust
    /// # // bbqueue test shim!
    /// # fn bbqtest() {
    /// use bbqueue::{BBQueue, StaticBufferProvider};
    ///
    /// // Create and split a new buffer of 6 elements
    /// let buffer: BBQueue<StaticBufferProvider<6>> = BBQueue::new_static();
    /// let (mut prod, cons) = buffer.try_split().unwrap();
    ///
    /// // Successfully obtain and commit a grant of four bytes
    /// let mut grant = prod.grant_exact(4).unwrap();
    /// assert_eq!(grant.buf().len(), 4);
    /// grant.commit(4);
    ///
    /// // Try to obtain a grant of three bytes
    /// assert!(prod.grant_exact(3).is_err());
    /// # // bbqueue test shim!
    /// # }
    /// #
    /// # fn main() {
    /// # #[cfg(not(feature = "thumbv6"))]
    /// # bbqtest();
    /// # }
    /// ```
    pub fn grant_exact(&mut self, sz: usize) -> Result<GrantW<'a, B>> {
        let inner = unsafe { &self.bbq.as_ref() };

        if atomic::swap(&inner.write_in_progress, true, AcqRel) {
            return Err(Error::GrantInProgress);
        }

        // Writer component. Must never write to `read`,
        // be careful writing to `load`
        let write = inner.write.load(Acquire);
        let read = inner.read.load(Acquire);
        let max = unsafe { self.bbq.as_ref().capacity() };
        let already_inverted = write < read;

        let start = if already_inverted {
            if (write + sz) < read {
                // Inverted, room is still available
                write
            } else {
                // Inverted, no room is available
                inner.write_in_progress.store(false, Release);
                return Err(Error::InsufficientSize);
            }
        } else {
            if write + sz <= max {
                // Non inverted condition
                write
            } else {
                // Not inverted, but need to go inverted

                // NOTE: We check sz < read, NOT <=, because
                // write must never == read in an inverted condition, since
                // we will then not be able to tell if we are inverted or not
                if sz < read {
                    // Invertible situation
                    0
                } else {
                    // Not invertible, no space
                    inner.write_in_progress.store(false, Release);
                    return Err(Error::InsufficientSize);
                }
            }
        };

        // Safe write, only viewed by this task
        inner.reserve.store(start + sz, Release);

        // This is sound, as UnsafeCell, MaybeUninit, and GenericArray
        // are all `#[repr(Transparent)]
        let start_of_buf_ptr = unsafe { (&*inner.buf.get()).storage().as_ptr() as *mut u8 };
        let grant_slice =
            unsafe { from_raw_parts_mut(start_of_buf_ptr.offset(start as isize), sz) };

        Ok(GrantW {
            buf: grant_slice.into(),
            bbq: self.bbq,
            to_commit: 0,
            phatom: PhantomData,
        })
    }

    /// Request a writable, contiguous section of memory of up to
    /// `sz` bytes. If a buffer of size `sz` is not available without
    /// wrapping, but some space (0 < available < sz) is available without
    /// wrapping, then a grant will be given for the remaining size at the
    /// end of the buffer. If no space is available for writing, an error
    /// will be returned.
    ///
    /// ```
    /// # // bbqueue test shim!
    /// # fn bbqtest() {
    /// use bbqueue::{BBQueue, StaticBufferProvider};
    ///
    /// // Create and split a new buffer of 6 elements
    /// let mut buffer: BBQueue<StaticBufferProvider<6>> = BBQueue::new_static();
    /// let (mut prod, mut cons) = buffer.try_split().unwrap();
    ///
    /// // Successfully obtain and commit a grant of four bytes
    /// let mut grant = prod.grant_max_remaining(4).unwrap();
    /// assert_eq!(grant.buf().len(), 4);
    /// grant.commit(4);
    ///
    /// // Release the four initial commited bytes
    /// let mut grant = cons.read().unwrap();
    /// assert_eq!(grant.buf().len(), 4);
    /// grant.release(4);
    ///
    /// // Try to obtain a grant of three bytes, get two bytes
    /// let mut grant = prod.grant_max_remaining(3).unwrap();
    /// assert_eq!(grant.buf().len(), 2);
    /// grant.commit(2);
    /// # // bbqueue test shim!
    /// # }
    /// #
    /// # fn main() {
    /// # #[cfg(not(feature = "thumbv6"))]
    /// # bbqtest();
    /// # }
    /// ```
    pub fn grant_max_remaining(&mut self, mut sz: usize) -> Result<GrantW<'a, B>> {
        let inner = unsafe { &self.bbq.as_ref() };

        if atomic::swap(&inner.write_in_progress, true, AcqRel) {
            return Err(Error::GrantInProgress);
        }

        // Writer component. Must never write to `read`,
        // be careful writing to `load`
        let write = inner.write.load(Acquire);
        let read = inner.read.load(Acquire);
        let max = unsafe { self.bbq.as_ref().capacity() };

        let already_inverted = write < read;

        let start = if already_inverted {
            // In inverted case, read is always > write
            let remain = read - write - 1;

            if remain != 0 {
                sz = min(remain, sz);
                write
            } else {
                // Inverted, no room is available
                inner.write_in_progress.store(false, Release);
                return Err(Error::InsufficientSize);
            }
        } else {
            if write != max {
                // Some (or all) room remaining in un-inverted case
                sz = min(max - write, sz);
                write
            } else {
                // Not inverted, but need to go inverted

                // NOTE: We check read > 1, NOT read >= 1, because
                // write must never == read in an inverted condition, since
                // we will then not be able to tell if we are inverted or not
                if read > 1 {
                    sz = min(read - 1, sz);
                    0
                } else {
                    // Not invertible, no space
                    inner.write_in_progress.store(false, Release);
                    return Err(Error::InsufficientSize);
                }
            }
        };

        // Safe write, only viewed by this task
        inner.reserve.store(start + sz, Release);

        // This is sound, as UnsafeCell, MaybeUninit, and GenericArray
        // are all `#[repr(Transparent)]
        let start_of_buf_ptr = unsafe { (&*inner.buf.get()).storage().as_ptr() as *mut u8 };
        let grant_slice =
            unsafe { from_raw_parts_mut(start_of_buf_ptr.offset(start as isize), sz) };

        Ok(GrantW {
            buf: grant_slice.into(),
            bbq: self.bbq,
            to_commit: 0,
            phatom: PhantomData,
        })
    }

    /// Async version of [Self::grant_exact].
    /// If the buffer can enventually provide a buffer of the requested size, the future
    /// will wait for the buffer to be read so the exact buffer can be requested.
    ///
    /// If it's not possible to request it, an error is returned.
    /// For example, given a buffer
    /// [0|1|2|3|4|5|6|7|8]
    ///              ^
    ///              Write pointer
    /// We cannot request a size of size 7, since we would loop over the read pointer
    /// even if the buffer is empty. In this case, an error is returned
    pub fn grant_exact_async(&'_ mut self, sz: usize) -> GrantExactFuture<'a, '_, B> {
        GrantExactFuture { prod: self, sz }
    }

    /// Async version of [Self::grant_max_remaining].
    /// Will wait for the buffer to at least 1 byte available, as soon as it does, return the grant.
    pub fn grant_max_remaining_async(
        &'_ mut self,
        sz: usize,
    ) -> GrantMaxRemainingFuture<'a, '_, B> {
        GrantMaxRemainingFuture { prod: self, sz }
    }
}

/// `Consumer` is the primary interface for reading data from a `BBQueue`.
pub struct Consumer<'a, B>
where
    B: StorageProvider,
{
    bbq: NonNull<BBQueue<B>>,
    pd: PhantomData<&'a ()>,
}

unsafe impl<'a, B> Send for Consumer<'a, B> where B: StorageProvider {}

impl<'a, B> Consumer<'a, B>
where
    B: StorageProvider,
{
    /// Obtains a contiguous slice of committed bytes. This slice may not
    /// contain ALL available bytes, if the writer has wrapped around. The
    /// remaining bytes will be available after all readable bytes are
    /// released
    ///
    /// ```rust
    /// # // bbqueue test shim!
    /// # fn bbqtest() {
    /// use bbqueue::{BBQueue, StaticBufferProvider};
    ///
    /// // Create and split a new buffer of 6 elements
    /// let mut buffer: BBQueue<StaticBufferProvider<6>> = BBQueue::new_static();
    /// let (mut prod, mut cons) = buffer.try_split().unwrap();
    ///
    /// // Successfully obtain and commit a grant of four bytes
    /// let mut grant = prod.grant_max_remaining(4).unwrap();
    /// assert_eq!(grant.buf().len(), 4);
    /// grant.commit(4);
    ///
    /// // Obtain a read grant
    /// let mut grant = cons.read().unwrap();
    /// assert_eq!(grant.buf().len(), 4);
    /// # // bbqueue test shim!
    /// # }
    /// #
    /// # fn main() {
    /// # #[cfg(not(feature = "thumbv6"))]
    /// # bbqtest();
    /// # }
    /// ```
    pub fn read(&mut self) -> Result<GrantR<'a, B>> {
        let inner = unsafe { &self.bbq.as_ref() };

        if atomic::swap(&inner.read_in_progress, true, AcqRel) {
            return Err(Error::GrantInProgress);
        }

        let write = inner.write.load(Acquire);
        let last = inner.last.load(Acquire);
        let mut read = inner.read.load(Acquire);

        // Resolve the inverted case or end of read
        if (read == last) && (write < read) {
            read = 0;
            // This has some room for error, the other thread reads this
            // Impact to Grant:
            //   Grant checks if read < write to see if inverted. If not inverted, but
            //     no space left, Grant will initiate an inversion, but will not trigger it
            // Impact to Commit:
            //   Commit does not check read, but if Grant has started an inversion,
            //   grant could move Last to the prior write position
            // MOVING READ BACKWARDS!
            inner.read.store(0, Release);
        }

        let sz = if write < read {
            // Inverted, only believe last
            last
        } else {
            // Not inverted, only believe write
            write
        } - read;

        if sz == 0 {
            inner.read_in_progress.store(false, Release);
            return Err(Error::InsufficientSize);
        }

        // This is sound, as UnsafeCell, MaybeUninit, and GenericArray
        // are all `#[repr(Transparent)]
        let start_of_buf_ptr = unsafe { (&*inner.buf.get()).storage().as_ptr() as *mut u8 };
        let grant_slice = unsafe { from_raw_parts_mut(start_of_buf_ptr.offset(read as isize), sz) };

        Ok(GrantR {
            buf: grant_slice.into(),
            bbq: self.bbq,
            to_release: 0,
            phatom: PhantomData,
        })
    }

    /// Obtains two disjoint slices, which are each contiguous of committed bytes.
    /// Combined these contain all previously commited data.
    pub fn split_read(&mut self) -> Result<SplitGrantR<'a, B>> {
        let inner = unsafe { &self.bbq.as_ref() };

        if atomic::swap(&inner.read_in_progress, true, AcqRel) {
            return Err(Error::GrantInProgress);
        }

        let write = inner.write.load(Acquire);
        let last = inner.last.load(Acquire);
        let mut read = inner.read.load(Acquire);

        // Resolve the inverted case or end of read
        if (read == last) && (write < read) {
            read = 0;
            // This has some room for error, the other thread reads this
            // Impact to Grant:
            //   Grant checks if read < write to see if inverted. If not inverted, but
            //     no space left, Grant will initiate an inversion, but will not trigger it
            // Impact to Commit:
            //   Commit does not check read, but if Grant has started an inversion,
            //   grant could move Last to the prior write position
            // MOVING READ BACKWARDS!
            inner.read.store(0, Release);
        }

        let (sz1, sz2) = if write < read {
            // Inverted, only believe last
            (last - read, write)
        } else {
            // Not inverted, only believe write
            (write - read, 0)
        };

        if sz1 == 0 {
            inner.read_in_progress.store(false, Release);
            return Err(Error::InsufficientSize);
        }

        // This is sound, as UnsafeCell, MaybeUninit, and GenericArray
        // are all `#[repr(Transparent)]
        let start_of_buf_ptr = unsafe { (&*inner.buf.get()).storage().as_ptr() as *mut u8 };
        let grant_slice1 =
            unsafe { from_raw_parts_mut(start_of_buf_ptr.offset(read as isize), sz1) };
        let grant_slice2 = unsafe { from_raw_parts_mut(start_of_buf_ptr, sz2) };

        Ok(SplitGrantR {
            buf1: grant_slice1.into(),
            buf2: grant_slice2.into(),
            bbq: self.bbq,
            to_release: 0,
            phatom: PhantomData,
        })
    }

    /// Async version of [Self::read].
    /// Will wait for the buffer to have data to read. When data is available, the grant is returned.
    pub fn read_async<'b>(&'b mut self) -> GrantReadFuture<'a, 'b, B> {
        GrantReadFuture { cons: self }
    }

    /// Async version of [Self::split_read].
    /// Will wait just like [Self::read_async], but returns the split grant to obtain all the available data.
    pub fn split_read_async<'b>(&'b mut self) -> GrantSplitReadFuture<'a, 'b, B> {
        GrantSplitReadFuture { cons: self }
    }
}

impl<B> BBQueue<B>
where
    B: StorageProvider,
{
    /// Returns the size of the backing storage.
    ///
    /// This is the maximum number of bytes that can be stored in this queue.
    ///
    /// ```rust
    /// # // bbqueue test shim!
    /// # fn bbqtest() {
    /// use bbqueue::{BBQueue, StaticBufferProvider};
    ///
    /// // Create a new buffer of 6 elements
    /// let mut buffer: BBQueue<StaticBufferProvider<6>> = BBQueue::new_static();
    /// assert_eq!(buffer.capacity(), 6);
    /// # // bbqueue test shim!
    /// # }
    /// #
    /// # fn main() {
    /// # #[cfg(not(feature = "thumbv6"))]
    /// # bbqtest();
    /// # }
    /// ```
    pub const fn capacity(&self) -> usize {
        self.capacity
    }
}

/// A structure representing a contiguous region of memory that
/// may be written to, and potentially "committed" to the queue.
///
/// NOTE: If the grant is dropped without explicitly commiting
/// the contents, or by setting a the number of bytes to
/// automatically be committed with `to_commit()`, then no bytes
/// will be comitted for writing.
///
/// If the `thumbv6` feature is selected, dropping the grant
/// without committing it takes a short critical section,
#[derive(Debug, PartialEq)]
pub struct GrantW<'a, B>
where
    B: StorageProvider,
{
    pub(crate) buf: NonNull<[u8]>,
    bbq: NonNull<BBQueue<B>>,
    pub(crate) to_commit: usize,
    phatom: PhantomData<&'a mut [u8]>,
}

unsafe impl<'a, B> Send for GrantW<'a, B> where B: StorageProvider {}

/// A structure representing a contiguous region of memory that
/// may be read from, and potentially "released" (or cleared)
/// from the queue
///
/// NOTE: If the grant is dropped without explicitly releasing
/// the contents, or by setting the number of bytes to automatically
/// be released with `to_release()`, then no bytes will be released
/// as read.
///
///
/// If the `thumbv6` feature is selected, dropping the grant
/// without releasing it takes a short critical section,
#[derive(Debug, PartialEq)]
pub struct GrantR<'a, B>
where
    B: StorageProvider,
{
    pub(crate) buf: NonNull<[u8]>,
    bbq: NonNull<BBQueue<B>>,
    pub(crate) to_release: usize,
    phatom: PhantomData<&'a mut [u8]>,
}

/// A structure representing up to two contiguous regions of memory that
/// may be read from, and potentially "released" (or cleared)
/// from the queue
#[derive(Debug, PartialEq)]
pub struct SplitGrantR<'a, B>
where
    B: StorageProvider,
{
    pub(crate) buf1: NonNull<[u8]>,
    pub(crate) buf2: NonNull<[u8]>,
    bbq: NonNull<BBQueue<B>>,
    pub(crate) to_release: usize,
    phatom: PhantomData<&'a mut [u8]>,
}

unsafe impl<'a, B> Send for GrantR<'a, B> where B: StorageProvider {}

unsafe impl<'a, B> Send for SplitGrantR<'a, B> where B: StorageProvider {}

impl<'a, B> GrantW<'a, B>
where
    B: StorageProvider,
{
    /// Finalizes a writable grant given by `grant()` or `grant_max()`.
    /// This makes the data available to be read via `read()`. This consumes
    /// the grant.
    ///
    /// If `used` is larger than the given grant, the maximum amount will
    /// be commited
    ///
    /// NOTE:  If the `thumbv6` feature is selected, this function takes a short critical
    /// section while committing.
    pub fn commit(mut self, used: usize) {
        self.commit_inner(used);
        forget(self);
    }

    /// Obtain access to the inner buffer for writing
    ///
    /// ```rust
    /// # // bbqueue test shim!
    /// # fn bbqtest() {
    /// use bbqueue::{BBQueue, StaticBufferProvider};
    ///
    /// // Create and split a new buffer of 6 elements
    /// let mut buffer: BBQueue<StaticBufferProvider<6>> = BBQueue::new_static();
    /// let (mut prod, mut cons) = buffer.try_split().unwrap();
    ///
    /// // Successfully obtain and commit a grant of four bytes
    /// let mut grant = prod.grant_max_remaining(4).unwrap();
    /// grant.buf().copy_from_slice(&[1, 2, 3, 4]);
    /// grant.commit(4);
    /// # // bbqueue test shim!
    /// # }
    /// #
    /// # fn main() {
    /// # #[cfg(not(feature = "thumbv6"))]
    /// # bbqtest();
    /// # }
    /// ```
    pub fn buf(&mut self) -> &mut [u8] {
        unsafe { from_raw_parts_mut(self.buf.as_ptr() as *mut u8, self.buf.len()) }
    }

    /// Sometimes, it's not possible for the lifetimes to check out. For example,
    /// if you need to hand this buffer to a function that expects to receive a
    /// `&'static mut [u8]`, it is not possible for the inner reference to outlive the
    /// grant itself.
    ///
    /// You MUST guarantee that in no cases, the reference that is returned here outlives
    /// the grant itself. Once the grant has been released, referencing the data contained
    /// WILL cause undefined behavior.
    ///
    /// Additionally, you must ensure that a separate reference to this data is not created
    /// to this data, e.g. using `DerefMut` or the `buf()` method of this grant.
    pub unsafe fn as_static_mut_buf(&mut self) -> &'static mut [u8] {
        transmute::<&mut [u8], &'static mut [u8]>(self.buf())
    }

    #[inline(always)]
    pub(crate) fn commit_inner(&mut self, used: usize) {
        let len = self.buf.len();
        let inner = unsafe { &mut self.bbq.as_ref() };

        // If there is no grant in progress, return early. This
        // generally means we are dropping the grant within a
        // wrapper structure
        if !inner.write_in_progress.load(Acquire) {
            return;
        }

        // Writer component. Must never write to READ,
        // be careful writing to LAST

        // Saturate the grant commit
        let used = min(len, used);

        let write = inner.write.load(Acquire);
        atomic::fetch_sub(&inner.reserve, len - used, AcqRel);

        let max = len;
        let last = inner.last.load(Acquire);
        let new_write = inner.reserve.load(Acquire);

        if (new_write < write) && (write != max) {
            // We have already wrapped, but we are skipping some bytes at the end of the ring.
            // Mark `last` where the write pointer used to be to hold the line here
            inner.last.store(write, Release);
        } else if new_write > last {
            // We're about to pass the last pointer, which was previously the artificial
            // end of the ring. Now that we've passed it, we can "unlock" the section
            // that was previously skipped.
            //
            // Since new_write is strictly larger than last, it is safe to move this as
            // the other thread will still be halted by the (about to be updated) write
            // value
            inner.last.store(max, Release);
        }
        // else: If new_write == last, either:
        // * last == max, so no need to write, OR
        // * If we write in the end chunk again, we'll update last to max next time
        // * If we write to the start chunk in a wrap, we'll update last when we
        //     move write backwards

        // Write must be updated AFTER last, otherwise read could think it was
        // time to invert early!
        inner.write.store(new_write, Release);

        // Allow subsequent grants
        inner.write_in_progress.store(false, Release);
        inner.read_waker.wake();
    }

    /// Configures the amount of bytes to be commited on drop.
    pub fn to_commit(&mut self, amt: usize) {
        self.to_commit = self.buf.len().min(amt);
    }
}

impl<'a, B> GrantR<'a, B>
where
    B: StorageProvider,
{
    /// Release a sequence of bytes from the buffer, allowing the space
    /// to be used by later writes. This consumes the grant.
    ///
    /// If `used` is larger than the given grant, the full grant will
    /// be released.
    ///
    /// NOTE:  If the `thumbv6` feature is selected, this function takes a short critical
    /// section while releasing.
    pub fn release(mut self, used: usize) {
        // Saturate the grant release
        let used = min(self.buf.len(), used);

        self.release_inner(used);
        forget(self);
    }

    pub(crate) fn shrink(&mut self, len: usize) {
        let mut new_buf: &mut [u8] = &mut [];
        core::mem::swap(&mut self.buf_mut(), &mut new_buf);
        let (new, _) = new_buf.split_at_mut(len);
        self.buf = new.into();
    }

    /// Obtain access to the inner buffer for reading
    ///
    /// ```
    /// # // bbqueue test shim!
    /// # fn bbqtest() {
    /// use bbqueue::{BBQueue, StaticBufferProvider};
    ///
    /// // Create and split a new buffer of 6 elements
    /// let mut buffer: BBQueue<StaticBufferProvider<6>> = BBQueue::new_static();
    /// let (mut prod, mut cons) = buffer.try_split().unwrap();
    ///
    /// // Successfully obtain and commit a grant of four bytes
    /// let mut grant = prod.grant_max_remaining(4).unwrap();
    /// grant.buf().copy_from_slice(&[1, 2, 3, 4]);
    /// grant.commit(4);
    ///
    /// // Obtain a read grant, and copy to a buffer
    /// let mut grant = cons.read().unwrap();
    /// let mut buf = [0u8; 4];
    /// buf.copy_from_slice(grant.buf());
    /// assert_eq!(&buf, &[1, 2, 3, 4]);
    /// # // bbqueue test shim!
    /// # }
    /// #
    /// # fn main() {
    /// # #[cfg(not(feature = "thumbv6"))]
    /// # bbqtest();
    /// # }
    /// ```
    pub fn buf(&self) -> &[u8] {
        unsafe { from_raw_parts(self.buf.as_ptr() as *const u8, self.buf.len()) }
    }

    /// Obtain mutable access to the read grant
    ///
    /// This is useful if you are performing in-place operations
    /// on an incoming packet, such as decryption
    pub fn buf_mut(&mut self) -> &mut [u8] {
        unsafe { from_raw_parts_mut(self.buf.as_ptr() as *mut u8, self.buf.len()) }
    }

    /// Sometimes, it's not possible for the lifetimes to check out. For example,
    /// if you need to hand this buffer to a function that expects to receive a
    /// `&'static [u8]`, it is not possible for the inner reference to outlive the
    /// grant itself.
    ///
    /// You MUST guarantee that in no cases, the reference that is returned here outlives
    /// the grant itself. Once the grant has been released, referencing the data contained
    /// WILL cause undefined behavior.
    ///
    /// Additionally, you must ensure that a separate reference to this data is not created
    /// to this data, e.g. using `Deref` or the `buf()` method of this grant.
    pub unsafe fn as_static_buf(&self) -> &'static [u8] {
        transmute::<&[u8], &'static [u8]>(self.buf())
    }

    #[inline(always)]
    pub(crate) fn release_inner(&mut self, used: usize) {
        let inner = unsafe { &self.bbq.as_ref() };

        // If there is no grant in progress, return early. This
        // generally means we are dropping the grant within a
        // wrapper structure
        if !inner.read_in_progress.load(Acquire) {
            return;
        }

        // This should always be checked by the public interfaces
        debug_assert!(used <= self.buf.len());

        // This should be fine, purely incrementing
        let _ = atomic::fetch_add(&inner.read, used, Release);

        inner.read_in_progress.store(false, Release);
        unsafe { self.bbq.as_ref().write_waker.wake() };
    }

    /// Configures the amount of bytes to be released on drop.
    pub fn to_release(&mut self, amt: usize) {
        self.to_release = self.buf.len().min(amt);
    }
}

impl<'a, B> SplitGrantR<'a, B>
where
    B: StorageProvider,
{
    /// Release a sequence of bytes from the buffer, allowing the space
    /// to be used by later writes. This consumes the grant.
    ///
    /// If `used` is larger than the given grant, the full grant will
    /// be released.
    ///
    /// NOTE:  If the `thumbv6` feature is selected, this function takes a short critical
    /// section while releasing.
    pub fn release(mut self, used: usize) {
        // Saturate the grant release
        let used = min(self.combined_len(), used);

        self.release_inner(used);
        forget(self);
    }

    /// Obtain access to both inner buffers for reading
    ///
    /// ```
    /// # // bbqueue test shim!
    /// # fn bbqtest() {
    /// use bbqueue::{BBQueue, StaticBufferProvider};
    ///
    /// // Create and split a new buffer of 6 elements
    /// let mut buffer: BBQueue<StaticBufferProvider<6>> = BBQueue::new_static();
    /// let (mut prod, mut cons) = buffer.try_split().unwrap();
    ///
    /// // Successfully obtain and commit a grant of four bytes
    /// let mut grant = prod.grant_max_remaining(4).unwrap();
    /// grant.buf().copy_from_slice(&[1, 2, 3, 4]);
    /// grant.commit(4);
    ///
    /// // Obtain a read grant, and copy to a buffer
    /// let mut grant = cons.read().unwrap();
    /// let mut buf = [0u8; 4];
    /// buf.copy_from_slice(grant.buf());
    /// assert_eq!(&buf, &[1, 2, 3, 4]);
    /// # // bbqueue test shim!
    /// # }
    /// #
    /// # fn main() {
    /// # #[cfg(not(feature = "thumbv6"))]
    /// # bbqtest();
    /// # }
    /// ```
    pub fn bufs(&self) -> (&[u8], &[u8]) {
        let buf1 = unsafe { from_raw_parts(self.buf1.as_ptr() as *const u8, self.buf1.len()) };
        let buf2 = unsafe { from_raw_parts(self.buf2.as_ptr() as *const u8, self.buf2.len()) };
        (buf1, buf2)
    }

    /// Obtain mutable access to both parts of the read grant
    ///
    /// This is useful if you are performing in-place operations
    /// on an incoming packet, such as decryption
    pub fn bufs_mut(&mut self) -> (&mut [u8], &mut [u8]) {
        let buf1 = unsafe { from_raw_parts_mut(self.buf1.as_ptr() as *mut u8, self.buf1.len()) };
        let buf2 = unsafe { from_raw_parts_mut(self.buf2.as_ptr() as *mut u8, self.buf2.len()) };
        (buf1, buf2)
    }

    #[inline(always)]
    pub(crate) fn release_inner(&mut self, used: usize) {
        let inner = unsafe { &self.bbq.as_ref() };

        // If there is no grant in progress, return early. This
        // generally means we are dropping the grant within a
        // wrapper structure
        if !inner.read_in_progress.load(Acquire) {
            return;
        }

        // This should always be checked by the public interfaces
        debug_assert!(used <= self.combined_len());

        if used <= self.buf1.len() {
            // This should be fine, purely incrementing
            let _ = atomic::fetch_add(&inner.read, used, Release);
        } else {
            // Also release parts of the second buffer
            inner.read.store(used - self.buf1.len(), Release);
        }

        inner.read_in_progress.store(false, Release);
    }

    /// Configures the amount of bytes to be released on drop.
    pub fn to_release(&mut self, amt: usize) {
        self.to_release = self.combined_len().min(amt);
    }

    /// The combined length of both buffers
    pub fn combined_len(&self) -> usize {
        self.buf1.len() + self.buf2.len()
    }
}

impl<'a, B> Drop for GrantW<'a, B>
where
    B: StorageProvider,
{
    fn drop(&mut self) {
        self.commit_inner(self.to_commit)
    }
}

impl<'a, B> Drop for GrantR<'a, B>
where
    B: StorageProvider,
{
    fn drop(&mut self) {
        self.release_inner(self.to_release)
    }
}

impl<'a, B> Drop for SplitGrantR<'a, B>
where
    B: StorageProvider,
{
    fn drop(&mut self) {
        self.release_inner(self.to_release)
    }
}

impl<'a, B> Deref for GrantW<'a, B>
where
    B: StorageProvider,
{
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        unsafe { from_raw_parts_mut(self.buf.as_ptr() as *mut u8, self.buf.len()) }
    }
}

impl<'a, B> DerefMut for GrantW<'a, B>
where
    B: StorageProvider,
{
    fn deref_mut(&mut self) -> &mut [u8] {
        self.buf()
    }
}

impl<'a, B> Deref for GrantR<'a, B>
where
    B: StorageProvider,
{
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.buf()
    }
}

impl<'a, B> DerefMut for GrantR<'a, B>
where
    B: StorageProvider,
{
    fn deref_mut(&mut self) -> &mut [u8] {
        self.buf_mut()
    }
}

/// Future returned [Producer::grant_exact_async]
pub struct GrantExactFuture<'a, 'b, B>
where
    B: StorageProvider,
{
    prod: &'b mut Producer<'a, B>,
    sz: usize,
}

impl<'a, 'b, B> Future for GrantExactFuture<'a, 'b, B>
where
    B: StorageProvider,
{
    type Output = Result<GrantW<'a, B>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Check if it's event  possible to get the requested size
        // Ex:
        // [0|1|2|3|4|5|6|7|8]
        //              ^
        //              Write pointer
        // Check if the buffer from 6 to 8 satisfies or if the buffer from 0 to 5 does.
        // If so, create the future, if not, we need the return since the future will never resolve.
        // Ideally, we could just wait for all the read to complete and reset the read and write to 0, but that is currently not supported
        let max = unsafe { self.prod.bbq.as_ref().capacity() };
        let write = unsafe { self.prod.bbq.as_ref().write.load(Acquire) };
        if self.sz > max || (self.sz > max - write && self.sz >= write) {
            return Poll::Ready(Err(Error::InsufficientSize));
        }

        let sz = self.sz;

        match self.prod.grant_exact(sz) {
            Ok(grant) => Poll::Ready(Ok(grant)),
            Err(e) => match e {
                Error::GrantInProgress | Error::InsufficientSize => {
                    unsafe { self.prod.bbq.as_ref().write_waker.register(cx.waker()) };
                    Poll::Pending
                }
                _ => Poll::Ready(Err(e)),
            },
        }
    }
}

/// Future returned [Producer::grant_max_remaining_async]
pub struct GrantMaxRemainingFuture<'a, 'b, B>
where
    B: StorageProvider,
{
    prod: &'b mut Producer<'a, B>,
    sz: usize,
}

impl<'a, 'b, B> Future for GrantMaxRemainingFuture<'a, 'b, B>
where
    B: StorageProvider,
{
    type Output = Result<GrantW<'a, B>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let sz = self.sz;

        match self.prod.grant_max_remaining(sz) {
            Ok(grant) => Poll::Ready(Ok(grant)),
            Err(e) => match e {
                Error::GrantInProgress | Error::InsufficientSize => {
                    unsafe { self.prod.bbq.as_ref().write_waker.register(cx.waker()) };
                    Poll::Pending
                }
                _ => Poll::Ready(Err(e)),
            },
        }
    }
}

/// Future returned [Consumer::read_async]
pub struct GrantReadFuture<'a, 'b, B>
where
    B: StorageProvider,
{
    cons: &'b mut Consumer<'a, B>,
}

impl<'a, 'b, B> Future for GrantReadFuture<'a, 'b, B>
where
    B: StorageProvider,
{
    type Output = Result<GrantR<'a, B>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.cons.read() {
            Ok(grant) => Poll::Ready(Ok(grant)),
            Err(e) => match e {
                Error::InsufficientSize | Error::GrantInProgress => {
                    unsafe { self.cons.bbq.as_ref().read_waker.register(cx.waker()) };
                    Poll::Pending
                }
                _ => Poll::Ready(Err(e)),
            },
        }
    }
}

/// Future returned [Consumer::split_read_async]
pub struct GrantSplitReadFuture<'a, 'b, B>
where
    B: StorageProvider,
{
    cons: &'b mut Consumer<'a, B>,
}

impl<'a, 'b, B> Future for GrantSplitReadFuture<'a, 'b, B>
where
    B: StorageProvider,
{
    type Output = Result<SplitGrantR<'a, B>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.cons.split_read() {
            Ok(grant) => Poll::Ready(Ok(grant)),
            Err(e) => match e {
                Error::InsufficientSize | Error::GrantInProgress => {
                    unsafe { self.cons.bbq.as_ref().read_waker.register(cx.waker()) };
                    Poll::Pending
                }
                _ => Poll::Ready(Err(e)),
            },
        }
    }
}

#[cfg(feature = "thumbv6")]
mod atomic {
    use core::sync::atomic::{
        AtomicBool, AtomicUsize,
        Ordering::{self, Acquire, Release},
    };
    use cortex_m::interrupt::free;

    #[inline(always)]
    pub fn fetch_add(atomic: &AtomicUsize, val: usize, _order: Ordering) -> usize {
        free(|_| {
            let prev = atomic.load(Acquire);
            atomic.store(prev.wrapping_add(val), Release);
            prev
        })
    }

    #[inline(always)]
    pub fn fetch_sub(atomic: &AtomicUsize, val: usize, _order: Ordering) -> usize {
        free(|_| {
            let prev = atomic.load(Acquire);
            atomic.store(prev.wrapping_sub(val), Release);
            prev
        })
    }

    #[inline(always)]
    pub fn swap(atomic: &AtomicBool, val: bool, _order: Ordering) -> bool {
        free(|_| {
            let prev = atomic.load(Acquire);
            atomic.store(val, Release);
            prev
        })
    }
}

#[cfg(not(feature = "thumbv6"))]
mod atomic {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[inline(always)]
    pub fn fetch_add(atomic: &AtomicUsize, val: usize, order: Ordering) -> usize {
        atomic.fetch_add(val, order)
    }

    #[inline(always)]
    pub fn fetch_sub(atomic: &AtomicUsize, val: usize, order: Ordering) -> usize {
        atomic.fetch_sub(val, order)
    }

    #[inline(always)]
    pub fn swap(atomic: &AtomicBool, val: bool, order: Ordering) -> bool {
        atomic.swap(val, order)
    }
}
