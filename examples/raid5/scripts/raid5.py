from spdk.rpc.cmd_parser import print_json

def spdk_rpc_plugin_initialize(subparsers):
    def rs_raid5_create(args):
        print_json(args.client.rs_raid5_create(name=args.name,
                                               strip_size_kb=args.strip_size_kb,
                                               children=args.children))
    p = subparsers.add_parser('rs_raid5_create',
                              help='Create ironspdk Rust RAID5 bdev')
    p.add_argument('-n', '--name', help="RAID5 bdev name", required=True)
    p.add_argument('-z', '--strip-size-kb', help='strip size in KB', type=int, required=True)
    p.add_argument('-c', '--children', help='Children bdevs, separated by comma')
    p.set_defaults(func=rs_raid5_create)

