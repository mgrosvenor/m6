// An m6 backend in C++, written from docs/m6-backend-protocol.md.
//
// Usage: m6-example-cpp <socket-path> <status-json-path>
//
// Why C++, per docs/m6-backend-examples.md §4: backends that hold substantial
// state in memory and answer from it. A Redis-shaped service, an in-memory
// index, a cache, a graph, a time series buffer, queried over HTTP.
//
// The reason is the standard library plus RAII: real containers, deterministic
// destruction, and no GC pause between a request arriving and being answered.
// When the work is "look it up in a large structure and serialise the answer",
// the language is not fighting you.
//
// The contrast with c/main.c is the point of having both. Same syscalls, same
// contract, but the socket and the payload are owned by objects whose
// destructors do the cleanup, so there is no unlink to forget on an error path.
//
// No framework, standard library only (spec §9 of the examples doc).
// docs/m6-backend-protocol.md §9 is the checklist this implements.

#include <cerrno>
#include <csignal>
#include <cstdio>
#include <cstring>
#include <fstream>
#include <ios>
#include <string>
#include <string_view>
#include <thread>

#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

namespace {

constexpr const char *kLanguage = "C++";
constexpr int kBacklog = 64;
// The proxy refuses a response whose header section exceeds 8192 bytes
// (spec §3.2). The same bound is applied to what we accept, so a peer cannot
// make us grow a buffer without limit (spec §6.2).
constexpr size_t kMaxHeaders = 8192;
constexpr size_t kMaxBody = 1u * 1024 * 1024;

// Signal handlers may only touch async-signal-safe things, so the path is a
// fixed buffer rather than a std::string and the flag is a sig_atomic_t.
char g_sock_path[512];
int g_listen_fd = -1;
volatile sig_atomic_t g_shutting_down = 0;

// Shutdown, spec §8.2: stop accepting, finish what is in flight, remove the
// socket, exit 0. A second signal exits immediately.
//
// Closing the listening fd from the handler is what makes accept() return, so
// the loop notices without polling.
void OnSignal(int) {
    if (g_shutting_down) {
        _exit(0);
    }
    g_shutting_down = 1;
    if (g_listen_fd >= 0) {
        ::close(g_listen_fd);
    }
}

// Owns an fd. The point of writing this example in C++ rather than C: every
// return path below closes the connection without saying so.
class Fd {
  public:
    explicit Fd(int fd) : fd_(fd) {}
    ~Fd() {
        if (fd_ >= 0) {
            ::close(fd_);
        }
    }
    Fd(const Fd &) = delete;
    Fd &operator=(const Fd &) = delete;
    int get() const { return fd_; }

  private:
    int fd_;
};

// Write all of it, resuming on partial writes and EINTR. A short write treated
// as complete is how a response gets truncated under load.
bool WriteAll(int fd, std::string_view data) {
    size_t off = 0;
    while (off < data.size()) {
        ssize_t n = ::write(fd, data.data() + off, data.size() - off);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return false;
        }
        if (n == 0) {
            return false;
        }
        off += static_cast<size_t>(n);
    }
    return true;
}

// Status line, Content-Type with charset, accurate Content-Length, then the body
// unless the request forbids one.
//
// `head_only` covers spec §3.3: no body for HEAD, 1xx, 204 or 304. For HEAD the
// Content-Length of the equivalent GET is still sent, which is what lets the
// proxy frame the response without waiting for bytes that never arrive.
void Respond(int fd, int status, std::string_view reason, std::string_view ctype,
             std::string_view body, bool head_only) {
    std::string head;
    head.reserve(160);
    head += "HTTP/1.1 ";
    head += std::to_string(status);
    head += ' ';
    head += reason;
    head += "\r\nContent-Type: ";
    head += ctype;
    head += "\r\nContent-Length: ";
    head += std::to_string(body.size());
    head += "\r\nConnection: close\r\n\r\n";
    if (!WriteAll(fd, head)) {
        return;
    }
    if (!head_only && !body.empty()) {
        WriteAll(fd, body);
    }
}

std::string HtmlPage(std::string_view title, std::string_view detail) {
    std::string s;
    s.reserve(200);
    s += "<!doctype html>\n<html lang=\"en\"><meta charset=\"utf-8\">\n<title>";
    s += title;
    s += "</title>\n<h1>";
    s += title;
    s += "</h1>\n<p>";
    s += detail;
    s += "</p>\n</html>\n";
    return s;
}

bool IEquals(std::string_view a, std::string_view b) {
    if (a.size() != b.size()) {
        return false;
    }
    for (size_t i = 0; i < a.size(); i++) {
        if (::tolower(static_cast<unsigned char>(a[i])) !=
            ::tolower(static_cast<unsigned char>(b[i]))) {
            return false;
        }
    }
    return true;
}

// Read the request, route it, respond. One request per connection (spec §1.3):
// nothing here loops waiting for a second.
void ServeConn(int raw_fd, const std::string &status_body) {
    Fd conn(raw_fd);
    std::string buf;
    buf.reserve(1024);
    char chunk[2048];

    size_t sep = std::string::npos;
    while (buf.size() < kMaxHeaders) {
        ssize_t n = ::read(conn.get(), chunk, sizeof chunk);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return;
        }
        if (n == 0) {
            break;
        }
        buf.append(chunk, static_cast<size_t>(n));
        sep = buf.find("\r\n\r\n");
        if (sep != std::string::npos) {
            break;
        }
    }

    if (sep == std::string::npos) {
        // 400 rather than silence: the proxy reports a dropped connection as
        // 502, which would blame the wrong side.
        std::string page = HtmlPage("400 Bad Request", "Malformed or oversized request.");
        Respond(conn.get(), 400, "Bad Request", "text/html; charset=utf-8", page, false);
        return;
    }

    std::string_view head(buf.data(), sep);
    size_t eol = head.find("\r\n");
    std::string_view request_line = head.substr(0, eol == std::string_view::npos ? head.size() : eol);

    size_t sp1 = request_line.find(' ');
    if (sp1 == std::string_view::npos) {
        std::string page = HtmlPage("400 Bad Request", "Unparseable request line.");
        Respond(conn.get(), 400, "Bad Request", "text/html; charset=utf-8", page, false);
        return;
    }
    size_t sp2 = request_line.find(' ', sp1 + 1);
    std::string_view method = request_line.substr(0, sp1);
    // The target is NOT percent-decoded by the proxy (spec §2.1). Nothing here
    // uses it to touch the filesystem, so it is matched as received bytes.
    std::string_view target = request_line.substr(
        sp1 + 1, (sp2 == std::string_view::npos ? request_line.size() : sp2) - sp1 - 1);

    bool head_only = (method == "HEAD");

    // Drain exactly Content-Length bytes. Reading beyond blocks until the
    // proxy's timeout (spec §2.6); leaving it unread leaves bytes queued on a
    // connection about to close. Nothing here uses the body.
    long want = 0;
    size_t pos = (eol == std::string_view::npos) ? head.size() : eol + 2;
    while (pos < head.size()) {
        size_t next = head.find("\r\n", pos);
        std::string_view line =
            head.substr(pos, (next == std::string_view::npos ? head.size() : next) - pos);
        size_t colon = line.find(':');
        if (colon != std::string_view::npos && IEquals(line.substr(0, colon), "content-length")) {
            std::string v(line.substr(colon + 1));
            want = std::strtol(v.c_str(), nullptr, 10);
            break;
        }
        if (next == std::string_view::npos) {
            break;
        }
        pos = next + 2;
    }
    if (want > 0 && static_cast<size_t>(want) <= kMaxBody) {
        size_t have = buf.size() - (sep + 4);
        while (have < static_cast<size_t>(want)) {
            ssize_t n = ::read(conn.get(), chunk, sizeof chunk);
            if (n <= 0) {
                break;
            }
            have += static_cast<size_t>(n);
        }
    }

    // Strip the query string: /status?x=1 is /status.
    std::string_view path = target.substr(0, target.find('?'));

    if (path == "/") {
        std::string page = HtmlPage(std::string("m6 backend example: ") + kLanguage,
                                    std::string("A minimal m6 backend written in ") + kLanguage +
                                        ".");
        Respond(conn.get(), 200, "OK", "text/html; charset=utf-8", page, head_only);
    } else if (path == "/status") {
        // Byte-identical across every example: the one file, served verbatim.
        Respond(conn.get(), 200, "OK", "application/json; charset=utf-8", status_body, head_only);
    } else if (path == "/health") {
        Respond(conn.get(), 200, "OK", "text/plain; charset=utf-8", "ok", head_only);
    } else if (path == "/boom") {
        // Fails on purpose. The proxy counts 5xx as a backend error and may
        // replace it with a styled error page (spec §4).
        std::string page =
            HtmlPage("500 Internal Server Error", "This endpoint fails on purpose.");
        Respond(conn.get(), 500, "Internal Server Error", "text/html; charset=utf-8", page,
                head_only);
    } else {
        // A real 404, not a 200 with an error page, so the status stays honest
        // to caches and crawlers (spec §4).
        std::string page = HtmlPage(
            "404 Not Found", std::string("This ") + kLanguage + " backend does not serve that path.");
        Respond(conn.get(), 404, "Not Found", "text/html; charset=utf-8", page, head_only);
    }
}

} // namespace

int main(int argc, char **argv) {
    // Exit 2 means "failed before binding", so a supervisor can tell a
    // misconfiguration from a crash (spec §8.3).
    if (argc != 3) {
        std::fprintf(stderr, "usage: %s <socket-path> <status-json-path>\n", argv[0]);
        return 2;
    }
    if (std::strlen(argv[1]) >= sizeof(sockaddr_un::sun_path)) {
        std::fprintf(stderr, "socket path too long for sockaddr_un: %s\n", argv[1]);
        return 2;
    }
    std::snprintf(g_sock_path, sizeof g_sock_path, "%s", argv[1]);

    // The payload is read from the one copy in the repository rather than
    // embedded per language, so byte-identity across the six is structural.
    std::string status_body;
    {
        std::ifstream f(argv[2], std::ios::binary);
        if (!f) {
            std::fprintf(stderr, "cannot read status payload %s\n", argv[2]);
            return 2;
        }
        status_body.assign(std::istreambuf_iterator<char>(f), std::istreambuf_iterator<char>());
        if (!f.good() && !f.eof()) {
            std::fprintf(stderr, "cannot read status payload %s\n", argv[2]);
            return 2;
        }
    }

    // A write to a socket the proxy has already closed would otherwise kill the
    // process; write() returning EPIPE is the survivable form of that.
    std::signal(SIGPIPE, SIG_IGN);

    struct sigaction sa {};
    sa.sa_handler = OnSignal;
    ::sigaction(SIGTERM, &sa, nullptr);
    ::sigaction(SIGINT, &sa, nullptr);

    // ── Binding sequence, spec §1.2, in order ────────────────────────────────

    // 0. The parent directory SHOULD be created if absent.
    {
        std::string dir(g_sock_path);
        size_t slash = dir.rfind('/');
        if (slash != std::string::npos && slash > 0) {
            ::mkdir(dir.substr(0, slash).c_str(), 0755);
        }
    }

    // 1. Remove any existing file. A stale socket from an unclean exit makes
    //    bind fail with EADDRINUSE.
    ::unlink(g_sock_path);

    g_listen_fd = ::socket(AF_UNIX, SOCK_STREAM, 0);
    if (g_listen_fd < 0) {
        std::fprintf(stderr, "socket: %s\n", std::strerror(errno));
        return 1;
    }

    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    std::snprintf(addr.sun_path, sizeof addr.sun_path, "%s", g_sock_path);

    // 2. Bind.
    if (::bind(g_listen_fd, reinterpret_cast<sockaddr *>(&addr), sizeof addr) < 0) {
        std::fprintf(stderr, "bind %s: %s\n", g_sock_path, std::strerror(errno));
        return 1;
    }

    // 3. chmod 0666. The proxy runs as a different user and cannot connect
    //    otherwise. The spec calls this the single most common cause of a
    //    backend that starts cleanly and is never contacted.
    if (::chmod(g_sock_path, 0666) < 0) {
        std::fprintf(stderr, "chmod %s: %s\n", g_sock_path, std::strerror(errno));
        return 1;
    }

    // 4. Listen, backlog at least 64.
    if (::listen(g_listen_fd, kBacklog) < 0) {
        std::fprintf(stderr, "listen: %s\n", std::strerror(errno));
        return 1;
    }

    // Spec §7 requires only that a new connection can be accepted while another
    // is handled, and allows threads, processes or an event loop. A detached
    // thread per connection is the clearest of the three to read.
    while (!g_shutting_down) {
        int fd = ::accept(g_listen_fd, nullptr, nullptr);
        if (fd < 0) {
            if (errno == EINTR) {
                continue;
            }
            break; // the signal handler closed the listener
        }
        try {
            std::thread(ServeConn, fd, std::cref(status_body)).detach();
        } catch (const std::system_error &) {
            // Out of threads: answer rather than drop, so the proxy sees a
            // prompt error instead of holding a connection for 30 seconds
            // (spec §5).
            Respond(fd, 503, "Service Unavailable", "text/html; charset=utf-8",
                    "<!doctype html>\n<title>503</title>\n<h1>503 Service Unavailable</h1>\n",
                    false);
            ::close(fd);
        }
    }

    // Removing the socket is what withdraws this member from the pool. Left
    // behind, the proxy keeps selecting a member that refuses every connection
    // (spec §8.2).
    ::unlink(g_sock_path);
    return 0;
}
