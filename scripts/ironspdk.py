from spdk.rpc.cmd_parser import print_json

def spdk_rpc_plugin_initialize(subparsers):
    def rs_bdev_delete(args):
        print_json(args.client.rs_bdev_delete(name=args.name))
    p = subparsers.add_parser('rs_bdev_delete',
                              help='Delete bdev created by ironspdk')
    p.add_argument('name', help="Name of the bdev")
    p.set_defaults(func=rs_bdev_delete)
