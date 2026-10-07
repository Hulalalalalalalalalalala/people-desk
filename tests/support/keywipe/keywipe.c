/*
 * keywipe.c -- LD_PRELOAD shim for wrapfile keygen temporary-key wipe tests.
 *
 * README ("内存中的临时密钥清零") promises that the 32-byte buffer that
 * directly holds the generated key is overwritten with zeroes before the
 * operation returns -- on success and on every failure path, and that the
 * erasure survives an optimizing compiler. File-level checks cannot see
 * that: the key file is written before the wipe and the process exits
 * after it, so neither the saved bytes, the output, nor the exit code can
 * tell a wiped buffer from a leaked one. This shim watches the buffer
 * itself, in-process, while the operation is still running.
 *
 * How it works:
 *
 * 1. Like randtrap, a seccomp filter (SECCOMP_RET_TRAP) intercepts the
 *    guest's getrandom(2) (the getrandom crate issues the syscall with
 *    inline assembly, so symbol interposition cannot see it). A SIGSYS
 *    handler emulates the call, serving bytes from /dev/urandom, or a
 *    fixed recognisable pattern, or a pattern that dries up after N bytes
 *    (a random source that fails mid-key). The handler records the address
 *    of the 32-byte key buffer and every byte delivered into it, so the
 *    tests can prove the run really handled non-zero secret bytes and that
 *    the saved file holds exactly those bytes (a wipe that ran *before*
 *    the save would leave a zeroed or truncated file).
 *
 * 2. The moment the key fill completes -- or fails mid-way -- the shim
 *    records what the buffer received. The watch itself is armed only
 *    once the save is over: when the key file's descriptor is closed
 *    (success and every file-stage failure path close it before the
 *    wipe), when the create fails, or immediately when the random source
 *    failed mid-key (no file is ever created in that case). Arming means
 *    mprotect()ing the page(s) holding the buffer to read-only. Between
 *    the arm and the wipe no syscall writes results into those stack
 *    pages, so the protection cannot disturb the operation (an earlier
 *    design armed right after the fill, and the kernel's fstat(2)/readlink(2)
 *    writes into the still-protected page failed with EFAULT); every
 *    *store* to the buffer -- in particular the wipe -- now faults.
 *
 * 3. The SIGSEGV handler makes the page(s) writable again and lets exactly
 *    one instruction run (single-stepping: on x86_64 via the TF flag in
 *    the saved context; on aarch64 by patching a BRK instruction after the
 *    faulting one -- only stores fault on a read-only page and a store
 *    never branches, so the next instruction is always pc+4). The
 *    post-step handler re-protects the page(s) and inspects the buffer.
 *    A store of a non-zero value into the buffer before all 32 bytes have
 *    been zeroed is a violation (the slot was reused before the wipe ran);
 *    once every byte has been observed stored as zero the wipe is
 *    confirmed and the watch disarms itself. Signal frames are kept off
 *    the watched pages with an alternate signal stack, and a genuine
 *    segfault elsewhere is chained to the previously installed handler.
 *
 * 4. A destructor reports the final state to the trace file, so a wipe
 *    that never happens is caught even if nothing ever touches the buffer
 *    again. An interposed write() logs the first output the command prints
 *    after the key fill, so the trace also proves the wipe completed
 *    before the operation reported its result.
 *
 * Page granularity needs one concession: the key buffer shares its page
 * with ordinary stack data, and the kernel refuses to write syscall
 * results into a read-only page -- it just returns EFAULT, without any
 * signal to intercept. The deferred arming above keeps every such
 * syscall outside the watched window; as a belt-and-braces guard the
 * shim also interposes fstat/fstat64 and briefly lifts the protection
 * around the real call if a caller's stat buffer ever overlaps the
 * watched pages. No guest instruction runs while the protection is
 * lifted, so no store to the key buffer can go unobserved through it.
 *
 * Activation: only inside an executable whose basename starts with
 * "wrapfile" (checked via /proc/self/exe), and only when one of the mode
 * variables below is set. Everything is configured through the
 * environment:
 *
 *   WRAPFILE_TEST_KEYWIPE=1             serve key bytes from /dev/urandom
 *                                       (the real OS random source)
 *   WRAPFILE_TEST_KEYWIPE_PATTERN=1     serve a fixed pattern (0x80, 0x81,
 *                                       ... by key byte index)
 *   WRAPFILE_TEST_KEYWIPE_FAIL_AFTER=N  the key fill receives N pattern
 *                                       bytes in total, then the source
 *                                       fails with EIO mid-key
 *   WRAPFILE_TEST_TRACE_FILE=PATH       trace log, appended:
 *
 *     TRAP armed / TRAP unavailable ...   (tests skip on the latter)
 *     GETRANDOM req=N ret=M               each trapped call (-1 = EIO)
 *     KEYFILL n=N hex=...                 the N bytes the key buffer received
 *     WATCH armed pages=K / WATCH unavailable ...
 *     WIPE confirmed zeroed=32            every byte observed stored as zero
 *     WIPE violated off=I                 a non-zero store into the buffer
 *                                         before it was fully zeroed
 *     WIPE missing nonzero=K              process exiting without a full wipe
 *     OUTPUT fd=N                         first stdout/stderr write after the
 *                                         key fill (must come after WIPE
 *                                         confirmed)
 *
 * The shim only activates inside the fault-configured guest; every other
 * process and every other syscall is unaffected. x86_64 and aarch64 only.
 */

#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/stat.h>
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
#error "keywipe supports x86_64 and aarch64 only"
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

#define KEY_LEN 32
#define ALTSTACK_SIZE 65536

enum serve_mode {
    MODE_REAL = 0, /* bytes from /dev/urandom */
    MODE_PATTERN,  /* fixed pattern bytes */
    MODE_FAIL_AFTER /* pattern bytes, then the source dries up mid-key */
};

static int active = 0;
static enum serve_mode cfg_mode = MODE_REAL;
static long cfg_arg = 0;

static int urandom_fd = -1;
static int trace_fd = -1;
static unsigned long page_size = 4096;

/* The guest's key buffer, learned from the first 32-byte getrandom
 * request (keygen draws the whole key with one full-length request; the
 * only earlier calls are short runtime probes, and a zero-length probe
 * never matches). */
static unsigned char *key_addr = 0;
static int have_key_addr = 0;
static int key_delivered = 0; /* bytes delivered into the key buffer */
static unsigned long long draw_index = 0; /* global, for pattern bytes */

/* Arming the watch is deferred until the save is over: fill_done is set
 * when the key fill ends (complete or failed); key_fd tracks the key
 * file's descriptor (-2 = not yet seen, -1 = none/closed). */
static int fill_done = 0;
static int key_fd = -2;

/* Watch state. */
static unsigned char shadow[KEY_LEN]; /* buffer snapshot at arm time */
static unsigned char zeroed[KEY_LEN]; /* bytes observed stored as zero */
static int watch_armed = 0;
static int wipe_confirmed = 0;
static int wipe_violated = 0;
static int output_seen = 0;
static uintptr_t watch_lo = 0, watch_len = 0;

/* A faulting store is being single-stepped: the post-step handler owes
 * the pages their read-only protection back. */
static int stepped = 0;
static int pending_off = -1; /* si_addr offset into the buffer, or -1 */

#if defined(__aarch64__)
static uint32_t saved_insn = 0;
static void *patch_site = 0;
static uintptr_t patch_page = 0;
#endif

static struct sigaction saved_segv;
static void *altstack_base = 0;

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

/* ----- async-signal-safe trace output (raw syscall, stack buffers) ----- */

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

static void trace_u64(unsigned long long v)
{
    char tmp[20];
    trace_write(tmp, fmt_u64(tmp, v));
}

static void trace_hex8(unsigned char b)
{
    char c[2];
    c[0] = "0123456789abcdef"[b >> 4];
    c[1] = "0123456789abcdef"[b & 15];
    trace_write(c, 2);
}

/* The byte handed out at global draw index `index` in the pattern modes.
 * Never a zero byte, so the tests can prove the run handled non-zero
 * secret material. */
static unsigned char pattern_byte(unsigned long long index)
{
    return (unsigned char)(0x80 + (index % 64));
}

/* ----- the wipe watch ----- */

static void disarm_watch(void)
{
    if (!watch_armed)
        return;
    mprotect((void *)watch_lo, watch_len, PROT_READ | PROT_WRITE);
    watch_armed = 0;
}

/* Called after every stepped store, with the pages already read-only
 * again: fold the store's effect into the zeroed map. */
static void check_wipe_state(void)
{
    int i, all;

    /* A wide store may have covered buffer bytes beyond si_addr; any
     * change since the snapshot is attributed by its new value. */
    for (i = 0; i < KEY_LEN; i++) {
        unsigned char cur = key_addr[i];
        if (cur == shadow[i])
            continue;
        if (cur == 0) {
            zeroed[i] = 1;
        } else if (!wipe_confirmed && !wipe_violated) {
            wipe_violated = 1;
            trace_str("WIPE violated off=");
            trace_u64((unsigned long long)i);
            trace_str("\n");
        }
        shadow[i] = cur;
    }

    /* The faulting store itself targeted this byte. */
    if (pending_off >= 0 && pending_off < KEY_LEN) {
        if (key_addr[pending_off] == 0) {
            zeroed[pending_off] = 1;
        } else if (!wipe_confirmed && !wipe_violated) {
            wipe_violated = 1;
            trace_str("WIPE violated off=");
            trace_u64((unsigned long long)pending_off);
            trace_str("\n");
        }
    }
    pending_off = -1;

    if (wipe_violated) {
        /* The buffer slot was reused before the wipe covered it; there
         * is nothing more to watch. */
        disarm_watch();
        return;
    }

    if (!wipe_confirmed) {
        all = 1;
        for (i = 0; i < KEY_LEN; i++)
            if (!zeroed[i])
                all = 0;
        if (all) {
            wipe_confirmed = 1;
            trace_str("WIPE confirmed zeroed=32\n");
            /* The wipe ran; the buffer's lifetime is over, so later
             * frame-reuse stores into the slot no longer matter. */
            disarm_watch();
        }
    }
}

static void on_segv(int sig, siginfo_t *info, void *uctx)
{
    uintptr_t a = (uintptr_t)info->si_addr;

    if (watch_armed && a >= watch_lo && a < watch_lo + watch_len) {
        ucontext_t *uc = (ucontext_t *)uctx;
        pending_off = (a >= (uintptr_t)key_addr &&
                       a < (uintptr_t)key_addr + KEY_LEN)
                          ? (int)(a - (uintptr_t)key_addr)
                          : -1;
#if defined(__x86_64__)
        /* Trap flag: the faulting store re-runs, then SIGTRAP fires. */
        uc->uc_mcontext.gregs[REG_EFL] |= 0x100UL;
#else
        /* No user-settable single-step flag on aarch64: plant a BRK right
         * after the faulting instruction. Only stores fault on a
         * read-only page, and a store never branches, so execution
         * always continues at pc+4. */
        uint64_t pc = uc->uc_mcontext.pc;
        patch_site = (void *)(pc + 4);
        patch_page = (uintptr_t)patch_site & ~(uintptr_t)(page_size - 1);
        saved_insn = *(uint32_t *)patch_site;
        mprotect((void *)patch_page, page_size,
                 PROT_READ | PROT_WRITE | PROT_EXEC);
        *(uint32_t *)patch_site = 0xD4200000u; /* BRK #0 */
        __builtin___clear_cache((char *)patch_site, (char *)patch_site + 4);
        mprotect((void *)patch_page, page_size, PROT_READ | PROT_EXEC);
#endif
        mprotect((void *)watch_lo, watch_len, PROT_READ | PROT_WRITE);
        stepped = 1;
        return;
    }

    /* Not one of our watch faults: chain to the handler that was
     * installed before us (the Rust runtime's stack-overflow guard). */
    if ((saved_segv.sa_flags & SA_SIGINFO) && saved_segv.sa_sigaction) {
        saved_segv.sa_sigaction(sig, info, uctx);
        return;
    }
    if (!(saved_segv.sa_flags & SA_SIGINFO) && saved_segv.sa_handler > SIG_IGN) {
        saved_segv.sa_handler(sig);
        return;
    }
    signal(SIGSEGV, SIG_DFL);
    raise(SIGSEGV);
}

static void on_trap(int sig, siginfo_t *info, void *uctx)
{
    ucontext_t *uc = (ucontext_t *)uctx;
    (void)sig;
    (void)info;

    if (stepped) {
        stepped = 0;
#if defined(__x86_64__)
        uc->uc_mcontext.gregs[REG_EFL] &= ~0x100UL;
#else
        /* Restore the instruction the BRK replaced. The kernel reports
         * the BRK's own address as the faulting pc, so returning with pc
         * unchanged re-executes the restored instruction. */
        mprotect((void *)patch_page, page_size,
                 PROT_READ | PROT_WRITE | PROT_EXEC);
        *(uint32_t *)patch_site = saved_insn;
        __builtin___clear_cache((char *)patch_site, (char *)patch_site + 4);
        mprotect((void *)patch_page, page_size, PROT_READ | PROT_EXEC);
        if ((void *)uc->uc_mcontext.pc != patch_site)
            uc->uc_mcontext.pc = (uint64_t)(uintptr_t)patch_site;
#endif
        mprotect((void *)watch_lo, watch_len, PROT_READ);
        check_wipe_state();
        return;
    }

    /* Not our step: die the way an unhandled SIGTRAP would. */
    signal(SIGTRAP, SIG_DFL);
    raise(SIGTRAP);
}

static void arm_watch(void)
{
    struct sigaction sa;
    stack_t ss;
    uintptr_t end;

    if (watch_armed || wipe_confirmed || wipe_violated)
        return;

    /* The buffer has not been stored to since the fill (the save only
     * reads it), so this snapshot is exactly what the source delivered. */
    memcpy(shadow, key_addr, KEY_LEN);

    watch_lo = (uintptr_t)key_addr & ~(uintptr_t)(page_size - 1);
    end = ((uintptr_t)key_addr + KEY_LEN + page_size - 1) &
          ~(uintptr_t)(page_size - 1);
    watch_len = end - watch_lo;

    /* Signal frames must never land on the watched pages: a fault taken
     * while the stack pointer sits inside them could not be delivered
     * otherwise. */
    ss.ss_sp = altstack_base;
    ss.ss_flags = 0;
    ss.ss_size = ALTSTACK_SIZE;
    if (altstack_base)
        sigaltstack(&ss, NULL);

    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = on_segv;
    sa.sa_flags = SA_SIGINFO | SA_ONSTACK;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGSEGV, &sa, &saved_segv);

    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = on_trap;
    sa.sa_flags = SA_SIGINFO | SA_ONSTACK;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGTRAP, &sa, NULL);

    if (mprotect((void *)watch_lo, watch_len, PROT_READ) != 0) {
        trace_str("WATCH unavailable errno=mprotect\n");
        return;
    }
    watch_armed = 1;
    trace_str("WATCH armed pages=");
    trace_u64(watch_len / page_size);
    trace_str("\n");
}

/* The key fill is done (32 bytes delivered) or has failed mid-way: record
 * what the buffer received. When the fill failed no file is ever created,
 * so the wipe follows at once and the watch arms immediately; otherwise
 * arming waits for the end of the save (see note_key_open / interposed
 * close). Runs inside the SIGSYS handler, before the guest sees the last
 * getrandom result. */
static void finish_key_fill(int failed)
{
    int i;

    trace_str("KEYFILL n=");
    trace_u64((unsigned long long)key_delivered);
    trace_str(" hex=");
    for (i = 0; i < key_delivered; i++)
        trace_hex8(key_addr[i]);
    trace_str("\n");

    fill_done = 1;
    if (failed)
        arm_watch();
}

/* ----- the emulated getrandom ----- */

static void trace_getrandom(size_t req, long ret)
{
    char line[64];
    size_t n = 0;
    const char *p;

    if (trace_fd < 0)
        return;
    for (p = "GETRANDOM req="; *p; p++)
        line[n++] = *p;
    n += fmt_u64(line + n, (unsigned long long)req);
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

static void on_sigsys(int sig, siginfo_t *info, void *uctx)
{
    ucontext_t *uc;
    unsigned char *buf;
    size_t len, allow, i;
    long ret;
    int is_key_call;

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

    /* keygen draws the whole key with one full-length (32-byte) request;
     * the first such request identifies the key buffer. Later calls into
     * the same 32-byte region are the rest of the same fill (short reads
     * or a source that dries up mid-key). */
    if (!have_key_addr && len == KEY_LEN) {
        key_addr = buf;
        have_key_addr = 1;
        key_delivered = 0;
    }
    is_key_call = have_key_addr && buf >= key_addr && buf < key_addr + KEY_LEN;

    allow = len;
    if (cfg_mode == MODE_FAIL_AFTER && is_key_call) {
        long remaining = cfg_arg - key_delivered;
        if (remaining <= 0) {
            /* The source dried up mid-key: fail without delivering. */
            trace_getrandom(len, -1);
            ret = -EIO;
            goto done;
        }
        if (allow > (size_t)remaining)
            allow = (size_t)remaining;
    }

    ret = 0;
    if (cfg_mode == MODE_REAL) {
        size_t done = 0;
        while (done < allow) {
            ssize_t r = read(urandom_fd, buf + done, allow - done);
            if (r <= 0) {
                ret = -EIO;
                break;
            }
            done += (size_t)r;
        }
        if (ret == 0)
            ret = (long)done;
    } else {
        for (i = 0; i < allow; i++)
            buf[i] = pattern_byte(draw_index + i);
        draw_index += allow;
        ret = (long)allow;
    }

    if (is_key_call && ret > 0)
        key_delivered += (int)ret;

    trace_getrandom(len, ret);

done:
    if (is_key_call && (key_delivered == KEY_LEN || ret < 0))
        finish_key_fill(ret < 0);
    /* The syscall was never executed; the value left in the return
     * register of the signal frame is what the guest sees. */
#if defined(__x86_64__)
    uc->uc_mcontext.gregs[REG_RAX] = ret;
#else
    uc->uc_mcontext.regs[0] = (unsigned long long)(long long)ret;
#endif
}

/* ----- tracking the key file's descriptor to time the watch ----- */

/* The first O_CREAT open after the key fill is the key file. Its close
 * ends the save on every path that creates the file (success, and the
 * abort-save cleanup after a permission/write/sync/close-stage failure),
 * and the wipe runs right after -- so the watch arms at the close. If
 * the create itself fails, the wipe follows on the error path and the
 * watch arms immediately. */

typedef int (*open_fn)(const char *, int, ...);
static open_fn real_open = 0;
static open_fn real_open64 = 0;

static void note_key_open(int fd, int flags)
{
    if (!active || !fill_done || key_fd != -2 || !(flags & O_CREAT))
        return;
    if (fd >= 0) {
        key_fd = fd;
    } else {
        key_fd = -1;
        arm_watch();
    }
}

int open(const char *path, int flags, ...)
{
    mode_t mode = 0;
    va_list ap;
    int fd;
    if (!real_open)
        real_open = (open_fn)dlsym(RTLD_NEXT, "open");
    if (flags & O_CREAT) {
        va_start(ap, flags);
        mode = va_arg(ap, mode_t);
        va_end(ap);
    }
    fd = real_open(path, flags, mode);
    note_key_open(fd, flags);
    return fd;
}

int open64(const char *path, int flags, ...)
{
    mode_t mode = 0;
    va_list ap;
    int fd;
    if (!real_open64)
        real_open64 = (open_fn)dlsym(RTLD_NEXT, "open64");
    if (flags & O_CREAT) {
        va_start(ap, flags);
        mode = va_arg(ap, mode_t);
        va_end(ap);
    }
    fd = real_open64(path, flags, mode);
    note_key_open(fd, flags);
    return fd;
}

typedef int (*close_fn)(int);
static close_fn real_close = 0;

int close(int fd)
{
    if (active && fill_done && key_fd >= 0 && fd == key_fd) {
        key_fd = -1; /* once: the descriptor number may be reused */
        arm_watch();
    }
    if (!real_close)
        real_close = (close_fn)dlsym(RTLD_NEXT, "close");
    return real_close(fd);
}

/* ----- syscalls that write results into the watched stack pages ----- */

/* The kernel returns EFAULT instead of signalling when a syscall's output
 * buffer sits in a read-only page, and keygen fstats the key file with the
 * stat struct on the stack. Lift the watch protection just for the real
 * call; no guest instruction runs meanwhile, so the key buffer cannot be
 * stored to unobserved through this window. */

static int overlaps_watch(const void *p, size_t n)
{
    uintptr_t a = (uintptr_t)p;
    uintptr_t b = a + n;
    return watch_armed && a < watch_lo + watch_len && b > watch_lo;
}

typedef int (*fstat_fn)(int, struct stat *);
static fstat_fn real_fstat = 0;

int fstat(int fd, struct stat *st)
{
    int r;
    if (!real_fstat)
        real_fstat = (fstat_fn)dlsym(RTLD_NEXT, "fstat");
    if (!overlaps_watch(st, sizeof(*st)))
        return real_fstat(fd, st);
    mprotect((void *)watch_lo, watch_len, PROT_READ | PROT_WRITE);
    r = real_fstat(fd, st);
    mprotect((void *)watch_lo, watch_len, PROT_READ);
    return r;
}

#ifdef __USE_LARGEFILE64
typedef int (*fstat64_fn)(int, struct stat64 *);
static fstat64_fn real_fstat64 = 0;

int fstat64(int fd, struct stat64 *st)
{
    int r;
    if (!real_fstat64)
        real_fstat64 = (fstat64_fn)dlsym(RTLD_NEXT, "fstat64");
    if (!overlaps_watch(st, sizeof(*st)))
        return real_fstat64(fd, st);
    mprotect((void *)watch_lo, watch_len, PROT_READ | PROT_WRITE);
    r = real_fstat64(fd, st);
    mprotect((void *)watch_lo, watch_len, PROT_READ);
    return r;
}
#endif

/* ----- first output after the key fill (ordering evidence) ----- */

typedef ssize_t (*write_fn)(int, const void *, size_t);
static write_fn real_write = 0;

ssize_t write(int fd, const void *buf, size_t count)
{
    if (!real_write)
        real_write = (write_fn)dlsym(RTLD_NEXT, "write");
    if (active && have_key_addr && !output_seen && (fd == 1 || fd == 2)) {
        output_seen = 1;
        trace_str("OUTPUT fd=");
        trace_u64((unsigned long long)fd);
        trace_str("\n");
    }
    return real_write(fd, buf, count);
}

/* ----- final verdict ----- */

__attribute__((destructor))
static void keywipe_fini(void)
{
    int i, nonzero;

    if (!active || !have_key_addr)
        return;
    if (wipe_confirmed) {
        trace_str("WIPE final confirmed\n");
        return;
    }
    if (wipe_violated) {
        trace_str("WIPE final violated\n");
        return;
    }
    /* The watch was still armed at exit: the buffer was never fully
     * zeroed. Whatever it still holds (the key itself, or frame-reuse
     * residue where the key used to be), the wipe never ran to
     * completion before the operation returned. */
    nonzero = 0;
    for (i = 0; i < KEY_LEN; i++)
        if (key_addr[i] != 0)
            nonzero++;
    trace_str("WIPE missing nonzero=");
    trace_u64((unsigned long long)nonzero);
    trace_str("\n");
}

/* ----- constructor: arm the getrandom trap when a mode is configured ----- */

__attribute__((constructor))
static void keywipe_init(void)
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

    if (getenv("WRAPFILE_TEST_KEYWIPE_PATTERN")) {
        cfg_mode = MODE_PATTERN;
    } else if ((v = getenv("WRAPFILE_TEST_KEYWIPE_FAIL_AFTER")) && v[0]) {
        cfg_mode = MODE_FAIL_AFTER;
        cfg_arg = strtol(v, NULL, 10);
    } else if (getenv("WRAPFILE_TEST_KEYWIPE")) {
        cfg_mode = MODE_REAL;
    } else {
        return;
    }
    active = 1;

    page_size = (unsigned long)sysconf(_SC_PAGESIZE);
    if (page_size == 0)
        page_size = 4096;

    /*
     * Raw openat(AT_FDCWD, ...) rather than open(): when another shim
     * (permfail) is preloaded alongside, its open() interposition would
     * otherwise track these descriptor opens as guest file creations.
     */
    if ((v = getenv("WRAPFILE_TEST_TRACE_FILE")) && v[0])
        trace_fd = (int)syscall(SYS_openat, AT_FDCWD, v,
                                O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0600);

    urandom_fd = (int)syscall(SYS_openat, AT_FDCWD, "/dev/urandom",
                              O_RDONLY | O_CLOEXEC, 0);

    altstack_base = mmap(NULL, ALTSTACK_SIZE, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (altstack_base == MAP_FAILED)
        altstack_base = 0;

    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = on_sigsys;
    sa.sa_flags = SA_SIGINFO | SA_ONSTACK;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGSYS, &sa, NULL) != 0) {
        trace_str("TRAP unavailable errno=SIGACTION\n");
        active = 0;
        return;
    }

    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 ||
        prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog) != 0) {
        /* No seccomp on this kernel: leave the real random source alone
         * and let the tests skip themselves. */
        trace_str("TRAP unavailable errno=SECCOMP\n");
        active = 0;
        return;
    }

    trace_str("TRAP armed\n");
}
