use std::os::raw::{c_char, c_int, c_void};

//////////////////////////////////////////////////////////////
// Minimal SPDK FFI
//////////////////////////////////////////////////////////////

#[repr(C)]
#[derive(Clone, Default)]
pub struct iovec {
    pub iov_base: *mut c_void,
    pub iov_len: usize,
}

pub const SPDK_BDEV_IO_STATUS_SUCCESS: i32 = 1;
pub const SPDK_BDEV_IO_STATUS_FAILED: i32 = -1;

pub type spdk_dif_type = i32;

pub const SPDK_DIF_DISABLE: spdk_dif_type = 0;
pub const SPDK_DIF_TYPE1: spdk_dif_type = 1;
pub const SPDK_DIF_TYPE2: spdk_dif_type = 2;
pub const SPDK_DIF_TYPE3: spdk_dif_type = 3;

pub type spdk_bdev_io_type = i32;

pub const SPDK_BDEV_IO_TYPE_INVALID: spdk_bdev_io_type = 0;
pub const SPDK_BDEV_IO_TYPE_READ: spdk_bdev_io_type = 1;
pub const SPDK_BDEV_IO_TYPE_WRITE: spdk_bdev_io_type = 2;
pub const SPDK_BDEV_IO_TYPE_UNMAP: spdk_bdev_io_type = 3;
pub const SPDK_BDEV_IO_TYPE_FLUSH: spdk_bdev_io_type = 4;
pub const SPDK_BDEV_IO_TYPE_RESET: spdk_bdev_io_type = 5;
pub const SPDK_BDEV_IO_TYPE_NVME_ADMIN: spdk_bdev_io_type = 6;
pub const SPDK_BDEV_IO_TYPE_NVME_IO: spdk_bdev_io_type = 7;
pub const SPDK_BDEV_IO_TYPE_NVME_IO_MD: spdk_bdev_io_type = 8;
pub const SPDK_BDEV_IO_TYPE_WRITE_ZEROES: spdk_bdev_io_type = 9;
pub const SPDK_BDEV_IO_TYPE_ZCOPY: spdk_bdev_io_type = 10;
pub const SPDK_BDEV_IO_TYPE_GET_ZONE_INFO: spdk_bdev_io_type = 11;
pub const SPDK_BDEV_IO_TYPE_ZONE_MANAGEMENT: spdk_bdev_io_type = 12;
pub const SPDK_BDEV_IO_TYPE_ZONE_APPEND: spdk_bdev_io_type = 13;
pub const SPDK_BDEV_IO_TYPE_COMPARE: spdk_bdev_io_type = 14;
pub const SPDK_BDEV_IO_TYPE_COMPARE_AND_WRITE: spdk_bdev_io_type = 15;
pub const SPDK_BDEV_IO_TYPE_ABORT: spdk_bdev_io_type = 16;
pub const SPDK_BDEV_IO_TYPE_SEEK_HOLE: spdk_bdev_io_type = 17;
pub const SPDK_BDEV_IO_TYPE_SEEK_DATA: spdk_bdev_io_type = 18;
pub const SPDK_BDEV_IO_TYPE_COPY: spdk_bdev_io_type = 19;
pub const SPDK_BDEV_IO_TYPE_NVME_IOV_MD: spdk_bdev_io_type = 20;
pub const SPDK_BDEV_IO_TYPE_NVME_NSSR: spdk_bdev_io_type = 21;
pub const SPDK_BDEV_IO_TYPE_WRITE_UNCORRECTABLE: spdk_bdev_io_type = 22;

// enum spdk_bdev_event_type
pub const SPDK_BDEV_EVENT_REMOVE: i32 = 0;
pub const SPDK_BDEV_EVENT_RESIZE: i32 = 1;
pub const SPDK_BDEV_EVENT_MEDIA_MANAGEMENT: i32 = 2;

pub type spdk_thread = c_void;
pub type spdk_poller = c_void;
pub type spdk_io_channel = c_void;
pub type spdk_bdev_desc = c_void;
pub type spdk_bdev = c_void;
pub type spdk_bdev_io = c_void;
pub type spdk_app_opts = c_void;
pub type spdk_json_val = c_void;
pub type spdk_cpuset = c_void;

unsafe extern "C" {
    // Unsafe utility functions

    pub fn smp_cpu_id() -> i32;

    // Unsafe C helpers for SPDK functions

    pub fn u_spdk_app_opts_size() -> usize;

    pub fn u_spdk_app_opts_init(opts: *mut spdk_app_opts, name: *const c_char);

    pub fn u_spdk_app_parse_args(argc: i32, argv: *mut *mut i8, opts: *mut spdk_app_opts) -> i32;

    pub fn u_spdk_app_set_shutdown_cb(opts: *mut spdk_app_opts, shutdown_cb: extern "C" fn());

    pub fn u_spdk_app_start(
        opts: *mut spdk_app_opts,
        start_fn: extern "C" fn(*mut c_void),
        arg: *mut c_void,
    ) -> i32;

    pub fn u_spdk_app_stop(rc: i32);

    pub fn u_spdk_cpuset_alloc() -> *mut spdk_cpuset;

    pub fn u_spdk_cpuset_free(set: *mut spdk_cpuset);

    pub fn u_spdk_io_channel_get_ctx(ch: *mut spdk_io_channel) -> *mut c_void;

    // Unsafe C shim functions

    pub fn u_bdev_io_get_type(io: *const spdk_bdev_io) -> i32;

    pub fn u_bdev_io_get_offset_blocks(io: *const spdk_bdev_io) -> u64;

    pub fn u_bdev_io_get_num_blocks(io: *const spdk_bdev_io) -> u64;

    pub fn u_bdev_io_get_iovec(io: *const spdk_bdev_io, iovp: *mut *mut iovec, iovcntp: *mut i32);

    pub fn u_bdev_io_get_bdev(io: *const spdk_bdev_io) -> *mut spdk_bdev;

    pub fn u_bdev_get_blocklen(io: *const spdk_bdev) -> u32;

    pub fn u_spdk_bdev_delete_by_name(name: *const c_char) -> i32;

    pub fn u_io_channel_get_rust_ctx(spdk_ch_ctx: *mut c_void) -> *mut c_void;

    pub fn u_io_channel_set_rust_ctx(spdk_ch_ctx: *mut c_void, rust_ctx: *mut c_void);

    pub fn u_bdev_open(
        bdev_name: *const c_char,
        write: bool,
        desc: *mut *mut spdk_bdev_desc,
    ) -> i32;

    // Unsafe C JSON helpers

    pub fn u_json_object_len(val: *const spdk_json_val) -> usize;

    pub fn u_json_val_name(val: *const spdk_json_val, i: usize) -> *const spdk_json_val;

    pub fn u_json_val_val(val: *const spdk_json_val, i: usize) -> *const spdk_json_val;

    pub fn u_json_val_len(val: *const spdk_json_val) -> usize;

    pub fn u_json_val_str_ptr(val: *const spdk_json_val) -> *const c_char;

    pub fn u_json_val_str_len(val: *const spdk_json_val) -> usize;

    // SPDK C FFI

    pub fn spdk_thread_create(name: *const c_char, cpumask: *const c_void) -> *mut spdk_thread;

    pub fn spdk_thread_send_msg(
        thread: *mut spdk_thread,
        fn_: extern "C" fn(*mut c_void),
        arg: *mut c_void,
    ) -> i32;

    pub fn spdk_get_thread() -> *mut spdk_thread;

    pub fn spdk_thread_get_id(thread: *mut spdk_thread) -> u64;

    pub fn spdk_thread_is_running(thread: *mut spdk_thread) -> bool;

    pub fn spdk_thread_is_exited(thread: *mut spdk_thread) -> bool;

    pub fn spdk_thread_exit(thread: *mut spdk_thread);

    pub fn spdk_bdev_desc_get_bdev(desc: *mut spdk_bdev_desc) -> *mut spdk_bdev;

    pub fn spdk_bdev_close(desc: *mut spdk_bdev_desc);

    pub fn spdk_bdev_io_get_md_buf(io: *mut spdk_bdev_io) -> *mut c_void;

    pub fn spdk_get_io_channel(io_device: *mut c_void) -> *mut spdk_io_channel;

    pub fn spdk_bdev_get_md_size(bdev: *const spdk_bdev) -> u32;

    pub fn spdk_bdev_is_md_separate(bdev: *const spdk_bdev) -> bool;

    pub fn spdk_bdev_get_io_channel(desc: *mut spdk_bdev_desc) -> *mut c_void;

    pub fn spdk_put_io_channel(ch: *mut spdk_io_channel);

    pub fn spdk_io_channel_ref(ch: *mut spdk_io_channel) -> *mut spdk_io_channel;

    pub fn spdk_io_channel_get_ref_count(ch: *mut spdk_io_channel) -> i32;

    pub fn spdk_bdev_io_type_supported(bdev: *mut spdk_bdev, io_type: i32) -> bool;

    pub fn spdk_bdev_get_dif_type(bdev: *mut spdk_bdev) -> i32;

    pub fn spdk_bdev_get_block_size(bdev: *mut spdk_bdev) -> u32;

    pub fn spdk_bdev_readv_blocks(
        desc: *mut spdk_bdev_desc,
        ch: *mut spdk_io_channel,
        iov: *const c_void,
        iovcnt: c_int,
        offset_blocks: u64,
        num_blocks: u64,
        cb: extern "C" fn(*mut spdk_bdev_io, bool, *mut c_void),
        cb_arg: *mut c_void,
    ) -> c_int;

    pub fn spdk_bdev_writev_blocks(
        desc: *mut spdk_bdev_desc,
        ch: *mut spdk_io_channel,
        iov: *const c_void,
        iovcnt: c_int,
        offset_blocks: u64,
        num_blocks: u64,
        cb: extern "C" fn(*mut spdk_bdev_io, bool, *mut c_void),
        cb_arg: *mut c_void,
    ) -> c_int;

    pub fn spdk_bdev_io_complete(io: *mut spdk_bdev_io, status: i32);

    pub fn spdk_bdev_free_io(io: *mut spdk_bdev_io);

    pub fn spdk_poller_register(
        fn_: extern "C" fn(*mut c_void) -> i32,
        arg: *mut c_void,
        period_us: u64,
    ) -> *mut spdk_poller;

    pub fn spdk_poller_unregister(ppoller: *mut *mut spdk_poller);

    pub fn spdk_poller_pause(poller: *mut spdk_poller);

    pub fn spdk_poller_resume(poller: *mut spdk_poller);

    pub fn spdk_cpuset_zero(set: *mut spdk_cpuset);

    pub fn spdk_cpuset_set_cpu(set: *mut spdk_cpuset, cpu: u32, state: bool);

    pub fn spdk_env_get_core_count() -> u32;

    pub fn spdk_dma_malloc(size: usize, align: usize, unused: *mut u64) -> *mut c_void;

    pub fn spdk_dma_free(buf: *mut c_void);
}
