// Page-cache control for a live MDBX database, so a replay can be run in a defined cache regime.
//
// Why this exists. reth reads trie nodes through libmdbx's mmap, not through read()/pread() -- a
// live node performed exactly 12 preads on mdbx.dat across 400 eth_getProof calls. So every
// read-interposition trick (F_NOCACHE, LD_PRELOAD on read) delays nothing, and the only lever that
// touches those reads is the page cache itself. macOS has no drop_caches, but msync with
// MS_INVALIDATE on a range of a MAP_SHARED mapping asks the kernel to drop the clean resident pages
// for that range -- per file, unprivileged, and effective against pages another process has mapped.
// MS_SYNC first means dirty pages are written back rather than discarded, so this is safe to run
// against a database a node is actively writing.
//
// Subcommands:
//   residency <file>            report resident pages / bytes / percentage via mincore
//   evict <file>                one MS_SYNC|MS_INVALIDATE pass, reporting residency either side
//   loop <file> <interval_ms>   evict repeatedly until killed; this is the shaping arm
//
// `loop` is the interesting one: run it alongside a replay and the node's trie reads become page
// faults against the device instead of hits against RAM, which is the condition
// celo-blockchain-planning#1453 identifies as the incident's root cause ("a node whose page cache is
// smaller than the state working set"). Continuous eviction is also a fair model of the production
// condition, where a co-scheduled second reth process is doing the evicting.
//
// Build: cc -O2 -o cachetool scripts/perf/cachetool.c

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static long page_size(void) { return sysconf(_SC_PAGESIZE); }

static double now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}

// Map the whole file MAP_SHARED. Read-write so MS_INVALIDATE applies to the same mapping type the
// node holds; falls back to read-only when the caller lacks write permission.
static void *map_file(const char *path, size_t *len_out, int *fd_out) {
    int fd = open(path, O_RDWR);
    int prot = PROT_READ | PROT_WRITE;
    if (fd < 0) {
        fd = open(path, O_RDONLY);
        prot = PROT_READ;
    }
    if (fd < 0) {
        fprintf(stderr, "open(%s): %s\n", path, strerror(errno));
        return NULL;
    }
    struct stat st;
    if (fstat(fd, &st) != 0 || st.st_size <= 0) {
        fprintf(stderr, "fstat(%s): %s\n", path, strerror(errno));
        close(fd);
        return NULL;
    }
    void *addr = mmap(NULL, (size_t)st.st_size, prot, MAP_SHARED, fd, 0);
    if (addr == MAP_FAILED) {
        fprintf(stderr, "mmap(%s): %s\n", path, strerror(errno));
        close(fd);
        return NULL;
    }
    *len_out = (size_t)st.st_size;
    *fd_out = fd;
    return addr;
}

// Resident page count for the mapping, via mincore. This is the ground truth for "is it cached",
// and it is why this tool reports a regime rather than assuming one.
static long resident_pages(void *addr, size_t len, long *total_out) {
    long ps = page_size();
    long pages = (long)((len + ps - 1) / ps);
    char *vec = malloc((size_t)pages);
    if (!vec) return -1;
    if (mincore(addr, len, vec) != 0) {
        free(vec);
        return -1;
    }
    long resident = 0;
    for (long i = 0; i < pages; i++) {
        if (vec[i] & MINCORE_INCORE) resident++;
    }
    free(vec);
    if (total_out) *total_out = pages;
    return resident;
}

static void report(const char *label, void *addr, size_t len) {
    long total = 0;
    long res = resident_pages(addr, len, &total);
    if (res < 0) {
        fprintf(stderr, "mincore: %s\n", strerror(errno));
        return;
    }
    double mib = (double)res * (double)page_size() / (1024.0 * 1024.0);
    printf("  %-8s %8ld / %8ld pages resident  %9.1f MiB  %5.1f%%\n", label, res, total, mib,
           total ? 100.0 * (double)res / (double)total : 0.0);
}

// One eviction pass. MS_SYNC before MS_INVALIDATE so nothing dirty is lost.
static int evict(void *addr, size_t len) {
    if (msync(addr, len, MS_SYNC) != 0) {
        fprintf(stderr, "msync(MS_SYNC): %s\n", strerror(errno));
        return -1;
    }
    if (msync(addr, len, MS_INVALIDATE) != 0) {
        fprintf(stderr, "msync(MS_INVALIDATE): %s\n", strerror(errno));
        return -1;
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr,
                "usage: %s residency <file>\n"
                "       %s evict <file>\n"
                "       %s loop <file> <interval_ms>\n",
                argv[0], argv[0], argv[0]);
        return 2;
    }
    const char *cmd = argv[1];
    const char *path = argv[2];

    size_t len = 0;
    int fd = -1;
    void *addr = map_file(path, &len, &fd);
    if (!addr) return 1;

    int rc = 0;
    if (strcmp(cmd, "residency") == 0) {
        printf("%s (%.1f MiB)\n", path, (double)len / (1024.0 * 1024.0));
        report("resident", addr, len);
    } else if (strcmp(cmd, "evict") == 0) {
        printf("%s (%.1f MiB)\n", path, (double)len / (1024.0 * 1024.0));
        report("before", addr, len);
        double t0 = now_ms();
        rc = evict(addr, len);
        double dt = now_ms() - t0;
        report("after", addr, len);
        printf("  evicted in %.0f ms\n", dt);
    } else if (strcmp(cmd, "loop") == 0) {
        if (argc < 4) {
            fprintf(stderr, "loop needs an interval in ms\n");
            rc = 2;
        } else {
            long interval = atol(argv[3]);
            // Unbuffered so a supervising script sees progress even when killed mid-run.
            setvbuf(stdout, NULL, _IONBF, 0);
            fprintf(stderr, "evicting %s every %ld ms until killed\n", path, interval);
            unsigned long passes = 0;
            while (1) {
                if (evict(addr, len) != 0) break;
                passes++;
                if (passes % 50 == 0) {
                    long total = 0;
                    long res = resident_pages(addr, len, &total);
                    fprintf(stderr, "  pass %lu, resident %ld/%ld\n", passes, res, total);
                }
                if (interval > 0) usleep((useconds_t)interval * 1000);
            }
        }
    } else {
        fprintf(stderr, "unknown subcommand: %s\n", cmd);
        rc = 2;
    }

    munmap(addr, len);
    if (fd >= 0) close(fd);
    return rc;
}
