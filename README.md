# ironspdk

Rust runtime for SPDK. Write high-performance usermode storage drivers in Rust.

A cutting-edge Rust runtime that brings the power of modern async/await to SPDK (Storage Performance Development Kit). Build block devices and storage modules with the safety and productivity of Rust, while maintaining C-level performance.

## Overview

`ironspdk` is an innovative framework that bridges the gap between Rust's memory safety and the raw performance of SPDK. Instead of struggling with C callbacks and manual memory management, you can now write storage drivers using idiomatic Rust with futures, channels, and all the ergonomic benefits Rust provides.

## Key Features

### 🦀 Idiomatic Rust Programming Model
* **No callback hell**: Use Rust's async/await syntax and futures for natural asynchronous I/O handling
* **Memory safety**: Leverage Rust's ownership and borrowing system to prevent data races and memory bugs
* **Type safety**: Compile-time guarantees replace runtime errors

### ⚡ SPDK Integration
* **Full SPDK primitives support**: SPDK lightweight threads, I/O channels, block device descriptors, and more are exposed to Rust
* **Tight runtime integration**: The `ironspdk` runtime executor extends the SPDK poller, allowing Rust code to seamlessly integrate with the SPDK event loop
* **Zero-copy I/O**: Work directly with SPDK I/O vectors and DMA buffers
* **Thread-safe operations**: The Rust type system enforces SPDK's multiple-threads-no-locks programming model at compile time
* **C <-> Rust interoperability**: SPDK (or any other library) C API is still here: just add `unsafe extern "C" spdk_...(...);` to your Rust code

### 🚀 Performance
* **C-level performance**: No runtime overhead - compiled to native code with optimizations
* **Lock-free design**: Leverages SPDK's thread-per-core architecture
* **Direct FFI binding**: Minimal abstraction over underlying SPDK C APIs

### 🔀 Comprehensive I/O Abstractions
* **Multiple I/O models**: Support for I/O references (`IoRef`), buffered I/O (`IoBuf`), and unified `Io` enum
* **I/O splitting**: Advanced utilities for splitting and reordering I/O operations
* **DMA buffers**: First-class support for aligned DMA memory allocation and management
* **Block device abstraction**: Simple trait-based interface for implementing custom block devices

## Architecture

### Core Components

```
ironspdk-sys/          # Low-level C FFI bindings to SPDK
ironspdk/              # High-level Rust runtime and abstractions
  ├── app.rs           # SpdkApp lifecycle management
  ├── lib.rs           # Core types: Bdev, IoRef, IoBuf, SpdkThread, Tcb
  ├── c.rs             # C FFI wrappers
  ├── c_enum.rs        # Enum conversions
  └── rpc.rs           # RPC command registration
examples/raid1/        # Simple RAID1 implementation example
```

### Runtime Executor

The `ironspdk` runtime leverages SPDK's poller mechanism:
- **Task Control Block (Tcb)**: Manages async task execution on each SPDK thread
- **Task scheduler**: Queues and polls futures
- **I/O channel management**: Tracks and manages block device I/O channels per thread
- **Waker integration**: Custom waker implementation to notify tasks in runqueue

## Usage

### Basic Setup

Add to your `Cargo.toml`:

```toml
[build-dependencies]
cc = "1.2.56"
ironspdk-sys = "0.1"

[dependencies]
ironspdk-sys = "0.1"
ironspdk = "0.1"
```

### Create a Simple Block Device

```rust
use ironspdk::{Bdev, BdevIoChannel, BdevIo, IoType, SpdkThread, RawBdevHandle};

struct MyBdevIoChannel {
    // I/O channel state (per-io_device-and-spdk_thread)
}

struct MyBdev {
    // Your block device global state, read-only for submit_io threads
}

impl Bdev for MyBdev {
    fn init(&self, ctx: RawBdevHandle) {
        // Initialize your block device
    }

    fn io_type_supported(&self, io_type: IoType) -> bool {
        matches!(io_type, IoType::Read | IoType::Write)
    }

    fn create_io_channel(&self) -> Box<BdevIoChannel> {
        // Create and return an I/O channel context
        Box::new(BdevIoChannel::new(MyBdevIoChannel {}))
    }

    fn submit_io(&self, ch: &mut BdevIoChannel, io: BdevIo) {
        // Handle I/O requests asynchronously
        SpdkThread::current().spawn(async move {
            // Process I/O...
            io.complete(IoStatus::Success);
        });
    }
}
```

### Build & Run

```bash
# Set up environment
export SPDK=/path/to/built/spdk
export PKG_CONFIG_PATH=$SPDK/build/lib/pkgconfig/

# Build
cargo build --release

# Run with specific CPU cores (0xf = cores 0-3)
sudo RUST_LOG=info ./target/release/your_app -m 0xf
```

## Examples

### RAID1 Block Device

The repository includes a simple yet functional RAID1 implementation (`examples/raid1/`). This example demonstrates:
- Mirroring I/O across two backend block devices
- Handling read/write operations
- RPC-based management interface

**Compare with SPDK's C implementation**: The Rust version is significantly more concise and readable, while maintaining identical performance.

#### Running the RAID1 Example

```bash
# Terminal 1: Start the RAID1 driver
cd ironspdk
SPDK=/path/to/built/spdk/
PKG_CONFIG_PATH=$SPDK/build/lib/pkgconfig/
cargo build --release
# run RAID1 usermode driver example at 4 CPU cores
sudo RUST_LOG=info ./target/release/raid1 -m 0xf

# Terminal 2: Create backend devices
SPDK=/path/to/built/spdk/
cd $SPDK
sudo ./scripts/rpc.py bdev_malloc_create -b malloc0 64 512
sudo ./scripts/rpc.py bdev_malloc_create -b malloc1 64 512

# Create RAID1 instance
sudo PYTHONPATH=/path/to/ironspdk/examples/raid1/ ./scripts/rpc.py \
    --plugin raid1 \
    rs_raid1_create --name my_ironspdk_raid1 -c malloc0,malloc1

# Export via ublk and benchmark with fio
sudo modprobe ublk_drv
sudo ./scripts/rpc.py ublk_create_target
sudo ./scripts/rpc.py ublk_start_disk my_ironspdk_raid1 1 -q $(nproc) -d 128

# Run I/O benchmark
TIME=30
sudo fio --filename=/dev/ublkb1 --direct=1 --numjobs=$(nproc) \
    --rw=randrw --bs=4096 --iodepth=32 --ioengine=libaio \
    --time_based=1 --runtime=$TIME --name=raid1_test

# Cleanup
sudo ./scripts/rpc.py ublk_stop_disk 1
sudo PYTHONPATH=/path/to/ironspdk/ ./scripts/rpc.py \
    --plugin ironspdk rs_bdev_delete my_ironspdk_raid1
sudo ./scripts/rpc.py bdev_malloc_delete malloc1
sudo ./scripts/rpc.py bdev_malloc_delete malloc0
```

## API Overview

### Core Types

#### `SpdkApp`
Main application entry point. Manages SPDK initialization, thread creation, and lifecycle.

```rust
let mut app = SpdkApp::new("my_app");
app.on_start(|| { /* startup code */ });
app.on_shutdown(|| { /* shutdown code */ });
app.run()?;
```

#### `SpdkThread`
Wrapper around SPDK threads. Enables spawning async tasks and inter-thread communication.

```rust
// create new SPDK thread at core 2
let thread = SpdkThread::new_at_cores("my_thread", [2]);

// run some code at this SPDK thread
thread.spawn(async { /* async work */ });

// stop SPDK threads this way only
thread.request_exit();
```

#### `Bdev` (Trait)
Implement this trait to create custom block devices.

```rust
pub trait Bdev {
    fn init(&self, ctx: RawBdevHandle);
    fn io_type_supported(&self, io_type: IoType) -> bool;
    fn create_io_channel(&self) -> Box<BdevIoChannel>;
    fn submit_io(&self, ch: &mut BdevIoChannel, io: BdevIo);
}
```

#### `BdevIo`
Represents a single I/O request. Provides access to request metadata and completion mechanism.

```rust
pub struct BdevIo { /* ... */ }

impl BdevIo {
    pub fn io_type(&self) -> IoType;
    pub fn offset_blocks(&self) -> u64;
    pub fn num_blocks(&self) -> u64;
    pub fn block_len(&self) -> usize;
    pub fn range(&self) -> Option<IoRange>;
    pub fn complete(&self, status: IoStatus);
}
```

#### `Io<'a>` (Enum)
Unified interface for working with I/O data. Can be either a reference to SPDK I/O vectors or a buffered copy.

```rust
pub enum Io<'a> {
    Ref(IoRef<'a>),    // Zero-copy reference to SPDK buffers
    Buf(IoBuf),        // Copy to/from DMA buffer
}

impl<'a> Io<'a> {
    pub fn iter_iov(&self) -> IoIter;                    // Iterate over buffers
    pub fn iter_iov_mut(&mut self) -> IoIterMut;         // Mutable iteration
    pub fn split(&'a self, child_block_len: Option<usize>) 
        -> Result<IoRefSplitter<'a>, Error>;             // Split I/O operations
    pub fn offset_blocks(&self) -> u64;
    pub fn num_blocks(&self) -> usize;
}
```

#### `DmaBuf`
DMA-allocated memory buffer with Send+Sync support for thread-safe sharing.

```rust
pub struct DmaBuf { /* ... */ }

impl DmaBuf {
    pub fn new(len: usize, align: usize) -> Result<Self, Error>;
    pub fn as_slice(&self) -> &[u8];
    pub fn as_mut_slice(&mut self) -> Result<&mut [u8], Error>;
    pub unsafe fn as_mut_slice_unchecked(&self) -> &mut [u8];
}
```

#### `Lbdev`
Client API for accessing lower-layer SPDK block devices.

```rust
pub struct Lbdev { /* ... */ }

impl Lbdev {
    pub fn open(name: &str) -> Result<Self, Error>;
    pub fn get_io_channel(&self) -> Rc<LbdevIoChannel>;
    pub fn read(&self, ch: &LbdevIoChannel, io: Io) -> Rc<LbdevIoResult>;
    pub fn write(&self, ch: &LbdevIoChannel, io: Io) -> Rc<LbdevIoResult>;
}
```

### Error Handling

All fallible operations return `Result<T, Error>`. The `Error` enum covers common SPDK scenarios:

```rust
pub enum Error {
    AlreadyExists,
    SpdkBdevNotFound(String),
    SpdkBdevCreate(i32),
    SpdkBdevOpen(i32),
    NoMemory,
    UnsupportedFeature,
    SharedBufferModification,
    // ... and more
}
```

## Requirements

* **Rust**: 1.70+
* **SPDK**: Built and configured (see [SPDK documentation](https://spdk.io/)), version v26.01 is supported
* **Linux**: confirmed support at 6.17+ kernels
* **Privileges**: Most operations require superuser access for hardware access and memory management

## Performance Characteristics

- **Latency**: Microsecond-scale I/O latency (same as C SPDK)
- **Throughput**: Limited only by underlying hardware (no Rust overhead)
- **CPU efficiency**: Lock-free design with thread-per-core scaling
- **Memory**: Minimal overhead compared to C implementation

## Licensing

Licensed under either of:

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
* BSD 3-Clause License ([LICENSE-BSD-3-Clause](LICENSE-BSD-3-Clause))

at your option.

## Contributing

Contributions are welcome! Please:

1. Ensure all tests pass: `cargo test`
2. Format code: `cargo fmt --`
3. Run clippy: `cargo clippy --locked --all --all-targets --tests -- -D warnings`
4. Document public APIs
5. Add tests for new functionality

## Getting Help

* **SPDK Documentation**: [https://spdk.io/doc/](https://spdk.io/doc/)
* **Rust async/await**: [https://rust-lang.github.io/async-book/](https://rust-lang.github.io/async-book/)
* **Repository Issues**: Open an issue on GitHub for bugs or feature requests

## Roadmap

- [ ] More public API documentation
- [ ] Documentation at docs.rs
- [ ] Test coverage (cargo test)
- [ ] Additional block device examples (encryption, RAID5)
- [ ] T10 PI (DIF/DIX) support
- [ ] SPDK bdev resizing support
- [ ] Performance profiling tools
- [ ] Higher-level storage abstractions
- [ ] FreeBSD support

## Related or Similar Projects

* [SPDK](https://spdk.io/) - Storage Performance Development Kit
* [Tokio](https://tokio.rs/) - Async Rust runtime
* [Rust for Linux](https://github.com/Rust-for-Linux/linux) - Bringing Rust to kernel space
