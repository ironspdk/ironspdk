use ctor::ctor;
use ironspdk::Error;
use ironspdk::define_bdev_opts;
use ironspdk::rpc;
use ironspdk::rpc_register;
use ironspdk::{
    Bdev, BdevCtx, BdevHandle, BdevIo, BdevIoChannel, Io, IoFuture, IoStatus, IoType, Lbdev,
    LbdevIoChannel, RawBdevHandle, RcBdevIoChannel, SpdkBdevOptsC, SpdkThread, Tcb, thread_id,
};
use log::debug;
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

impl Drop for Raid1IoChannel {
    fn drop(&mut self) {
        debug!("DROP Raid1IoChannel {:p}", &self);
    }
}

struct Raid1Bdev {
    name: String,
    children_names: Vec<String>,
    workers: Vec<SpdkThread>,
}

impl Drop for Raid1Bdev {
    fn drop(&mut self) {
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
    pub fn new(name: &str, children_names: Vec<&str>) -> Result<Self, Error> {
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
            children_names: children_names.iter().map(|&s| s.to_string()).collect(),
            workers,
        })
    }

    fn owner_thread_idx(&self, off: u64) -> usize {
        ((off / 0x100) % (self.workers.len() as u64)) as usize
    }

    async fn submit_read(&self, sender_thread: &SpdkThread, ch: &mut Raid1IoChannel, io: BdevIo) {
        debug_assert!(!ch.children.is_empty());

        let n = ch.children.len();
        debug_assert!(ch.chans.len() == n);

        // round-robin read
        let next = (ch.next_read + 1) % n;
        ch.next_read = next;
        // TODO read from next child on failure

        let ioref = Io::from_bdev_io(&io, 0).expect("Cannot convert to IoRef");
        let res = ch.children[next].read(&ch.chans[next], ioref);

        res.future().await;

        let status = if res.success() {
            IoStatus::Success
        } else {
            IoStatus::Failure
        };
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

fn rpc_rs_raid1_create(args: rpc::RpcCmdArgs) -> rpc::RpcCmdResult {
    let name = args.get("name").ok_or(Error::InvalidArguments)?;
    let children_names_cs = args.get("children").ok_or(Error::InvalidArguments)?;
    let children_names: Vec<&str> = children_names_cs.split(',').collect();

    let bdevh: BdevHandle = Arc::new(Raid1Bdev::new(name, children_names)?);

    ironspdk::bdev_registry_add(name.to_string(), bdevh.clone())?;

    let ctx = Box::new(BdevCtx {
        name: name.to_string(),
        bdev: bdevh.clone(),
        spdk_bdev: std::ptr::null_mut(),
    });

    let opts = Raid1BdevOpts::from_rpc(&args)?;

    // TODO: open children, get their blocklen and blockcnt; validate and
    // set up corresponding opts fields

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
