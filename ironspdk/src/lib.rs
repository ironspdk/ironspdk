#![doc = r#"

# ironspdk

`ironspdk` provides Rust abstractions for building SPDK applications and
custom SPDK block devices.

The library integrates Rust's ownership and asynchronous programming model
with SPDK's reactor-based execution model. An SPDK thread acts as the
execution context for Rust futures: tasks are polled by an SPDK poller rather
than by a separate Rust async runtime.

## Execution model

Most `ironspdk` operations are tied to the current SPDK thread. In particular,
SPDK I/O channels are thread-local resources and must only be accessed from
the SPDK thread that owns them.

[`SpdkThread`] provides a handle for addressing an SPDK thread and for
submitting Rust futures to that thread.

[`TlsKey`] provides typed thread-local storage associated with an SPDK
thread.

## I/O model

Incoming SPDK bdev requests are represented by [`BdevIo`]. Their data can be
accessed without copying through [`Io::Ref`] or copied into an owned DMA
buffer using [`Io::Buf`] and [`DmaBuf`].

The [`Bdev`] trait is the primary interface for implementing a
`ironspdk`-backed SPDK block device.

For accessing lower-layer SPDK block devices, [`Lbdev`] provides an
asynchronous interface built on SPDK's bdev client API.

## Thread SAFETY

`ironspdk` does not provide general-purpose thread-safe access to SPDK objects.
Types that can be moved or shared between Rust threads are explicitly
documented as handles; the underlying SPDK operation still occurs according
to SPDK's thread-affinity rules.

When an API is restricted to an SPDK thread, that restriction is part of its
safety contract and must be respected by callers.
"#]
#![allow(dead_code)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use ctor::ctor;
use log::{debug, error, warn};
use num_enum::{IntoPrimitive, TryFromPrimitive};
use parking_lot::{Mutex, RwLock};
use paste::paste;
use smallvec::SmallVec;
use std::any::Any;
use std::cell::{Cell, RefCell, UnsafeCell};
use std::cmp::min;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::ffi::CString;
use std::future::Future;
use std::iter::{Map, Once};
use std::marker::PhantomData;
use std::os::raw::c_void;
use std::pin::Pin;
use std::ptr::NonNull;
use std::rc::Rc;
use std::slice::{Iter, IterMut, from_raw_parts, from_raw_parts_mut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use thiserror::Error;

mod c_enum;
use c_enum::*;
mod app;
mod c;
pub use app::*;
pub mod rpc;

static BDEV_REGISTRY: OnceLock<Mutex<HashMap<String, BdevHandle>>> = OnceLock::new();
static TCB_REGISTRY: OnceLock<RwLock<HashMap<ThreadKey, TcbPtr>>> = OnceLock::new();

/// Errors that can occur during ironspdk operations.
#[derive(Debug, Error)]
pub enum Error {
    #[error("This entity already exists")]
    AlreadyExists,
    #[error("SPDK block device '{0}' not found")]
    SpdkBdevNotFound(String),
    #[error("Failed to delete SPDK block device: {0}")]
    SpdkBdevDelete(i32),
    #[error("Failed to create SPDK block device: {0}")]
    SpdkBdevCreate(i32),
    #[error("Failed to open SPDK block device: {0}")]
    SpdkBdevOpen(i32),
    #[error("Unknown RPC command '{0}")]
    RpcCmdUnknown(String),
    #[error("Invalid arguments")]
    InvalidArguments,
    #[error("Invalid field '{0}'")]
    InvalidField(String),
    #[error("Out of memory")]
    NoMemory,
    #[error("Unsupported feature")]
    UnsupportedFeature,
    #[error("Attempt to modify shared buffer")]
    SharedBufferModification,
    #[error("Unsupported operation")]
    UnsupportedOperation,
    #[error("Out of range")]
    OutOfRange,
    #[error("Integer downcast error")]
    IntDowncast,
    #[error("Integer parse error")]
    IntParseError(#[from] std::num::ParseIntError),
}

#[derive(Copy, Clone, Hash, Eq, PartialEq)]
pub struct BdevId(usize);

#[derive(Copy, Clone, Hash, Eq, PartialEq)]
struct ThreadKey(usize);

impl ThreadKey {
    fn from_thread(thread: *mut c::spdk_thread) -> Self {
        Self(thread as usize)
    }
}

#[derive(Copy, Clone, Hash, Eq, PartialEq)]
struct TcbPtr(usize);

impl TcbPtr {
    fn from_tcb(tcb: *mut Tcb) -> Self {
        Self(tcb as usize)
    }

    pub fn ptr(&self) -> usize {
        self.0
    }
}

fn tcb_registry() -> &'static RwLock<HashMap<ThreadKey, TcbPtr>> {
    TCB_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

c_enum! {
    pub enum DifType: i32 {
        Disable = c::SPDK_DIF_DISABLE,
        Type1   = c::SPDK_DIF_TYPE1,
        Type2   = c::SPDK_DIF_TYPE2,
        Type3   = c::SPDK_DIF_TYPE3,
    }
}

c_enum! {
/// Type of I/O operation.
///
/// This enum represents the different types of I/O operations that can be
/// performed on block devices. It corresponds to SPDK's I/O type constants
/// but provides a safe Rust interface.
///
/// # Examples
///
/// ```no_run
/// use ironspdk::IoType;
///
/// let io_type = IoType::Read;
/// println!("I/O type: {:?}", io_type);
/// ```
    pub enum IoType: i32 {
        Invalid     = c::SPDK_BDEV_IO_TYPE_INVALID,
        Read        = c::SPDK_BDEV_IO_TYPE_READ,
        Write       = c::SPDK_BDEV_IO_TYPE_WRITE,
        Unmap       = c::SPDK_BDEV_IO_TYPE_UNMAP,
        Flush       = c::SPDK_BDEV_IO_TYPE_FLUSH,
        Reset       = c::SPDK_BDEV_IO_TYPE_RESET,
        NvmeAdmin   = c::SPDK_BDEV_IO_TYPE_NVME_ADMIN,
        NvmeIo      = c::SPDK_BDEV_IO_TYPE_NVME_IO,
        NvmeIoMd    = c::SPDK_BDEV_IO_TYPE_NVME_IO_MD,
        WriteZeroes = c::SPDK_BDEV_IO_TYPE_WRITE_ZEROES,
        Zcopy       = c::SPDK_BDEV_IO_TYPE_ZCOPY,
        GenZoneInfo = c::SPDK_BDEV_IO_TYPE_GET_ZONE_INFO,
        ZoneManagement = c::SPDK_BDEV_IO_TYPE_ZONE_MANAGEMENT,
        ZoneAppend  = c::SPDK_BDEV_IO_TYPE_ZONE_APPEND,
        Compare     = c::SPDK_BDEV_IO_TYPE_COMPARE,
        CompareAndWrite = c::SPDK_BDEV_IO_TYPE_COMPARE_AND_WRITE,
        Abort       = c::SPDK_BDEV_IO_TYPE_ABORT,
        SeekHole    = c::SPDK_BDEV_IO_TYPE_SEEK_HOLE,
        SeekData    = c::SPDK_BDEV_IO_TYPE_SEEK_DATA,
        Copy        = c::SPDK_BDEV_IO_TYPE_COPY,
        NvmeIovMd   = c::SPDK_BDEV_IO_TYPE_NVME_IOV_MD,
        NvmeNssr    = c::SPDK_BDEV_IO_TYPE_NVME_NSSR,
        WriteUncorrectable = c::SPDK_BDEV_IO_TYPE_WRITE_UNCORRECTABLE,
    }
}

/// A buffer allocated from DMA-capable memory.
///
/// `DmaBuf` owns the underlying allocation and releases it with
/// [`spdk_dma_free`](https://spdk.io/doc/memory.html) when dropped. The
/// allocation is suitable for I/O operations that require DMA-accessible
/// memory.
///
/// The buffer can be accessed as a byte slice with [`as_slice`](Self::as_slice)
/// or [`as_mut_slice`](Self::as_mut_slice). Mutable access follows the usual
/// Rust borrowing rules for a particular `DmaBuf`. Clones are not allowed.
///
/// # Examples
///
/// Allocate a 16 KiB DMA buffer and initialize it:
///
/// ```no_run
/// use ironspdk::DmaBuf;
///
/// fn alloc_and_init() -> Result<DmaBuf, ironspdk::Error> {
///     let mut buf = DmaBuf::new_zeroed(16 * 1024)?;
///     buf.as_mut_slice()[0] = 42;
///     Ok(buf)
/// }
/// ```
///
/// Use [`new_aligned`](Self::new_aligned) when an alignment requirement other
/// than the default is needed.
///
/// # Errors
///
/// The constructors return [`Error::NoMemory`] if SPDK cannot allocate the
/// requested memory.
#[derive(Debug)]
pub struct DmaBuf {
    ptr: NonNull<u8>,
    len: usize,
}

unsafe impl Send for DmaBuf {}
unsafe impl Sync for DmaBuf {}

impl Drop for DmaBuf {
    fn drop(&mut self) {
        unsafe { c::spdk_dma_free(self.ptr.as_ptr() as *mut _) }
    }
}

impl DmaBuf {
    /// Default alignment for DMA buffers (4KB).
    const ALIGN_4K: usize = 4096;

    /// Creates a new DMA buffer of the specified length.
    pub fn new(len: usize) -> Result<Self, Error> {
        Self::alloc(len, Self::ALIGN_4K, false)
    }

    /// Creates a new DMA buffer with custom alignment.
    pub fn new_aligned(len: usize, align: usize) -> Result<Self, Error> {
        Self::alloc(len, align, false)
    }

    /// Creates a new zeroed DMA buffer of the specified length.
    pub fn new_zeroed(len: usize) -> Result<Self, Error> {
        Self::alloc(len, Self::ALIGN_4K, true)
    }

    /// Creates a new zeroed DMA buffer with custom alignment.
    pub fn new_aligned_zeroed(len: usize, align: usize) -> Result<Self, Error> {
        Self::alloc(len, align, true)
    }

    fn alloc(len: usize, align: usize, zeroed: bool) -> Result<Self, Error> {
        let ptr = if zeroed {
            unsafe { c::spdk_dma_zmalloc(len, align, std::ptr::null_mut()) }
        } else {
            unsafe { c::spdk_dma_malloc(len, align, std::ptr::null_mut()) }
        };
        let ptr = NonNull::new(ptr as *mut u8).ok_or(Error::NoMemory)?;
        Ok(Self { ptr, len })
    }

    /// Returns the length of the buffer in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

// Performance tests show SmallVec<[c::iovec; 2]> is slightly
// faster (~= 0.7%) than Vec<c::iovec>
type Iovecs = SmallVec<[c::iovec; 2]>;

/// A zero-copy view of an SPDK bdev I/O request.
///
/// `IoRef` describes the data buffers belonging to an existing
/// [`BdevIo`]. It contains the request's scatter/gather I/O vectors rather
/// than owning or copying their contents.
///
/// An `IoRef` therefore provides a view into memory owned by the original
/// SPDK I/O request. The view must not outlive that request.
///
/// `IoRef` also carries the logical block range associated with the view.
/// The range is expressed in units of `block_len` bytes.
///
/// Use [`Io::from_bdev_io`] to create an `IoRef` from an incoming bdev
/// request. Use [`IoRef::to_buf`] when an owned DMA buffer is required.
///
/// # Zero-copy behavior
///
/// Creating an `IoRef` does not copy the request data. Operations such as
/// [`iter_iov`](Io::iter_iov) access the original I/O vectors directly.
///
/// In contrast, [`IoRef::to_buf`] allocates a new [`DmaBuf`] and copies the
/// referenced data into it.
///
/// # Block size
///
/// `block_len` controls the logical block size used by the resulting view.
/// A value of `0` means that the block size of the underlying bdev request
/// is used.
///
/// Currently, requests using DIF metadata are not supported.
#[derive(Debug)]
pub struct IoRef<'a> {
    /// Scatter-gather list
    data_iovs: Iovecs,
    /// Logical block address (LBA)
    offset_blocks: u64,
    /// Offset in parent IoRef (in blocks), zero for parent
    ref_offset: usize,
    /// Number of blocks in the view
    num_blocks: usize,
    /// Block length in bytes
    block_len: usize,
    /// Phantom data to maintain lifetime
    _marker: PhantomData<&'a IoRef<'a>>,
}

impl<'a> IoRef<'a> {
    /// Creates an I/O reference from a BdevIo.
    fn from_bdev_io(io: &BdevIo, block_len: usize) -> Result<Self, Error> {
        if io.dif_type() != DifType::Disable {
            error!("DIF metadata is not supported yet");
            return Err(Error::UnsupportedFeature);
        }
        // check block_len is aligned to power of 2
        if block_len != 0 && (!block_len.is_power_of_two() || block_len < 512) {
            error!("IoRef::from_bdev_io: invalid block_len: {}", block_len);
            return Err(Error::InvalidArguments);
        }

        let mut data_ptr: *mut c::iovec = std::ptr::null_mut();
        let mut data_cnt: i32 = 0;

        let raw = io.raw.as_ptr();

        unsafe { c::u_bdev_io_get_iovec(raw, &mut data_ptr, &mut data_cnt) };

        let data_iovs = unsafe { from_raw_parts_mut(data_ptr, data_cnt as usize) };

        let num_blocks: usize = io.num_blocks().try_into().map_err(|_| Error::IntDowncast)?;
        let parent_block_len = io.block_len();

        let size: usize = num_blocks * parent_block_len;

        let block_len = if block_len != 0 {
            block_len
        } else {
            parent_block_len
        };
        let num_blocks = size / block_len;

        let parent_offset_blocks = io.offset_blocks();
        let offset_blocks = parent_offset_blocks * (parent_block_len as u64) / (block_len as u64);

        Ok(Self {
            data_iovs: SmallVec::from_slice(data_iovs),
            offset_blocks,
            ref_offset: 0usize,
            num_blocks,
            block_len,
            _marker: PhantomData,
        })
    }

    /// Updates the offset in blocks.
    /// This method is useful when splitting or reordering I/O operations.
    pub fn update_offset_blocks(&mut self, offset_blocks: u64) {
        self.offset_blocks = offset_blocks;
    }

    pub fn num_blocks(&self) -> usize {
        self.num_blocks
    }

    /// Returns the total number of bytes in the I/O reference.
    pub fn total_bytes(&self) -> usize {
        self.num_blocks * self.block_len
    }

    /// Converts the I/O reference to an owned I/O buffer.
    pub fn to_buf(&self) -> Result<IoBuf, Error> {
        let total = self.total_bytes();
        let mut dmabuf = DmaBuf::new(total)?;
        let data = dmabuf.as_mut_slice();
        let mut dst_offset = 0;
        for iov in &self.data_iovs {
            let src = iov.iov_base as *const u8;
            let len = iov.iov_len;
            unsafe {
                std::ptr::copy_nonoverlapping(src, data.as_mut_ptr().add(dst_offset), len);
            }
            dst_offset += len;
        }
        debug_assert!(dst_offset == total);
        Ok(IoBuf {
            data: dmabuf,
            offset_blocks: self.offset_blocks,
            num_blocks: self.num_blocks,
            block_len: self.block_len,
        })
    }
}

#[derive(Debug)]
pub struct IoBuf {
    data: DmaBuf,
    offset_blocks: u64,
    num_blocks: usize,
    block_len: usize,
}

impl IoBuf {
    pub fn new(data: DmaBuf, offset_blocks: u64, block_len: usize) -> Result<IoBuf, Error> {
        // check data alignment
        let data_len = data.len();
        if !data_len.is_multiple_of(block_len) {
            error!("data length is not aligned to block length");
            return Err(Error::InvalidArguments);
        }
        Ok(Self {
            data,
            offset_blocks,
            num_blocks: data_len / block_len,
            block_len,
        })
    }

    pub fn num_blocks(&self) -> usize {
        self.num_blocks
    }

    pub fn total_bytes(&self) -> usize {
        self.num_blocks * self.block_len
    }

    pub fn as_slice(&self) -> &[u8] {
        self.data.as_slice()
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.data.as_mut_slice()
    }
}

type IoVecIter<'a> = Iter<'a, c::iovec>;

pub enum IoIter<'a> {
    Ref(Map<IoVecIter<'a>, fn(&'a c::iovec) -> &'a [u8]>),
    Buf(Once<&'a [u8]>),
}

impl<'a> Iterator for IoIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            IoIter::Ref(iter) => iter.next(),
            IoIter::Buf(iter) => iter.next(),
        }
    }
}

type IoVecIterMut<'a> = IterMut<'a, c::iovec>;

pub enum IoIterMut<'a> {
    Ref(Map<IoVecIterMut<'a>, fn(&'a mut c::iovec) -> &'a mut [u8]>),
    Buf(Once<&'a mut [u8]>),
}

impl<'a> Iterator for IoIterMut<'a> {
    type Item = &'a mut [u8];

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            IoIterMut::Ref(iter) => iter.next(),
            IoIterMut::Buf(iter) => iter.next(),
        }
    }
}

/// A sequential splitter for a zero-copy [`IoRef`].
///
/// `IoRefSplitter` divides an [`IoRef`] into a sequence of smaller [`IoRef`]s.
/// Each child refers to a consecutive portion of the parent's data; no data is
/// copied. The splitter maintains an internal cursor and [`take`](Self::take)
/// advances that cursor after successfully creating a child.
///
/// The child block length is specified when the splitter is created. If no
/// child block length is specified, the parent's block length is used. Each
/// call to [`take`](Self::take) then consumes the requested number of child
/// blocks from the current position.
///
/// Splitting operates on the byte range covered by the parent I/O rather than
/// on its individual I/O vectors. Consequently, a child may begin or end in
/// the middle of an [`iovec`]. The resulting child still references the
/// original memory and preserves the parent's scatter/gather layout for the
/// portion it covers.
///
/// The splitter borrows the parent [`IoRef`] for its entire lifetime, and all
/// child [`IoRef`]s returned by [`take`](Self::take) retain the same underlying
/// lifetime. The parent must therefore remain valid while the splitter and
/// its children are in use.
///
/// `IoRefSplitter` is intended for cases where a larger I/O request must be
/// processed as a sequence of smaller requests, such as dividing a request
/// into device-specific blocks or RAID stripes.
///
/// # Examples
///
/// ```no_run
/// use ironspdk::{Io, IoRefSplitter};
///
/// fn split_io(io: &Io) -> Result<(), ironspdk::Error> {
///     let mut splitter = io.split(Some(4096))?;
///
///     let first = splitter.take(1)?;
///     let second = splitter.take(2)?;
///
///     assert_eq!(first.num_blocks(), 1);
///     assert_eq!(second.num_blocks(), 2);
///     Ok(())
/// }
/// ```
pub struct IoRefSplitter<'a> {
    parent_iovs: &'a [c::iovec],
    parent_total_bytes: usize,
    child_block_len: usize,
    cursor_bytes: usize,
}

fn slice_iovs(
    iovs: &[c::iovec],
    mut offset: usize,
    mut len: usize,
) -> Result<Iovecs, Error> {
    let mut result: Iovecs = Iovecs::new();
    for iov in iovs {
        if offset >= iov.iov_len {
            offset -= iov.iov_len;
            continue;
        }
        let start = offset;
        let avail = iov.iov_len - start;
        let take = min(avail, len);
        let new_iov = c::iovec {
            iov_base: unsafe { iov.iov_base.add(start) },
            iov_len: take,
        };
        result.push(new_iov);

        len -= take;
        offset = 0;
        if len == 0 {
            return Ok(result);
        }
    }
    Err(Error::OutOfRange)
}

impl<'a> IoRefSplitter<'a> {
    fn new(parent: &'a IoRef<'a>, child_block_len: Option<usize>) -> Self {
        let child_block_len = child_block_len.unwrap_or(parent.block_len);
        let parent_total_bytes = parent.num_blocks * parent.block_len;
        Self {
            parent_iovs: &parent.data_iovs,
            parent_total_bytes,
            child_block_len,
            cursor_bytes: 0,
        }
    }

    /// Takes the next `blocks` blocks from the splitter.
    ///
    /// The returned [`IoRef`] refers to the next consecutive portion of the
    /// parent's data. No data is copied. The splitter advances its cursor only
    /// after successfully creating the child.
    ///
    /// `blocks` is measured in the child block size specified when the splitter
    /// was created. The requested range must fit entirely within the remaining
    /// portion of the parent I/O.
    ///
    /// The returned reference has the requested child block size and a
    /// `ref_offset` identifying its position within the parent. Its
    /// `offset_blocks` is initialized to zero; callers that need a logical LBA
    /// must set it explicitly, typically using [`IoRef::update_offset_blocks`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::OutOfRange`] if the requested range extends beyond the
    /// remaining data in the parent I/O.
    pub fn take(&mut self, blocks: usize) -> Result<IoRef<'a>, Error> {
        let bytes = blocks * self.child_block_len;
        if self.cursor_bytes + bytes > self.parent_total_bytes {
            return Err(Error::OutOfRange);
        }
        let iovs = slice_iovs(self.parent_iovs, self.cursor_bytes, bytes)?;
        debug_assert!(self.cursor_bytes.is_multiple_of(self.child_block_len));
        let ioref = IoRef {
            data_iovs: iovs,
            offset_blocks: 0u64, // must be set later manually by the caller
            ref_offset: self.cursor_bytes / self.child_block_len,
            num_blocks: blocks,
            block_len: self.child_block_len,
            _marker: PhantomData,
        };
        self.cursor_bytes += bytes;
        Ok(ioref)
    }
}

/// An I/O operation that can be either a reference or an owned buffer.
///
/// This enum allows handling both zero-copy (`IoRef`) and copy-based (`IoBuf`)
/// I/O operations uniformly. It provides methods to query properties and
/// iterate over I/O vectors.
///
/// # Examples
///
/// Creating an I/O operation:
///
/// ```no_run
/// use ironspdk::{DmaBuf, Io};
///
/// fn create_io() -> Result<Io<'static>, ironspdk::Error> {
///     let buf = DmaBuf::new(4096)?;
///     let io = Io::new_buf(buf, 0, 512)?;
///     println!("I/O blocks: {}", io.num_blocks());
///     Ok(io)
/// }
/// ```
#[derive(Debug)]
pub enum Io<'a> {
    Ref(IoRef<'a>),
    Buf(IoBuf),
}

impl<'a> Io<'a> {
    /// Creates a new I/O operation from a DMA buffer.
    pub fn new_buf(data: DmaBuf, offset_blocks: u64, block_len: usize) -> Result<Self, Error> {
        let buf = IoBuf::new(data, offset_blocks, block_len)?;
        Ok(Io::Buf(buf))
    }

    /// Creates an I/O operation from a BdevIo.
    pub fn from_bdev_io(io: &BdevIo, block_len: usize) -> Result<Self, Error> {
        Ok(Io::Ref(IoRef::from_bdev_io(io, block_len)?))
    }

    /// Returns `true` if this is a reference (zero-copy).
    pub fn is_ref(&self) -> bool {
        match self {
            Io::Ref(_) => true,
            Io::Buf(_) => false,
        }
    }

    /// Splits the I/O into smaller blocks.
    ///
    /// # Parameters
    ///
    /// * `child_block_len` - Optional block length for children (defaults to parent's)
    ///
    /// # Returns
    ///
    /// Returns an `IoRefSplitter` for splitting the I/O, or an error if
    /// the I/O is not a reference (`Error::UnsupportedOperation`).
    pub fn split(&'a self, child_block_len: Option<usize>) -> Result<IoRefSplitter<'a>, Error> {
        match self {
            Io::Ref(ioref) => Ok(IoRefSplitter::new(ioref, child_block_len)),
            _ => Err(Error::UnsupportedOperation),
        }
    }

    /// Returns the offset in blocks (LBA).
    pub fn offset_blocks(&self) -> u64 {
        match self {
            Io::Ref(ioref) => ioref.offset_blocks,
            Io::Buf(iobuf) => iobuf.offset_blocks,
        }
    }

    /// Returns the number of blocks.
    pub fn num_blocks(&self) -> usize {
        match self {
            Io::Ref(ioref) => ioref.num_blocks,
            Io::Buf(iobuf) => iobuf.num_blocks,
        }
    }

    /// Returns the block length in bytes.
    pub fn block_len(&self) -> usize {
        match self {
            Io::Ref(ioref) => ioref.block_len,
            Io::Buf(iobuf) => iobuf.block_len,
        }
    }


    /// Returns an iterator over the data buffers of the I/O.
    ///
    /// For [`Io::Ref`], the iterator yields one slice for each scatter/gather
    /// I/O vector.
    ///
    /// For [`Io::Buf`], which owns a contiguous [`DmaBuf`], the
    /// iterator yields a single slice containing the entire buffer.
    ///
    /// The returned slices cover the data associated with the I/O; their total
    /// length is equal to [`Io::total_bytes`](Self::total_bytes).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use ironspdk::{DmaBuf, Error, Io};
    ///
    /// fn count_total_bytes(io: &Io) -> Result<usize, Error> {
    ///     let buf = DmaBuf::new(1024)?;
    ///     let io = Io::new_buf(buf, 0, 512)?;
    ///
    ///     // The buffer contains two 512-byte blocks, so the I/O is 1024 bytes.
    ///     let total: usize = io.iter_iov().map(|s| s.len()).sum();
    ///     assert_eq!(total, io.num_blocks() * 512);
    ///
    ///     Ok(total)
    /// }
    /// ```
    pub fn iter_iov(&self) -> IoIter<'_> {
        match self {
            Io::Ref(ioref) => {
                fn map_iovec(iovec: &c::iovec) -> &[u8] {
                    unsafe { from_raw_parts(iovec.iov_base as *const u8, iovec.iov_len) }
                }
                IoIter::Ref(ioref.data_iovs.iter().map(map_iovec))
            }
            Io::Buf(iobuf) => IoIter::Buf(std::iter::once(iobuf.as_slice())),
        }
    }

    /// Returns a mutable iterator over I/O vectors.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use ironspdk::{DmaBuf, Error, Io};
    ///
    /// fn fill_iov(io: &mut Io) -> Result<(), Error> {
    ///     let buf = DmaBuf::new(1024)?;
    ///     let mut io = Io::new_buf(buf, 0, 512)?;
    ///     for slice in io.iter_iov_mut() {
    ///         slice.fill(0xFF);
    ///     }
    ///     Ok(())
    /// }
    /// ```
    pub fn iter_iov_mut(&mut self) -> IoIterMut<'_> {
        match self {
            Io::Ref(ioref) => {
                fn map_iovec(iovec: &mut c::iovec) -> &mut [u8] {
                    unsafe { from_raw_parts_mut(iovec.iov_base as *mut u8, iovec.iov_len) }
                }
                IoIterMut::Ref(ioref.data_iovs.iter_mut().map(map_iovec))
            }
            Io::Buf(iobuf) => IoIterMut::Buf(std::iter::once(iobuf.as_mut_slice())),
        }
    }
}

/// Represents a range of blocks in an I/O operation.
#[derive(Debug, Copy, Clone)]
pub struct IoRange {
    lba: u64,
    num_blocks: u64,
}

/// Status of an I/O operation.
#[derive(PartialEq)]
pub enum IoStatus {
    Success,
    Failure,
}

/// Rust wrapper around SPDK's `struct spdk_bdev_io`
/// with completion future. Should be used by implementors of
/// 'trait Bdev'
///
/// This structure represents an in-flight or completed I/O operation. It provides
/// a safe interface to SPDK's I/O completion mechanism via futures.
///
/// # Async Completion
///
/// I/O operations are completed asynchronously using futures:
///
/// ```no_run
/// use ironspdk::BdevIo;
///
/// async fn process_io(io: BdevIo) {
///     // Wait for I/O completion
///     io.future().await;
///     println!("I/O completed");
/// }
/// ```
///
/// # SAFETY
///
/// The `BdevIo` wrapper maintains safety invariants:
/// - The raw SPDK pointer is guaranteed non-null
/// - The future is properly synchronized
/// - Completion is handled safely
pub struct BdevIo {
    /// Raw SPDK bdev_io pointer
    raw: NonNull<c::spdk_bdev_io>,

    /// Future for async completion.
    /// BdevIo is immutable by nature, but related future must be mutable.
    /// That's why we use UnsafeCell here.
    fut: UnsafeCell<IoFuture>,
}

impl std::fmt::Debug for BdevIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "BdevIo(raw:{:p}, {:#?}, of={} num={})",
            self.raw,
            self.io_type(),
            self.offset_blocks(),
            self.num_blocks()
        )
    }
}

impl BdevIo {
    /// Creates a new BdevIo wrapper from a raw SPDK pointer.
    pub fn new(raw: *mut c::spdk_bdev_io) -> Self {
        let fut = UnsafeCell::new(IoFuture::new());
        let raw = NonNull::new(raw).expect("bdev io pointer must not be null");
        Self { raw, fut }
    }

    /// Returns a mutable reference to the I/O future.
    /// This method should be called to complete async I/O.
    ///
    /// # SAFETY
    ///
    /// This method is safe because:
    /// - The future is only accessed by one thread at a time
    /// - SPDK guarantees that I/O completion callbacks are serialized
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use ironspdk::BdevIo;
    ///
    /// async fn wait_for_io(io: &BdevIo) {
    ///     io.future().await;
    /// }
    /// ```
    #[allow(clippy::mut_from_ref)]
    pub fn future(&self) -> &mut IoFuture {
        unsafe { &mut *self.fut.get() }
    }

    fn spdk_complete(&self, status: i32) {
        self.future().complete();
        unsafe { c::spdk_bdev_io_complete(self.raw.as_ptr(), status) };
    }

    /// Completes the I/O operation with the given status.
    ///
    /// This method signals the future and calls SPDK's completion function.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use ironspdk::{BdevIo, IoStatus};
    ///
    /// fn complete_io(io: &BdevIo) {
    ///     io.complete(IoStatus::Success);
    /// }
    /// ```
    pub fn complete(&self, status: IoStatus) {
        let status = match status {
            IoStatus::Success => c::SPDK_BDEV_IO_STATUS_SUCCESS,
            IoStatus::Failure => c::SPDK_BDEV_IO_STATUS_FAILED,
        };
        self.spdk_complete(status);
    }

    /// Completes the I/O operation on a specific SPDK thread.
    ///
    /// This is useful when the I/O needs to be completed on a different thread
    /// than the one it was submitted on.
    pub fn complete_on(self, thread: &SpdkThread, status: IoStatus) {
        thread.send_msg(move || {
            self.complete(status);
        });
    }

    /// Returns the I/O type (read, write, etc.).
    pub fn io_type(&self) -> IoType {
        let c_io_type = unsafe { c::u_bdev_io_get_type(self.raw.as_ptr()) };
        IoType::try_from_c(c_io_type).unwrap_or_else(|_| panic!("Invalid C io type: {}", c_io_type))
    }

    /// Returns the offset in blocks (LBA).
    pub fn offset_blocks(&self) -> u64 {
        unsafe { c::u_bdev_io_get_offset_blocks(self.raw.as_ptr()) }
    }

    /// Returns the number of blocks in the I/O.
    pub fn num_blocks(&self) -> u64 {
        unsafe { c::u_bdev_io_get_num_blocks(self.raw.as_ptr()) }
    }

    /// Returns the I/O range if applicable.
    ///
    /// Only read and write I/O types have a range. Other types return `None`.
    pub fn range(&self) -> Option<IoRange> {
        match self.io_type() {
            IoType::Read | IoType::Write => Some(IoRange {
                lba: self.offset_blocks(),
                num_blocks: self.num_blocks(),
            }),
            _ => None,
        }
    }

    fn spdk_bdev(&self) -> NonNull<c::spdk_bdev> {
        let spdk_bdev = unsafe { c::u_bdev_io_get_bdev(self.raw.as_ptr()) };
        NonNull::new(spdk_bdev).expect("bdev pointer must not be null")
    }

    fn bdev_id(&self) -> BdevId {
        BdevId(self.spdk_bdev().as_ptr() as usize)
    }

    /// Returns the block length in bytes.
    pub fn block_len(&self) -> usize {
        let bdev = self.spdk_bdev().as_ptr();
        (unsafe { c::spdk_bdev_get_block_size(bdev) }) as usize
    }

    pub fn dif_type(&self) -> DifType {
        let bdev = self.spdk_bdev().as_ptr();
        let c_dif_type = unsafe { c::spdk_bdev_get_dif_type(bdev) };
        DifType::try_from_c(c_dif_type)
            .unwrap_or_else(|_| panic!("Invalid dif type {}", c_dif_type))
    }
}

/// Bdev I/O channel container (bdev+spdk_thread context is stored here)
#[derive(Debug)]
pub struct BdevIoChannel {
    inner: Box<dyn Any>,
}

impl BdevIoChannel {
    pub fn new<T: Any>(v: T) -> Self {
        Self { inner: Box::new(v) }
    }

    fn downcast_mut<T: Any>(&mut self) -> &mut T {
        self.inner
            .downcast_mut::<T>()
            .expect("invalid io channel type")
    }
}

/// Lightweight wrapper of 'struct spdk_io_channel'.
///
/// Borrows existing SPDK channel.
/// Constructed by ironspdk linbrary and passed to `Bdev.submit_io()`.
/// This is a borrowed reference that does not acquire
/// ownership of the channel.
///
/// # SAFETY
///
/// The handle may only be used on the SPDK thread that owns the channel.
/// Use [`RcBdevIoChannel`] when a channel must be retained by a custom
/// SPDK thread.
///
/// # Usage
///
/// ```no_run
/// use ironspdk::{Bdev, BdevIo, BdevIoChannel, BdevIoChannelRef, IoType,
///                RawBdevHandle, SpdkThread};
/// use std::os::raw::c_void;
/// use std::ptr::NonNull;
///
/// struct MyIoChannel;
/// struct MyBdev;
///
/// impl Bdev for MyBdev {
///     fn init(&self, rawbdev: RawBdevHandle) { todo!() }
///     fn io_type_supported(&self, io_type: IoType) -> bool { todo!() }
///     fn create_io_channel(&self) -> Box<BdevIoChannel> { todo!() }
///     fn submit_io(&self, ch: BdevIoChannelRef, io: BdevIo) {
///         // ... process I/O ...
///         let submit_thread = SpdkThread::current();
///         submit_thread.spawn(async move {
///             let ch = ch.downcast_mut::<MyIoChannel>();
///             // ... use channel ...
///         });
///     }
/// }
/// ```
#[derive(Clone, Copy)]
pub struct BdevIoChannelRef {
    /// Raw SPDK `struct spdk_io_channnel *` pointer
    raw: NonNull<c::spdk_io_channel>,
}

impl std::fmt::Debug for BdevIoChannelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BdevIoChannelRef(raw: {:p})", self.raw.as_ptr())
    }
}

impl BdevIoChannelRef {
    /// Creates a borrowed reference to an existing SPDK I/O channel.
    ///
    /// # SAFETY
    ///
    /// `raw` must be a valid `spdk_io_channel` belonging to the current
    /// SPDK thread. The returned value does not acquire a reference to the
    /// channel and must not outlive the channel.
    ///
    /// # Panics
    ///
    /// Panics if `raw` is null.
    pub(crate) unsafe fn from_raw(raw: *mut c::spdk_io_channel) -> Self {
        Self {
            raw: NonNull::new(raw).expect("SPDK io channel null pointer"),
        }
    }

    /// Returns mutable access to the Rust channel context.
    ///
    /// # SAFETY invariant
    /// SPDK invokes a bdev's submit_request callback serially for a given
    /// SPDK thread, and the channel belongs exclusively to that thread.
    /// Therefore the returned mutable reference will not be used concurrently.
    #[allow(clippy::mut_from_ref)]
    pub fn downcast_mut<T: Any>(&self) -> &mut T {
        let spdk_ch_ctx = unsafe { c::u_spdk_io_channel_get_ctx(self.raw.as_ptr()) };
        let io_ch_ctx = unsafe { c::u_io_channel_get_rust_ctx(spdk_ch_ctx) };
        debug_assert!(!io_ch_ctx.is_null());
        let ch = unsafe { &mut *(io_ch_ctx as *mut BdevIoChannel) };
        ch.downcast_mut::<T>()
    }
}

/// Reference-counted bdev I/O channel wrapper.
/// It uses (struct spdk_io_channel).ref as a non-atomic reference counter.
/// It should be used by custom SPDK threads (created manually
/// with SpdkThread::new() by user).
pub struct RcBdevIoChannel {
    raw: NonNull<c::spdk_io_channel>,
}

impl std::fmt::Debug for RcBdevIoChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let refcnt = unsafe { c::spdk_io_channel_get_ref_count(self.raw.as_ptr()) };
        write!(
            f,
            "RcBdevIoChannel(raw: {:p}, ref: {})",
            self.raw.as_ptr(),
            refcnt
        )
    }
}

impl Clone for RcBdevIoChannel {
    fn clone(&self) -> Self {
        unsafe { c::spdk_io_channel_ref(self.raw.as_ptr()) };
        Self {
            raw: NonNull::new(self.raw.as_ptr()).expect("io channel must not be NULL"),
        }
    }
}

impl Drop for RcBdevIoChannel {
    fn drop(&mut self) {
        unsafe { c::spdk_put_io_channel(self.raw.as_ptr()) };
    }
}

impl RcBdevIoChannel {
    pub fn new(rawbdev: RawBdevHandle) -> Self {
        let raw = unsafe { c::spdk_get_io_channel(rawbdev.as_ptr()) };
        Self {
            raw: NonNull::new(raw).expect("bdev null pointer"),
        }
    }

    #[allow(clippy::mut_from_ref)]
    pub fn downcast_mut<T: Any>(&self) -> &mut T {
        let spdk_ch_ctx = unsafe { c::u_spdk_io_channel_get_ctx(self.raw.as_ptr()) };
        let io_ch_ctx = unsafe { c::u_io_channel_get_rust_ctx(spdk_ch_ctx) };
        debug_assert!(!io_ch_ctx.is_null());
        let ch: &mut BdevIoChannel = unsafe { &mut *(io_ch_ctx as *mut BdevIoChannel) };
        ch.downcast_mut::<T>()
    }
}

/// A Rust implementation of an SPDK block device.
///
/// Implement this trait to expose a Rust object as an SPDK bdev. The
/// implementation is responsible for accepting I/O requests from SPDK,
/// performing the requested operation, and completing each request.
///
/// A `Bdev` implementation is executed by SPDK's reactor threads. The
/// [`Bdev::submit_io`] (equivalent of SPDK's .submit_request())
/// method is called in the context of the
/// SPDK thread that submitted the request, and the implementation must obey
/// SPDK's thread-affinity rules. In particular, SPDK objects such as I/O
/// channels must only be accessed from the SPDK thread that owns them.
///
/// # I/O processing
///
/// [`Bdev::submit_io`] receives a [`BdevIo`] representing one SPDK I/O request.
/// The request may contain scatter/gather buffers and can be accessed
/// through the [`BdevIo`] API without copying the data.
///
/// `fn submit_io()` is not async. However it can spawn a future on SPDK
/// thread (current or another).
/// The future is polled by the `ironspdk` executor associated with related
/// SPDK thread. This makes it possible to express asynchronous I/O using
/// Rust's `async`/`await` syntax while retaining SPDK's reactor-based
/// execution model.
///
/// The implementation must eventually complete the request by calling
/// [`BdevIo::complete`] or [`BdevIo::complete_on`].
///
/// # I/O completion
///
/// An implementation should normally perform the asynchronous work and then
/// complete the request by calling [`BdevIo::complete`] or
/// [`BdevIo::complete_on`] with the appropriate [`IoStatus`]:
///
/// ```no_run
/// use ironspdk::{Bdev, BdevIo, BdevIoChannel, BdevIoChannelRef, IoStatus,
///                IoType, RawBdevHandle};
/// use std::os::raw::c_void;
/// use std::ptr::NonNull;
///
/// struct MyBdev;
///
/// impl Bdev for MyBdev {
///     fn init(&self, rawbdev: RawBdevHandle) { todo!() }
///     fn io_type_supported(&self, io_type: IoType) -> bool { todo!() }
///     fn create_io_channel(&self) -> Box<BdevIoChannel> { todo!() }
///     fn submit_io(&self, ch: BdevIoChannelRef, io: BdevIo) {
///         // Perform the operation...
///
///         io.complete(IoStatus::Success);
///     }
/// }
/// ```
///
/// The exact lifetime of `io` is determined by the `BdevIo` API. References
/// obtained from the request must not be retained after the request is no
/// longer valid.
///
/// # I/O channels
///
/// [`Bdev::create_io_channel`] is used by SPDK to create
/// the per-thread context required by the bdev implementation. An I/O
/// channel is associated with a particular SPDK thread and is not a global
/// resource shared by all threads.
///
/// The channel context can be obtained from `submit_io()` through the
/// [`BdevIoChannelRef`] associated with the current request.
///
/// If an implementation needs to perform work on another SPDK thread, it
/// must use that thread's I/O channel rather than using the channel belonging
/// to the submitting thread.
///
/// # Thread SAFETY
///
/// The trait itself does not require a bdev implementation to be `Send` or
/// `Sync`. This is intentional: a bdev may contain state that is accessed
/// exclusively from an SPDK thread.
///
/// When state is accessed from multiple SPDK threads, the implementation
/// must provide the required synchronization or otherwise arrange its work so
/// that each piece of mutable state has a single owning SPDK thread.
///
/// # Supported I/O types
///
/// [`Bdev::io_type_supported`] determines which SPDK I/O
/// operations the bdev accepts. SPDK may reject or avoid submitting an
/// operation that the bdev reports as unsupported.
///
/// A bdev should only report an I/O type as supported when its implementation
/// can correctly process the corresponding request and complete it according
/// to SPDK's bdev semantics.
///
/// # Implementing a bdev
///
/// A typical implementation consists of:
///
/// 1. [`io_type_supported`](Bdev::io_type_supported) to advertise supported
///    operations.
/// 2. [`create_io_channel`](Bdev::create_io_channel) to create per-SPDK-thread
///    state.
/// 3. [`submit_io`](Bdev::submit_io) to process requests asynchronously.
/// 4. [`BdevIo::complete`] to report the result of each request.
///
/// The bdev can then be registered with SPDK using the appropriate bdev
/// registration API.
///
/// # SAFETY and invariants
///
/// Implementations must preserve the invariants required by both Rust and
/// SPDK. In particular:
///
/// * An I/O request must not be completed more than once.
/// * An I/O request must eventually be completed unless SPDK explicitly
///   permits it to remain outstanding.
/// * Buffers borrowed from a request must not outlive the request.
/// * SPDK thread-affine objects must only be accessed from their owning
///   SPDK thread.
/// * An I/O channel must remain valid for as long as the implementation uses
///   it.
///
/// Violating these requirements can result in memory corruption, use-after-free,
/// data races, or other undefined behavior.
///
/// # Examples
///
/// A minimal bdev implementation has the following shape:
///
/// ```no_run
/// use ironspdk::{Bdev, BdevIo, BdevIoChannel, BdevIoChannelRef, IoStatus, IoType, RawBdevHandle, SpdkThread};
///
/// struct MyBdev;
///
/// impl Bdev for MyBdev {
///     fn init(&self, rawbdev: RawBdevHandle) {
///         // Perform initialization,
///         // e.g. spawn workers, allocate I/O channels etc.
///     }
///
///     fn io_type_supported(&self, io_type: IoType) -> bool {
///         matches!(io_type, IoType::Read | IoType::Write)
///     }
///
///     fn create_io_channel(&self) -> Box<BdevIoChannel> {
///         // Create per-SPDK-thread state.
///         todo!()
///     }
///
///     fn submit_io(&self, ch: BdevIoChannelRef, io: BdevIo) {
///         SpdkThread::current().spawn(async move {
///             // Process `io` using `channel`.
///             io.complete(IoStatus::Success);
///         });
///     }
/// }
/// ```
pub trait Bdev {
    fn init(&self, rawbdev: RawBdevHandle);

    fn io_type_supported(&self, io_type: IoType) -> bool;

    fn create_io_channel(&self) -> Box<BdevIoChannel>;

    fn submit_io(&self, ch: BdevIoChannelRef, io: BdevIo);
}

/// Handle for passing Bdev-s to C FFI
pub type BdevHandle = Arc<dyn Bdev + Send + Sync + 'static>;

/// Handle for passing bdevs between `ironspdk` SpdkThread-s
pub type RawBdevHandle = NonNull<c::spdk_bdev>;

pub struct BdevCtx {
    pub name: String,
    pub bdev: BdevHandle,
    pub spdk_bdev: *mut c::spdk_bdev,
}

impl Drop for BdevCtx {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        {
            assert!(
                !self.spdk_bdev.is_null(),
                "error: drop BdevCtx with .spdk_bdev==NULL"
            );
            debug!("DROP BdevCtx name='{}'", self.name);
        }
    }
}

fn bdev_registry() -> &'static Mutex<HashMap<String, BdevHandle>> {
    BDEV_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

#[unsafe(no_mangle)]
extern "C" fn rsu_bdev_ctx_set_spdk_bdev(ctx: *mut c_void, bdev: *mut c::spdk_bdev) {
    assert!(!ctx.is_null());
    let ctx = unsafe { &mut *(ctx as *mut BdevCtx) };
    ctx.spdk_bdev = bdev;
}

#[unsafe(no_mangle)]
extern "C" fn rsu_bdev_ctx_get_spdk_bdev(ctx: *mut c_void) -> *mut c::spdk_bdev {
    assert!(!ctx.is_null());
    unsafe { (*(ctx as *mut BdevCtx)).spdk_bdev }
}

#[unsafe(no_mangle)]
extern "C" fn rsu_bdev_ctx_drop(ctx: *mut c_void) {
    assert!(!ctx.is_null());

    let ctx = unsafe { Box::from_raw(ctx as *mut BdevCtx) };

    // remove from registry
    let _ = bdev_registry_remove(ctx.name.clone());
    // Arc<dyn Bdev> dropped here
}

pub fn bdev_registry_add(name: String, bdevh: BdevHandle) -> Result<(), Error> {
    let mut reg = bdev_registry().lock();
    if reg.contains_key(&name) {
        return Err(Error::AlreadyExists);
    }
    reg.insert(name, bdevh);
    Ok(())
}

pub fn bdev_registry_remove(name: String) -> Result<BdevHandle, Error> {
    let mut reg = bdev_registry().lock();
    reg.remove(name.as_str())
        .ok_or(Error::SpdkBdevNotFound(name))
}

fn rpc_rs_bdev_delete(args: rpc::RpcCmdArgs) -> rpc::RpcCmdResult {
    let name = args.get("name").unwrap();

    // Check 'name' is in bdev registry. Do not delete bdevs created not by Rust code
    {
        let reg = bdev_registry().lock();
        if !reg.contains_key(name.as_str()) {
            return Err(Error::SpdkBdevNotFound(name.to_string()));
        }
    }

    let name_c = CString::new(name.as_str()).unwrap();
    let name_c_str = name_c.as_ptr();
    let rc = unsafe { c::u_spdk_bdev_delete_by_name(name_c_str) };
    if rc != 0 {
        return Err(Error::SpdkBdevDelete(rc));
    }
    Ok(format!("Successfully deleted bdev '{}'", name))
}
rpc_register!("rs_bdev_delete", rpc_rs_bdev_delete);

#[unsafe(no_mangle)]
extern "C" fn rsu_io_channel_create(bdev_ctxt: *mut c_void) -> *mut c_void {
    let ctx: &BdevCtx = unsafe { &*(bdev_ctxt as *const BdevCtx) };
    let bdevh = ctx.bdev.clone();
    let ch_boxed = Box::into_raw(bdevh.create_io_channel());
    ch_boxed as *mut c_void
}

#[unsafe(no_mangle)]
extern "C" fn rsu_io_channel_destroy(ctxt: *mut c_void) {
    debug_assert!(!ctxt.is_null());
    unsafe { drop(Box::from_raw(ctxt as *mut BdevIoChannel)) };
}

#[unsafe(no_mangle)]
extern "C" fn rsu_bdev_io_type_supported(bdev_ctxt: *mut c_void, c_io_type: i32) -> bool {
    debug_assert!(!bdev_ctxt.is_null());
    let ctx: &BdevCtx = unsafe { &*(bdev_ctxt as *const BdevCtx) };
    let io_type = match IoType::try_from_c(c_io_type) {
        Ok(io_type) => io_type,
        // Maybe new SPDK I/O type, not suported by ironspdk. Return 'false'
        _ => {
            warn!("Unsupported C io type: {}", c_io_type);
            return false;
        }
    };
    let bdev = ctx.bdev.clone();
    bdev.io_type_supported(io_type)
}

#[unsafe(no_mangle)]
extern "C" fn rsu_bdev_init(bdev_ctxt: *mut c_void) {
    let ctx: &BdevCtx = unsafe { &*(bdev_ctxt as *const BdevCtx) };
    let rawbdev = NonNull::new(ctx.spdk_bdev).expect("bdev pointer must not be NULL");
    ctx.bdev.init(rawbdev);
}

#[unsafe(no_mangle)]
extern "C" fn rsu_bdev_submit_request(
    bdev_ctxt: *mut c_void,
    ch: *mut c::spdk_io_channel,
    io: *mut c::spdk_bdev_io,
) {
    debug_assert!(!bdev_ctxt.is_null());
    debug_assert!(!ch.is_null());

    let ctx: &BdevCtx = unsafe { &*(bdev_ctxt as *const BdevCtx) };

    let io = BdevIo::new(io);

    let ch = unsafe { BdevIoChannelRef::from_raw(ch) };

    ctx.bdev.submit_io(ch, io);
}

// SPDK poller trampoline
extern "C" fn poller_fn(ctx: *mut c_void) -> i32 {
    let tcb = unsafe { &mut *(ctx as *mut Tcb) };

    // Normal execution
    // If poll() detects thread is exited, it unregisters the poller
    if tcb.poll() {
        1 // busy
    } else {
        0 // idle
    }
}

pub struct CpuSet {
    raw: NonNull<c::spdk_cpuset>,
}

impl Default for CpuSet {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuSet {
    /// Create empty CPU set
    pub fn new() -> Self {
        let raw = unsafe { c::u_spdk_cpuset_alloc() };
        Self {
            raw: NonNull::new(raw).expect("failed to allocate cpuset"),
        }
    }

    /// Create CPU set from iterator of cores
    pub fn from_cores<I>(cores: I) -> Self
    where
        I: IntoIterator<Item = u32>,
    {
        let mut set = Self::new();
        for core in cores {
            set.set(core);
        }
        set
    }

    /// Set a core in the cpuset
    pub fn set(&mut self, core: u32) {
        unsafe { c::spdk_cpuset_set_cpu(&mut *self.raw.as_ptr(), core, true) }
    }

    pub fn clear(&mut self) {
        unsafe { c::spdk_cpuset_zero(self.raw.as_ptr()) }
    }

    /// Expose raw pointer for FFI
    pub fn as_ptr(&self) -> *const c::spdk_cpuset {
        self.raw.as_ptr()
    }
}

impl Drop for CpuSet {
    fn drop(&mut self) {
        unsafe { c::u_spdk_cpuset_free(self.raw.as_ptr()) }
    }
}

/// SPDK thread thin wrapper around `struct spdk_thread`.
/// Represents an SPDK lightweight thread with its associated reactor.
///
/// SPDK uses a thread-per-core model where each thread has its own
/// reactor and processes I/O completions. This structure provides
/// a safe interface to SPDK's threading primitives.
///
/// # Thread Safety
///
/// - Each SPDK thread is bound to a specific CPU core
/// - I/O channels are thread-local
/// - Messages can be sent between threads using `send_msg`
///
/// # Examples
///
/// ```no_run
/// use ironspdk::SpdkThread;
/// use log::debug;
///
/// debug!("current SPDK thread ID: {}", SpdkThread::current().id());
/// ```
#[derive(Clone)]
pub struct SpdkThread {
    /// Raw SPDK `struct spdk_thread *` pointer
    raw: NonNull<c::spdk_thread>,
}

// SpdkThread is a thread id, it is movable between threads
// (implements Sync+Send)
unsafe impl Send for SpdkThread {}
unsafe impl Sync for SpdkThread {}

/// Convenience wrapper function to get current SPDK thread ID.
pub fn thread_id() -> u64 {
    SpdkThread::current().id()
}

impl SpdkThread {
    /// Returns the current SPDK thread.
    ///
    /// # Panics
    ///
    /// Panics if called outside of an SPDK thread.
    pub fn current() -> Self {
        let raw = unsafe { c::spdk_get_thread() };
        Self {
            raw: NonNull::new(raw).expect("Failed to get current SPDK thread"),
        }
    }

    /// Returns `true` if this thread is the current SPDK thread.
    pub fn is_current(&self) -> bool {
        self.raw.as_ptr() == unsafe { c::spdk_get_thread() }
    }

    /// Returns the number of CPU cores available to SPDK.
    pub fn core_count() -> u32 {
        unsafe { c::spdk_env_get_core_count() }
    }

    /// Wrapper around `spdk_thread_is_running()`.
    pub fn is_running(&self) -> bool {
        unsafe { c::spdk_thread_is_running(self.raw.as_ptr()) }
    }

    /// Wrapper around `spdk_thread_is_exited()`.
    pub fn is_exited(&self) -> bool {
        unsafe { c::spdk_thread_is_exited(self.raw.as_ptr()) }
    }

    /// Creates a new SPDK thread with the given name.
    pub fn new(name: &str) -> Self {
        Self::new_at_cpuset(name, None)
    }

    /// Creates a new SPDK thread with the given name at specified CPU cores.
    pub fn new_at_cores<I>(name: &str, cores: I) -> Self
    where
        I: IntoIterator<Item = u32>,
    {
        let cpuset = CpuSet::from_cores(cores);
        Self::new_at_cpuset(name, Some(&cpuset))
    }

    ///
    /// Creates a new SPDK thread with the given name and optional CPU set.
    pub fn new_at_cpuset(name: &str, cpuset: Option<&CpuSet>) -> Self {
        let name_c = CString::new(name).unwrap();
        let raw = unsafe {
            c::spdk_thread_create(
                name_c.as_ptr(),
                cpuset.map(|c| c.as_ptr()).unwrap_or_else(std::ptr::null),
            )
        };
        Self {
            raw: NonNull::new(raw).expect("failed to create SPDK thread"),
        }
    }

    /// Returns SPDK thread ID.
    pub fn id(&self) -> u64 {
        unsafe { c::spdk_thread_get_id(self.raw.as_ptr()) }
    }

    fn send_msg<F>(&self, f: F)
    where
        F: FnOnce() + 'static,
    {
        extern "C" fn trampoline(ctx: *mut c_void) {
            let f = unsafe { Box::<Box<dyn FnOnce()>>::from_raw(ctx as _) };
            f();
        }

        let boxed: Box<Box<dyn FnOnce()>> = Box::new(Box::new(f));

        let rc = unsafe {
            c::spdk_thread_send_msg(
                self.raw.as_ptr(),
                trampoline,
                Box::into_raw(boxed) as *mut _,
            )
        };
        if rc != 0 {
            panic!("spdk_thread_send_msg failed: {}", rc);
        }
    }

    /// Spawns a future on this thread.
    ///
    /// The future will be polled on the thread's reactor.
    pub fn spawn<F>(&self, fut: F)
    where
        F: Future<Output = ()> + 'static,
    {
        self.send_msg(move || {
            let tcb = Tcb::current();
            tcb.spawn(fut);
        });
    }

    /// Requests the SPDK thread to exit.
    ///
    /// # SAFETY
    ///
    /// This is the only legitimate way to request an SPDK thread to exit.
    /// It will trigger the thread's exit sequence.
    pub fn request_exit(&self) {
        self.send_msg(|| unsafe {
            c::spdk_thread_exit(c::spdk_get_thread());
        });
    }
}

pub const TLS_SLOTS: usize = 4;

/// SPDK TLS interface
///
/// It introduces SPDK TLS (which is not present in native SPDK)
/// ironspdk library user can store/load values at several TLS keys
/// (maximum count of keys is TLS_SLOTS). SPDK TLS values are visible
/// to current SPDK thread only and live while it lives.
/// SPDK TLS is cheap: takes one cache line of memory and
/// O(1) of time at loading/storing.
///
/// # SAFETY
///
/// The code of TlsKey::new() and TlsKey::alloc() panics if key slot
/// count exceeds TLS_SLOTS. This in intentional: SPDK TLS is a limited resource
/// and should not be wasted.
///
/// # Examples
///
/// ```no_run
/// use ironspdk::TlsKey;
///
/// struct MyIoChannel;
///
/// fn my_ioch_set_and_get() {
///    // allocate the key at free slot
///    let my_ioch_tls: TlsKey<MyIoChannel> = TlsKey::alloc();
///    my_ioch_tls.set(MyIoChannel {});
///    let val = my_ioch_tls.get().expect("TLS value mut be set");
/// }
/// ```
///
/// ```no_run
/// use ironspdk::TlsKey;
///
/// struct MyIoChannel;
///
/// // explicitly take a slot with index 0
/// static MY_IOCH_TLS: TlsKey<MyIoChannel> = TlsKey::new(0);
/// fn my_ioch_set_and_get() {
///    MY_IOCH_TLS.set(MyIoChannel {});
///    let val = MY_IOCH_TLS.get().expect("TLS value mut be set");
/// }
/// ```
pub struct TlsKey<T: 'static> {
    slot: usize,
    _marker: PhantomData<fn() -> T>,
}

unsafe impl<T: 'static> Send for TlsKey<T> {}
unsafe impl<T: 'static> Sync for TlsKey<T> {}

impl<T: 'static> Clone for TlsKey<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: 'static> Copy for TlsKey<T> {}

impl<T: 'static> TlsKey<T> {
    /// Const constructor for `static` declarations.
    /// The caller picks the index explicitly.
    pub const fn new(slot: usize) -> Self {
        assert!(slot < TLS_SLOTS, "TLS slot index out of bounds");
        Self {
            slot,
            _marker: PhantomData,
        }
    }

    /// Dynamic allocator for non-static use
    pub fn alloc() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let slot = NEXT.fetch_add(1, Ordering::Relaxed);
        assert!(slot < TLS_SLOTS, "out of TLS slots");
        Self {
            slot,
            _marker: PhantomData,
        }
    }

    #[inline]
    pub fn set(&self, value: T) {
        Tcb::current().tls.set(self, value);
    }

    #[inline]
    pub fn get(&self) -> Option<&T> {
        Tcb::current().tls.get(self)
    }

    #[inline]
    pub fn get_mut(&self) -> Option<&mut T> {
        Tcb::current().tls.get_mut(self)
    }

    pub fn clear(&self) {
        Tcb::current().tls.clear_slot(self.slot);
    }
}

// SPDK TLS representation.
// No heap, no atomics, no locks, no reallocs
#[repr(align(64))]
struct Tls {
    slots: [UnsafeCell<*mut c_void>; TLS_SLOTS],
    drop_fns: [UnsafeCell<Option<DropFn>>; TLS_SLOTS],
}

type DropFn = unsafe fn(*mut c_void);

// SAFETY: TLS is only accessed by SPDK thread that owns the TCB
unsafe impl Send for Tls {}
unsafe impl Sync for Tls {}

unsafe fn drop_box<T>(p: *mut c_void) {
    drop(unsafe { Box::from_raw(p as *mut T) });
}

impl Default for Tls {
    fn default() -> Self {
        Self::new()
    }
}

impl Tls {
    fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| UnsafeCell::new(std::ptr::null_mut())),
            drop_fns: std::array::from_fn(|_| UnsafeCell::new(None)),
        }
    }

    #[inline]
    fn set<T: 'static>(&self, key: &TlsKey<T>, value: T) {
        self.clear_slot(key.slot);
        let ptr = Box::into_raw(Box::new(value));
        unsafe {
            *self.slots[key.slot].get() = ptr as *mut c_void;
            *self.drop_fns[key.slot].get() = Some(drop_box::<T>);
        }
    }

    #[inline]
    fn get<T: 'static>(&self, key: &TlsKey<T>) -> Option<&T> {
        let ptr = unsafe { *self.slots[key.slot].get() };
        if ptr.is_null() {
            None
        } else {
            Some(unsafe { &*(ptr as *const T) })
        }
    }

    #[inline]
    #[allow(clippy::mut_from_ref)]
    fn get_mut<T: 'static>(&self, key: &TlsKey<T>) -> Option<&mut T> {
        let ptr = unsafe { *self.slots[key.slot].get() };
        if ptr.is_null() {
            None
        } else {
            Some(unsafe { &mut *(ptr as *mut T) })
        }
    }

    fn clear_all(&self) {
        for i in 0..TLS_SLOTS {
            self.clear_slot(i);
        }
    }

    #[inline]
    fn clear_slot(&self, index: usize) {
        let ptr = unsafe { *self.slots[index].get() };
        if !ptr.is_null() {
            if let Some(drop_fn) = unsafe { *self.drop_fns[index].get() } {
                unsafe { drop_fn(ptr) };
            }
            unsafe {
                *self.slots[index].get() = std::ptr::null_mut();
                *self.drop_fns[index].get() = None;
            }
        }
    }
}

//
// Use local TLS to access TCB in O(1) time.
// Store spdk_thread ID (which is unique and increases on creation of
// new spdk_thread-s) and TCB.
// This facilitates survival if and when SPDK migrates spdk_thread to other reactor
//
std::thread_local! {
    // 0 means “no cache”. spdk_thread IDs start at 1.
    static CACHED_THREAD_ID: Cell<u64> = const { Cell::new(0) };
    static CACHED_TCB: Cell<*const Tcb> = const { Cell::new(std::ptr::null()) };
}

// Thread Control Block (per SPDK thread)
// It stores
//     - executor tied with SPDK poller
//     - thread-local storage
// This interface is internal for ironspdk runtime.
struct Tcb {
    runq: RefCell<VecDeque<Rc<Task>>>,
    poller: Cell<*mut c::spdk_poller>,
    tls: Tls,
}

impl Tcb {
    // Get current TCB
    // Hot path: 2 loads + 1 u64 compare + 1 branch
    // Slow path: one RwLock read, executed on SPDK thread cross-reactor migration
    #[inline]
    fn current() -> &'static Self {
        let thread = unsafe { c::spdk_get_thread() };
        assert!(
            !thread.is_null(),
            "Tcb::current() called outside of SPDK thread"
        );

        let current_id = unsafe { c::spdk_thread_get_id(thread) };
        let cached_id = CACHED_THREAD_ID.with(|c| c.get());
        if cached_id == current_id {
            let ptr = CACHED_TCB.with(|c| c.get());
            // SAFETY: SPDK thread IDs are unique and monotonic.
            // Equity means thread has not changed and CACHED_TCB points to
            // real TCB
            unsafe { &*ptr }
        } else {
            Self::current_slow(thread, current_id)
        }
    }

    #[cold]
    fn tcb_ptr_from_registry(thread_key: ThreadKey) -> *const Tcb {
        // fast path: try read existing value
        {
            let map = tcb_registry().read();
            if let Some(&tcb_ptr) = map.get(&thread_key) {
                return unsafe { &*(tcb_ptr.ptr() as *const Tcb) };
            }
        }
        // slow path: read-or-create value
        let mut map = tcb_registry().write();
        let tcb_ptr = map.entry(thread_key).or_insert_with(|| {
            let tcb = Tcb::new();
            TcbPtr::from_tcb(tcb)
        });
        tcb_ptr.ptr() as *const Tcb
    }

    #[cold]
    fn current_slow(thread: *mut c::spdk_thread, thread_id: u64) -> &'static Self {
        let thread_key = ThreadKey::from_thread(thread);
        let tcb_ptr = Self::tcb_ptr_from_registry(thread_key);

        CACHED_THREAD_ID.with(|c| c.set(thread_id));
        CACHED_TCB.with(|c| c.set(tcb_ptr));

        unsafe { &*tcb_ptr }
    }

    fn new() -> *mut Tcb {
        let tcb = Box::new(Tcb {
            runq: RefCell::new(VecDeque::new()),
            poller: Cell::new(std::ptr::null_mut()),
            tls: Tls::new(),
        });

        let tcb_ptr = Box::into_raw(tcb);
        let poller = unsafe { c::spdk_poller_register(poller_fn, tcb_ptr as *mut _, 0) };
        assert!(!poller.is_null(), "Failed to create poller");
        unsafe { (*tcb_ptr).poller.set(poller) };
        tcb_ptr
    }

    fn spawn<F>(&self, fut: F)
    where
        F: Future<Output = ()> + 'static,
    {
        let task = Rc::new(Task {
            future: RefCell::new(Box::pin(fut)),
            state: Cell::new(TaskState::Idle),
        });
        self.runq.borrow_mut().push_back(task);
    }

    fn poll(&self) -> bool {
        if !SpdkThread::current().is_running() {
            // current SPDK thread is exiting or exited
            self.shutdown();
            return false;
        }
        let mut busy = false;
        loop {
            let task = {
                // need to drop runq borrow when polling
                let mut runq = self.runq.borrow_mut();
                runq.pop_front()
            };
            if let Some(task) = task {
                let task: Rc<Task> = task;
                Task::poll(task);
                busy = true;
            } else {
                break;
            }
        }
        busy
    }

    fn shutdown(&self) {
        let thread = unsafe { c::spdk_get_thread() };
        assert!(!thread.is_null(), "Not on SPDK thread");

        // Call drop() on TLS slots. Write lock must not be taken yet
        self.tls.clear_all();

        // Drain run queue
        self.runq.borrow_mut().clear();

        unsafe {
            c::spdk_poller_unregister(&mut (self.poller.get() as *mut _));
        }

        // Remove from TCB_REGISTRY
        let mut map = tcb_registry().write();
        let thread_key = ThreadKey::from_thread(thread);
        let _ = map.remove(&thread_key).expect("TCB not found in registry");
    }
}

impl Drop for Tcb {
    fn drop(&mut self) {
        self.tls.clear_all();
    }
}

/// Task (wrapper of Future)
struct Task {
    future: RefCell<Pin<Box<dyn Future<Output = ()>>>>,
    state: Cell<TaskState>,
}

#[derive(Clone, Copy, PartialEq)]
enum TaskState {
    Idle,
    Running,
    Notified,
    Ready,
}

impl Task {
    fn poll(task: Rc<Task>) {
        if task.state.get() == TaskState::Running {
            return;
        }

        task.state.set(TaskState::Running);

        let waker = unsafe { Waker::from_raw(raw_waker(task.clone())) };
        let mut cx = Context::from_waker(&waker);

        let poll_result = {
            let mut fut = task.future.borrow_mut();
            fut.as_mut().poll(&mut cx)
        };

        match poll_result {
            Poll::Ready(_) => {
                task.state.set(TaskState::Ready);
            }
            Poll::Pending => {
                if task.state.get() == TaskState::Notified {
                    task.state.set(TaskState::Idle);
                    Tcb::current().runq.borrow_mut().push_back(task.clone());
                } else {
                    task.state.set(TaskState::Idle);
                }
            }
        }
    }

    fn wake(task: &Rc<Task>) {
        let tcb = Tcb::current();
        if !SpdkThread::current().is_running() {
            tcb.shutdown();
            return;
        }

        match task.state.get() {
            TaskState::Running => {
                task.state.set(TaskState::Notified);
            }
            TaskState::Idle => {
                task.state.set(TaskState::Notified);
                tcb.runq.borrow_mut().push_back(task.clone());
            }
            TaskState::Notified | TaskState::Ready => {}
        }
    }
}

// RawWaker for future
unsafe fn raw_waker(task: Rc<Task>) -> RawWaker {
    RawWaker::new(Rc::into_raw(task) as *const (), &WAKER_VTABLE)
}

static WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(clone_waker, wake, wake_by_ref, drop_waker);

unsafe fn clone_waker(ptr: *const ()) -> RawWaker {
    let rc = unsafe { Rc::from_raw(ptr as *const Task) };
    let cloned = rc.clone();
    std::mem::forget(rc);
    unsafe { raw_waker(cloned) }
}

unsafe fn wake(ptr: *const ()) {
    let rc = unsafe { Rc::from_raw(ptr as *const Task) };
    Task::wake(&rc);
    // do not forget rc, consume refcnt
}

unsafe fn wake_by_ref(ptr: *const ()) {
    let rc = unsafe { Rc::from_raw(ptr as *const Task) };
    Task::wake(&rc);
    std::mem::forget(rc); // forget rc, do not consume refcnt
}

unsafe fn drop_waker(ptr: *const ()) {
    let rc = unsafe { Rc::from_raw(ptr as *const Task) };
    drop(rc);
}

/// SPDK-pure IoFuture
#[derive(Default)]
pub struct IoFuture {
    done: bool,
    waker: Option<Waker>,
}

impl Future for IoFuture {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.done {
            Poll::Ready(())
        } else {
            self.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl IoFuture {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn complete(&mut self) {
        self.done = true;
        if let Some(w) = self.waker.take() {
            w.wake();
        }
    }
}

// *** Client code for lower-layer bdevs ***

/// Thin wrapper around `struct spdk_bdev_desc`
#[derive(Clone, Debug)]
pub struct BdevDesc {
    raw: NonNull<c::spdk_bdev_desc>,
}

impl BdevDesc {
    pub fn open(name: &str, write: bool) -> Result<Self, Error> {
        let name_c = CString::new(name).unwrap();
        let mut desc: *mut c::spdk_bdev_desc = std::ptr::null_mut();

        let rc = unsafe { c::u_bdev_open(name_c.as_ptr(), write, &mut desc) };
        if rc != 0 {
            return Err(Error::SpdkBdevOpen(rc));
        }
        let raw = NonNull::new(desc).expect("bdev desc must not be NULL");
        Ok(Self { raw })
    }

    pub fn bdev(&self) -> *mut c::spdk_bdev {
        unsafe { c::spdk_bdev_desc_get_bdev(self.raw.as_ptr()) }
    }

    pub fn block_len(&self) -> usize {
        (unsafe { c::u_bdev_get_blocklen(self.bdev()) }) as usize
    }

    pub fn number_of_blocks(&self) -> u64 {
        unsafe { c::u_bdev_get_blockcnt(self.bdev()) }
    }
}

impl Drop for BdevDesc {
    fn drop(&mut self) {
        unsafe { c::spdk_bdev_close(self.raw.as_ptr()) };
        debug!("DROP BdevDesc");
    }
}

/// SPDK I/O channel intended to use with Lbdev
#[derive(Clone, Debug)]
pub struct LbdevIoChannel {
    raw: NonNull<c::spdk_io_channel>,
}

impl Drop for LbdevIoChannel {
    fn drop(&mut self) {
        unsafe { c::spdk_put_io_channel(self.raw.as_ptr()) };
        debug!("DROP SpdkIoChannel");
    }
}

impl LbdevIoChannel {
    pub fn new(raw: NonNull<c::spdk_io_channel>) -> Self {
        Self { raw }
    }
}

pub struct LbdevIoCtx {
    iovs: Iovecs, //Vec<c::iovec>,
    result: Rc<LbdevIoResult>,
}

pub struct LbdevIoResult {
    fut: UnsafeCell<IoFuture>,
    success: Cell<bool>,
}

impl LbdevIoResult {
    #[allow(clippy::mut_from_ref)]
    pub fn future(&self) -> &mut IoFuture {
        unsafe { &mut *self.fut.get() }
    }

    pub fn success(&self) -> bool {
        self.success.get()
    }
}

extern "C" fn spdk_rwio_complete_cb(
    bdev_io: *mut c::spdk_bdev_io,
    success: bool,
    cb_arg: *mut std::ffi::c_void,
) {
    let ctx = unsafe { Rc::from_raw(cb_arg as *const LbdevIoCtx) };
    // pass status to caller which awaits
    ctx.result.success.set(success);
    // wake waiter
    let fut = unsafe { &mut *ctx.result.fut.get() };
    fut.complete();

    // this callback must free bdev_io
    unsafe { c::spdk_bdev_free_io(bdev_io) };

    // ctx ref count is decremented here
}

/// Lower SPDK block device which is accessed using client SPDK API.
/// Used by application code and implementors of 'trait Bdev'
/// for accessing lower-layer bdevs
#[derive(Clone, Debug)]
pub struct Lbdev {
    name: String,
    desc: Box<BdevDesc>,
}

impl Lbdev {
    pub fn open(name: &str) -> Result<Self, Error> {
        let desc = Box::new(BdevDesc::open(name, true)?);
        Ok(Self {
            name: name.to_string(),
            desc,
        })
    }

    pub fn desc(&self) -> &BdevDesc {
        &self.desc
    }

    pub fn get_io_channel(&self) -> Rc<LbdevIoChannel> {
        let ch = unsafe { c::spdk_bdev_get_io_channel(self.desc.raw.as_ptr()) };
        let ch = NonNull::new(ch).expect("spdk_bdev_get_io_channel failed");
        Rc::new(LbdevIoChannel::new(ch))
    }

    pub fn read(&self, ch: &LbdevIoChannel, mut io: Io) -> Rc<LbdevIoResult> {
        let mut iovs_c: Iovecs = Iovecs::new();
        for iov_slice in io.iter_iov_mut() {
            let iov_c = c::iovec {
                iov_base: iov_slice.as_mut_ptr() as *mut _,
                iov_len: iov_slice.len(),
            };
            iovs_c.push(iov_c);
        }

        let result = Rc::new(LbdevIoResult {
            fut: UnsafeCell::new(IoFuture::new()),
            success: Cell::new(false),
        });
        let ctx = Rc::new(LbdevIoCtx {
            iovs: iovs_c,
            result: result.clone(),
        });

        // increase ref count for spdk_rwio_complete_cb()
        let ctx_ptr = Rc::into_raw(ctx.clone()) as *mut _;

        let lba = io.offset_blocks();
        let num_blocks = io.num_blocks();
        let rc = unsafe {
            c::spdk_bdev_readv_blocks(
                self.desc.raw.as_ptr(),
                ch.raw.as_ptr(),
                ctx.iovs.as_ptr() as *mut c_void,
                ctx.iovs.len() as i32,
                lba,
                num_blocks as u64,
                spdk_rwio_complete_cb,
                ctx_ptr,
            )
        };
        if rc != 0 {
            // spdk_bdev_readv_blocks() failed, the callback was not called
            // need to drop ctx ref count
            drop(unsafe { Rc::from_raw(ctx_ptr as *const LbdevIoCtx) });

            result.success.set(false);
            let fut = unsafe { &mut *result.fut.get() };
            fut.complete();
        }
        result
    }

    pub fn write(&self, ch: &LbdevIoChannel, io: Io) -> Rc<LbdevIoResult> {
        let mut iovs_c: Iovecs = Iovecs::new();
        for iov_slice in io.iter_iov() {
            let iov_c = c::iovec {
                iov_base: iov_slice.as_ptr() as *mut _,
                iov_len: iov_slice.len(),
            };
            iovs_c.push(iov_c);
        }

        let result = Rc::new(LbdevIoResult {
            fut: UnsafeCell::new(IoFuture::new()),
            success: Cell::new(false),
        });
        let ctx = Rc::new(LbdevIoCtx {
            iovs: iovs_c,
            result: result.clone(),
        });

        // increase ref count for spdk_rwio_complete_cb()
        let ctx_ptr = Rc::into_raw(ctx.clone()) as *mut _;

        let lba = io.offset_blocks();
        let num_blocks = io.num_blocks();
        let rc = unsafe {
            c::spdk_bdev_writev_blocks(
                self.desc.raw.as_ptr(),
                ch.raw.as_ptr(),
                ctx.iovs.as_ptr() as *mut c_void,
                ctx.iovs.len() as i32,
                lba,
                num_blocks as u64,
                spdk_rwio_complete_cb,
                ctx_ptr,
            )
        };
        if rc != 0 {
            // spdk_bdev_writev_blocks() failed, the callback was not called
            // need to drop ctx ref count
            drop(unsafe { Rc::from_raw(ctx_ptr as *const LbdevIoCtx) });

            result.success.set(false);
            let fut = unsafe { &mut *result.fut.get() };
            fut.complete();
        }
        result
    }
}

// *** SPDK bdev options support ***

/// SPDK bdev options placeholder. Should be used with define_bdev_opts!
/// macro to generate code which converts RPC arguments to bdev options
/// Put here fields of 'struct spdk_bdev' which may be passed as RPC arguments
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SpdkBdevOptsC {
    pub blocklen: u32,
    pub blockcnt: u64,
    pub write_cache: bool,
    pub phys_blocklen: u32,
    pub split_on_write_unit: u32,
    pub split_on_optimal_io_boundary: u32,
    pub md_interleave: u32,
    pub dif_is_head_of_md: u32,
    pub write_unit_size: u32,
    pub optimal_io_boundary: u32,
    pub preferred_write_alignment: u32,
    pub preferred_write_granularity: u32,
    pub optimal_write_size: u32,
    pub preferred_unmap_alignment: u32,
    pub preferred_unmap_granularity: u32,
}

/// Codegen conversion from RPC arguments to bdev options
#[macro_export]
macro_rules! define_bdev_opts {
    (
        $name:ident {
            $(
                $field:ident : $ty:ty = $default:expr
            ),* $(,)?
        }
    ) => {
        #[derive(Debug, Clone)]
        pub struct $name {
            $( pub $field: $ty ),*
        }

        impl $name {
            pub fn from_rpc(args: &rpc::RpcCmdArgs) -> Result<Self, ironspdk::Error> {
                Ok(Self {
                    $(
                        $field: {
                            if let Some(v) = args.get(stringify!($field)) {
                                v.parse::<$ty>()
                                    .map_err(|_| ironspdk::Error::InvalidField(stringify!($field).to_string()))?
                            } else {
                                $default
                            }
                        }
                    ),*
                })
            }

            pub fn to_c(&self) -> SpdkBdevOptsC {
                let mut cfg = SpdkBdevOptsC::default();

                $(
                    cfg.$field = self.$field;
                )*

                cfg
            }
        }
    };
}
