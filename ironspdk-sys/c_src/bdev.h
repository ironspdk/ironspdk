#pragma once

#include <spdk/bdev.h>
#include <spdk/bdev_module.h>

// This structure must be synchronized with Rust's structure with same name
struct SpdkBdevOptsC {
    uint32_t blocklen;
    uint64_t blockcnt;
    bool write_cache;
    uint32_t phys_blocklen;
};

int u_bdev_destruct(void *ctx);

void u_bdev_submit_request(struct spdk_io_channel *ch,
                           struct spdk_bdev_io *bdev_io);

bool u_bdev_io_type_supported(void *ctx, enum spdk_bdev_io_type io_type);

struct spdk_io_channel *u_bdev_get_io_channel(void *ctx);

struct spdk_bdev *u_bdev_alloc(const char *name,
                               const struct SpdkBdevOptsC *opts,
                               void *bdevctx);

int u_bdev_register(const char *name, struct spdk_bdev *bdev);
