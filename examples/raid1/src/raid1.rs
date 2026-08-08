use ctor::ctor;
use ironspdk::Error;
use ironspdk::define_bdev_opts;
use ironspdk::rpc;
use ironspdk::rpc_register;
use ironspdk::{
    Bdev, BdevCtx, BdevHandle, BdevIo, BdevIoChannel, BdevIoChannelRef, Io, IoStatus, IoType,
    Lbdev, LbdevIoChannel, RawBdevHandle, SpdkBdevOptsC, SpdkThread, thread_id,
};
use log::{debug, error, warn};
use paste::paste;
use std::os::raw::{c_char, c_void};
use std::rc::Rc;
use std::sync::Arc;

struct Raid1IoChannel {
    children: Vec<Rc<Lbdev>>,
    chans: Vec<Rc<LbdevIoChannel>>,
    next_read: usize,
}

struct Raid1Bdev {
    name: String,
    children_names: Vec<String>,
}

impl Drop for Raid1Bdev {
    fn drop(&mut self) {
        debug!("DROP Raid1Bdev #{} name='{}'", thread_id(), self.name);
    }
}

impl Bdev for Raid1Bdev {
    fn init(&self, _rawbdev: RawBdevHandle) {}

    fn io_type_supported(&self, io_type: IoType) -> bool {
        // TODO: support RESET, FLUSH, UNMAP
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

    fn submit_io(&self, ch: BdevIoChannelRef, io: BdevIo) {
        let this_ptr = self as *const Raid1Bdev;
        let current = SpdkThread::current();
        current.spawn(async move {
            let this = unsafe { &*this_ptr };
            let ch = ch.downcast_mut::<Raid1IoChannel>();
            debug_assert!(!ch.children.is_empty());
            debug_assert!(ch.chans.len() == ch.children.len());
            match io.io_type() {
                IoType::Read => this.read(ch, io).await,
                IoType::Write => this.write(ch, io).await,
                IoType::Flush => {
                    // flush==noop for now. TODO implement flush
                    io.complete(IoStatus::Success);
                }
                _ => {
                    io.complete(IoStatus::Failure);
                }
            }
        });
    }
}

impl Raid1Bdev {
    pub fn new(name: &str, children_names: Vec<&str>) -> Result<Self, Error> {
        Ok(Self {
            name: name.to_string(),
            children_names: children_names.iter().map(|&s| s.to_string()).collect(),
        })
    }

    async fn read(&self, ch: &mut Raid1IoChannel, io: BdevIo) {
        let n = ch.children.len();

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
            error!("Read error (all disks failed) #{} {:?}", thread_id(), io);
        }

        io.complete(status);
    }

    async fn write(&self, ch: &mut Raid1IoChannel, io: BdevIo) {
        let mut crs = Vec::new();
        for (idx, child) in ch.children.iter().enumerate() {
            let ioref = Io::from_bdev_io(&io, 0).expect("Cannot convert to IoRef");
            let child_res = child.write(&ch.chans[idx], ioref);
            crs.push(child_res);
        }
        let mut status = IoStatus::Failure;
        let mut failures = Vec::new();
        // Wait until all lower bdevs are done, check results
        for (idx, child_res) in crs.iter().enumerate() {
            child_res.future().await;

            // At least one lower bdev writes successfully. Overall result is SUCCESS
            if child_res.success() {
                status = IoStatus::Success;
            } else {
                failures.push(idx);
            }
        }
        if !failures.is_empty() {
            if status == IoStatus::Failure {
                error!("Write error (all disks failed) #{} {:?}", thread_id(), io);
            } else {
                warn!("Partial write #{} {:?} {:?}", thread_id(), failures, io);
            }
        }
        io.complete(status);
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

    let bdevh: BdevHandle = Arc::new(Raid1Bdev::new(name, children_names)?);

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
