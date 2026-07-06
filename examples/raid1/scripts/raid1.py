from spdk.rpc.cmd_parser import print_json

def spdk_rpc_plugin_initialize(subparsers):
    def rs_raid1_create(args):
        print_json(args.client.rs_raid1_create(name=args.name,
                                               children=args.children))
    p = subparsers.add_parser('rs_raid1_create',
                              help='Create ironspdk Rust RAID1 bdev')
    p.add_argument('-b', '--name', help="Name of the bdev")
    p.add_argument('-c', '--children', help='Children bdevs, separated by comma')
    p.set_defaults(func=rs_raid1_create)
