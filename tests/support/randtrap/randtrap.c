/*
 * randtrap.c -- LD_PRELOAD shim for wrapfile keygen random-source tests.
 *
 * The getrandom crate used by wrapfile issues the getrandom(2) syscall with
 * inline assembly, so symbol interposition (as in the permfail shim) cannot
 * see it. This shim instead installs a seccomp filter (SECCOMP_RET_TRAP)
 * for getrandom inside the guest process and emulates the syscall in a
 * SIGSYS handler, letting the tests make the OS random source fail in
 * controlled ways on a machine where it works fine. The filter only traps
 * getrandom inside the fault-configured guest: every other syscall and
 * every other process -- including other wrapfile invocations -- keeps
 * using the real random source.
 *
 * Activation: only inside an executable whose basename starts with
 * "wrapfile" (checked via /proc/self/exe), and only when one of the fault
 * variables below is set. Everything is configured through the environment:
 *
 *   WRAPFILE_TEST_GETRANDOM_FAIL=1        every getrandom call fails with
 *                                         EIO before a single byte is drawn
 *   WRAPFILE_TEST_GETRANDOM_FAIL_AFTER=N  calls succeed until N bytes in
 *                                         total have been handed out (the
 *                                         first call is truncated to N if
 *                                         needed), then fail with EIO -- a
 *                                         source that dries up mid-key
 *   WRAPFILE_TEST_GETRANDOM_PARTIAL=N     every call returns at most N
 *                                         bytes but always succeeds -- a
 *                                         source that only ever delivers
 *                                         short reads (NOT a failure: the
 *                                         guest must keep drawing)
 *   WRAPFILE_TEST_TRACE_FILE=PATH         one GETRANDOM line per trapped
 *                                         call is appended here, plus a
 *                                         TRAP line when the shim arms
 *
 * Bytes handed to the guest follow a fixed, recognisable pattern that is
 * never valid text (0x80, 0x81, ... by global draw index), so the tests can
 * prove that on success the key file holds exactly the bytes the source
 * delivered -- nothing invented, nothing zero-padded -- and that on failure
 * those bytes leak nowhere: not on stdout/stderr in raw or hex form, and
 * not into any file left behind.
 *
 * The constructor records "TRAP armed" / "TRAP unavailable ..." in the
 * trace file so tests can skip cleanly on kernels without seccomp.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/ucontext.h>
#include <unistd.h>

#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>

#if defined(__x86_64__)
#define TRAP_AUDIT_ARCH AUDIT_ARCH_X86_64
#elif defined(__aarch64__)
#define TRAP_AUDIT_ARCH AUDIT_ARCH_AARCH64
#else
#error "randtrap supports x86_64 and aarch64 only"
#endif

#ifndef SYS_getrandom
#if defined(__x86_64__)
#define SYS_getrandom 318
#else
#define SYS_getrandom 278
#endif
#endif

/* Stable kernel constant (asm-generic/siginfo.h); some glibc versions do
 * not expose it through <signal.h>. */
#ifndef SYS_SECCOMP
#define SYS_SECCOMP 1
#endif

enum fault_mode {
    MODE_OFF = 0,
    MODE_FAIL_ALL,   /* every call fails before a single byte is drawn */
    MODE_FAIL_AFTER, /* hand out cfg_arg bytes in total, then fail */
    MODE_PARTIAL     /* every call succeeds but returns at most cfg_arg bytes */
};

static enum fault_mode cfg_mode = MODE_OFF;
static long cfg_arg = 0;
static unsigned long long delivered = 0;
static int trace_fd = -1;

/* The byte handed out at global draw index `index`. The Rust tests
 * recompute the same sequence (see pattern_byte in keygen_random_source.rs). */
static unsigned char pattern_byte(unsigned long long index)
{
    return (unsigned char)(0x80 + (index % 64));
}

/* ----- activation: only inside the wrapfile executable ----- */

static int process_active(void)
{
    char buf[512];
    ssize_t n = readlink("/proc/self/exe", buf, sizeof(buf) - 1);
    if (n <= 0)
        return 0;
    buf[n] = '\0';
    const char *base = strrchr(buf, '/');
    base = base ? base + 1 : buf;
    return strncmp(base, "wrapfile", 8) == 0;
}

/* ----- async-signal-safe trace output ----- */

static size_t str_len(const char *s)
{
    size_t n = 0;
    while (s[n])
        n++;
    return n;
}

static void trace_write(const char *s, size_t n)
{
    if (trace_fd < 0)
        return;
    /* Raw syscall: async-signal-safe and immune to interposition. */
    while (n > 0) {
        long r = syscall(SYS_write, trace_fd, s, n);
        if (r <= 0)
            return;
        s += r;
        n -= (size_t)r;
    }
}

static void trace_str(const char *s)
{
    trace_write(s, str_len(s));
}

static size_t fmt_u64(char *out, unsigned long long v)
{
    char tmp[20];
    size_t n = 0, i;
    do {
        tmp[n++] = (char)('0' + (v % 10));
        v /= 10;
    } while (v);
    for (i = 0; i < n; i++)
        out[i] = tmp[n - 1 - i];
    return n;
}

/* One line per trapped call: "GETRANDOM req=N base=B ret=M" on (possibly
 * short) success, "GETRANDOM req=N base=B ret=-1 errno=EIO" on injected
 * failure. base is the global draw index of the first byte this call hands
 * out, so tests can reconstruct exactly which pattern bytes each caller
 * received (the process runtime may draw bytes of its own before main).
 * Called from the SIGSYS handler, so it only uses a stack buffer and the
 * raw write syscall. */
static void trace_getrandom(size_t req, unsigned long long base, long ret)
{
    char line[96];
    size_t n = 0;
    const char *p;

    if (trace_fd < 0)
        return;

    for (p = "GETRANDOM req="; *p; p++)
        line[n++] = *p;
    n += fmt_u64(line + n, (unsigned long long)req);
    for (p = " base="; *p; p++)
        line[n++] = *p;
    n += fmt_u64(line + n, base);
    for (p = " ret="; *p; p++)
        line[n++] = *p;
    if (ret < 0) {
        for (p = "-1 errno=EIO"; *p; p++)
            line[n++] = *p;
    } else {
        n += fmt_u64(line + n, (unsigned long long)ret);
    }
    line[n++] = '\n';
    trace_write(line, n);
}

/* ----- the emulated getrandom ----- */

static void on_sigsys(int sig, siginfo_t *info, void *uctx)
{
    ucontext_t *uc;
    unsigned char *buf;
    size_t len, k, i;
    long ret;

    (void)sig;
    if (info->si_code != SYS_SECCOMP || info->si_syscall != SYS_getrandom) {
        /* Not our trap: die the way an unhandled SIGSYS would. */
        signal(SIGSYS, SIG_DFL);
        raise(SIGSYS);
        return;
    }

    uc = (ucontext_t *)uctx;
#if defined(__x86_64__)
    buf = (unsigned char *)(uintptr_t)uc->uc_mcontext.gregs[REG_RDI];
    len = (size_t)uc->uc_mcontext.gregs[REG_RSI];
#else
    buf = (unsigned char *)(uintptr_t)uc->uc_mcontext.regs[0];
    len = (size_t)uc->uc_mcontext.regs[1];
#endif

    unsigned long long base = delivered;
    if (cfg_mode == MODE_FAIL_ALL) {
        ret = -EIO;
    } else if (cfg_mode == MODE_FAIL_AFTER) {
        long long remaining = (long long)cfg_arg - (long long)delivered;
        if (remaining <= 0) {
            ret = -EIO;
        } else {
            k = len < (size_t)remaining ? len : (size_t)remaining;
            for (i = 0; i < k; i++)
                buf[i] = pattern_byte(delivered + i);
            delivered += k;
            ret = (long)k;
        }
    } else { /* MODE_PARTIAL */
        k = len < (size_t)cfg_arg ? len : (size_t)cfg_arg;
        for (i = 0; i < k; i++)
            buf[i] = pattern_byte(delivered + i);
        delivered += k;
        ret = (long)k;
    }

    trace_getrandom(len, base, ret);

    /* The syscall was never executed; the value left in the return
     * register of the signal frame is what the guest sees. */
#if defined(__x86_64__)
    uc->uc_mcontext.gregs[REG_RAX] = ret;
#else
    uc->uc_mcontext.regs[0] = (unsigned long long)(long long)ret;
#endif
}

/* ----- constructor: arm the trap when a fault is configured ----- */

__attribute__((constructor))
static void randtrap_init(void)
{
    const char *v;
    struct sigaction sa;
    struct sock_filter filter[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS,
                 (unsigned)offsetof(struct seccomp_data, arch)),
        /* Arch match: fall through to the syscall-number check. Anything
         * else (a compat/foreign-ABI syscall) is simply never trapped. */
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, TRAP_AUDIT_ARCH, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS,
                 (unsigned)offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getrandom, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_TRAP),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog prog = {
        .len = (unsigned short)(sizeof(filter) / sizeof(filter[0])),
        .filter = filter,
    };

    if (!process_active())
        return;

    if ((v = getenv("WRAPFILE_TEST_GETRANDOM_FAIL")) && v[0] == '1') {
        cfg_mode = MODE_FAIL_ALL;
    } else if ((v = getenv("WRAPFILE_TEST_GETRANDOM_FAIL_AFTER")) && v[0]) {
        cfg_mode = MODE_FAIL_AFTER;
        cfg_arg = strtol(v, NULL, 10);
    } else if ((v = getenv("WRAPFILE_TEST_GETRANDOM_PARTIAL")) && v[0]) {
        cfg_mode = MODE_PARTIAL;
        cfg_arg = strtol(v, NULL, 10);
        if (cfg_arg < 1)
            cfg_mode = MODE_OFF;
    }
    if (cfg_mode == MODE_OFF)
        return;

    if ((v = getenv("WRAPFILE_TEST_TRACE_FILE")) && v[0])
        trace_fd = open(v, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0600);

    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = on_sigsys;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGSYS, &sa, NULL) != 0) {
        trace_str("TRAP unavailable errno=SIGACTION\n");
        cfg_mode = MODE_OFF;
        return;
    }

    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 ||
        prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog) != 0) {
        /* No seccomp on this kernel: leave the real random source alone
         * and let the tests skip themselves. */
        trace_str("TRAP unavailable errno=SECCOMP\n");
        cfg_mode = MODE_OFF;
        return;
    }

    trace_str("TRAP armed\n");
}
