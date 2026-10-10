#pragma once

void u_spdk_app_opts_init(struct spdk_app_opts *opts, const char *name);
size_t u_spdk_app_opts_size(void);
int u_spdk_app_parse_args(int argc, char **argv, struct spdk_app_opts *opts);
void u_spdk_app_set_shutdown_cb(struct spdk_app_opts *opts,
                                spdk_app_shutdown_cb cb);
int u_spdk_app_start(struct spdk_app_opts *opts,
                     spdk_msg_fn start_fn,
                     void *arg);
void u_spdk_app_stop(int rc);

struct spdk_cpuset *u_spdk_cpuset_alloc(void);
void u_spdk_cpuset_free(struct spdk_cpuset *set);

void *u_spdk_io_channel_get_ctx(struct spdk_io_channel *ch);

unsigned int u_bdev_io_get_type(struct spdk_bdev_io *io);
uint64_t u_bdev_io_get_offset_blocks(struct spdk_bdev_io *io);
uint64_t u_bdev_io_get_num_blocks(struct spdk_bdev_io *io);
void u_bdev_io_get_iovec(struct spdk_bdev_io *bdev_io,
                         struct iovec **iovp, int *iovcntp);
struct spdk_bdev *u_bdev_io_get_bdev(const struct spdk_bdev_io *io);
uint32_t u_bdev_get_blocklen(struct spdk_bdev *bdev);
uint64_t u_bdev_get_blockcnt(struct spdk_bdev *bdev);

