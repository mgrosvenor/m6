/*
 * An m6 backend in C, written from docs/m6-backend-protocol.md.
 *
 * Usage: m6-example-c <socket-path> <status-json-path>
 *
 * Why C, per docs/m6-backend-examples.md §4: backends where the runtime itself
 * is the problem. No allocator you did not choose, no garbage collector, no
 * interpreter, a static binary measured in tens of kilobytes. The motivating
 * case is IoT with m6-http as the front door: a sensor on an ESP32 or a small
 * Linux board serves a handful of endpoints while the proxy holds TLS, HTTP/2,
 * caching and rate limiting, none of which the device could reasonably
 * implement. The contract is shaped the way it is partly so this is possible.
 *
 * This is also the example that shows how little the contract actually asks
 * for. Everything below is one file, the standard library, and POSIX sockets.
 * Spec §9 is the checklist it implements.
 *
 * Threading: one detached thread per connection. Spec §7 requires only that a
 * backend can accept a new connection while handling another, and allows
 * threads, processes or an event loop. Thread-per-connection is the clearest of
 * the three to read, which is the point of an example.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

#define LANGUAGE "C"
#define BACKLOG 64
/* The proxy refuses a response whose header section exceeds 8192 bytes
 * (spec §3.2). The same bound is applied to the request we accept, so a peer
 * cannot make us grow a buffer without limit (spec §6.2). */
#define MAX_HEADERS 8192
#define MAX_BODY (1 * 1024 * 1024)

static char g_sock_path[512];
static char *g_status_body = NULL;
static size_t g_status_len = 0;
static int g_listen_fd = -1;
static volatile sig_atomic_t g_shutting_down = 0;

/* ── Shutdown, spec §8.2 ──────────────────────────────────────────────────
 *
 * Stop accepting, finish what is in flight, remove the socket, exit 0. A second
 * signal exits immediately.
 *
 * Closing the listening fd from the handler is what makes accept() return, so
 * the main loop notices without polling. Only async-signal-safe calls here:
 * close() and _exit() are, printf() is not.
 */
static void on_signal(int sig) {
    (void)sig;
    if (g_shutting_down) {
        _exit(0); /* second signal: immediate */
    }
    g_shutting_down = 1;
    if (g_listen_fd >= 0) {
        close(g_listen_fd);
    }
}

/* Write all of buf, resuming on partial writes and EINTR. A short write treated
 * as complete is how a response gets truncated under load. */
static int write_all(int fd, const char *buf, size_t len) {
    size_t off = 0;
    while (off < len) {
        ssize_t n = write(fd, buf + off, len - off);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        if (n == 0) {
            return -1;
        }
        off += (size_t)n;
    }
    return 0;
}

/* Send a complete response: status line, Content-Type, accurate Content-Length,
 * then the body unless the request forbids one.
 *
 * `head_only` covers spec §3.3: no body for HEAD, and for 1xx, 204 and 304. For
 * HEAD the Content-Length of the equivalent GET is still sent, which is what
 * the spec asks for and what lets the proxy frame the response without waiting
 * for bytes that will not arrive. */
static void respond(int fd, int status, const char *reason, const char *ctype,
                    const char *body, size_t body_len, int head_only) {
    char head[512];
    int n = snprintf(head, sizeof head,
                     "HTTP/1.1 %d %s\r\n"
                     "Content-Type: %s\r\n"
                     "Content-Length: %zu\r\n"
                     "Connection: close\r\n"
                     "\r\n",
                     status, reason, ctype, body_len);
    if (n < 0 || (size_t)n >= sizeof head) {
        return;
    }
    if (write_all(fd, head, (size_t)n) < 0) {
        return;
    }
    if (!head_only && body_len > 0) {
        (void)write_all(fd, body, body_len);
    }
}

static void html_page(char *out, size_t cap, const char *title, const char *detail) {
    snprintf(out, cap,
             "<!doctype html>\n<html lang=\"en\"><meta charset=\"utf-8\">\n"
             "<title>%s</title>\n<h1>%s</h1>\n<p>%s</p>\n</html>\n",
             title, title, detail);
}

/* Read the request, route it, respond, close. One request per connection
 * (spec §1.3): nothing here loops waiting for a second one. */
static void *serve_conn(void *arg) {
    int fd = (int)(intptr_t)arg;

    char buf[MAX_HEADERS + 1];
    size_t used = 0;
    const char *end = NULL;

    /* Read until the end of the header section. */
    while (used < MAX_HEADERS) {
        ssize_t n = read(fd, buf + used, MAX_HEADERS - used);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            close(fd);
            return NULL;
        }
        if (n == 0) {
            break;
        }
        used += (size_t)n;
        buf[used] = '\0';
        end = strstr(buf, "\r\n\r\n");
        if (end) {
            break;
        }
    }
    buf[used] = '\0';

    if (!end) {
        /* No complete header section, or larger than we accept. 400 rather than
         * silence: the proxy reports a dropped connection as 502 and that would
         * blame the wrong side. */
        char page[256];
        html_page(page, sizeof page, "400 Bad Request", "Malformed or oversized request.");
        respond(fd, 400, "Bad Request", "text/html; charset=utf-8", page, strlen(page), 0);
        close(fd);
        return NULL;
    }

    /* Method and target from the request line. The target is NOT
     * percent-decoded by the proxy (spec §2.1), and nothing below uses it to
     * touch the filesystem, so it is compared as received bytes. */
    char method[16] = {0};
    char target[1024] = {0};
    if (sscanf(buf, "%15s %1023s", method, target) != 2) {
        char page[256];
        html_page(page, sizeof page, "400 Bad Request", "Unparseable request line.");
        respond(fd, 400, "Bad Request", "text/html; charset=utf-8", page, strlen(page), 0);
        close(fd);
        return NULL;
    }

    int head_only = (strcmp(method, "HEAD") == 0);

    /* Drain exactly Content-Length bytes. Reading beyond it blocks until the
     * proxy's timeout (spec §2.6); leaving it unread is also wrong, because the
     * bytes stay queued on a connection we are about to close. Nothing here
     * uses the body: the routes are all GET-shaped. */
    const char *cl = NULL;
    for (const char *p = buf; p && p < end; p = strchr(p, '\n')) {
        if (*p == '\n') {
            p++;
        }
        if (p >= end) {
            break;
        }
        if (strncasecmp(p, "content-length:", 15) == 0) {
            cl = p + 15;
            break;
        }
    }
    if (cl) {
        long want = strtol(cl, NULL, 10);
        if (want > 0 && want <= MAX_BODY) {
            size_t have = used - (size_t)((end + 4) - buf);
            char sink[4096];
            while (have < (size_t)want) {
                ssize_t n = read(fd, sink, sizeof sink);
                if (n <= 0) {
                    break;
                }
                have += (size_t)n;
            }
        }
    }

    /* Strip the query string before matching: /status?x=1 is /status. */
    char *q = strchr(target, '?');
    if (q) {
        *q = '\0';
    }

    char page[512];
    if (strcmp(target, "/") == 0) {
        html_page(page, sizeof page, "m6 backend example: " LANGUAGE,
                  "A minimal m6 backend written in " LANGUAGE ".");
        respond(fd, 200, "OK", "text/html; charset=utf-8", page, strlen(page), head_only);
    } else if (strcmp(target, "/status") == 0) {
        /* Byte-identical across every example: the one file, served verbatim. */
        respond(fd, 200, "OK", "application/json; charset=utf-8", g_status_body, g_status_len,
                head_only);
    } else if (strcmp(target, "/health") == 0) {
        respond(fd, 200, "OK", "text/plain; charset=utf-8", "ok", 2, head_only);
    } else if (strcmp(target, "/boom") == 0) {
        /* Fails on purpose. The proxy counts 5xx as a backend error and may
         * replace it with a styled error page (spec §4). */
        html_page(page, sizeof page, "500 Internal Server Error", "This endpoint fails on purpose.");
        respond(fd, 500, "Internal Server Error", "text/html; charset=utf-8", page, strlen(page),
                head_only);
    } else {
        /* A real 404, not a 200 with an error page, so the status stays honest
         * to caches and crawlers (spec §4). */
        html_page(page, sizeof page, "404 Not Found",
                  "This " LANGUAGE " backend does not serve that path.");
        respond(fd, 404, "Not Found", "text/html; charset=utf-8", page, strlen(page), head_only);
    }

    close(fd);
    return NULL;
}

/* Read the whole payload file. One copy in the repository, used verbatim by
 * every example, so byte-identity across the six is structural. */
static int load_status(const char *path) {
    FILE *f = fopen(path, "rb");
    if (!f) {
        return -1;
    }
    if (fseek(f, 0, SEEK_END) != 0) {
        fclose(f);
        return -1;
    }
    long len = ftell(f);
    if (len < 0 || fseek(f, 0, SEEK_SET) != 0) {
        fclose(f);
        return -1;
    }
    g_status_body = malloc((size_t)len);
    if (!g_status_body) {
        fclose(f);
        return -1;
    }
    if (fread(g_status_body, 1, (size_t)len, f) != (size_t)len) {
        fclose(f);
        return -1;
    }
    fclose(f);
    g_status_len = (size_t)len;
    return 0;
}

int main(int argc, char **argv) {
    /* Exit 2 means "failed before binding", so a supervisor can tell a
     * misconfiguration from a crash (spec §8.3). */
    if (argc != 3) {
        fprintf(stderr, "usage: %s <socket-path> <status-json-path>\n", argv[0]);
        return 2;
    }
    if (strlen(argv[1]) >= sizeof(((struct sockaddr_un *)0)->sun_path)) {
        fprintf(stderr, "socket path too long for sockaddr_un: %s\n", argv[1]);
        return 2;
    }
    snprintf(g_sock_path, sizeof g_sock_path, "%s", argv[1]);

    if (load_status(argv[2]) != 0) {
        fprintf(stderr, "cannot read status payload %s: %s\n", argv[2], strerror(errno));
        return 2;
    }

    /* A write to a socket the proxy has already closed would otherwise kill the
     * process; write() returning EPIPE is the survivable form of that. */
    signal(SIGPIPE, SIG_IGN);

    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_signal;
    sigaction(SIGTERM, &sa, NULL);
    sigaction(SIGINT, &sa, NULL);

    /* ── Binding sequence, spec §1.2, in order ─────────────────────────── */

    /* 0. The parent directory SHOULD be created if absent. */
    char dir[512];
    snprintf(dir, sizeof dir, "%s", g_sock_path);
    char *slash = strrchr(dir, '/');
    if (slash) {
        *slash = '\0';
        mkdir(dir, 0755); /* may already exist; bind will report a real problem */
    }

    /* 1. Remove any existing file. A stale socket from an unclean exit makes
     *    bind fail with EADDRINUSE. */
    unlink(g_sock_path);

    g_listen_fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (g_listen_fd < 0) {
        fprintf(stderr, "socket: %s\n", strerror(errno));
        return 1;
    }

    struct sockaddr_un addr;
    memset(&addr, 0, sizeof addr);
    addr.sun_family = AF_UNIX;
    snprintf(addr.sun_path, sizeof addr.sun_path, "%s", g_sock_path);

    /* 2. Bind. */
    if (bind(g_listen_fd, (struct sockaddr *)&addr, sizeof addr) < 0) {
        fprintf(stderr, "bind %s: %s\n", g_sock_path, strerror(errno));
        return 1;
    }

    /* 3. chmod 0666. The proxy runs as a different user and cannot connect
     *    otherwise. The spec calls this the single most common cause of a
     *    backend that starts cleanly and is never contacted. */
    if (chmod(g_sock_path, 0666) < 0) {
        fprintf(stderr, "chmod %s: %s\n", g_sock_path, strerror(errno));
        return 1;
    }

    /* 4. Listen, backlog at least 64. */
    if (listen(g_listen_fd, BACKLOG) < 0) {
        fprintf(stderr, "listen: %s\n", strerror(errno));
        return 1;
    }

    while (!g_shutting_down) {
        int fd = accept(g_listen_fd, NULL, NULL);
        if (fd < 0) {
            if (errno == EINTR) {
                continue;
            }
            break; /* the signal handler closed the listener */
        }
        pthread_t t;
        if (pthread_create(&t, NULL, serve_conn, (void *)(intptr_t)fd) != 0) {
            /* Out of threads: answer rather than drop, so the proxy sees a
             * prompt error instead of holding a connection for 30 seconds
             * (spec §5). */
            const char *page = "<!doctype html>\n<title>503</title>\n<h1>503 Service Unavailable</h1>\n";
            respond(fd, 503, "Service Unavailable", "text/html; charset=utf-8", page, strlen(page),
                    0);
            close(fd);
            continue;
        }
        pthread_detach(t);
    }

    /* Removing the socket is what withdraws this member from the pool. Left
     * behind, the proxy keeps selecting a member that refuses every
     * connection (spec §8.2). */
    unlink(g_sock_path);
    free(g_status_body);
    return 0;
}
