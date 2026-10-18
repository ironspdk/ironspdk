#!/usr/bin/env python3

"""
ironspdk / SPDK RAID5 matrix benchmark.

Examples:

sudo ./bench-raid5.py \
    --bdev malloc \
    --affinity 0x1,0xff \
    --jobs 1,2,4,8,16,32 \
    --rw randread,randwrite,randrw \
    --isa sse,native \
    --time 10,30 \
    --format table

sudo ./bench-raid5.py \
    --bdev nvme \
    --affinity 0xf,0xff \
    --jobs 8,16,32 \
    --rw randread,randwrite \
    --isa sse,native \
    --time 30 \
    --format csv > results.csv

The SPDK/raid5f and ironspdk/raid5 implementations are always
tested in round-robin order.

Every fio test is executed twice; only the second result is recorded.
"""

import argparse
import csv
import json
import os
import re
import signal
import subprocess
import sys
import time
import traceback
from dataclasses import dataclass

# Configuration
SCRIPT_DIR = os.path.realpath(os.path.dirname(__file__))
SRCDIR = os.path.realpath(os.path.join(SCRIPT_DIR, ".."))
SPDK = os.path.join(SRCDIR, "extern/spdk")
RAID5_BINARY = os.path.join(SRCDIR, "target/release/raid5")
RAID5_SCRIPTS = os.path.join(SRCDIR, "examples/raid5/scripts")
IRONSPDK_SCRIPTS = os.path.join(SRCDIR, "scripts")
RPC = os.path.join(SPDK, "scripts/rpc.py")
NAME = "raid5"
UBLK_DISK = 1
# RAID5 configuration for tests: 2+1 disks, strip size 16 kb,
# always full-stripe I/O
STRIP_KB = 16
BS = "32768"
IODEPTH = 32
NUM_DISKS = 3

"""
SPDK malloc bdev parameters:
64 blocks, 512-byte blocks
"""
MALLOC_BLOCKS = 64
MALLOC_BLOCKLEN = 512

# ublk parameters
UBLK_QUEUE_DEPTH = 128


@dataclass
class Result:
    implementation: str
    isa: str
    affinity: str
    jobs: int
    rw: str
    runtime: int
    bw_read: float
    bw_write: float


def log(*args, file=sys.stderr):
    print(*args, file=file, flush=True)


def run(cmd, *, check=True, env=None):
    """Run a command, reporting captured output when it fails."""
    log("+", " ".join(map(str, cmd)))
    try:
        return subprocess.run(
            cmd,
            check=check,
            capture_output=True,
            text=True,
            env=env,
        )
    except subprocess.CalledProcessError as e:
        log(f"Command failed with exit code {e.returncode}")
        log(f"Command: {e.cmd}")
        log(f"stdout:\n{e.stdout or ''}")
        log(f"stderr:\n{e.stderr or ''}")
        raise

def sudo(cmd, **kwargs):
    return run(["sudo"] + cmd, **kwargs)


def rpc(*args, **kwargs):
    return sudo([RPC] + list(args), **kwargs)


def kill_process(proc):
    if proc is None:
        return
    if proc.poll() is not None:
        return
    log("+ stopping raid5")
    try:
        proc.send_signal(signal.SIGINT)
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()


def parse_int(s):
    return int(s, 0)


def parse_affinity_masks(spec):
    """
    Parse affinity list, check for correctness.
    """
    parts = spec.split(",")

    available_cpus = len(os.sched_getaffinity(0))

    for part in parts:
        if not re.match('0x[0-9a-fA-F]+', part):
            print(f'Error: invalid affinity "{part}"', file=sys.stderr)
            exit(1)
        mask = int(part, 16)
        if mask == 0:
            raise ValueError(f"Affinity mask must not be zero: {part}")
        if mask >> available_cpus != 0:
            raise ValueError(f'Affinity mask exceeds core count: mask: {part}, core count: {available_cpus}')
    return parts


def build(isa):
    """
    Build the release binary.

    SSE:
        make release

    native:
        RUSTFLAGS="-C target-cpu=native" make release
    """

    env = os.environ.copy()

    if isa == "native":
        env["RUSTFLAGS"] = "-C target-cpu=native"
    elif isa == "sse":
        # Make sure an inherited RUSTFLAGS doesn't accidentally affect
        # the SSE/default build.
        env.pop("RUSTFLAGS", None)
    else:
        raise ValueError(f"unknown ISA: {isa}")

    cmd = ["make", "release"]

    log(f"\n===== BUILD {isa.upper()} =====")

    run(cmd, check=True, env=env)

    if not os.path.exists(RAID5_BINARY):
        raise RuntimeError(f"release binary not found: {RAID5_BINARY}")


def start_spdk(affinity):
    """
    Start the Rust/SPDK application.

    Both SPDK raid5f and ironspdk RAID5 are available through RPC
    in this same runtime.
    """

    cmd = ["sudo", RAID5_BINARY,
           "-m", affinity,
           "--base-virtaddr=0x100000000"]

    log(f"\n===== START SPDK affinity={affinity} =====")

    f = open('/tmp/spdk-log.txt', 'a')

    proc = subprocess.Popen(cmd, stdout=f, stderr=f, text=True)

    # Give SPDK time to initialize.
    time.sleep(2)

    if proc.poll() is not None:
        raise RuntimeError(f"raid5 exited immediately with code {proc.returncode}")

    return proc


def stop_spdk(proc):
    kill_process(proc)
    time.sleep(1)


def delete_malloc_bdevs():
    for i in range(NUM_DISKS):
        rpc("bdev_malloc_delete", f"malloc{i}", check=False)


def create_malloc_disks():
    disks = []
    for i in range(NUM_DISKS):
        name = f"malloc{i}"
        rpc("bdev_malloc_create", "-b", name, str(MALLOC_BLOCKS), str(MALLOC_BLOCKLEN))
        disks.append(name)
    return disks


def attach_nvme_disks():
    """
    Attach the first three NVMe controllers reported by SPDK setup.sh.

    This follows scripts/test-raid5.sh: controllers are named Nvme1..Nvme3
    and their namespace bdevs are Nvme1n1, Nvme2n2, and Nvme3n3.
    """
    status = sudo([os.path.join(SPDK, "scripts/setup.sh"), "status"])
    pci_addrs = []
    for line in status.stdout.splitlines():
        fields = line.split()
        if fields and fields[0] == "NVMe" and len(fields) >= 2:
            pci_addrs.append(fields[1])

    if len(pci_addrs) < NUM_DISKS:
        raise RuntimeError(
            f"Too few NVMe disks: found {len(pci_addrs)}, need at least {NUM_DISKS}")

    try:
        for i, pci_addr in enumerate(pci_addrs[:NUM_DISKS], start=1):
            rpc("bdev_nvme_attach_controller",
                "-b", f"Nvme{i}",
                "-t", "PCIe",
                "-a", pci_addr)
    except Exception:
        # Detach any controllers attached before the failure.
        detach_nvme_disks()
        raise

    return [f"Nvme{i}n{i}" for i in range(1, 4)]


def detach_nvme_disks():
    for i in range(1, 4):
        rpc("bdev_nvme_detach_controller", f"Nvme{i}", check=False)


def create_raid5(implementation, disks):
    if implementation == "spdk":
        rpc("bdev_raid_create", "-n", NAME,
            "-r", "5f", "-z", str(STRIP_KB),  "-b", " ".join(disks))
    elif implementation == "ironspdk":
        sudo(["env", f"PYTHONPATH={RAID5_SCRIPTS}",
              RPC, "--plugin", "raid5", "rs_raid5_create",
                "--name", NAME,
                "-z", str(STRIP_KB),
                "-c", " ".join(disks)])
    else:
        raise ValueError(f"unknown implementation: {implementation}")


def affinity_cpu_count(mask):
    return bin(parse_int(mask)).count("1")


def setup_ublk():
    sudo(["modprobe", "ublk_drv"], check=False)
    rpc("ublk_create_target", check=False)


def start_ublk(affinity):
    queues = affinity_cpu_count(affinity)
    rpc("ublk_start_disk", NAME, str(UBLK_DISK),
        "-q", str(queues), "-d", str(UBLK_QUEUE_DEPTH))


def stop_ublk():
    rpc("ublk_stop_disk", str(UBLK_DISK), check=False)


def stop_raid5(implementation):
    if implementation == "ironspdk":
        sudo(["env", f"PYTHONPATH={IRONSPDK_SCRIPTS}",
             RPC, "--plugin", "ironspdk", "rs_bdev_delete", NAME],
             check=False)
    elif implementation == "spdk":
        # SPDK raid5f deletion.
        rpc("bdev_raid_delete", NAME,
            check=False)
    else:
        raise ValueError(implementation)


def run_fio(rw, jobs, runtime):
    """
    Run fio twice.

    First run: warm-up, result ignored.
    Second run: recorded.
    """
    fio_cmd = [
        "fio",
        f"--filename=/dev/ublkb{UBLK_DISK}",
        "--direct=1",
        f"--numjobs={jobs}",
        f"--rw={rw}",
        f"--bs={BS}",
        "--offset=0",
        f"--iodepth={IODEPTH}",
        "--ioengine=libaio",
        "--random_generator=lfsr",
        "--time_based=1",
        f"--runtime={runtime}",
        "--name=bench",
        "--output-format=json",
    ]
    log(f"      warmup: {rw} jobs={jobs} time={runtime}")
    sudo(fio_cmd, check=True)
    log(f"      measured: {rw} jobs={jobs} time={runtime}")
    result = sudo(fio_cmd, check=True)
    return parse_fio_bw(result.stdout)


def parse_fio_bw(output):
    """
    Extract READ and WRITE bandwidths (in KiB/s) from fio JSON.
    """
    data = json.loads(output)
    total_kib_read = 0
    total_kib_write = 0
    for job in data["jobs"]:
        total_kib_read += job["read"]["bw"]
        total_kib_write += job["write"]["bw"]
    return (total_kib_read / 1024.0, total_kib_write / 1024.0)


def run_matrix(args):
    affinities = parse_affinity_masks(args.affinity)
    jobs = [int(x) for x in args.jobs.split(",") if x.strip()]
    rws = [x.strip() for x in args.rw.split(",") if x.strip()]
    isas = [x.strip().lower() for x in args.isa.split(",") if x.strip()]
    runtimes = [int(x) for x in args.time.split(",") if x.strip()]
    results = []

    # ------------------------------------------------------------------
    # Important:
    #
    # Build once per ISA.
    #
    # For each affinity we restart SPDK.
    #
    # Within one runtime:
    #
    #     SPDK
    #     ironspdk
    #     SPDK
    #     ironspdk
    #
    # according to the matrix dimensions.
    # ------------------------------------------------------------------

    for isa in isas:
        build(isa)

        for affinity in affinities:
            proc = None
            nvme_disks_attached = False
            try:
                proc = start_spdk(affinity)
                if args.bdev == "nvme":
                    # Keep controllers attached for all measurements using
                    # this SPDK process; only the RAID bdev is recreated.
                    nvme_disks = attach_nvme_disks()
                    nvme_disks_attached = True
                setup_ublk()
                for runtime in runtimes:
                    for rw in rws:
                        for job in jobs:
                            # ------------------------------------------------
                            # Round robin:
                            #
                            # SPDK
                            # SPDK measured
                            # ironspdk
                            # ironspdk measured
                            #
                            # ------------------------------------------------
                            for implementation in ["ironspdk", "spdk"]:
                                log(
                                    f"\n===== {implementation.upper()} "
                                    f"ISA={isa} "
                                    f"affinity={affinity} "
                                    f"jobs={job} "
                                    f"rw={rw} "
                                    f"time={runtime} =====",
                                )
                                # It is practical to recreate malloc bdev each time:
                                # this is cheap operation and bdev is initially zeroed
                                disks = (
                                    create_malloc_disks()
                                    if args.bdev == "malloc"
                                    else nvme_disks
                                )
                                create_raid5(implementation, disks)
                                start_ublk(affinity)

                                try:
                                    bw_read, bw_write = run_fio(rw, job, runtime)
                                except Exception as e:
                                    log(f"fio exception: {e}")
                                    log(traceback.format_exc())
                                    raise
                                finally:
                                    stop_ublk()
                                    stop_raid5(implementation)
                                    log("stopping bdevs")
                                    if args.bdev == "malloc":
                                        delete_malloc_bdevs()

                                result = Result(
                                    implementation=implementation,
                                    isa=isa,
                                    affinity=affinity,
                                    jobs=job,
                                    rw=rw,
                                    runtime=runtime,
                                    bw_read=bw_read,
                                    bw_write=bw_write,
                                )
                                results.append(result)
                                log(
                                    f"RESULT "
                                    f"{implementation} "
                                    f"{isa} "
                                    f"{affinity} "
                                    f"jobs={job} "
                                    f"rw={rw} "
                                    f"time={runtime} "
                                    f"bw_read={bw_read:.1f} MiB/s",
                                    f"bw_write={bw_write:.1f} MiB/s",
                                )
            except Exception as e:
                log(f"matrix bench exception {e}\n")
                log(traceback.format_exc())
                raise
            finally:
                if nvme_disks_attached:
                    detach_nvme_disks()
                stop_spdk(proc)
    return results


# Results
def print_table(results):
    if not results:
        return

    headers = [
        "implementation",
        "isa",
        "affinity",
        "jobs",
        "rw",
        "time",
        "bw READ",
        "bw WRITE",
    ]

    rows = []

    for r in results:
        rows.append(
            [
                r.implementation,
                r.isa,
                r.affinity,
                str(r.jobs),
                r.rw,
                str(r.runtime),
                f"{r.bw_read:.1f}",
                f"{r.bw_write:.1f}",
            ]
        )

    widths = [
        max(
            len(headers[i]),
            *(len(row[i]) for row in rows),
        )
        for i in range(len(headers))
    ]

    def line(left, middle, right, fill="─"):
        return left + middle.join(fill * (w + 2) for w in widths) + right

    log(file=sys.stdout)
    log(line("┌", "┬", "┐"), file=sys.stdout)

    log(
        "│"
        + "│".join(f" {headers[i]:<{widths[i]}} " for i in range(len(headers)))
        + "│",
        file=sys.stdout,
    )
    log(line("├", "┼", "┤"), file=sys.stdout)

    for row in rows:
        log(
            "│"
            + "│".join(f" {row[i]:<{widths[i]}} " for i in range(len(headers)))
            + "│",
            file=sys.stdout,
        )

    log(line("└", "┴", "┘"), file=sys.stdout)


def print_csv(results):
    writer = csv.writer(sys.stdout)

    writer.writerow(
        [
            "implementation",
            "isa",
            "affinity",
            "jobs",
            "rw",
            "runtime",
            "bw_read",
            "bw_write",
        ]
    )

    for r in results:
        writer.writerow(
            [
                r.implementation,
                r.isa,
                r.affinity,
                r.jobs,
                r.rw,
                r.runtime,
                f"{r.bw_read:.1f}",
                f"{r.bw_write:.1f}",
            ]
        )


def main():
    parser = argparse.ArgumentParser(description="ironspdk/SPDK RAID5 matrix benchmark")

    parser.add_argument("--affinity", required=True,
                        help="comma-separated affinity masks; e.g. 0x1,0xff")

    parser.add_argument("--jobs", required=True,
                        help="comma-separated fio numjobs; e.g. 1,2,4,8,16,32")

    parser.add_argument("--rw", required=True,
                        help="comma-separated fio workloads; e.g. randread,randwrite,randrw")

    parser.add_argument("--isa", required=True,
                        help="comma-separated builds: sse,native")

    parser.add_argument("--time", required=True,
                        help="comma-separated fio runtimes in seconds; e.g. 10,30,60")

    parser.add_argument(
        "--bdev",
        choices=("malloc", "nvme"),
        default="malloc",
        help="backend bdev type: malloc (default) or nvme",
    )

    parser.add_argument("--format", choices=("table", "csv"), default="table")

    args = parser.parse_args()

    results = run_matrix(args)

    if args.format == "table":
        print_table(results)
    else:
        print_csv(results)


if __name__ == "__main__":
    main()
