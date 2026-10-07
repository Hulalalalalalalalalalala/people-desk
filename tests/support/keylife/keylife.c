/*
 * keylife.c -- external ptrace observer for the keygen temporary-key
 * lifecycle regression tests.
 *
 * It is NOT an LD_PRELOAD shim and changes nothing in the observed program:
 * the test harness forks, the child PTRACE_TRACEME's itself, redirects its
 * stdout/stderr to capture files, and execs the real wrapfile binary; this
 * program watches from outside. That lets a regression test prove
 * properties no black-box observation can -- deleting the output file,
 * keeping the key out of stdout/stderr, and exiting 0/1 are all compatible
 * with the 32 secret bytes never being wiped from the stack (the whole
 * process image vanishes on exit either way, and a stack frame may be
 * reused after the function returns). The memory holding the delivered
 * bytes has to be inspected *while the operation is still in flight*.
 *
 * What is observed for one `wrapfile keygen <path>` run
 * ----------------------------------------------------
 * The key draw (getrandom(2); the getrandom crate issues it with inline
 * assembly, and under the tests the randtrap shim emulates it with a
 * SECCOMP_RET_TRAP SIGSYS handler), the key-file open/write/fsync/close
 * sequence, the user-space instruction at which the 32-byte buffer is
 * overwritten, and the first stdout/stderr write after that:
 *
 *   1. The 32-byte draw really is delivered into one buffer (address P):
 *      after the fill completes, [P, P+32) holds 32 *non-zero* bytes (the
 *      deterministic randtrap pattern, 0x80.. by draw index). A wipe that
 *      "succeeded" on an all-zero buffer would prove nothing.
 *   2. On the save path the same bytes are still there every time the key
 *      bytes are offered to write(2) on the key fd -- zeroing cannot have
 *      happened before/while saving (which would turn the saved file into
 *      an all-zero or truncated key) -- and they are still there when
 *      close(2) on the key fd returns on the success path.
 *   3. After the watch arm point the observer single-steps the guest and
 *      requires the whole 32-byte buffer to become zero within a bounded
 *      number of user instructions -- a tight loop right there, not a later
 *      frame reuse -- and before the program performs its first write(2) to
 *      stdout or stderr. A missing wipe, a wipe the optimizer deleted (the
 *      guest is inspected under both debug and release builds), a wipe
 *      covering only part of the buffer, and a wipe placed before the save
 *      are all FAILs.
 *
 * Watch arm points: success  -> the key fd's close(2) exit;
 *                   randfail -> the emulated getrandom error visible after
 *                               SIGSYS handler rt_sigreturn;
 *                   writefail-> the key fd write(2) exit carrying EIO;
 *                   closefail-> the key fd's close(2) exit. The permfail
 *                               shim reports the close error in user space
 *                               after really releasing the descriptor, so
 *                               the close syscall itself succeeds; the
 *                               failed save then reaches abort_save, whose
 *                               unlinkat(2) of this run's file is observed
 *                               while stepping and must still see the full
 *                               key (when the shim also refuses the unlink
 *                               in user space no unlinkat syscall happens,
 *                               so that event is simply absent).
 *                   chmodfail-> the permfail shim rejects fchmod(0600) in
 *                               user space (EPERM, no syscall); the abort
 *                               cleanup's unlinkat(2) is the first
 *                               observable syscall afterwards and must
 *                               still see the full key, as must the abort's
 *                               close(2) of the key fd, whose exit arms the
 *                               wipe watch.
 *                   statfail -> the fchmod succeeded but the shim fails the
 *                               confirming fstat in user space (EBADF) after
 *                               really performing it; from there on the
 *                               sequence is identical to chmodfail.
 *
 * chmodfail/statfail never reach a key write, so the key fd cannot be
 * identified by the content of a write(2): it is the descriptor the guest
 * itself creates with openat(O_CREAT|O_EXCL) after the fill completed (the
 * shims open their trace file without O_EXCL, and the runtime's own opens
 * all precede the fill, so this identifies exactly the key file).
 *
 * Partial-draw failure needs one extra trick. When the random source dries
 * up after `held` (< 32) bytes, the remaining 32-held slots have never been
 * written by the source and stay zero from the array's zero initialization,
 * so inspecting buffer contents alone cannot tell a full-buffer wipe from a
 * wipe covering only the delivered prefix. Immediately after observing the
 * emulated getrandom failure (while the guest is stopped), the observer
 * writes a non-zero sentinel pattern into [P+held, P+32) -- memory the
 * product never reads on the error path (getrandom already returned Err and
 * the command only returns from there). The wipe must erase the sentinel
 * too.
 *
 * Output (stdout) is one `KEY=VALUE` event per line for the Rust harness:
 *
 *   EVENT name=FILL_COMPLETE held=32 secret=<hex>
 *   EVENT name=WRITE_BORROWS fd=N held=32
 *   EVENT name=CLOSE_STILL_HELD fd=N held=32
 *   EVENT name=WRITE_FAILED held=32
 *   EVENT name=ABORT_UNLINK fd=N held=32
 *   EVENT name=ABORT_CLOSE_STILL_HELD fd=N held=32
 *   EVENT name=RANDOM_FAILED held=N sentinel=M
 *   EVENT name=WIPED held=N sentinel=M steps=K
 *   EVENT name=OUTPUT_AFTER_WIPE fd=N
 *   EVENT name=GUEST_EXIT rc=N
 *   FAIL reason=...
 *
 * Exit status: 0 = every property for the requested mode held; 1 = a
 * regression was observed (a FAIL line was printed); 2 = usage/environment
 * error. The guest's stdout/stderr go to the capture files named on the
 * command line, so the event stream on this program's stdout stays
 * parseable.
 *
 * Linux ptrace semantics relied on:
 *   - PTRACE_TRACEME from the forked child before exec;
 *   - PTRACE_GET_SYSCALL_INFO tells syscall-entry/exit stops apart even
 *     with a seccomp-trap SIGSYS interleaved (a trapped syscall has no exit
 *     stop: ENTRY -> SIGSYS delivery -> handler -> rt_sigreturn, after which
 *     the signal frame holds the emulated return value and PC);
 *   - PTRACE_SINGLESTEP runs one user instruction. On arm64 stepping over
 *     an svc first re-reports the syscall-entry stop (registers hold the
 *     call arguments, the syscall has not run) and then traps at the
 *     instruction following the svc with the return value in x0; on x86_64
 *     the single-step trap whose RIP is the syscall instruction is the
 *     before-execution observation point. Both are checked, so a
 *     stdout/stderr write is caught *before* it leaves the process.
 *
 * Supported architectures: x86_64 and aarch64 (the same set as the randtrap
 * shim). Compilation refuses anything else.
 */

#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ptrace.h>
#include <linux/ptrace.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef PTRACE_GET_SYSCALL_INFO
#define PTRACE_GET_SYSCALL_INFO 0x420e
#endif
#ifndef PTRACE_SYSCALL_INFO_NONE
#define PTRACE_SYSCALL_INFO_NONE 0
#define PTRACE_SYSCALL_INFO_ENTRY 1
#define PTRACE_SYSCALL_INFO_EXIT 2
#define PTRACE_SYSCALL_INFO_SECCOMP 3
#endif

#if defined(__aarch64__)
#include <asm/ptrace.h>
typedef struct user_pt_regs gregs_t;
#define GREG_NT NT_PRSTATUS
static long greg_nr(const gregs_t *g) { return (long)g->regs[8]; }
static long greg_arg(const gregs_t *g, int i) { return (long)g->regs[i]; }
static long greg_ret(const gregs_t *g) { return (long)g->regs[0]; }
static unsigned long greg_pc(const gregs_t *g) { return g->pc; }
/* svc #imm: 1101 0100 000x xxxx xxxx xxxx xx00 0001 */
static int is_syscall_insn_word(uint32_t insn)
{
    return (insn & 0xffe0001fu) == 0xd4000001u;
}
#elif defined(__x86_64__)
#include <sys/user.h>
typedef struct user_regs_struct gregs_t;
#define GREG_NT NT_PRSTATUS
static long greg_nr(const gregs_t *g) { return (long)g->orig_rax; }
static long greg_arg(const gregs_t *g, int i)
{
    switch (i) {
    case 0: return (long)g->rdi;
    case 1: return (long)g->rsi;
    case 2: return (long)g->rdx;
    case 3: return (long)g->r10;
    case 4: return (long)g->r8;
    default: return (long)g->r9;
    }
}
static long greg_ret(const gregs_t *g) { return (long)g->rax; }
static unsigned long greg_pc(const gregs_t *g) { return g->rip; }
static int is_syscall_insn_word(uint32_t insn)
{
    /* Little-endian first two bytes of `syscall` (0f 05). */
    return (insn & 0xffffu) == 0x050fu;
}
#else
#error "keylife supports x86_64 and aarch64 only"
#endif

/* write/writev/openat/close/getrandom/exit_group/rt_sigreturn numbers come
 * from <sys/syscall.h>, where they are per-architecture (e.g. exit_group is
 * 94 on arm64 and 231 on x86_64). */
#ifndef SYS_rt_sigreturn
#error "SYS_rt_sigreturn not defined for this architecture"
#endif
#define SYS_rt_sigreturn_local SYS_rt_sigreturn

/* glibc's unlink() is unlinkat(AT_FDCWD, ...) on both supported arches. */
#ifndef SYS_unlinkat
#error "SYS_unlinkat not defined for this architecture"
#endif

#define KEYN 32

/* Upper bound on user instructions between the watch arm point and the
 * buffer being fully zero. The real wipe is a few hundred instructions at
 * -O0 and fewer at -O2; the bound only turns a missing wipe into a fast
 * FAIL instead of single-stepping all the way to process exit. */
#define WIPE_STEP_BUDGET 2000000L

enum mode { MODE_SUCCESS, MODE_RANDFAIL, MODE_WRITEFAIL, MODE_CLOSEFAIL,
            MODE_CHMODFAIL, MODE_STATFAIL };

struct observer {
    pid_t pid;
    enum mode mode;

    uint64_t p;                 /* address of the guest's 32-byte buffer */
    int phase;                  /* 0 pre-fill, 1 filling, 2 filled,
                                   4 wipe watch armed, 5 wipe observed */
    int held;                   /* pattern prefix delivered so far */
    int sentinel;               /* sentinel bytes planted in undelivered tail */
    uint8_t secret[KEYN];       /* observed full key (once filled) */
    int keyfd;                  /* fd of the key file, or -1 */
    int abort_unlink_seen;      /* abort_save's unlink after a failed save */
    int output_after_wipe;
    int fill_req32_seen;
    int wipe_reported;
    long wipe_steps;
    int failed;
    char fail_reason[256];

    /* pending syscall-entry state */
    int entry_valid;
    long entry_nr;
    uint64_t entry_a0, entry_a1, entry_a2;
};

static void die_usage(void)
{
    fprintf(stderr,
        "usage: keylife <success|randfail|writefail|closefail|chmodfail|"
        "statfail> <stdout-cap> <stderr-cap> -- <guest> [args...]\n");
    exit(2);
}

static void set_fail(struct observer *o, const char *fmt, ...)
{
    if (o->failed)
        return;
    va_list ap;
    va_start(ap, fmt);
    o->failed = 1;
    vsnprintf(o->fail_reason, sizeof o->fail_reason, fmt, ap);
    va_end(ap);
    printf("FAIL reason=%s\n", o->fail_reason);
    fflush(stdout);
}

/* ---- remote memory ---- */

static int remote_read(struct observer *o, uint64_t addr, void *buf, size_t n)
{
    struct iovec local = { buf, n };
    struct iovec remote = { (void *)addr, n };
    return process_vm_readv(o->pid, &local, 1, &remote, 1, 0) == (ssize_t)n
               ? 0
               : -1;
}

static int remote_write(struct observer *o, uint64_t addr, const void *buf,
                        size_t n)
{
    struct iovec local = { (void *)buf, n };
    struct iovec remote = { (void *)addr, n };
    return process_vm_writev(o->pid, &local, 1, &remote, 1, 0) == (ssize_t)n
               ? 0
               : -1;
}

static void get_gregs(struct observer *o, gregs_t *g)
{
    struct iovec iov = { g, sizeof *g };
    if (ptrace(PTRACE_GETREGSET, o->pid, (void *)GREG_NT, &iov) < 0)
        set_fail(o, "PTRACE_GETREGSET: %s", strerror(errno));
}

static int read_keybuf(struct observer *o, uint8_t out[KEYN])
{
    memset(out, 0, KEYN);
    if (!o->p)
        return -1;
    return remote_read(o, o->p, out, KEYN);
}

/* ---- the randtrap delivery pattern ---- */

static uint8_t pattern_next(uint8_t b)
{
    return (uint8_t)(0x80 + (((unsigned)b - 0x80u + 1u) % 64u));
}

/* Length of the contiguous non-zero pattern prefix the random source left:
 * starts with a 0x80..0xBF byte, every next byte advances by one modulo
 * the pattern period, and an undelivered slot is still zero. */
static int pattern_prefix(const uint8_t *buf)
{
    int i = 0;
    if (buf[0] < 0x80 || buf[0] >= 0xc0)
        return 0;
    while (i < KEYN && buf[i] != 0) {
        if (i > 0 && buf[i] != pattern_next(buf[i - 1]))
            break;
        i++;
    }
    return i;
}

static int all_zero(const uint8_t *buf, int n)
{
    for (int i = 0; i < n; i++)
        if (buf[i])
            return 0;
    return 1;
}

static void print_hex(const uint8_t *buf, int n)
{
    for (int i = 0; i < n; i++)
        printf("%02x", buf[i]);
}

/* [P, P+32) must still equal the observed full key while the save borrows
 * it: a premature or partial wipe shows up here. */
static void require_still_holds_full_key(struct observer *o, const char *what,
                                         long fd)
{
    uint8_t cur[KEYN];
    if (read_keybuf(o, cur) != 0) {
        set_fail(o, "%s: cannot read key buffer", what);
        return;
    }
    if (memcmp(cur, o->secret, KEYN) != 0) {
        set_fail(o, "%s fd=%ld: key buffer changed before the save finished",
                 what, fd);
        return;
    }
    printf("EVENT name=%s fd=%ld held=%d\n", what, fd, KEYN);
    fflush(stdout);
}

/* Classify the buffer after a getrandom completion of the key fill. */
static void refresh_fill(struct observer *o, long ret)
{
    uint8_t cur[KEYN];
    if (read_keybuf(o, cur) != 0) {
        set_fail(o, "cannot read key buffer after getrandom");
        return;
    }
    int pre = pattern_prefix(cur);

    if (ret < 0) {
        if (o->mode != MODE_RANDFAIL) {
            set_fail(o,
                     "unexpected getrandom failure (ret=%ld) in mode %d",
                     ret, (int)o->mode);
            return;
        }
        if (pre <= 0 || pre >= KEYN) {
            set_fail(o,
                     "randfail scenario expected a partial delivery "
                     "(1..31 bytes), got prefix=%d ret=%ld",
                     pre, ret);
            return;
        }
        /* Plant a non-zero sentinel in the slots the source never
         * delivered: the product cannot read them on this error path, and
         * a full-buffer wipe must erase them as well. Bytes stay outside
         * the 0x80..0xBF pattern band so they cannot be mistaken for
         * random material. */
        uint8_t sent[KEYN];
        int m = KEYN - pre;
        for (int i = 0; i < m; i++)
            sent[i] =
                (uint8_t)(0xd0 + ((unsigned)(i * 5 + pre + 1) & 0x0f));
        if (remote_write(o, o->p + (uint64_t)pre, sent, (size_t)m) != 0) {
            set_fail(o, "cannot plant sentinel in undelivered tail");
            return;
        }
        o->held = pre;
        o->sentinel = m;
        memcpy(o->secret, cur, KEYN);
        o->phase = 4;
        o->wipe_steps = 0;
        printf("EVENT name=RANDOM_FAILED held=%d sentinel=%d\n", pre, m);
        fflush(stdout);
        return;
    }

    o->held = pre;
    if (pre == KEYN) {
        memcpy(o->secret, cur, KEYN);
        o->phase = 2;
        printf("EVENT name=FILL_COMPLETE held=32 secret=");
        print_hex(cur, KEYN);
        putchar('\n');
        fflush(stdout);
    } else {
        o->phase = 1;
    }
}

/* A completion of a getrandom that belongs to keygen's fill. `ret` is what
 * the guest observes (kernel return or the emulated SIGSYS value). */
static void on_getrandom_done(struct observer *o, long ret)
{
    if (!o->fill_req32_seen || o->phase >= 2 || o->phase == 4)
        return;
    refresh_fill(o, ret);
}

/* ---- syscall-entry/exit handling (normal syscall-stop mode) ---- */

static void on_syscall_entry(struct observer *o,
                             const struct ptrace_syscall_info *si)
{
    long nr = (long)si->entry.nr;
    o->entry_valid = 1;
    o->entry_nr = nr;
    o->entry_a0 = (uint64_t)si->entry.args[0];
    o->entry_a1 = (uint64_t)si->entry.args[1];
    o->entry_a2 = (uint64_t)si->entry.args[2];

    if (nr == SYS_getrandom) {
        unsigned long len = (unsigned long)si->entry.args[1];
        /* The 32-byte request identifies keygen's own draw: the crate's
         * zero-length probe and the runtime startup draws use other
         * lengths, and the key fill is the process's first/final 32-byte
         * run. Once the key is filled a later unrelated 32-byte request
         * must never re-point P at a different buffer. */
        if (len == KEYN && !o->fill_req32_seen && o->phase < 2) {
            o->p = (uint64_t)si->entry.args[0];
            o->fill_req32_seen = 1;
            o->phase = 1;
            o->held = 0;
            o->sentinel = 0;
            o->wipe_reported = 0;
        }
        return;
    }

    if (nr == SYS_write || nr == SYS_writev) {
        long fd = (long)si->entry.args[0];
        /* After the wipe the first stdout/stderr write confirms ordering. */
        if (o->phase == 5 && (fd == 1 || fd == 2)) {
            if (!o->output_after_wipe) {
                o->output_after_wipe = 1;
                printf("EVENT name=OUTPUT_AFTER_WIPE fd=%ld\n", fd);
                fflush(stdout);
            }
        }

        /* Identify the key fd by *content*, not by "the first openat after
         * the fill": the product saves with plain write(2) of the key
         * buffer, so the first write whose offered bytes equal the key's
         * prefix can only target the key file. This stays correct even if
         * the runtime opens/writes something else between the fill and the
         * key file. Short writes advance the pointer, so only the first
         * offer (offset 0) is used for identification. */
        if (o->phase == 2 && o->keyfd < 0 &&
            !(fd == 1 || fd == 2)) {
            uint8_t offered[KEYN];
            size_t peek = (size_t)si->entry.args[2];
            if (peek > KEYN)
                peek = KEYN;
            if (peek >= 4 &&
                remote_read(o, (uint64_t)si->entry.args[1], offered,
                            peek) == 0 &&
                memcmp(offered, o->secret, peek) == 0) {
                o->keyfd = (int)fd;
            }
        }

        /* Every offer of key bytes to the key fd must borrow the still-live
         * full key (checked at the buffer's own address, not the offer
         * pointer, so short writes with advancing pointers are covered). */
        if (o->phase >= 2 && o->keyfd >= 0 && fd == (long)o->keyfd)
            require_still_holds_full_key(o, "WRITE_BORROWS", fd);
    }

    if (nr == SYS_unlinkat) {
        /* writefail mode: the failed save (the permfail shim makes the libc
         * write() return EIO without a syscall, so the failure itself is
         * not a syscall-exit event) reaches abort_save, whose first step is
         * unlinking this invocation's file. At that moment the complete key
         * must still be in the buffer: the wipe is only allowed afterwards,
         * on the way out of the operation. */
        if (o->mode == MODE_WRITEFAIL && o->phase == 2 &&
            o->keyfd >= 0 && !o->abort_unlink_seen) {
            require_still_holds_full_key(o, "WRITE_FAILED", (long)o->keyfd);
            if (!o->failed) {
                o->abort_unlink_seen = 1;
                printf("EVENT name=ABORT_UNLINK fd=%d held=%d\n", o->keyfd,
                       KEYN);
                fflush(stdout);
            }
        }
        /* chmodfail/statfail: the permission failure is reported by the
         * permfail shim in user space, so abort_save's unlinkat is the
         * first observable syscall of the abort. No key byte was ever
         * written, yet the full key is already in the buffer and must
         * still be there now -- the wipe is only allowed afterwards. */
        if ((o->mode == MODE_CHMODFAIL || o->mode == MODE_STATFAIL) &&
            o->phase == 2 && o->keyfd >= 0 && !o->abort_unlink_seen) {
            require_still_holds_full_key(o, "ABORT_UNLINK", (long)o->keyfd);
            if (!o->failed)
                o->abort_unlink_seen = 1;
        }
    }
}

static void on_syscall_exit(struct observer *o,
                            const struct ptrace_syscall_info *si)
{
    long rval = (long)si->exit.rval;
    int iserr = si->exit.is_error;

    if (!o->entry_valid)
        return;

    switch (o->entry_nr) {
    case SYS_getrandom:
        on_getrandom_done(o, rval);
        break;

    case SYS_openat:
        /* chmodfail/statfail: the save fails before any key byte is
         * written, so the key fd cannot be recognized by the content of a
         * write. It is instead the descriptor the guest itself creates with
         * O_CREAT|O_EXCL after the fill completed -- the shims open their
         * trace file without O_EXCL, and the runtime's own opens all
         * precede the fill, so this identifies exactly the key file. */
        if ((o->mode == MODE_CHMODFAIL || o->mode == MODE_STATFAIL) &&
            o->phase == 2 && o->keyfd < 0 && !iserr && rval >= 0 &&
            (o->entry_a2 & (uint64_t)(O_CREAT | O_EXCL)) ==
                (uint64_t)(O_CREAT | O_EXCL)) {
            o->keyfd = (int)rval;
        }
        break;

    case SYS_write:
    case SYS_writev:
        /* The write failure itself is injected inside libc by the permfail
         * shim (it returns EIO without issuing the syscall), so it is
         * recognized at abort_save's unlink entry, not here. */
        break;

    case SYS_close:
        if (o->phase == 2 && o->keyfd >= 0 &&
            (long)o->entry_a0 == (long)o->keyfd && !iserr) {
            /* chmodfail/statfail (like writefail) reach this close from
             * abort_save's cleanup after the unlink: the full key must
             * still be present, and the wipe is only allowed afterwards. */
            int abort_close =
                (o->mode == MODE_WRITEFAIL || o->mode == MODE_CHMODFAIL ||
                 o->mode == MODE_STATFAIL) &&
                o->abort_unlink_seen;
            if (o->mode == MODE_SUCCESS || o->mode == MODE_CLOSEFAIL) {
                /* closefail: the close *syscall* succeeds -- the permfail
                 * shim reports the unrecoverable close error in user space
                 * afterwards, so this is also the failed-close arm point. */
                require_still_holds_full_key(o, "CLOSE_STILL_HELD",
                                            (long)o->keyfd);
            } else if (abort_close) {
                /* abort_save unlinked first and now closes the still-open
                 * key fd; the full key must still be present at this point
                 * and wiped only afterwards, on the way out. */
                require_still_holds_full_key(o, "ABORT_CLOSE_STILL_HELD",
                                            (long)o->keyfd);
            }
            if (!o->failed &&
                (o->mode == MODE_SUCCESS || o->mode == MODE_CLOSEFAIL ||
                 abort_close)) {
                o->phase = 4;
                o->held = KEYN;
                o->sentinel = 0;
                o->wipe_steps = 0;
            }
        }
        break;
    }

    o->entry_valid = 0;
}

/* ---- SIGSYS (seccomp-emulated getrandom) handling ---- */

/* The randtrap shim emulates a trapped getrandom inside a SIGSYS handler:
 * delivery stop -> handler runs (its own trace write) -> rt_sigreturn ->
 * the guest resumes after the svc with the emulated value in the return
 * register. Drive that sequence and hand the emulated result to the fill
 * tracker. Returns 0 normally, -1 once the guest is gone or a FAIL was
 * raised. */
static int drive_sigsys(struct observer *o)
{
    int status;
    int delivered = 0;
    for (;;) {
        /* Inject SIGSYS exactly once: re-injecting on every stop would
         * queue additional SIGSYSs behind the handler's blocked signal,
         * and sigreturn unblocking it would re-enter the handler forever. */
        if (ptrace(PTRACE_SYSCALL, o->pid, 0,
                   delivered ? 0 : (void *)(long)SIGSYS) < 0) {
            set_fail(o, "ptrace inject SIGSYS: %s", strerror(errno));
            return -1;
        }
        delivered = 1;
        if (waitpid(o->pid, &status, 0) < 0) {
            set_fail(o, "waitpid in SIGSYS: %s", strerror(errno));
            return -1;
        }
        if (WIFEXITED(status) || WIFSIGNALED(status))
            return -1;
        if (!WIFSTOPPED(status))
            continue;
        int sig = WSTOPSIG(status);
        if (sig == (SIGTRAP | 0x80)) {
            struct ptrace_syscall_info si;
            memset(&si, 0, sizeof si);
            if (ptrace(PTRACE_GET_SYSCALL_INFO, o->pid, sizeof si, &si) < 0)
                continue;
            if (si.op == PTRACE_SYSCALL_INFO_ENTRY &&
                (long)si.entry.nr == SYS_rt_sigreturn_local) {
                /* Run rt_sigreturn; on its exit the restored frame carries
                 * the emulated getrandom result. */
                if (ptrace(PTRACE_SYSCALL, o->pid, 0, 0) < 0)
                    return -1;
                if (waitpid(o->pid, &status, 0) < 0)
                    return -1;
                if (WIFEXITED(status) || WIFSIGNALED(status))
                    return -1;
                gregs_t g;
                get_gregs(o, &g);
                if (o->failed)
                    return -1;
                on_getrandom_done(o, greg_ret(&g));
                return o->failed ? -1 : 0;
            }
        } else if (sig == SIGSYS) {
            /* A second SIGSYS stop before the handler ran (nested trap
             * bookkeeping): it is already being delivered; resume with no
             * new injection. */
            delivered = 1;
            continue;
        }
    }
}

/* ---- wipe watch (single-step mode) ---- */

/* If the guest is stopped immediately before a write/exit_group syscall
 * (entry syscall-stop, or a single-step trap on the syscall instruction),
 * return its number and fd argument; otherwise -1. */
static long pending_syscall(struct observer *o, int sig,
                            const struct ptrace_syscall_info *si,
                            const gregs_t *g, long *fd_out)
{
    if (sig == (SIGTRAP | 0x80) && si->op == PTRACE_SYSCALL_INFO_ENTRY) {
        long nr = (long)si->entry.nr;
        *fd_out = (long)si->entry.args[0];
        return nr;
    }
    if (sig == SIGTRAP) {
        unsigned long pc = greg_pc(g);
        uint32_t insn = 0;
        if (remote_read(o, pc, &insn, sizeof insn) == 0 &&
            is_syscall_insn_word(insn)) {
            long nr = greg_nr(g);
            *fd_out = greg_arg(g, 0);
            return nr;
        }
    }
    return -1;
}

/* One single-step observation. Returns 1 while the watch stays armed. */
static int watch_step(struct observer *o, int sig,
                      const struct ptrace_syscall_info *si, const gregs_t *g)
{
    o->wipe_steps++;
    if (o->wipe_steps > WIPE_STEP_BUDGET) {
        set_fail(o,
                 "key buffer was not zeroed within %ld user instructions",
                 WIPE_STEP_BUDGET);
        return 0;
    }

    uint8_t cur[KEYN];
    if (read_keybuf(o, cur) != 0) {
        set_fail(o, "cannot read key buffer while wipe-watching");
        return 0;
    }

    long fd = -1;
    long nr = pending_syscall(o, sig, si, g, &fd);

    /* closefail: after the failed close the save reaches abort_save, whose
     * first step is unlinking this invocation's file. At that moment the
     * complete key must still be in the buffer: the wipe is only allowed
     * afterwards, on the way out of the operation. (When the shim refuses
     * the unlink in user space no unlinkat syscall happens at all, so this
     * event is simply absent from that run.) */
    if (o->mode == MODE_CLOSEFAIL && nr == SYS_unlinkat &&
        !o->abort_unlink_seen) {
        if (memcmp(cur, o->secret, KEYN) != 0) {
            set_fail(o,
                     "abort unlink: key buffer changed before the save "
                     "finished");
            return 0;
        }
        o->abort_unlink_seen = 1;
        printf("EVENT name=ABORT_UNLINK fd=%d held=%d\n", o->keyfd, KEYN);
        fflush(stdout);
    }

    if ((nr == SYS_write || nr == SYS_writev) && (fd == 1 || fd == 2)) {
        if (!all_zero(cur, KEYN)) {
            set_fail(o,
                     "guest performed fd %ld output before the key buffer "
                     "was zeroed (held=%d sentinel=%d, step %ld)",
                     fd, o->held, o->sentinel, o->wipe_steps);
            return 0;
        }
        if (!o->output_after_wipe) {
            o->output_after_wipe = 1;
            printf("EVENT name=OUTPUT_AFTER_WIPE fd=%ld\n", fd);
            fflush(stdout);
        }
    }
    if (nr == SYS_exit_group && !all_zero(cur, KEYN)) {
        set_fail(o,
                 "guest reached exit_group with the key buffer not zeroed "
                 "(held=%d sentinel=%d)",
                 o->held, o->sentinel);
        return 0;
    }

    if (!o->wipe_reported && all_zero(cur, KEYN)) {
        o->wipe_reported = 1;
        o->phase = 5;
        printf("EVENT name=WIPED held=%d sentinel=%d steps=%ld\n", o->held,
               o->sentinel, o->wipe_steps);
        fflush(stdout);
        return 0;
    }
    return 1;
}

/* ---- child ---- */

static void run_child(char **argv, const char *out_cap, const char *err_cap)
{
    int outfd = open(out_cap, O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (outfd < 0)
        _exit(126);
    int errfd = open(err_cap, O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (errfd < 0)
        _exit(126);
    dup2(outfd, STDOUT_FILENO);
    dup2(errfd, STDERR_FILENO);
    if (outfd > STDERR_FILENO)
        close(outfd);
    if (errfd > STDERR_FILENO)
        close(errfd);

    ptrace(PTRACE_TRACEME, 0, 0, 0);
    raise(SIGSTOP);
    execv(argv[0], argv);
    _exit(127);
}

int main(int argc, char **argv)
{
    if (argc < 6)
        die_usage();
    enum mode mode;
    if (!strcmp(argv[1], "success"))
        mode = MODE_SUCCESS;
    else if (!strcmp(argv[1], "randfail"))
        mode = MODE_RANDFAIL;
    else if (!strcmp(argv[1], "writefail"))
        mode = MODE_WRITEFAIL;
    else if (!strcmp(argv[1], "closefail"))
        mode = MODE_CLOSEFAIL;
    else if (!strcmp(argv[1], "chmodfail"))
        mode = MODE_CHMODFAIL;
    else if (!strcmp(argv[1], "statfail"))
        mode = MODE_STATFAIL;
    else
        die_usage();
    const char *out_cap = argv[2];
    const char *err_cap = argv[3];
    if (strcmp(argv[4], "--") != 0)
        die_usage();
    char **guest_argv = &argv[5];

    struct observer o;
    memset(&o, 0, sizeof o);
    o.mode = mode;
    o.keyfd = -1;

    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        return 2;
    }
    if (pid == 0)
        run_child(guest_argv, out_cap, err_cap);
    o.pid = pid;

    int status;
    if (waitpid(pid, &status, WUNTRACED) < 0) {
        perror("waitpid initial");
        kill(pid, SIGKILL);
        waitpid(pid, &status, 0);
        return 2;
    }
    if (WIFEXITED(status) || WIFSIGNALED(status)) {
        fprintf(stderr, "guest exited before tracing could start\n");
        return 2;
    }
    /* TRACEEXEC makes the post-exec stop a proper PTRACE_EVENT stop (plain
     * SIGTRAP with the event marker in the wait status) instead of a legacy
     * SIGTRAP that looks like a signal to deliver -- delivering that would
     * kill the guest at the very first stop. */
    if (ptrace(PTRACE_SETOPTIONS, pid, 0,
               (void *)(long)(PTRACE_O_TRACESYSGOOD | PTRACE_O_TRACEEXEC)) < 0) {
        fprintf(stderr, "PTRACE_SETOPTIONS: %s\n", strerror(errno));
        kill(pid, SIGKILL);
        waitpid(pid, &status, 0);
        return 2;
    }

    int stepping = 0;
    int guest_rc = -1;
    int inject_sig = 0;
    for (;;) {
        long request = stepping ? PTRACE_SINGLESTEP : PTRACE_SYSCALL;
        if (ptrace(request, pid, 0,
                   inject_sig ? (void *)(long)inject_sig : 0) < 0) {
            if (errno == ESRCH)
                break;
            set_fail(&o, "ptrace continue: %s", strerror(errno));
            break;
        }
        inject_sig = 0;
        if (waitpid(pid, &status, 0) < 0) {
            set_fail(&o, "waitpid: %s", strerror(errno));
            break;
        }
        if (WIFEXITED(status)) {
            guest_rc = WEXITSTATUS(status);
            printf("EVENT name=GUEST_EXIT rc=%d\n", guest_rc);
            fflush(stdout);
            break;
        }
        if (WIFSIGNALED(status)) {
            set_fail(&o, "guest killed by signal %d", WTERMSIG(status));
            break;
        }
        if (!WIFSTOPPED(status)) {
            set_fail(&o, "unexpected wait status 0x%x", status);
            break;
        }
        int sig = WSTOPSIG(status);
        unsigned int event = (unsigned int)((unsigned)status >> 16);

        /* A seccomp-trapped getrandom has no ordinary entry/exit pair:
         * drive the SIGSYS handler before doing anything else. */
        if (sig == SIGSYS) {
            if (drive_sigsys(&o) < 0)
                break;
            if (o.phase == 4 && !o.failed)
                stepping = 1;
            continue;
        }

        /* PTRACE_EVENT stops (e.g. PTRACE_EVENT_EXEC after the child's
         * execve) are bookkeeping, not signals: continue silently. */
        if (sig == SIGTRAP && event != 0)
            continue;

        gregs_t g;
        struct ptrace_syscall_info si;
        memset(&si, 0, sizeof si);
        if (sig == (SIGTRAP | 0x80))
            (void)ptrace(PTRACE_GET_SYSCALL_INFO, pid, sizeof si, &si);
        get_gregs(&o, &g);
        if (o.failed)
            break;

        if (stepping) {
            /* Inside the watch window a plain SIGTRAP (no event marker, no
             * syscall-good bit) is the single-step trap; a syscall-good
             * stop is stepping over a syscall instruction. Anything else is
             * a genuine signal to reinject. */
            if (sig != SIGTRAP && sig != (SIGTRAP | 0x80)) {
                inject_sig = sig;
                continue;
            }
            if (!watch_step(&o, sig, &si, &g))
                stepping = 0;
            continue;
        }

        if (sig != (SIGTRAP | 0x80)) {
            /* Genuine signal: deliver it to the guest and keep tracing. */
            inject_sig = sig;
            continue;
        }

        if (si.op == PTRACE_SYSCALL_INFO_ENTRY)
            on_syscall_entry(&o, &si);
        else if (si.op == PTRACE_SYSCALL_INFO_EXIT)
            on_syscall_exit(&o, &si);
        if (o.failed)
            break;
        if (o.phase == 4)
            stepping = 1;
    }

    if (!o.failed) {
        if (o.phase != 5) {
            set_fail(&o,
                     "wipe was never observed (phase=%d held=%d sentinel=%d)",
                     o.phase, o.held, o.sentinel);
        } else if (!o.output_after_wipe) {
            set_fail(&o,
                     "buffer was wiped but the program then made no "
                     "stdout/stderr write to confirm ordering");
        } else if (mode == MODE_SUCCESS && o.keyfd < 0) {
            set_fail(&o, "success path never opened the key file");
        } else if (mode == MODE_RANDFAIL && o.sentinel == 0) {
            set_fail(&o, "randfail path never planted the tail sentinel");
        } else if (mode == MODE_WRITEFAIL && o.keyfd < 0) {
            set_fail(&o, "writefail path never opened the key file");
        } else if (mode == MODE_CLOSEFAIL && o.keyfd < 0) {
            set_fail(&o, "closefail path never opened the key file");
        } else if ((mode == MODE_CHMODFAIL || mode == MODE_STATFAIL) &&
                   o.keyfd < 0) {
            set_fail(&o, "permfail path never opened the key file");
        } else if ((mode == MODE_CHMODFAIL || mode == MODE_STATFAIL) &&
                   !o.abort_unlink_seen) {
            set_fail(&o,
                     "permfail path never reached the abort cleanup unlink");
        }
    }

    if (o.failed) {
        kill(pid, SIGKILL);
        waitpid(pid, &status, 0);
    }

    (void)guest_rc;
    return o.failed ? 1 : 0;
}
