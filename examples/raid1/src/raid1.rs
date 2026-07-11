use ctor::ctor;
use ironspdk::Error;
use ironspdk::define_bdev_opts;
use ironspdk::rpc;
use ironspdk::rpc_register;
use ironspdk::{
    Bdev, BdevCtx, BdevHandle, BdevIo, BdevIoChannel, Io, IoFuture, IoStatus, IoType, Lbdev,
    LbdevIoChannel, RawBdevHandle, RcBdevIoChannel, SpdkBdevOptsC, SpdkThread, Tcb, thread_id,
};
use log::{debug, error};
use paste::paste;
use std::cell::{Cell, UnsafeCell};
use std::os::raw::{c_char, c_void};
use std::rc::Rc;
use std::sync::Arc;

struct RaidIoResult {
    remain: Cell<usize>,
    success: Cell<bool>,
    fut: UnsafeCell<IoFuture>,
}

impl RaidIoResult {
    fn new(n: usize) -> Rc<Self> {
        Rc::new(Self {
            remain: Cell::new(n),
            success: Cell::new(true),
            fut: UnsafeCell::new(IoFuture::new()),
        })
    }

    #[allow(clippy::mut_from_ref)]
    fn future(&self) -> &mut IoFuture {
        unsafe { &mut *self.fut.get() }
    }

    fn child_done(&self, ok: bool) {
        if !ok {
            self.success.set(false);
        }
        debug_assert!(self.remain.get() != 0);
        let remain = self.remain.get() - 1;
        self.remain.set(remain);
        if remain == 0 {
            self.future().complete();
        }
    }
}

struct Raid1IoChannel {
    children: Vec<Rc<Lbdev>>,
    chans: Vec<Rc<LbdevIoChannel>>,
    next_read: usize,
}

struct Raid1Bdev {
    name: String,

    /// Strip size, bytes
    strip_size: usize,

    /// Block length, bytes
    blocklen: usize,

    children_names: Vec<String>,
    workers: Vec<SpdkThread>,
}

impl Drop for Raid1Bdev {
    fn drop(&mut self) {
        // Need to stop all workers. This is mandatory.
        for w in &self.workers {
            w.request_exit();
        }
        debug!("DROP Raid1Bdev #{} name='{}'", thread_id(), self.name);
    }
}

impl Bdev for Raid1Bdev {
    fn init(&self, rawbdev: RawBdevHandle) {
        for w in &self.workers {
            w.spawn(async move {
                let refch = RcBdevIoChannel::new(rawbdev);
                Tcb::current().set_io_channel(rawbdev, refch);
            });
        }
    }

    fn io_type_supported(&self, io_type: IoType) -> bool {
        matches!(io_type, IoType::Read | IoType::Write | IoType::Flush)
    }

    fn create_io_channel(&self) -> Box<BdevIoChannel> {
        let mut children: Vec<Rc<Lbdev>> = Vec::with_capacity(self.children_names.len());
        for cname in self.children_names.clone() {
            match Lbdev::open(cname.as_str()) {
                Ok(dev) => {
                    children.push(Rc::new(dev));
                }
                Err(err) => {
                    panic!("Failed to open block device '{}': {}", cname, err);
                }
            }
        }
        let mut chans = Vec::with_capacity(self.children_names.len());
        for child in &children {
            chans.push(child.get_io_channel());
        }
        Box::new(BdevIoChannel::new(Raid1IoChannel {
            children,
            chans,
            next_read: 0,
        }))
    }

    fn submit_io(&self, _ch: &mut BdevIoChannel, io: BdevIo) {
        let idx = self.owner_thread_idx(io.offset_blocks());
        let owner_thread = self.workers[idx].clone();
        let sender_thread = SpdkThread::current();
        let self_ptr = self as *const Raid1Bdev;
        owner_thread.spawn(async move {
            let self1 = unsafe { &*self_ptr };
            let refch = Tcb::current()
                .io_channel(&io)
                .expect("I/O channel not found");
            let ch = refch.downcast_mut::<Raid1IoChannel>();
            match io.io_type() {
                IoType::Read => self1.submit_read(&sender_thread, ch, io).await,
                IoType::Write => self1.submit_write(&sender_thread, ch, io).await,
                IoType::Flush => {
                    // flush==noop for now. TODO implement flush
                    io.complete_on(&sender_thread, IoStatus::Success);
                }
                _ => {
                    io.complete_on(&sender_thread, IoStatus::Failure);
                }
            }
        });
    }
}

impl Raid1Bdev {
    pub fn new(
        name: &str,
        blocklen: usize,
        strip_size: usize,
        children_names: Vec<&str>,
    ) -> Result<Self, Error> {
        let n = SpdkThread::core_count();
        let mut workers = Vec::with_capacity(n as usize);
        for core in 0..n {
            workers.push(SpdkThread::new_at_cores(
                format!("{}_worker_{}", name, core).as_str(),
                [core],
            ));
        }
        Ok(Self {
            name: name.to_string(),
            strip_size,
            blocklen,
            children_names: children_names.iter().map(|&s| s.to_string()).collect(),
            workers,
        })
    }

    fn owner_thread_idx(&self, off: u64) -> usize {
        let strip_size_in_blocks = (self.strip_size / self.blocklen) as u64;
        ((off / strip_size_in_blocks) % (self.workers.len() as u64)) as usize
    }

    async fn submit_read(&self, sender_thread: &SpdkThread, ch: &mut Raid1IoChannel, io: BdevIo) {
        debug_assert!(!ch.children.is_empty());

        let n = ch.children.len();
        debug_assert!(ch.chans.len() == n);

        let mut status = IoStatus::Failure;

        for _ in 0..n {
            // round-robin read
            let next = (ch.next_read + 1) % n;
            ch.next_read = next;

            let ioref = Io::from_bdev_io(&io, 0).expect("Cannot convert to IoRef");
            let res = ch.children[next].read(&ch.chans[next], ioref);
            res.future().await;

            if res.success() {
                status = IoStatus::Success;
                break;
            }

            // Failover. Read from next child.
            debug!("FAILOVER #{} {} {:?}", thread_id(), next, io);
        }

        if status == IoStatus::Failure {
            error!("Read error (all children failed) #{} {:?}", thread_id(), io);
        }

        io.complete_on(sender_thread, status);
    }

    async fn submit_write(&self, sender_thread: &SpdkThread, ch: &mut Raid1IoChannel, io: BdevIo) {
        debug_assert!(!ch.children.is_empty());
        debug_assert!(ch.chans.len() == ch.children.len());

        let res = RaidIoResult::new(ch.children.len());

        let mut crs = Vec::new();
        for (idx, child) in ch.children.iter().enumerate() {
            let ioref = Io::from_bdev_io(&io, 0).expect("Cannot convert to IoRef");
            let child_res = child.write(&ch.chans[idx], ioref);
            crs.push(child_res);
        }
        for child_res in crs {
            child_res.future().await;
            res.child_done(child_res.success());
        }

        res.future().await;

        let status = if res.success.get() {
            IoStatus::Success
        } else {
            IoStatus::Failure
        };
        io.complete_on(sender_thread, status);
    }
}

/// Default strip size (if not specified in arguments), bytes
const DEFAULT_STRIP_SIZE: usize = 128 * 1024;

define_bdev_opts!(Raid1BdevOpts {
    blocklen: u32 = 512,                    // default: 512 bytes
    blockcnt: u64 = 64 * 1024 * 1024 / 512, // default: 64 MBytes
    write_cache: bool = false,
});

unsafe extern "C" {
    fn raid1_bdev_create(
        name: *const c_char,
        opts: *const SpdkBdevOptsC,
        rscx: *const c_void,
    ) -> i32;
}

fn parse_strip_size(args: rpc::RpcCmdArgs, blocklen: usize) -> Result<usize, Error> {
    let strip_size_kb = if let Some(strip_size_kb_str) = args.get("strip-size-kb") {
        strip_size_kb_str.parse::<u32>()?
    } else {
        0
    };
    let mut strip_size = (strip_size_kb * 1024) as usize;
    if strip_size == 0 {
        strip_size = DEFAULT_STRIP_SIZE / blocklen * blocklen;
    }
    if strip_size.is_multiple_of(blocklen) {
        Ok(strip_size)
    } else {
        Err(Error::InvalidArguments)
    }
}

fn parse_children(children_names: &Vec<&str>) -> Result<(usize, u64), Error> {
    let mut blocklen: Option<usize> = None;
    let mut num_blocks: Option<u64> = None;
    if children_names.is_empty() {
        return Err(Error::InvalidArguments);
    }
    for cname in children_names {
        match Lbdev::open(cname) {
            Ok(dev) => {
                let bdev_blocklen = dev.desc().block_len();
                match blocklen {
                    None => blocklen = Some(bdev_blocklen),
                    Some(existing) if existing != bdev_blocklen => {
                        return Err(Error::InvalidArguments);
                    }
                    _ => {}
                }
                let bdev_num_blocks = dev.desc().number_of_blocks();
                match num_blocks {
                    None => num_blocks = Some(bdev_num_blocks),
                    Some(existing) if existing != bdev_num_blocks => {
                        return Err(Error::InvalidArguments);
                    }
                    _ => {}
                }
            }
            Err(err) => {
                return Err(err);
            }
        }
    }
    Ok((blocklen.unwrap(), num_blocks.unwrap()))
}

fn rpc_rs_raid1_create(args: rpc::RpcCmdArgs) -> rpc::RpcCmdResult {
    let name = args.get("name").ok_or(Error::InvalidArguments)?;
    let children_names_cs = args.get("children").ok_or(Error::InvalidArguments)?;
    let children_names: Vec<&str> = children_names_cs.split(',').collect();
    let (blocklen, num_blocks) = parse_children(&children_names)?;
    let strip_size = parse_strip_size(args.clone(), blocklen)?;

    let bdevh: BdevHandle = Arc::new(Raid1Bdev::new(name, blocklen, strip_size, children_names)?);

    ironspdk::bdev_registry_add(name.to_string(), bdevh.clone())?;

    let ctx = Box::new(BdevCtx {
        name: name.to_string(),
        bdev: bdevh.clone(),
        spdk_bdev: std::ptr::null_mut(),
    });

    let mut opts = Raid1BdevOpts::from_rpc(&args)?;
    opts.blockcnt = num_blocks;
    opts.blocklen = blocklen as u32;

    let opts_c = opts.to_c();
    let ctx_ptr = Box::into_raw(ctx) as *mut c_void;
    let name_c = std::ffi::CString::new(name.as_str()).unwrap();

    let rc = unsafe { raid1_bdev_create(name_c.as_ptr(), &opts_c, ctx_ptr) };
    if rc != 0 {
        unsafe { drop(Box::from_raw(ctx_ptr as *mut BdevCtx)) };
        let _ = ironspdk::bdev_registry_remove(name.to_string())?;
        return Err(Error::SpdkBdevCreate(rc));
    }

    Ok(format!("Successfully created RAID1 bdev: '{}'", name))
}
rpc_register!("rs_raid1_create", rpc_rs_raid1_create);
