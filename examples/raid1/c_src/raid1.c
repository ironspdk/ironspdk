#include <spdk/log.h>
#include "bdev.h"
#include "rpc.h"

// these callbacks may be overridden by the user
static struct spdk_bdev_fn_table rs_raid1_bdev_fn_table = {
    .destruct = u_bdev_destruct,
    .submit_request = u_bdev_submit_request,
    .io_type_supported = u_bdev_io_type_supported,
    .get_io_channel = u_bdev_get_io_channel,
};

static int rs_raid1_module_init(void)
{
    SPDK_NOTICELOG("\n");
    return 0;
}

static void rs_raid1_module_fini(void)
{
    SPDK_NOTICELOG("\n");
}

static struct spdk_bdev_module rs_raid1_module = {
    .name = "rs_raid1",
    .module_init = rs_raid1_module_init,
    .module_fini = rs_raid1_module_fini,
};

SPDK_BDEV_MODULE_REGISTER(rs_raid1, &rs_raid1_module);

int raid1_bdev_create(const char *name, const struct SpdkBdevOptsC *opts, void *bdevctx)
{
    struct spdk_bdev *bdev;

    bdev = u_bdev_alloc(name, opts, bdevctx);
    if (!bdev)
        return -ENOMEM;
    bdev->fn_table = &rs_raid1_bdev_fn_table;
    bdev->module = &rs_raid1_module;
    return u_bdev_register(name, bdev);
}

RUST_SPDK_RPC(rs_raid1_create)
