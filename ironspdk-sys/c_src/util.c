#define _GNU_SOURCE
#include <sched.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>
#include "util.h"

int smp_cpu_id(void)
{
    unsigned cpu, node;

    syscall(SYS_getcpu, &cpu, &node, NULL);
    return (int)cpu;
}

int smp_cpu_count(void)
{
    return sysconf(_SC_NPROCESSORS_ONLN);
}
