// An m6 backend in Go, written from docs/m6-backend-protocol.md.
//
// Usage: m6-example-go <socket-path> <status-json-path>
//
// Why Go, per docs/m6-backend-examples.md §4: backends whose work is mostly
// waiting on other things. Goroutines make a thousand concurrent outbound calls
// unremarkable and the standard library ships a good HTTP client and JSON codec,
// so a backend that is mostly integration is mostly stdlib.
//
// This example is also the one that tests the contract hardest, because
// net.Listen("unix", ...) with http.Serve puts a mature, strict HTTP
// implementation behind the socket rather than a parser written for the
// occasion. If m6-http emits something subtly wrong, this is where it shows.
//
// No framework: net/http only. Spec §9 is the checklist this implements.
package main

import (
	"context"
	"errors"
	"fmt"
	"net"
	"net/http"
	"os"
	"os/signal"
	"syscall"
)

const language = "Go"

func main() {
	// Exit 2 means "failed before binding", so a supervisor can tell a
	// misconfiguration from a crash (spec §8.3).
	if len(os.Args) != 3 {
		fmt.Fprintf(os.Stderr, "usage: %s <socket-path> <status-json-path>\n", os.Args[0])
		os.Exit(2)
	}
	sockPath, statusPath := os.Args[1], os.Args[2]

	// The /status payload is read from the one copy in the repository rather
	// than embedded per language. Byte-identity across the six examples is then
	// structural: there is nothing to keep in step.
	statusBody, err := os.ReadFile(statusPath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "cannot read status payload %s: %v\n", statusPath, err)
		os.Exit(2)
	}

	listener, err := listen(sockPath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "cannot listen on %s: %v\n", sockPath, err)
		os.Exit(1)
	}

	srv := &http.Server{Handler: routes(statusBody)}
	// The proxy opens a connection, writes one request, reads one response and
	// closes (spec §1.3). It sends `Connection: close`, so Go would close
	// anyway; disabling keep-alives makes the one-request-per-connection rule
	// explicit rather than a consequence of what the proxy happens to send.
	srv.SetKeepAlivesEnabled(false)

	// Shutdown, spec §8.2: stop accepting, finish what is in flight, remove the
	// socket, exit 0. Second signal exits immediately.
	sigs := make(chan os.Signal, 2)
	signal.Notify(sigs, syscall.SIGTERM, syscall.SIGINT)
	go func() {
		<-sigs
		go func() {
			<-sigs
			os.Exit(0)
		}()
		// Graceful: Shutdown stops the listener and waits for active requests.
		_ = srv.Shutdown(context.Background())
		// Removing the socket is what withdraws this member from the pool. Left
		// behind, the proxy keeps selecting a member that refuses every
		// connection.
		_ = os.Remove(sockPath)
		os.Exit(0)
	}()

	if err := srv.Serve(listener); err != nil && !errors.Is(err, http.ErrServerClosed) {
		fmt.Fprintf(os.Stderr, "serve: %v\n", err)
		_ = os.Remove(sockPath)
		os.Exit(1)
	}
}

// listen performs the binding sequence in spec §1.2, in order.
func listen(path string) (net.Listener, error) {
	// 0. The parent directory SHOULD be created if absent.
	if dir := dirOf(path); dir != "" {
		_ = os.MkdirAll(dir, 0o755)
	}
	// 1. Remove any existing file: a stale socket from an unclean exit makes
	//    bind fail with EADDRINUSE.
	if err := os.Remove(path); err != nil && !errors.Is(err, os.ErrNotExist) {
		return nil, err
	}
	// 2. Bind, and 4. listen. Go's ListenUnix uses a backlog of at least 128,
	//    which satisfies the SHOULD of 64.
	l, err := net.Listen("unix", path)
	if err != nil {
		return nil, err
	}
	// 3. chmod 0666. The proxy runs as a different user and cannot connect
	//    otherwise. The spec calls this the single most common cause of a
	//    backend that starts cleanly and is never contacted.
	if err := os.Chmod(path, 0o666); err != nil {
		_ = l.Close()
		return nil, err
	}
	return l, nil
}

func dirOf(path string) string {
	for i := len(path) - 1; i >= 0; i-- {
		if path[i] == '/' {
			return path[:i]
		}
	}
	return ""
}

func routes(statusBody []byte) http.Handler {
	mux := http.NewServeMux()

	// net/http writes Content-Length itself when the body is written in one
	// call and no Transfer-Encoding is set, and it suppresses the body for HEAD
	// and for 204/304 (spec §3.3). It is set explicitly all the same: the spec
	// requires an accurate Content-Length and this example is read as a
	// realisation of that checklist, so the requirement should be visible.
	send := func(w http.ResponseWriter, status int, contentType string, body []byte) {
		w.Header().Set("Content-Type", contentType)
		w.Header().Set("Content-Length", fmt.Sprint(len(body)))
		// No Cache-Control: a backend that sends nothing is treated as
		// uncacheable (spec §3.5), and these examples deliberately leave
		// caching to the proxy so the tests can prove the proxy adds it.
		w.WriteHeader(status)
		_, _ = w.Write(body)
	}

	page := func(title, detail string) []byte {
		return []byte("<!doctype html>\n<html lang=\"en\"><meta charset=\"utf-8\">\n" +
			"<title>" + title + "</title>\n<h1>" + title + "</h1>\n<p>" + detail + "</p>\n</html>\n")
	}

	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		// ServeMux's "/" matches everything, so the 404 is dispatched here.
		// A path this backend does not serve gets a real 404, not a 200 with an
		// error page, so the status stays honest to caches and crawlers
		// (spec §4).
		if r.URL.Path != "/" {
			send(w, http.StatusNotFound, "text/html; charset=utf-8",
				page("404 Not Found", "This "+language+" backend does not serve that path."))
			return
		}
		send(w, http.StatusOK, "text/html; charset=utf-8",
			page("m6 backend example: "+language, "A minimal m6 backend written in "+language+"."))
	})

	mux.HandleFunc("/status", func(w http.ResponseWriter, r *http.Request) {
		// Byte-identical across every example. Served verbatim, so nothing here
		// re-encodes or re-orders it.
		send(w, http.StatusOK, "application/json; charset=utf-8", statusBody)
	})

	mux.HandleFunc("/health", func(w http.ResponseWriter, r *http.Request) {
		send(w, http.StatusOK, "text/plain; charset=utf-8", []byte("ok"))
	})

	mux.HandleFunc("/boom", func(w http.ResponseWriter, r *http.Request) {
		// Exercises the backend error path. The proxy counts 5xx as a backend
		// error and may replace it with a styled error page (spec §4).
		send(w, http.StatusInternalServerError, "text/html; charset=utf-8",
			page("500 Internal Server Error", "This endpoint fails on purpose."))
	})

	return mux
}
