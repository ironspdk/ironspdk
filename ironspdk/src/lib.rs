#![allow(dead_code)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use ctor::ctor;
use log::debug;
use log::error;
use num_enum::{IntoPrimitive, TryFromPrimitive};
use parking_lot::{Mutex, RwLock};
use paste::paste;
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
}

#[derive(Copy, Clone, Hash, Eq, PartialEq)]
pub(crate) struct BdevId(usize);

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

static TCB_REGISTRY: OnceLock<RwLock<HashMap<ThreadKey, TcbPtr>>> = OnceLock::new();

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
    }
}

struct DmaBufInner {
    ptr: NonNull<u8>,
    len: usize,
}

unsafe impl Send for DmaBufInner {}
unsafe impl Sync for DmaBufInner {}

impl Drop for DmaBufInner {
    fn drop(&mut self) {
        unsafe { c::spdk_dma_free(self.ptr.as_ptr() as *mut _) }
    }
}

/// Data buffer allocated from DMA memory.
/// It may be shared between threads, so it implements Send+Sync+Clone.
#[derive(Clone)]
pub struct DmaBuf {
    inner: Arc<DmaBufInner>,
}

impl DmaBuf {
    pub fn new(len: usize, align: usize) -> Result<Self, Error> {
        let ptr = unsafe { c::spdk_dma_malloc(len, align, std::ptr::null_mut()) };
        let ptr = NonNull::new(ptr as *mut u8).ok_or(Error::NoMemory)?;
        Ok(Self {
            inner: Arc::new(DmaBufInner { ptr, len }),
        })
    }

    pub fn len(&self) -> usize {
        self.inner.len
    }

    pub fn is_empty(&self) -> bool {
        self.inner.len == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { from_raw_parts(self.inner.ptr.as_ptr(), self.inner.len) }
    }

    /// Safe mutation only if uniquely owned. Returns error otherwise.
    pub fn as_mut_slice(&mut self) -> Result<&mut [u8], Error> {
        let inner = Arc::get_mut(&mut self.inner).ok_or(Error::SharedBufferModification)?;
        let rc = unsafe { from_raw_parts_mut(inner.ptr.as_ptr(), self.inner.len) };
        Ok(rc)
    }

    /// Get write access to shared buffer without checking reference count.
    /// This is needed for performance reasons (encrypted disks as an example).
    /// # SAFETY
    /// It is programmer's responsibility to ensure:
    /// - no concurrent mutable accesses
    /// - no concurrent immutable accesses during mutation
    /// - proper synchronization across SpdkThread-s
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn as_mut_slice_unchecked(&self) -> &mut [u8] {
        unsafe { from_raw_parts_mut(self.inner.ptr.as_ptr(), self.inner.len) }
    }
}

pub struct IoRef<'a> {
    data_iovs: Vec<c::iovec>,
    offset_blocks: u64, // LBA
    ref_offset: usize,  // offset in parent ioref in self.block_len's, zero for parent
    num_blocks: usize,
    block_len: usize,
    _marker: PhantomData<&'a IoRef<'a>>,
}

impl<'a> IoRef<'a> {
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
            data_iovs: data_iovs.to_vec(),
            offset_blocks,
            ref_offset: 0usize,
            num_blocks,
            block_len,
            _marker: PhantomData,
        })
    }

    /// Change 'offset_blocks' of IoRef
    /// (for example when splitting or reordering) using this method
    pub fn update_offset_blocks(&mut self, offset_blocks: u64) {
        self.offset_blocks = offset_blocks;
    }

    pub fn total_bytes(&self) -> usize {
        self.num_blocks * self.block_len
    }

    pub fn to_buf(&self) -> Result<IoBuf, Error> {
        let total = self.total_bytes();
        let mut dmabuf = DmaBuf::new(total, 64)?;
        let data = dmabuf.as_mut_slice()?;
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

pub struct IoBuf {
    data: DmaBuf,
    offset_blocks: u64,
    num_blocks: usize,
    block_len: usize,
}

impl IoBuf {
    pub fn new(data: &DmaBuf, offset_blocks: u64, block_len: usize) -> Result<IoBuf, Error> {
        // check data alignment
        let data_len = data.len();
        if !data_len.is_multiple_of(block_len) {
            error!("data length is not aligned to block length");
            return Err(Error::InvalidArguments);
        }
        Ok(Self {
            data: data.clone(),
            offset_blocks,
            num_blocks: data_len / block_len,
            block_len,
        })
    }

    pub fn total_bytes(&self) -> usize {
        self.num_blocks * self.block_len
    }

    pub fn as_slice(&self) -> &[u8] {
        self.data.as_slice()
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.data
            .as_mut_slice()
            .expect("Attempt to modify shared buffer")
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
) -> Result<Vec<c::iovec>, Error> {
    let mut result = Vec::new();
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

pub enum Io<'a> {
    Ref(IoRef<'a>),
    Buf(IoBuf),
}

impl<'a> Io<'a> {
    pub fn new_buf(data: &DmaBuf, offset_blocks: u64, block_len: usize) -> Result<Self, Error> {
        let buf = IoBuf::new(data, offset_blocks, block_len)?;
        Ok(Io::Buf(buf))
    }

    pub fn from_bdev_io(io: &BdevIo, block_len: usize) -> Result<Self, Error> {
        Ok(Io::Ref(IoRef::from_bdev_io(io, block_len)?))
    }

    pub fn is_ref(&self) -> bool {
        match self {
            Io::Ref(_) => true,
            Io::Buf(_) => false,
        }
    }

    pub fn split(&'a self, child_block_len: Option<usize>) -> Result<IoRefSplitter<'a>, Error> {
        match self {
            Io::Ref(ioref) => Ok(IoRefSplitter::new(ioref, child_block_len)),
            _ => Err(Error::UnsupportedOperation),
        }
    }

    pub fn offset_blocks(&self) -> u64 {
        match self {
            Io::Ref(ioref) => ioref.offset_blocks,
            Io::Buf(iobuf) => iobuf.offset_blocks,
        }
    }

    pub fn num_blocks(&self) -> usize {
        match self {
            Io::Ref(ioref) => ioref.num_blocks,
            Io::Buf(iobuf) => iobuf.num_blocks,
        }
    }

    pub fn block_len(&self) -> usize {
        match self {
            Io::Ref(ioref) => ioref.block_len,
            Io::Buf(iobuf) => iobuf.block_len,
        }
    }

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

#[derive(Debug, Copy, Clone)]
pub struct IoRange {
    lba: u64,
    num_blocks: u64,
}

pub enum IoStatus {
    Success,
    Failure,
}

/// Rust BdevIo wrapper around 'struct spdk_bdev_io'
/// with completion future. Should be used by implementors of
/// 'trait Bdev'
pub struct BdevIo {
    raw: NonNull<c::spdk_bdev_io>,

    // BdevIo is immutable by nature, but related future must be mutable.
    // That's why we use UnsafeCell here
    fut: UnsafeCell<IoFuture>,
}

impl BdevIo {
    pub fn new(raw: *mut c::spdk_bdev_io) -> Self {
        let fut = UnsafeCell::new(IoFuture::new());
        let raw = NonNull::new(raw).expect("bdev io pointer must not be null");
        Self { raw, fut }
    }

    /// This method should be called to complete async I/O.
    ///     Example:
    ///         io.future().await;
    #[allow(clippy::mut_from_ref)]
    pub fn future(&self) -> &mut IoFuture {
        unsafe { &mut *self.fut.get() }
    }

    fn spdk_complete(&self, status: i32) {
        self.future().complete();
        unsafe { c::spdk_bdev_io_complete(self.raw.as_ptr(), status) };
    }

    pub fn complete(&self, status: IoStatus) {
        let status = match status {
            IoStatus::Success => c::SPDK_BDEV_IO_STATUS_SUCCESS,
            IoStatus::Failure => c::SPDK_BDEV_IO_STATUS_FAILED,
        };
        self.spdk_complete(status);
    }

    pub fn complete_on(self, thread: &SpdkThread, status: IoStatus) {
        thread.send_msg(move || {
            self.complete(status);
        });
    }

    pub fn io_type(&self) -> IoType {
        let c_io_type = unsafe { c::u_bdev_io_get_type(self.raw.as_ptr()) };
        IoType::try_from_c(c_io_type).unwrap_or_else(|_| panic!("Invalid C io type: {}", c_io_type))
    }

    pub fn offset_blocks(&self) -> u64 {
        unsafe { c::u_bdev_io_get_offset_blocks(self.raw.as_ptr()) }
    }

    pub fn num_blocks(&self) -> u64 {
        unsafe { c::u_bdev_io_get_num_blocks(self.raw.as_ptr()) }
    }

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
        write!(f, "raw: {:p}, ref: {}", self.raw, refcnt)
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

/// Rust Bdev trait. If you write ironspdk Rust bdev module,
/// you should implement this trait.
pub trait Bdev {
    fn init(&self, ctx: RawBdevHandle);

    fn io_type_supported(&self, io_type: IoType) -> bool;

    fn create_io_channel(&self) -> Box<BdevIoChannel>;

    fn submit_io(&self, ch: &mut BdevIoChannel, io: BdevIo);
}

/// Handle for passing Bdev-s to C FFI
pub type BdevHandle = Arc<dyn Bdev + Send + Sync + 'static>;

/// Handle for passing bdevs between Rust SpdkThread-s
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
    let io_type = IoType::try_from_c(c_io_type)
        .unwrap_or_else(|_| panic!("Invalid C io type: {}", c_io_type));
    let bdev = ctx.bdev.clone();
    bdev.io_type_supported(io_type)
}

#[unsafe(no_mangle)]
extern "C" fn rsu_bdev_init(bdev_ctxt: *mut c_void) {
    let ctx: &BdevCtx = unsafe { &*(bdev_ctxt as *const BdevCtx) };
    ctx.bdev
        .init(NonNull::new(ctx.spdk_bdev).expect("bdev pointer must not be NULL"));
}

#[unsafe(no_mangle)]
extern "C" fn rsu_bdev_submit_request(
    bdev_ctxt: *mut c_void,
    io_ch_ctxt: *mut c_void,
    io: *mut c::spdk_bdev_io,
) {
    debug_assert!(!bdev_ctxt.is_null());
    debug_assert!(!io_ch_ctxt.is_null());

    let ctx: &BdevCtx = unsafe { &*(bdev_ctxt as *const BdevCtx) };

    let io = BdevIo::new(io);
    let ch: &mut BdevIoChannel = unsafe { &mut *(io_ch_ctxt as *mut BdevIoChannel) };

    SpdkThread::current().spawn(async move {
        ctx.bdev.submit_io(ch, io);
    });
}

// SPDK poller trampoline
extern "C" fn poller_fn(ctx: *mut c_void) -> i32 {
    let tcb = unsafe { &mut *(ctx as *mut Tcb) };

    // Normal execution
    // Is poll() detects thread is exited, it unregisters the poller
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
    pub(crate) fn as_ptr(&self) -> *const c::spdk_cpuset {
        self.raw.as_ptr()
    }
}

impl Drop for CpuSet {
    fn drop(&mut self) {
        unsafe { c::u_spdk_cpuset_free(self.raw.as_ptr()) }
    }
}

// *** Rust asynchronous runtime for SPDK ***

/// SPDK thread wrapper
#[derive(Clone)]
pub struct SpdkThread {
    raw: NonNull<c::spdk_thread>,
}

// SpdkThread is a thread id, it is movable between threads
// (implements Sync+Send)
unsafe impl Send for SpdkThread {}
unsafe impl Sync for SpdkThread {}

pub fn thread_id() -> u64 {
    SpdkThread::current().id()
}

impl SpdkThread {
    pub fn current() -> Self {
        let raw = unsafe { c::spdk_get_thread() };
        Self {
            raw: NonNull::new(raw).expect("Failed to get current SPDK thread"),
        }
    }

    pub fn is_current(&self) -> bool {
        self.raw.as_ptr() == unsafe { c::spdk_get_thread() }
    }

    pub fn core_count() -> u32 {
        unsafe { c::spdk_env_get_core_count() }
    }

    pub fn is_running(&self) -> bool {
        unsafe { c::spdk_thread_is_running(self.raw.as_ptr()) }
    }

    pub fn is_exited(&self) -> bool {
        unsafe { c::spdk_thread_is_exited(self.raw.as_ptr()) }
    }

    pub fn new(name: &str) -> Self {
        Self::new_at_cpuset(name, None)
    }

    pub fn new_at_cores<I>(name: &str, cores: I) -> Self
    where
        I: IntoIterator<Item = u32>,
    {
        let cpuset = CpuSet::from_cores(cores);
        Self::new_at_cpuset(name, Some(&cpuset))
    }

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

    pub fn spawn<F>(&self, fut: F)
    where
        F: Future<Output = ()> + 'static,
    {
        self.send_msg(move || {
            let tcb = Tcb::current();
            tcb.spawn(fut);
        });
    }

    pub fn request_exit(&self) {
        self.send_msg(|| unsafe {
            c::spdk_thread_exit(c::spdk_get_thread());
        });
    }
}

/// Rust thread control block (per SPDK thread)
/// It contains
///     - executor tied with SPDK poller
///     - storage for I/O channels owned by this SPDK thread
pub struct Tcb {
    runq: RefCell<VecDeque<Rc<Task>>>,
    poller: Cell<*mut c::spdk_poller>,
    io_channels: RefCell<HashMap<BdevId, RefCell<RcBdevIoChannel>>>,
}

impl Tcb {
    pub fn current() -> &'static Self {
        let thread = unsafe { c::spdk_get_thread() };
        assert!(!thread.is_null(), "Not on SPDK thread");

        let thread_key = ThreadKey::from_thread(thread);

        // fast path (lock for read)
        {
            let map = tcb_registry().read();
            if let Some(&tcb_ptr) = map.get(&thread_key) {
                return unsafe { &*(tcb_ptr.ptr() as *const Tcb) };
            }
        }

        // slow path (lock for write)
        let mut map = tcb_registry().write();
        let tcb_ptr = map.entry(thread_key).or_insert_with(|| {
            let tcb = Tcb::new();
            TcbPtr::from_tcb(tcb)
        });
        unsafe { &*(tcb_ptr.ptr() as *const Tcb) }
    }

    fn new() -> *mut Tcb {
        let tcb = Box::new(Tcb {
            runq: RefCell::new(VecDeque::new()),
            poller: Cell::new(std::ptr::null_mut()),
            io_channels: RefCell::new(HashMap::new()),
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
        let mut map = tcb_registry().write();
        let thread = unsafe { c::spdk_get_thread() };
        assert!(!thread.is_null(), "Not on SPDK thread");

        // Remove association between thread and TCB
        let thread_key = ThreadKey::from_thread(thread);
        let _ = map.remove(&thread_key).unwrap();

        // Drain and drop io channels
        self.io_channels.borrow_mut().clear();

        // Drain run queue and drop tasks
        self.runq.borrow_mut().clear();

        unsafe {
            c::spdk_poller_unregister(&mut (self.poller.get() as *mut _));
        }
    }

    pub fn set_io_channel(&self, rawbdev: RawBdevHandle, ch: RcBdevIoChannel) {
        self.io_channels
            .borrow_mut()
            .insert(BdevId(rawbdev.as_ptr() as usize), RefCell::new(ch));
    }

    pub fn io_channel(&self, io: &BdevIo) -> Option<RcBdevIoChannel> {
        self.io_channels
            .borrow()
            .get(&io.bdev_id())
            .map(|ch| ch.borrow().clone())
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

/// Thin wrapper around 'struct spdk_bdev_desc'
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
        let bdev = self.bdev();
        (unsafe { c::u_bdev_get_blocklen(bdev) }) as usize
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
    iovs: Vec<c::iovec>,
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

    pub fn get_io_channel(&self) -> Rc<LbdevIoChannel> {
        let ch = unsafe { c::spdk_bdev_get_io_channel(self.desc.raw.as_ptr()) };
        let ch = NonNull::new(ch).expect("spdk_bdev_get_io_channel failed");
        Rc::new(LbdevIoChannel::new(ch))
    }

    pub fn read(&self, ch: &LbdevIoChannel, io: Io) -> Rc<LbdevIoResult> {
        self.rwio(false, ch, io)
    }

    pub fn write(&self, ch: &LbdevIoChannel, io: Io) -> Rc<LbdevIoResult> {
        self.rwio(true, ch, io)
    }

    fn rwio(&self, write: bool, ch: &LbdevIoChannel, mut io: Io) -> Rc<LbdevIoResult> {
        let mut iovs_c: Vec<c::iovec> = Vec::new();
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
        let f = if write {
            c::spdk_bdev_writev_blocks
        } else {
            c::spdk_bdev_readv_blocks
        };

        let lba = io.offset_blocks();
        let num_blocks = io.num_blocks();
        let rc = unsafe {
            f(
                self.desc.raw.as_ptr(),
                ch.raw.as_ptr(),
                ctx.iovs.as_ptr() as *const c_void,
                ctx.iovs.len() as i32,
                lba,
                num_blocks as u64,
                spdk_rwio_complete_cb,
                ctx_ptr,
            )
        };
        if rc != 0 {
            // f() failed, the callback was not called
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
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SpdkBdevOptsC {
    pub blocklen: u32,
    pub blockcnt: u64,
    pub write_cache: bool,
    pub phys_blocklen: u32,
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
