#include <spdk/event.h>
#include <spdk/log.h>
#include <spdk/bdev_module.h>
#include "rpc.h"
#include "util.h"

RUST_SPDK_RPC(rs_bdev_delete)

size_t u_spdk_app_opts_size(void)
{
    return sizeof(struct spdk_app_opts);
}

int u_bdev_io_get_type(struct spdk_bdev_io *io)
{
    return (int)io->type;
}

uint64_t u_bdev_io_get_offset_blocks(struct spdk_bdev_io *io)
{
    return io->u.bdev.offset_blocks;
}

uint64_t u_bdev_io_get_num_blocks(struct spdk_bdev_io *io)
{
    return io->u.bdev.num_blocks;
}

uint32_t u_bdev_get_blocklen(struct spdk_bdev *bdev)
{
    return bdev->blocklen;
}

void u_bdev_io_get_iovec(struct spdk_bdev_io *bdev_io,
                         struct iovec **iovp, int *iovcntp)
{
    spdk_bdev_io_get_iovec(bdev_io, iovp, iovcntp);
}

/* Initialize opts safely (hides sizeof from Rust) */
void u_spdk_app_opts_init(struct spdk_app_opts *opts, const char *name)
{
    spdk_app_opts_init(opts, sizeof(*opts));
    opts->name = name; // spkd_app_start() requires 'name' to be specified
}

static int __noop_arg_parse(int ch, char *arg)
{
    (void)ch;
    (void)arg;
    return 0;
}

int u_spdk_app_parse_args(int argc, char **argv, struct spdk_app_opts *opts)
{
    return spdk_app_parse_args(argc, argv, opts, "", NULL, __noop_arg_parse, NULL);
}

void u_spdk_app_set_shutdown_cb(struct spdk_app_opts *opts,
                                spdk_app_shutdown_cb cb)
{
    opts->shutdown_cb = cb;
}

struct spdk_cpuset *u_spdk_cpuset_alloc(void)
{
    return calloc(1, sizeof(struct spdk_cpuset));
}

void u_spdk_cpuset_free(struct spdk_cpuset *set)
{
    free(set);
}

void *u_spdk_io_channel_get_ctx(struct spdk_io_channel *ch)
{
    return spdk_io_channel_get_ctx(ch);
}

/* Start SPDK app */
int u_spdk_app_start(struct spdk_app_opts *opts,
                     spdk_msg_fn start_fn,
                     void *arg)
{
    return spdk_app_start(opts, start_fn, arg);
}

/* Stop SPDK app */
void u_spdk_app_stop(int rc)
{
    spdk_app_stop(rc);
}
