use ctor::ctor;
use ironspdk::Error;
use ironspdk::define_bdev_opts;
use ironspdk::rpc;
use ironspdk::rpc_register;
use ironspdk::{
    Bdev, BdevCtx, BdevHandle, BdevIo, BdevIoChannel, BdevIoChannelRef, DmaBuf, Io, IoStatus,
    IoType, Lbdev, LbdevIoChannel, LbdevIoCtx, RawBdevHandle, RcBdevIoChannel, SpdkBdevOptsC,
    SpdkThread, TlsKey, thread_id,
};
use log::{debug, error};
use paste::paste;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::os::raw::{c_char, c_void};
use std::rc::Rc;
use std::sync::Arc;

static IOCH_TLS: TlsKey<RcBdevIoChannel> = TlsKey::new(0);

type Vec<T> = smallvec::SmallVec<[T; 8]>;

struct QueuedIo {
    io: BdevIo,
    sender: SpdkThread,
}

#[derive(Default)]
struct StripeQueue {
    pending: VecDeque<QueuedIo>,
    draining: bool,
}

// One segment of an incoming I/O mapped to corresponding strip
struct Strip<'io> {
    // disk this strip lives on
    disk: usize,
    // offset of the segment within the strip
    offset_in_strip: u64,
    // length of the segment in blocks
    blocks: u64,
    // zero-copy view of the segment data (request memory)
    io: Io<'io>,
}

// Split an incoming I/O into per-strip segments (zero-copy)
fn split_strips<'io>(
    ioref: &'io Io<'io>,
    stripe_base: u64,
    strip_blocks: usize,
    stripe_off: u64,
    num_blocks: u64,
    parity_disk_idx: usize,
    num_data_disks: usize,
) -> Vec<Strip<'io>> {
    let mut splitter = ioref.split(None).expect("must be IoRef");
    let mut strips = Vec::with_capacity(num_data_disks);
    let (mut cursor, mut remain) = (stripe_off, num_blocks);
    while remain != 0 {
        let offset_in_strip = cursor % strip_blocks as u64;
        let take = remain.min(strip_blocks as u64 - offset_in_strip);
        let mut io = Io::Ref(splitter.take(take as usize).expect("short buffer"));
        let strip_idx = (cursor / strip_blocks as u64) as usize;
        io.update_offset_blocks(stripe_base + offset_in_strip);
        strips.push(Strip {
            disk: data_disk_idx(strip_idx, parity_disk_idx),
            offset_in_strip,
            blocks: take,
            io,
        });

        cursor += take;
        remain -= take;
    }
    strips
}

struct Raid5IoChannel {
    disks: Vec<Rc<Lbdev>>,
    chans: Vec<Rc<LbdevIoChannel>>,
    stripes: RefCell<HashMap<u64, StripeQueue>>,
}

impl Raid5IoChannel {
    fn enqueue(&self, stripe_idx: u64, req: QueuedIo) -> bool {
        let mut stripes = self.stripes.borrow_mut();
        let sq = stripes.entry(stripe_idx).or_default();
        sq.pending.push_back(req);
        let rc = sq.draining;
        sq.draining = true;
        rc
    }

    fn dequeue(&self, stripe_idx: u64) -> Option<QueuedIo> {
        let mut stripes = self.stripes.borrow_mut();
        let sq = stripes.get_mut(&stripe_idx).expect("untracked stripe");
        match sq.pending.pop_front() {
            Some(req) => Some(req),
            None => {
                stripes.remove(&stripe_idx);
                None
            }
        }
    }
}

struct Raid5Bdev {
    name: String,
    children_names: Vec<String>,
    workers: Vec<SpdkThread>,
    strip_blocks: usize,
    stripe_blocks: usize,
    blocklen: usize,
}

impl Drop for Raid5Bdev {
    fn drop(&mut self) {
        // Need to stop all workers. This is mandatory.
        for w in &self.workers {
            w.request_exit();
        }
        debug!("DROP Raid5Bdev #{} name='{}'", thread_id(), self.name);
    }
}

impl Bdev for Raid5Bdev {
    fn init(&self, rawbdev: RawBdevHandle) {
        // We must create a thread-local I/O channel for each worker thread
        // to submit I/O
        for w in &self.workers {
            w.spawn(async move {
                // Obtain an I/O channel for current bdev+thread
                let refch = RcBdevIoChannel::new(rawbdev);
                // Put the I/O channel into thread-local storage
                IOCH_TLS.set(refch);
            });
        }
    }

    fn io_type_supported(&self, io_type: IoType) -> bool {
        // TODO: support RESET, FLUSH, UNMAP
        matches!(io_type, IoType::Read | IoType::Write)
    }

    fn create_io_channel(&self) -> Box<BdevIoChannel> {
        let mut disks = Vec::with_capacity(self.children_names.len());
        for disk_name in self.children_names.clone() {
            match Lbdev::open(disk_name.as_str()) {
                Ok(dev) => {
                    disks.push(Rc::new(dev));
                }
                Err(err) => {
                    panic!("Failed to open block device '{}': {}", disk_name, err);
                }
            }
        }
        let mut chans = Vec::with_capacity(self.children_names.len());
        for disk in &disks {
            chans.push(disk.get_io_channel());
        }
        Box::new(BdevIoChannel::new(Raid5IoChannel {
            disks,
            chans,
            stripes: RefCell::new(HashMap::new()),
        }))
    }

    fn submit_io(&self, _ch: BdevIoChannelRef, io: BdevIo) {
        let stripe_idx = self.stripe_idx(io.offset_blocks());
        let owner_idx = self.owner_idx(stripe_idx);
        let owner = self.workers[owner_idx].clone();
        let sender = SpdkThread::current();
        let this_ptr = self as *const Raid5Bdev;
        owner.spawn(async move {
            let this = unsafe { &*this_ptr };
            let refch = IOCH_TLS.get().expect("I/O channel not found");
            let ch = refch.downcast_mut::<Raid5IoChannel>();

            // enqueue request
            if ch.enqueue(stripe_idx, QueuedIo { io, sender }) {
                return; // already draining
            }
            // drain requests
            while let Some(req) = ch.dequeue(stripe_idx) {
                this.process_req(ch, stripe_idx, req).await;
            }
        });
    }
}

impl Raid5Bdev {
    pub fn new(
        name: &str,
        blocklen: usize,
        strip_blocks: usize,
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
        let ndatadisks = children_names.len() - 1;
        Ok(Self {
            name: name.to_string(),
            children_names: children_names.iter().map(|&s| s.to_string()).collect(),
            workers,
            strip_blocks,
            stripe_blocks: ndatadisks * strip_blocks,
            blocklen,
        })
    }

    fn disk_count(&self) -> usize {
        self.children_names.len()
    }

    fn data_disk_count(&self) -> usize {
        self.children_names.len() - 1
    }

    fn stripe_idx(&self, off_blocks: u64) -> u64 {
        off_blocks / self.stripe_blocks as u64
    }

    fn owner_idx(&self, stripe_idx: u64) -> usize {
        (stripe_idx % self.workers.len() as u64) as usize
    }

    fn parity_disk_idx(&self, stripe_idx: u64) -> usize {
        (stripe_idx % self.children_names.len() as u64) as usize
    }

    fn stripe_offset(&self, off_blocks: u64) -> u64 {
        off_blocks % self.stripe_blocks as u64
    }

    async fn process_req(&self, ch: &Raid5IoChannel, stripe_idx: u64, req: QueuedIo) {
        let QueuedIo { io, sender } = req;
        let stripe_off = self.stripe_offset(io.offset_blocks());
        if stripe_off + io.num_blocks() > self.stripe_blocks as u64 {
            error!("cross-stripe I/O not supported");
            io.complete_on(&sender, IoStatus::Failure);
            return;
        }
        match io.io_type() {
            IoType::Read => self.read(&sender, ch, io, stripe_idx).await,
            IoType::Write => self.write(&sender, ch, io, stripe_idx).await,
            IoType::Flush => io.complete_on(&sender, IoStatus::Success),
            _ => io.complete_on(&sender, IoStatus::Failure),
        }
    }

    async fn read(&self, sender: &SpdkThread, ch: &Raid5IoChannel, io: BdevIo, stripe_idx: u64) {
        let parity_disk_idx = self.parity_disk_idx(stripe_idx);
        let stripe_base = stripe_idx * self.strip_blocks as u64;
        let blocklen = io.block_len();
        let stripe_off = self.stripe_offset(io.offset_blocks());
        let num_blocks = io.num_blocks();

        let ioref = Io::from_bdev_io(&io, blocklen).expect("bad io");
        let mut strips = split_strips(
            &ioref,
            stripe_base,
            self.strip_blocks,
            stripe_off,
            num_blocks,
            parity_disk_idx,
            self.data_disk_count(),
        );

        // Submit one read per strip segment to its data disk
        let mut ctxs = Vec::with_capacity(strips.len());
        for _ in &strips {
            ctxs.push(LbdevIoCtx::new());
        }
        let mut futs = Vec::with_capacity(strips.len());
        for (s, ctx) in strips.iter_mut().zip(ctxs.iter_mut()) {
            futs.push(ch.disks[s.disk].read(&ch.chans[s.disk], &mut s.io, ctx));
        }

        let mut failed = Vec::new();
        for (k, fut) in futs.into_iter().enumerate() {
            if !fut.await {
                failed.push(k);
            }
        }

        match failed.len() {
            0 => io.complete_on(sender, IoStatus::Success),
            1 => {
                // Degraded read: reconstruct the failed strip segment from
                // parity + all other data disks, then copy into the request.
                let k = failed[0];
                let s = &strips[k];
                // TODO: do not read data strips (except failed one) again,
                // we have already read them
                match self
                    .reconstruct(ch, s.disk, stripe_base + s.offset_in_strip, s.blocks)
                    .await
                {
                    Some(buf) => {
                        if let Io::Ref(r) = &strips[k].io {
                            unsafe {
                                r.copy_from_slice(buf.as_slice());
                            }
                        }
                        debug!(
                            "READ #{} data strip recovered; stripe={} {:?}",
                            thread_id(),
                            stripe_idx,
                            io
                        );
                        io.complete_on(sender, IoStatus::Success);
                    }
                    None => {
                        error!(
                            "raid5: #{} read repair failed (>1 disks) stripe={} {:?}",
                            thread_id(),
                            stripe_idx,
                            io
                        );
                        io.complete_on(sender, IoStatus::Failure);
                    }
                }
            }
            _ => {
                error!(
                    "raid5: #{} read failed on {} disks stripe={} {:?}",
                    thread_id(),
                    failed.len(),
                    stripe_idx,
                    io
                );
                io.complete_on(sender, IoStatus::Failure);
            }
        }
    }

    async fn write(&self, sender: &SpdkThread, ch: &Raid5IoChannel, io: BdevIo, stripe_idx: u64) {
        let stripe_off = self.stripe_offset(io.offset_blocks());
        if stripe_off == 0 && io.num_blocks() == self.stripe_blocks as u64 {
            self.write_full_stripe(sender, ch, io, stripe_idx).await;
        } else {
            self.write_partial_stripe(sender, ch, io, stripe_idx).await;
        }
    }

    async fn write_partial_stripe(
        &self,
        sender: &SpdkThread,
        ch: &Raid5IoChannel,
        io: BdevIo,
        stripe_idx: u64,
    ) {
        let parity_disk_idx = self.parity_disk_idx(stripe_idx);
        let stripe_base = stripe_idx * self.strip_blocks as u64;
        let blocklen = io.block_len();
        let num_blocks = io.num_blocks();
        let stripe_off = self.stripe_offset(io.offset_blocks());

        let ioref = Io::from_bdev_io(&io, blocklen).expect("bad io");
        let mut strips = split_strips(
            &ioref,
            stripe_base,
            self.strip_blocks,
            stripe_off,
            num_blocks,
            parity_disk_idx,
            self.data_disk_count(),
        );
        let nseg = strips.len();

        let mut fail_mask: u64 = 0; // informational; disk count may exceed 64
        let mut nfailures = 0;

        // Phase 1. Read old data segments and old parity segment
        let mut old_ios = Vec::with_capacity(nseg + 1);
        for s in &strips {
            let old_data_buf =
                DmaBuf::new_aligned(s.blocks as usize * blocklen, 4096).expect("no memory");
            let old_data_io = Io::new_buf(old_data_buf, stripe_base + s.offset_in_strip, blocklen)
                .expect("bad old data io");
            old_ios.push(old_data_io);
        }
        let old_parity_buf = DmaBuf::new(self.strip_blocks * blocklen).expect("no memory");
        let old_parity_io =
            Io::new_buf(old_parity_buf, stripe_base, blocklen).expect("bad parity io");
        old_ios.push(old_parity_io);
        let mut ctxs = Vec::with_capacity(nseg + 1);
        for _ in 0..nseg + 1 {
            ctxs.push(LbdevIoCtx::new());
        }
        let mut futs = Vec::with_capacity(nseg + 1);
        let mut ctx_iter = ctxs.iter_mut();
        for (k, s) in strips.iter().enumerate() {
            let disk_idx = s.disk;
            let fut = ch.disks[disk_idx].read(
                &ch.chans[disk_idx],
                &mut old_ios[k],
                ctx_iter.next().unwrap(),
            );
            futs.push((disk_idx, fut));
        }
        let old_parity_fut = ch.disks[parity_disk_idx].read(
            &ch.chans[parity_disk_idx],
            &mut old_ios[nseg],
            ctx_iter.next().unwrap(),
        );
        futs.push((parity_disk_idx, old_parity_fut));

        // NOTE: LbdevIoFuture instances must not outlive related LbdevIoCtx instances.
        // This is intentional. Borrow checker keeps this invariant.
        // The only way to await multiple LbdevIoFuture-s is doing it in batch loop.
        let mut last_failed_disk_idx: Option<usize> = None;
        for (disk_idx, fut) in futs {
            if !fut.await {
                fail_mask |= 1u64 << disk_idx;
                nfailures += 1;
                last_failed_disk_idx = Some(disk_idx);
            }
        }

        if nfailures >= 2 {
            self.complete_write(
                sender,
                io,
                stripe_idx,
                nfailures,
                last_failed_disk_idx,
                fail_mask,
            );
            return;
        }

        if nfailures == 1 {
            let failed_disk_idx = last_failed_disk_idx.unwrap();

            if failed_disk_idx == parity_disk_idx {
                // Parity read failure
                // Reconstruct old parity from data strips
                match self
                    .reconstruct(
                        ch,
                        failed_disk_idx,
                        stripe_base,
                        self.strip_blocks.try_into().unwrap(),
                    )
                    .await
                {
                    Some(dma) => {
                        let iobuf =
                            Io::new_buf(dma, stripe_base, blocklen).expect("bad old parity io");
                        old_ios[nseg] = iobuf;
                        debug!(
                            "RMW #{} parity strip recovered; stripe={} {:?}",
                            thread_id(),
                            stripe_idx,
                            io
                        );
                    }
                    None => {
                        error!(
                            "raid5: #{} parity reconstruction failed; stripe={} {:?}",
                            thread_id(),
                            stripe_idx,
                            io
                        );
                        io.complete_on(sender, IoStatus::Failure);
                        return;
                    }
                }
            } else {
                // Data read failure
                // Reconstruct old data ( = XOR(parity, all other data disks))
                let k = strips
                    .iter()
                    .position(|s| s.disk == failed_disk_idx)
                    .expect("failed data strip must be present");
                let s = &strips[k];
                // reconstruct old contents of failed strip
                match self
                    .reconstruct(
                        ch,
                        failed_disk_idx,
                        stripe_base + s.offset_in_strip,
                        s.blocks,
                    )
                    .await
                {
                    Some(buf) => {
                        let iobuf = Io::new_buf(buf, stripe_base + s.offset_in_strip, blocklen)
                            .expect("bad io");
                        old_ios[k] = iobuf;
                        debug!(
                            "RMW #{} data strip recovered; stripe={} {:?}",
                            thread_id(),
                            stripe_idx,
                            io
                        );
                    }
                    None => {
                        error!(
                            "raid5: #{} data reconstruction failed; stripe={} {:?}",
                            thread_id(),
                            stripe_idx,
                            io
                        );
                        io.complete_on(sender, IoStatus::Failure);
                        return;
                    }
                }
            }
        }

        // Phase 2. Compute new parity segment
        //if new_parity_buf.is_none() {
        let mut new_parity_buf =
            DmaBuf::new_zeroed(self.strip_blocks * blocklen).expect("no memory");
        let buf = new_parity_buf.as_mut_slice();
        xor(buf, &old_ios[nseg]); // buf ^= old parity
        for (k, s) in strips.iter().enumerate() {
            let start = s.offset_in_strip as usize * blocklen;
            let len = s.blocks as usize * blocklen;
            xor(&mut buf[start..start + len], &old_ios[k]); // buf ^= old data
            xor(&mut buf[start..start + len], &s.io); // buf ^= new data
        }

        // Phase 3. Write new data segments and new parity segment

        // NOTE: LbdevIoCtx and LbdevIoFuture instances which used for READ
        // must not be reused for WRITE in the same scope.
        // This is dangerous, futures are not movable.
        // API designed so that Rust borrow checker explicitly disables reuse.

        let mut wctxs = Vec::with_capacity(nseg + 1);
        for _ in 0..nseg + 1 {
            wctxs.push(LbdevIoCtx::new());
        }
        let mut wfuts = Vec::with_capacity(nseg + 1);
        let mut wctx_iter = wctxs.iter_mut();
        for s in &mut strips {
            let fut =
                ch.disks[s.disk].write(&ch.chans[s.disk], &mut s.io, wctx_iter.next().unwrap());
            wfuts.push((s.disk, fut));
        }
        let mut new_parity_io = Io::new_buf(new_parity_buf, stripe_base, blocklen).expect("bad io");
        let fut = ch.disks[parity_disk_idx].write(
            &ch.chans[parity_disk_idx],
            &mut new_parity_io,
            wctx_iter.next().unwrap(),
        );
        wfuts.push((parity_disk_idx, fut));

        last_failed_disk_idx = None;
        fail_mask = 0;
        nfailures = 0;
        for (disk_idx, fut) in wfuts {
            if !fut.await {
                fail_mask |= 1u64 << disk_idx;
                nfailures += 1;
                last_failed_disk_idx = Some(disk_idx);
            }
        }

        self.complete_write(
            sender,
            io,
            stripe_idx,
            nfailures,
            last_failed_disk_idx,
            fail_mask,
        );
    }

    async fn reconstruct(
        &self,
        ch: &Raid5IoChannel,
        skip: usize,
        off_blocks: u64,
        blocks: u64,
    ) -> Option<DmaBuf> {
        let ndisks = self.disk_count();
        let nbytes = blocks as usize * self.blocklen;

        let mut bufs = Vec::with_capacity(ndisks - 1);
        for _ in 0..ndisks - 1 {
            bufs.push(DmaBuf::new(nbytes).expect("no memory"));
        }
        let mut ios: Vec<Io<'_>> = bufs
            .into_iter()
            .map(|b| Io::new_buf(b, off_blocks, self.blocklen).expect("bad io"))
            .collect();
        let mut ctxs = Vec::with_capacity(ndisks - 1);
        for _ in 0..ndisks - 1 {
            ctxs.push(LbdevIoCtx::new());
        }
        let mut futs = Vec::with_capacity(ndisks - 1);
        for (i, (d, ctx)) in (0..ndisks)
            .filter(|&d| d != skip)
            .zip(ctxs.iter_mut())
            .enumerate()
        {
            let fut = ch.disks[d].read(&ch.chans[d], &mut ios[i], ctx);
            futs.push(fut);
        }
        for fut in futs {
            if !fut.await {
                return None;
            }
        }

        let mut outbuf = DmaBuf::new_zeroed(nbytes).expect("no memory");
        let acc = outbuf.as_mut_slice();
        for io in &ios {
            xor(acc, io);
        }
        Some(outbuf)
    }

    async fn write_full_stripe(
        &self,
        sender: &SpdkThread,
        ch: &Raid5IoChannel,
        io: BdevIo,
        stripe_idx: u64,
    ) {
        let parity_disk_idx = self.parity_disk_idx(stripe_idx);
        let stripe_base = stripe_idx * self.strip_blocks as u64; // offset of the stripe on the disk
        let blocklen = io.block_len();
        debug_assert_eq!(blocklen, self.blocklen);

        let ioref = Io::from_bdev_io(&io, blocklen).expect("bad io");
        let strips = split_strips(
            &ioref,
            stripe_base,
            self.strip_blocks,
            0,
            self.stripe_blocks as u64,
            parity_disk_idx,
            self.data_disk_count(),
        );

        let mut parity_dmabuf =
            DmaBuf::new_aligned_zeroed(self.strip_blocks * blocklen, 4096).expect("no memory");
        for s in &strips {
            xor(parity_dmabuf.as_mut_slice(), &s.io);
        }

        let parity_io = Io::new_buf(parity_dmabuf, stripe_base, blocklen).expect("bad parity io");
        let mut parity_io = Some(parity_io);

        let ndisks = self.disk_count();
        let mut ctxs = Vec::with_capacity(ndisks);
        let mut ios = Vec::with_capacity(ndisks);
        let mut strip_iter = strips.into_iter();
        for disk_idx in 0..ndisks {
            let io = if disk_idx == parity_disk_idx {
                parity_io.take().expect("parity IO already consumed")
            } else {
                let s = strip_iter.next().expect("no data ioref");
                s.io
            };
            ios.push(io);
            ctxs.push(LbdevIoCtx::new());
        }

        // submit write requests
        let mut futs = Vec::with_capacity(ctxs.len());
        for ((disk_idx, ctx), mut io) in ctxs.iter_mut().enumerate().zip(ios) {
            let fut = ch.disks[disk_idx].write(&ch.chans[disk_idx], &mut io, ctx);
            futs.push(fut);
        }

        // await results
        let mut fail_mask: u64 = 0; // informational mask (disk count may exceed 64)
        let mut nfailures = 0;
        let mut last_failed_disk_idx: Option<usize> = None;
        for (idx, fut) in futs.into_iter().enumerate() {
            if !fut.await {
                nfailures += 1;
                fail_mask |= 1 << idx;
                last_failed_disk_idx = Some(idx);
            }
        }

        self.complete_write(
            sender,
            io,
            stripe_idx,
            nfailures,
            last_failed_disk_idx,
            fail_mask,
        );
    }

    // Apply the RAID5 write failure policy:
    //
    //  0 failed disks  -> Success,
    //  1 failed data disk   -> Success (degraded stripe),
    //  1 failed parity disk -> Failure,
    //  >1 failed disks -> Failure.
    fn complete_write(
        &self,
        sender: &SpdkThread,
        io: BdevIo,
        stripe_idx: u64,
        nfailures: usize,
        last_failed_disk_idx: Option<usize>,
        fail_mask: u64,
    ) {
        let status = if nfailures == 0 {
            IoStatus::Success
        } else if nfailures == 1 {
            let strip_type_str =
                if last_failed_disk_idx.unwrap() == self.parity_disk_idx(stripe_idx) {
                    "parity"
                } else {
                    "data"
                };
            error!(
                "raid5: #{} {} write failed in stripe={} fail_mask=0x{:x} {:?}",
                thread_id(),
                strip_type_str,
                stripe_idx,
                fail_mask,
                io
            );
            IoStatus::Failure
        } else {
            // more than one failures
            error!(
                "raid5: #{} write failed ({} failed disks) in stripe={} fail_mask=0x{:x} {:?}",
                thread_id(),
                nfailures,
                stripe_idx,
                fail_mask,
                io
            );
            IoStatus::Failure
        };
        io.complete_on(sender, status);
    }
}

fn xor(dst: &mut [u8], io: &Io) {
    let mut pos = 0;
    for seg in io.iter_iov() {
        for (d, &b) in dst[pos..pos + seg.len()].iter_mut().zip(seg) {
            *d ^= b;
        }
        pos += seg.len();
    }
    assert_eq!(pos, dst.len(), "xor: I/O size mismatch");
}

fn data_disk_idx(strip_idx: usize, parity_disk_idx: usize) -> usize {
    if strip_idx < parity_disk_idx {
        strip_idx
    } else {
        strip_idx + 1
    }
}

define_bdev_opts!(Raid5BdevOpts {
    blocklen: u32 = 512,                    // default: 512 bytes
    blockcnt: u64 = 64 * 1024 * 1024 / 512, // default: 64 MBytes
    write_cache: bool = false,
    write_unit_size: u32 = 0,
    optimal_io_boundary: u32 = 0,
    split_on_write_unit: u32 = 0,
    split_on_optimal_io_boundary: u32 = 0,
});

unsafe extern "C" {
    fn raid5_bdev_create(
        name: *const c_char,
        opts: *const SpdkBdevOptsC,
        rscx: *const c_void,
    ) -> i32;
}

fn parse_strip_size(args: rpc::RpcCmdArgs, blocklen: usize) -> Result<usize, Error> {
    let strip_size_kb = if let Some(strip_size_kb_str) = args.get("strip_size_kb") {
        strip_size_kb_str.parse::<u32>()?
    } else {
        0
    };
    let strip_size = (strip_size_kb * 1024) as usize;
    if strip_size == 0 {
        return Err(Error::InvalidArguments);
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
        error!("empty name");
        return Err(Error::InvalidArguments);
    }
    for disk_name in children_names {
        match Lbdev::open(disk_name) {
            Ok(dev) => {
                let bdev_blocklen = dev.desc().block_len();
                match blocklen {
                    None => blocklen = Some(bdev_blocklen),
                    Some(existing) if existing != bdev_blocklen => {
                        error!("disks with different block sizes");
                        return Err(Error::InvalidArguments);
                    }
                    _ => {}
                }
                let bdev_num_blocks = dev.desc().number_of_blocks();
                match num_blocks {
                    None => num_blocks = Some(bdev_num_blocks),
                    Some(existing) if existing != bdev_num_blocks => {
                        error!("disks with different sizes");
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
    if blocklen.unwrap() == 0 {
        return Err(Error::InvalidArguments);
    }
    Ok((blocklen.unwrap(), num_blocks.unwrap()))
}

fn rpc_rs_raid5_create(args: rpc::RpcCmdArgs) -> rpc::RpcCmdResult {
    let name = args.get("name").ok_or(Error::InvalidArguments)?;
    let children_names_cs = args.get("children").ok_or(Error::InvalidArguments)?;
    let children_names: Vec<&str> = children_names_cs.split(',').collect();
    let ndisks = children_names.len();
    if ndisks < 3 {
        error!("Need at least 3 children disks");
        return Err(Error::InvalidArguments);
    }
    let (blocklen, child_blocks) = parse_children(&children_names)?;
    let strip_size = parse_strip_size(args.clone(), blocklen)?;
    let strip_blocks = strip_size / blocklen;

    let bdevh: BdevHandle = Arc::new(Raid5Bdev::new(
        name,
        blocklen,
        strip_blocks,
        children_names,
    )?);

    ironspdk::bdev_registry_add(name.to_string(), bdevh.clone())?;

    let ctx = Box::new(BdevCtx {
        name: name.to_string(),
        bdev: bdevh.clone(),
        spdk_bdev: std::ptr::null_mut(),
    });

    let mut opts = Raid5BdevOpts::from_rpc(&args)?;

    // report RAID5 logical disk size
    let usable_child_blocks = child_blocks / strip_blocks as u64 * strip_blocks as u64;
    opts.blockcnt = (ndisks as u64 - 1)
        .checked_mul(usable_child_blocks)
        .ok_or(Error::InvalidArguments)?;
    // report RAID5 logical disk's blocklen
    opts.blocklen = blocklen.try_into().map_err(|_| Error::InvalidArguments)?;

    // force SPDK to split I/O at stripe boundary
    let optimal_io_boundary = (ndisks - 1)
        .checked_mul(strip_blocks)
        .ok_or(Error::InvalidArguments)?;
    opts.optimal_io_boundary = optimal_io_boundary
        .try_into()
        .map_err(|_| Error::InvalidArguments)?;
    opts.split_on_optimal_io_boundary = 1;

    let opts_c = opts.to_c();
    let ctx_ptr = Box::into_raw(ctx) as *mut c_void;
    let name_c = std::ffi::CString::new(name.as_str()).unwrap();

    let rc = unsafe { raid5_bdev_create(name_c.as_ptr(), &opts_c, ctx_ptr) };
    if rc != 0 {
        unsafe { drop(Box::from_raw(ctx_ptr as *mut BdevCtx)) };
        let _ = ironspdk::bdev_registry_remove(name.to_string())?;
        return Err(Error::SpdkBdevCreate(rc));
    }

    Ok(format!("Successfully created RAID5 bdev: '{}'", name))
}
rpc_register!("rs_raid5_create", rpc_rs_raid5_create);
