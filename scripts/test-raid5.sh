#!/bin/bash

set -euo pipefail

SCRIPT_FILE=$(basename $0)
SCRIPT_PATH=$(realpath $(dirname "$0"))
D=$(realpath "$SCRIPT_PATH/..")
SPDK="$D/extern/spdk"

usage() {
    cat <<EOF

Usage:

$SCRIPT_FILE [-p] [-s] [-n] [-S <block_size>]
             [-t <time>] [-m <method>] [-j <numjobs>] [-V] [-h]

    -p  Run 'fio' under 'perf'
    -s  Run SPDK raid5f if set (otherwise run examples/raid5)
    -n  Run RAID5 on NVMe devices if set (otherwise run it on malloc bdevs)
    -S  Block size to be set in --bs argument of 'fio'
            (default value is '32k';
            it is invalid to specify value other than '32k' if -s is also set)
    -t  Time to run fio, seconds (default value is 60)
    -m  Method to be set in --rw argument of 'fio'
            (default value is 'randrw')
    -j  Number of jobs for 'fio' (default value is 1)
    -V  Add '--verify=md5' option to 'fio' is set (also, -j is ignored
            and number of jobs reset to 1)
    -h  Print this help and exit
EOF
}

TIME=60
JOBS=1
PERF=0
SPDK_RAID5F=0
NVME=0
VERIFY=0
RW=randrw
BS=32k
NAME="raid5"

while getopts "psnS:t:m:j:Vh" flag; do
    case "$flag" in
        h) usage; exit 0 ;;
        p) PERF=1 ;;
        s) SPDK_RAID5F=1 ;;
        S) BS=$OPTARG ;;
        n) NVME=1 ;;
        m) RW=$OPTARG ;;
        t) TIME=$OPTARG ;;
        j) JOBS=$OPTARG ;;
        V) VERIFY=1 ;;
    esac
done

if [ $SPDK_RAID5F -ne 0 ] && [[ $BS != "32k" ]]; then
    echo "Invalid block size for SPDK raid5f: $BS" >&2
    exit 1
fi

VERIFY_OPTS=""
if [ $VERIFY -ne 0 ]; then
    JOBS=1
    VERIFY_OPTS="--verify=md5"
fi

N=1

shutdown() {
set +e
sudo $SPDK/scripts/rpc.py ublk_stop_disk $N
if [ $SPDK_RAID5F -ne 0 ]; then #SPDK raid5f
    sudo $SPDK/scripts/rpc.py bdev_raid_delete "$NAME" 2>&1 >/dev/null
else
    sudo PYTHONPATH=$D/scripts $SPDK/scripts/rpc.py \
        --plugin ironspdk rs_bdev_delete "$NAME" 2>&1 >/dev/null
fi
if [ $NVME -ne 0 ]; then
    for i in $(seq 1 3); do
        sudo $SPDK/scripts/rpc.py \
            bdev_nvme_detach_controller Nvme${i} 2>&1 >/dev/null
    done
else
    for i in $(seq 1 3); do
        sudo $SPDK/scripts/rpc.py \
            bdev_malloc_delete malloc${i} 2>&1 >/dev/null
    done
fi
}

trap shutdown EXIT

DISKS=""
if [ $NVME -ne 0 ]; then
    #GET_STATUS="sudo $SPDK/scripts/setup.sh status >/dev/null"
    #2>/dev/null | grep '^NVMe' | awk '{print $2}'"
    NVME_COUNT=$(sudo $SPDK/scripts/setup.sh status 2>/dev/null | grep '^NVMe' | wc -l)
    if [ $NVME_COUNT -lt 3 ]; then
        echo "Too few NVMe disks: $NVME_COUNT" >&2
        exit 1
    fi
    # take 3 NVMe disks
    NVME_PCI_ADDRS=$(sudo $SPDK/scripts/setup.sh status 2>/dev/null | grep '^NVMe' | awk '{print $2}' | head -n3)
    #NVME_PCI_ADDRS=$($GET_NVME_PCI_ADDRS | head -n3)
    i=1
    DISKS=""
    for NVME_PCI_ADDR in $NVME_PCI_ADDRS; do
        sudo $SPDK/scripts/rpc.py bdev_nvme_attach_controller \
            -b Nvme${i} -t PCIe -a $NVME_PCI_ADDR
        DISKS="${DISKS} Nvme${i}n${i}"
        i=$((i + 1))
    done
else
    for i in $(seq 1 3); do
        DISK=$(sudo $SPDK/scripts/rpc.py bdev_malloc_create -b malloc${i} 64 512)
        DISKS="${DISKS} ${DISK}"
    done
fi

# Set up RAID5
# 3 disks (2 data, 1 parity)
# strip size: 16 KB
if [ $SPDK_RAID5F -ne 0 ]; then #SPDK raid5f
    sudo $SPDK/scripts/rpc.py bdev_raid_create -n "$NAME" -r 5f -z 16 -b "$DISKS"
else # ironspdk raid5
    sudo PYTHONPATH=$D/examples/raid5/scripts/ $SPDK/scripts/rpc.py --plugin raid5 rs_raid5_create --name "$NAME" -z 16 -c "$DISKS"
fi

# set up ublk

sudo modprobe ublk_drv
sudo $SPDK/scripts/rpc.py ublk_create_target >/dev/null 2>/dev/null || true
N=$(sudo $SPDK/scripts/rpc.py ublk_start_disk "$NAME" $N -q $(nproc) -d 128)
DEV=/dev/ublkb${N}

# run fio

PID=$(pidof raid5)
FIO="fio --filename=$DEV --direct=1 --numjobs=$JOBS --rw=$RW --bs=$BS --offset=0 --iodepth=32 --ioengine=libaio --random_generator=lfsr --time_based=1 --runtime=$TIME $VERIFY_OPTS --name=f"

set +e
if [ $PERF -ne 0 ]; then
    sudo perf record --call-graph dwarf,16384 -F 999 -p $PID -- $FIO
else
    sudo $FIO
fi

if [ $? -ne 0 ]; then
    echo fio FAILED
else
    echo fio OK
fi
