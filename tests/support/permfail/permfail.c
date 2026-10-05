/*
 * permfail.c -- LD_PRELOAD shim for wrapfile keygen permission tests.
 *
 * Two jobs:
 *
 * 1. Tracing. For the file the guest creates with open(O_CREAT|...) it logs
 *    to WRAPFILE_TEST_TRACE_FILE (one line per event):
 *
 *      BORN        fd=N mode=0oNNNN   mode right after the creating open()
 *      FCHMOD      ret=0/-1 mode=0oNNNN
 *      FSTAT       ret=0/-1 mode=0oNNNN size=N   (mode reported to guest)
 *      FIRST_WRITE mode=0oNNNN size=N  immediately before the first write()
 *      WRITE       off=N req=N ret=N [hex=...]   each write() on the key fd;
 *                    ret=-1 errno=ENAME for injected faults
 *      FSYNC       ret=0/-1
 *
 *    FIRST_WRITE with mode=0o600 and size=0 proves the program had both set
 *    and *confirmed* 0600 before a single key byte was written. The WRITE
 *    stream (off ascending from 0 with no gaps or overlaps, ending at 32)
 *    proves partial writes were resumed without duplicating or dropping
 *    bytes, and FSYNC ret=0 after the last WRITE proves the key was synced
 *    to disk before the program reported success.
 *
 * 2. Fault injection (only for a descriptor the guest itself created):
 *      WRAPFILE_TEST_FAIL_FCHMOD=1      fchmod() fails with EPERM
 *      WRAPFILE_TEST_FAIL_FSTAT=1       fstat()/fstat64() fail (EBADF), so the
 *                                        resulting mode cannot be confirmed
 *      WRAPFILE_TEST_LIE_MODE=OCTAL     fstat reports these low 12 mode bits
 *                                        (e.g. 0000) while the real fchmod
 *                                        succeeded -- a filesystem that silently
 *                                        stores another mode
 *      WRAPFILE_TEST_PARTIAL_WRITE=N    write() accepts at most N bytes per
 *                                        call (a kernel that only takes part
 *                                        of the buffer)
 *      WRAPFILE_TEST_WRITE_EINTR=N      the first N write() calls fail with
 *                                        EINTR, then writes succeed normally
 *      WRAPFILE_TEST_FAIL_WRITE_AFTER=N after N bytes in total have landed,
 *                                        write() fails with EIO (the last
 *                                        partial write is clamped so exactly
 *                                        N bytes are on disk before the error)
 *      WRAPFILE_TEST_FAIL_FSYNC=1       fsync() fails with EIO
 *      WRAPFILE_TEST_TRACE_BYTES=1      append hex=... of the bytes each
 *                                        write() actually landed, so tests can
 *                                        reconstruct the byte stream and check
 *                                        nothing secret leaks to stdout/stderr
 *
 * The shim only activates inside an executable whose basename starts with
 * "wrapfile" (checked via /proc/self/exe), so a wrapper shell used to set
 * the umask is never affected. After execve() the guest starts with a fresh
 * table, so only the guest's own O_CREAT descriptor (the key file) is
 * tracked; stdout/stderr and unrelated fds pass through untouched.
 */

#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef O_CLOEXEC
#define O_CLOEXEC 0
#endif

typedef int (*open_fn)(const char *, int, ...);
typedef int (*open64_fn)(const char *, int, ...);
typedef int (*fchmod_fn)(int, mode_t);
typedef int (*fstat_fn)(int, struct stat *);
typedef int (*fstat64_fn)(int, struct stat64 *);
typedef ssize_t (*write_fn)(int, const void *, size_t);
typedef int (*fsync_fn)(int);

static open_fn    real_open;
static open64_fn  real_open64;
static fchmod_fn  real_fchmod;
static fstat_fn   real_fstat;
static fstat64_fn real_fstat64;
static write_fn   real_write;
static fsync_fn   real_fsync;

static int cfg_fail_fchmod = 0;
static int cfg_fail_fstat  = 0;
static int cfg_lie_mode    = -1; /* -1 = disabled */
static long cfg_partial_write = 0;      /* 0 = disabled */
static int  cfg_write_eintr = 0;        /* first N write() calls fail EINTR */
static long cfg_fail_write_after = -1;  /* -1 = disabled */
static int  cfg_fail_fsync = 0;
static int  cfg_trace_bytes = 0;

/* Descriptor table: fds the guest created with O_CREAT. */
#define FD_MAX 1024
static unsigned char tracked[FD_MAX];
static unsigned char written[FD_MAX];
static unsigned char eintr_left[FD_MAX];
static unsigned long long written_total[FD_MAX];

/* ----- activation: only inside the wrapfile executable ----- */

static int process_active(void)
{
    static int cached = -1;
    if (cached != -1)
        return cached;

    char buf[512];
    ssize_t n = readlink("/proc/self/exe", buf, sizeof(buf) - 1);
    if (n <= 0) {
        cached = 0;
        return 0;
    }
    buf[n] = '\0';
    const char *base = strrchr(buf, '/');
    base = base ? base + 1 : buf;
    cached = (strncmp(base, "wrapfile", 8) == 0) ? 1 : 0;
    return cached;
}

static void load_config(void)
{
    static int loaded = 0;
    if (loaded)
        return;
    loaded = 1;

    const char *v;
    if ((v = getenv("WRAPFILE_TEST_FAIL_FCHMOD")) && v[0] == '1')
        cfg_fail_fchmod = 1;
    if ((v = getenv("WRAPFILE_TEST_FAIL_FSTAT")) && v[0] == '1')
        cfg_fail_fstat = 1;
    if ((v = getenv("WRAPFILE_TEST_TRACE_FILE")))
        (void)v; /* opened lazily on first trace() */
    if ((v = getenv("WRAPFILE_TEST_LIE_MODE")) && v[0] != '\0')
        cfg_lie_mode = (int)strtol(v, NULL, 8);
    if ((v = getenv("WRAPFILE_TEST_PARTIAL_WRITE")) && v[0] != '\0')
        cfg_partial_write = strtol(v, NULL, 10);
    if ((v = getenv("WRAPFILE_TEST_WRITE_EINTR")) && v[0] != '\0')
        cfg_write_eintr = atoi(v);
    if ((v = getenv("WRAPFILE_TEST_FAIL_WRITE_AFTER")) && v[0] != '\0')
        cfg_fail_write_after = strtol(v, NULL, 10);
    if ((v = getenv("WRAPFILE_TEST_FAIL_FSYNC")) && v[0] == '1')
        cfg_fail_fsync = 1;
    if ((v = getenv("WRAPFILE_TEST_TRACE_BYTES")) && v[0] == '1')
        cfg_trace_bytes = 1;
}

static int trace_fd(void)
{
    static int fd = -2; /* -2 = not initialised */
    if (fd != -2)
        return fd;

    const char *path = getenv("WRAPFILE_TEST_TRACE_FILE");
    if (!path || !path[0]) {
        fd = -1;
        return fd;
    }
    /*
     * Raw openat(AT_FDCWD, ...) syscall rather than open(): going through
     * our own open() wrapper inside the wrapfile process would mark the
     * trace file as a tracked, created descriptor and mis-attribute writes
     * to it. The syscall bypasses all libc interposition.
     */
    fd = (int)syscall(SYS_openat, AT_FDCWD, path,
                      O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0600);
    return fd;
}

static void trace(const char *fmt, ...)
{
    int fd = trace_fd();
    if (fd < 0)
        return;
    va_list ap;
    va_start(ap, fmt);
    vdprintf(fd, fmt, ap);
    va_end(ap);
}

/* ----- per-descriptor helpers ----- */

static fstat_fn get_real_fstat(void)
{
    if (!real_fstat)
        real_fstat = (fstat_fn)dlsym(RTLD_NEXT, "fstat");
    return real_fstat;
}

static int is_tracked(int fd)
{
    return fd >= 0 && fd < FD_MAX && tracked[fd];
}

static void note_created(int fd)
{
    if (fd < 0 || fd >= FD_MAX)
        return;
    tracked[fd] = 1;
    written[fd] = 0;
    eintr_left[fd] = (cfg_write_eintr > 0 && cfg_write_eintr < 256)
                         ? (unsigned char)cfg_write_eintr
                         : 0;
    written_total[fd] = 0;

    struct stat st;
    fstat_fn fn = get_real_fstat();
    if (fn && fn(fd, &st) == 0) {
        trace("BORN fd=%d mode=0%04o\n", fd,
              (unsigned)(st.st_mode & 07777));
    }
}

/*
 * Shared fstat handling for tracked fds: perform the real fstat (via the
 * passed function), apply fault/lie policy, log the result, and tell the
 * caller what to return.
 */
static int handle_fstat(int fd, void *st, int is64)
{
    int rc = is64 ? real_fstat64(fd, (struct stat64 *)st)
                  : real_fstat(fd, (struct stat *)st);

    if (!is_tracked(fd))
        return rc;

    if (rc != 0) {
        trace("FSTAT ret=-1 mode=00000 size=0\n");
        return rc;
    }

    mode_t real_mode;
    off_t real_size;
    if (is64) {
        struct stat64 *s = (struct stat64 *)st;
        real_mode = s->st_mode;
        real_size = s->st_size;
    } else {
        struct stat *s = (struct stat *)st;
        real_mode = s->st_mode;
        real_size = s->st_size;
    }

    if (cfg_fail_fstat) {
        trace("FSTAT ret=-1 mode=0%04o size=%lld\n",
              (unsigned)(real_mode & 07777), (long long)real_size);
        errno = EBADF;
        return -1;
    }

    mode_t reported = real_mode;
    if (cfg_lie_mode >= 0) {
        reported = (real_mode & ~(mode_t)07777) | (mode_t)cfg_lie_mode;
        if (is64)
            ((struct stat64 *)st)->st_mode = reported;
        else
            ((struct stat *)st)->st_mode = reported;
    }

    trace("FSTAT ret=0 mode=0%04o size=%lld\n",
          (unsigned)(reported & 07777), (long long)real_size);
    return 0;
}

/* ----- intercepted libc functions ----- */

int open(const char *path, int flags, ...)
{
    if (!real_open)
        real_open = (open_fn)dlsym(RTLD_NEXT, "open");

    mode_t mode = 0;
    if (flags & O_CREAT) {
        va_list ap;
        va_start(ap, flags);
        mode = va_arg(ap, mode_t);
        va_end(ap);
    }

    int fd = real_open(path, flags, mode);

    if (process_active() && (flags & O_CREAT)) {
        load_config();
        if (fd >= 0)
            note_created(fd);
        trace("OPEN ret=%d flags=0%o mode=0%04o\n", fd, flags,
              (unsigned)(mode & 07777));
    }
    return fd;
}

int open64(const char *path, int flags, ...)
{
    if (!real_open64)
        real_open64 = (open64_fn)dlsym(RTLD_NEXT, "open64");

    mode_t mode = 0;
    if (flags & O_CREAT) {
        va_list ap;
        va_start(ap, flags);
        mode = va_arg(ap, mode_t);
        va_end(ap);
    }

    int fd = real_open64(path, flags, mode);

    if (process_active() && (flags & O_CREAT)) {
        load_config();
        if (fd >= 0)
            note_created(fd);
        trace("OPEN ret=%d flags=0%o mode=0%04o\n", fd, flags,
              (unsigned)(mode & 07777));
    }
    return fd;
}

int fchmod(int fd, mode_t mode)
{
    if (!real_fchmod)
        real_fchmod = (fchmod_fn)dlsym(RTLD_NEXT, "fchmod");

    if (!process_active())
        return real_fchmod(fd, mode);
    load_config();

    if (is_tracked(fd) && cfg_fail_fchmod) {
        trace("FCHMOD ret=-1 mode=0%04o\n", (unsigned)(mode & 07777));
        errno = EPERM;
        return -1;
    }

    int rc = real_fchmod(fd, mode);
    if (is_tracked(fd)) {
        trace("FCHMOD ret=%d mode=0%04o\n", rc,
              (unsigned)(mode & 07777));
    }
    return rc;
}

int fstat(int fd, struct stat *st)
{
    if (!real_fstat)
        real_fstat = (fstat_fn)dlsym(RTLD_NEXT, "fstat");
    if (!process_active())
        return real_fstat(fd, st);
    load_config();
    if (!is_tracked(fd))
        return real_fstat(fd, st);
    return handle_fstat(fd, st, 0);
}

int fstat64(int fd, struct stat64 *st)
{
    if (!real_fstat64)
        real_fstat64 = (fstat64_fn)dlsym(RTLD_NEXT, "fstat64");
    if (!process_active())
        return real_fstat64(fd, st);
    load_config();
    if (!is_tracked(fd))
        return real_fstat64(fd, st);
    return handle_fstat(fd, st, 1);
}

ssize_t write(int fd, const void *buf, size_t count)
{
    if (!real_write)
        real_write = (write_fn)dlsym(RTLD_NEXT, "write");

    if (!process_active() || !is_tracked(fd))
        return real_write(fd, buf, count);
    load_config();

    if (!written[fd]) {
        written[fd] = 1;
        struct stat st;
        fstat_fn fn = get_real_fstat();
        if (fn && fn(fd, &st) == 0) {
            trace("FIRST_WRITE mode=0%04o size=%lld\n",
                  (unsigned)(st.st_mode & 07777),
                  (long long)st.st_size);
        } else {
            trace("FIRST_WRITE mode=unknown size=unknown\n");
        }
    }

    /* A recoverable interruption: the call fails before any byte is
     * consumed, and the next call succeeds. */
    if (eintr_left[fd] > 0) {
        eintr_left[fd]--;
        trace("WRITE ret=-1 errno=EINTR\n");
        errno = EINTR;
        return -1;
    }

    /* A kernel that only accepts part of the buffer per call. */
    size_t allowed = count;
    if (cfg_partial_write > 0 && allowed > (size_t)cfg_partial_write)
        allowed = (size_t)cfg_partial_write;

    /* An unrecoverable error once N bytes have landed. */
    if (cfg_fail_write_after >= 0) {
        long long remaining =
            (long long)cfg_fail_write_after - (long long)written_total[fd];
        if (remaining <= 0) {
            trace("WRITE ret=-1 errno=EIO off=%llu\n", written_total[fd]);
            errno = EIO;
            return -1;
        }
        if (allowed > (size_t)remaining)
            allowed = (size_t)remaining;
    }

    ssize_t r = real_write(fd, buf, allowed);
    if (r > 0) {
        unsigned long long off = written_total[fd];
        written_total[fd] += (unsigned long long)r;
        if (cfg_trace_bytes) {
            char *hex = malloc((size_t)r * 2 + 1);
            if (hex) {
                static const char digits[] = "0123456789abcdef";
                const unsigned char *p = buf;
                for (ssize_t i = 0; i < r; i++) {
                    hex[i * 2] = digits[p[i] >> 4];
                    hex[i * 2 + 1] = digits[p[i] & 0xf];
                }
                hex[r * 2] = '\0';
                trace("WRITE off=%llu req=%zu ret=%zd hex=%s\n",
                      off, allowed, r, hex);
                free(hex);
            } else {
                trace("WRITE off=%llu req=%zu ret=%zd\n", off, allowed, r);
            }
        } else {
            trace("WRITE off=%llu req=%zu ret=%zd\n", off, allowed, r);
        }
    } else {
        trace("WRITE off=%llu req=%zu ret=%zd\n",
              written_total[fd], allowed, r);
    }
    return r;
}

int fsync(int fd)
{
    if (!real_fsync)
        real_fsync = (fsync_fn)dlsym(RTLD_NEXT, "fsync");

    if (!process_active())
        return real_fsync(fd);
    load_config();

    if (is_tracked(fd) && cfg_fail_fsync) {
        trace("FSYNC ret=-1\n");
        errno = EIO;
        return -1;
    }

    int rc = real_fsync(fd);
    if (is_tracked(fd))
        trace("FSYNC ret=%d\n", rc);
    return rc;
}
