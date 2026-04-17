#include <spdk/log.h>
#include "bdev.h"
#include "util.h"
#include "rust-unsafe.h"


struct io_channel_ctx {
    void *rust_ch;
};

void *u_io_channel_get_rust_ctx(void *io_ch_ctx)
{
    struct io_channel_ctx *chctx = io_ch_ctx;
    return chctx->rust_ch;
}

void u_io_channel_set_rust_ctx(void *io_ch_ctx, void *rust_ctx)
{
    struct io_channel_ctx *chctx = io_ch_ctx;
    chctx->rust_ch = rust_ctx;
}

static int u_bdev_io_channel_create(void *io_device, void *ctx)
{
    struct spdk_bdev *bdev = io_device;
    struct io_channel_ctx *chctx = ctx;

    chctx->rust_ch = rsu_io_channel_create(bdev->ctxt);
    SPDK_NOTICELOG("cpu=%d io_device=%p ctx=%p rust_ch=%p\n",
                   smp_cpu_id(), io_device, ctx, chctx->rust_ch);
    return 0;
}

static void u_bdev_io_channel_destroy(void *io_device, void *ctx)
{
    struct io_channel_ctx *chctx = ctx;
    (void)io_device;

    SPDK_NOTICELOG("cpu=%d io_device=%p ctx=%p rust_ch=%p\n",
                   smp_cpu_id(), io_device, ctx, chctx->rust_ch);
    rsu_io_channel_destroy(chctx->rust_ch);
}

int u_bdev_destruct(void *ctx)
{
    struct spdk_bdev *bdev;

    // destruct C state
    bdev = rsu_bdev_ctx_get_spdk_bdev(ctx);
    SPDK_NOTICELOG("ctx=%p bdev=%p\n", ctx, bdev);
    spdk_io_device_unregister(bdev, NULL);
    free(bdev->name);
    free(bdev);

    // destruct Rust state
    rsu_bdev_ctx_drop(ctx);
    return 0;
}

void u_bdev_submit_request(struct spdk_io_channel *ch,
                           struct spdk_bdev_io *bdev_io)
{
    void *bdev_ctxt = bdev_io->bdev->ctxt;
    void *io_ch_ctxt = spdk_io_channel_get_ctx(ch);
    struct io_channel_ctx *chctx = io_ch_ctxt;

    rsu_bdev_submit_request(bdev_ctxt, chctx->rust_ch, bdev_io);
}

bool u_bdev_io_type_supported(void *ctx, enum spdk_bdev_io_type io_type)
{
    return rsu_bdev_io_type_supported(ctx, io_type);
}

struct spdk_io_channel *u_bdev_get_io_channel(void *ctx)
{
    struct spdk_bdev *bdev;

    SPDK_NOTICELOG("cpu=%d ctx=%p\n", smp_cpu_id(), ctx);
    bdev = rsu_bdev_ctx_get_spdk_bdev(ctx);
    return spdk_get_io_channel(bdev);
}

struct spdk_bdev *u_bdev_alloc(const char *name,
                               const struct SpdkBdevOptsC *opts,
                               void *bdevctx)
{
    struct spdk_bdev *bdev;

    bdev = calloc(1, sizeof(*bdev));
    if (!bdev)
        return NULL;
    bdev->name = strdup(name);
    if (!bdev->name) {
        free(bdev);
        return NULL;
    }
    bdev->product_name = "Rust/SPDK block device";
    bdev->ctxt = bdevctx;
    rsu_bdev_ctx_set_spdk_bdev(bdevctx, bdev);

    // fill bdev options
    #define OPT(field) bdev->field = opts->field
    OPT(blocklen);
    OPT(blockcnt);
    OPT(write_cache);
    OPT(phys_blocklen);
    #undef OPT

    return bdev;
}

int u_bdev_register(const char *name, struct spdk_bdev *bdev)
{
    int rc;

    spdk_io_device_register(bdev,
                            u_bdev_io_channel_create,
                            u_bdev_io_channel_destroy,
                            sizeof(struct io_channel_ctx),
                            name);
    rc = spdk_bdev_register(bdev);
    if (rc == 0)
        rsu_bdev_init(bdev->ctxt);
    return rc;
}

static void
empty_bdev_event_cb(enum spdk_bdev_event_type type, struct spdk_bdev *bdev, void *ctx)
{
    (void)bdev;
    (void)ctx;

    SPDK_NOTICELOG("Unexpected event type: %d\n", type);
}

int u_spdk_bdev_delete_by_name(const char *name)
{
    int rc;
    struct spdk_bdev_desc *desc;
    struct spdk_bdev *bdev;

    rc = spdk_bdev_open_ext(name, false, empty_bdev_event_cb, NULL, &desc);
    if (rc != 0) {
        SPDK_ERRLOG("Failed to open bdev with name: '%s'\n", name);
        return rc;
    }

    bdev = spdk_bdev_desc_get_bdev(desc);
    assert(bdev);

    spdk_bdev_unregister(bdev, NULL, NULL);

    spdk_bdev_close(desc);

    return 0;
}

static void rs_bdev_event_cb(enum spdk_bdev_event_type type,
                            struct spdk_bdev *bdev, void *ctx)
{
    (void)ctx;
    SPDK_NOTICELOG("%s event %d bdev:%p\n", __func__, type, bdev);
}

int u_bdev_open(const char *bdev_name, bool write,
                struct spdk_bdev_desc **desc)
{
    return spdk_bdev_open_ext(bdev_name, write,
                              rs_bdev_event_cb, NULL, desc);
}

struct spdk_bdev *u_bdev_io_get_bdev(const struct spdk_bdev_io *io)
{
    return io->bdev;
}
