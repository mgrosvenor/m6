/*
 * perf_client: time N requests against a unix socket and print one summary line.
 *
 * Usage: perf_client <socket> <path> <requests>
 *
 * Written in C because the Python client this replaces put its own interpreter
 * time inside every reading, and never checked the status line, so a 404 or a
 * 500 was timed as a fast render.
 *
 * Standard library and POSIX only. Built with:
 *   cc -O2 -std=c11 -Wall -Wextra -Werror -o perf_client tools/perf_client.c
 */
#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

/* "HTTP/1.1 200 " is 13 bytes. Every response, warm-up included, must begin
 * with it, so a request that hit a 404 or a 500 fails the run instead of being
 * timed as fast. */
#define STATUS_PREFIX "HTTP/1.1 200 "
#define STATUS_PREFIX_LEN 13

/* One request: a fresh connection, one GET, read to EOF, close. The time is
 * CLOCK_MONOTONIC from before connect to after the last read, as the Python
 * client measured it. On success, fills *ns and *bytes and returns 0. On a
 * failed call or a non-200 status, prints `perf_client: <path>: <what>` to
 * stderr and returns 1. */
static int once(const char *sock_path, const char *path, long long *ns,
                long long *bytes) {
    if (strlen(sock_path) >= sizeof(((struct sockaddr_un *)0)->sun_path)) {
        fprintf(stderr, "perf_client: %s: socket path too long\n", path);
        return 1;
    }

    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) {
        fprintf(stderr, "perf_client: %s: socket: %s\n", path, strerror(errno));
        return 1;
    }

    struct sockaddr_un addr;
    memset(&addr, 0, sizeof addr);
    addr.sun_family = AF_UNIX;
    snprintf(addr.sun_path, sizeof addr.sun_path, "%s", sock_path);

    struct timespec t0;
    struct timespec t1;
    clock_gettime(CLOCK_MONOTONIC, &t0);

    if (connect(fd, (struct sockaddr *)&addr, sizeof addr) != 0) {
        fprintf(stderr, "perf_client: %s: connect: %s\n", path, strerror(errno));
        close(fd);
        return 1;
    }

    char req[4096];
    int reqlen = snprintf(req, sizeof req,
                          "GET %s HTTP/1.1\r\nHost: x\r\n"
                          "Accept-Encoding: identity\r\nConnection: close\r\n\r\n",
                          path);
    if (reqlen < 0 || (size_t)reqlen >= sizeof req) {
        fprintf(stderr, "perf_client: %s: request line too long\n", path);
        close(fd);
        return 1;
    }

    ssize_t off = 0;
    while (off < reqlen) {
        ssize_t w = write(fd, req + off, (size_t)(reqlen - off));
        if (w < 0) {
            if (errno == EINTR) {
                continue;
            }
            fprintf(stderr, "perf_client: %s: write: %s\n", path,
                    strerror(errno));
            close(fd);
            return 1;
        }
        off += w;
    }

    long long total = 0;
    char prefix[STATUS_PREFIX_LEN];
    size_t prefix_len = 0;
    char buf[65536];
    for (;;) {
        ssize_t r = read(fd, buf, sizeof buf);
        if (r < 0) {
            if (errno == EINTR) {
                continue;
            }
            fprintf(stderr, "perf_client: %s: read: %s\n", path,
                    strerror(errno));
            close(fd);
            return 1;
        }
        if (r == 0) {
            break;
        }
        total += (long long)r;
        if (prefix_len < STATUS_PREFIX_LEN) {
            size_t take = (size_t)r < (STATUS_PREFIX_LEN - prefix_len)
                              ? (size_t)r
                              : (STATUS_PREFIX_LEN - prefix_len);
            memcpy(prefix + prefix_len, buf, take);
            prefix_len += take;
        }
    }

    clock_gettime(CLOCK_MONOTONIC, &t1);
    close(fd);

    if (prefix_len < STATUS_PREFIX_LEN ||
        memcmp(prefix, STATUS_PREFIX, STATUS_PREFIX_LEN) != 0) {
        fprintf(stderr, "perf_client: %s: status line was not 200\n", path);
        return 1;
    }

    if (ns != NULL) {
        *ns = (t1.tv_sec - t0.tv_sec) * 1000000000LL + (t1.tv_nsec - t0.tv_nsec);
    }
    if (bytes != NULL) {
        *bytes = total;
    }
    return 0;
}

static int cmp_ll(const void *a, const void *b) {
    long long x = *(const long long *)a;
    long long y = *(const long long *)b;
    return (x > y) - (x < y);
}

int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "usage: perf_client <socket> <path> <requests>\n");
        return 2;
    }
    const char *sock_path = argv[1];
    const char *path = argv[2];

    char *end = NULL;
    long requests = strtol(argv[3], &end, 10);
    if (end == argv[3] || *end != '\0' || requests <= 0) {
        fprintf(stderr, "usage: perf_client <socket> <path> <requests>\n");
        return 2;
    }

    /* macOS has no MSG_NOSIGNAL; a write to a closed socket must not kill us. */
    signal(SIGPIPE, SIG_IGN);

    /* 30 warm-up requests, then <requests> timed ones. */
    for (int i = 0; i < 30; i++) {
        if (once(sock_path, path, NULL, NULL) != 0) {
            return 1;
        }
    }

    long long *readings = malloc((size_t)requests * sizeof(long long));
    if (readings == NULL) {
        fprintf(stderr, "perf_client: %s: out of memory\n", path);
        return 1;
    }

    long long last_bytes = 0;
    for (long i = 0; i < requests; i++) {
        long long ns = 0;
        long long bytes = 0;
        if (once(sock_path, path, &ns, &bytes) != 0) {
            free(readings);
            return 1;
        }
        readings[i] = ns;
        last_bytes = bytes;
    }

    qsort(readings, (size_t)requests, sizeof(long long), cmp_ll);

    /* The median of an even count is the mean of the two middle readings, in
     * integer nanoseconds. p90 is the reading at index n*9/10 of the sorted
     * readings. */
    long long median;
    if (requests % 2 == 1) {
        median = readings[requests / 2];
    } else {
        median = (readings[requests / 2 - 1] + readings[requests / 2]) / 2;
    }
    long long p90 = readings[requests * 9 / 10];

    printf("%lld %lld %lld\n", median, p90, last_bytes);
    free(readings);
    return 0;
}
