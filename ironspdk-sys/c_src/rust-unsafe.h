#pragma once


// Unsafe extern "C" Rust functions

void *rsu_io_channel_create(void *bdev_ctxt);

void rsu_io_channel_destroy(void *io_ch_ctxt);

void rsu_bdev_ctx_set_spdk_bdev(void *ctx, struct spdk_bdev *bdev);

struct spdk_bdev *rsu_bdev_ctx_get_spdk_bdev(void *ctx);

void rsu_bdev_ctx_drop(void *ctx);

bool rsu_bdev_io_type_supported(void *ctxt, enum spdk_bdev_io_type io_type);

void rsu_bdev_init(void *bdev_ctxt);

void rsu_bdev_submit_request(void *bdev_ctxt, void *io_ch_ctxt, struct spdk_bdev_io *io);
