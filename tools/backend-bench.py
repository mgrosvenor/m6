#!/usr/bin/env python3
"""Benchmark the backend examples, per docs/m6-backend-examples.md §5.

    tools/backend-bench.py                  # every language it can run
    tools/backend-bench.py --only rust-plain,rust-m6core
    tools/backend-bench.py --duration 5 --workers 4
    tools/backend-bench.py --json

Measures the first layer of §5.2, **direct to the Unix socket**: how expensive
is this implementation of the contract. The second layer, through `m6-http`, is
still owed and is listed in §11 of that document.

# Everything runs from RAM

§5.4. The binaries, the payload and the sockets are staged onto tmpfs before a
run, so cold start and throughput measure the runtime rather than the
filesystem. Cold start reads the binary off disk, the gap is largest for the
largest binary, and `rust-m6core` is 43x the size of its pair: left on disk that
would land as a penalty against `m6-core` that has nothing to do with
`m6-core`.

On Linux `/dev/shm` is tmpfs and is used. macOS has none by default, so a run
there is labelled NOT-RAM and is not comparable to a build-host run. That label
is printed in the output rather than being a footnote somebody skips.

# The generator's own cost is measured, not assumed

§5.4's honesty requirement. A Python client doing trivial per-request work can
easily be the bottleneck, and then every backend reports the same number and the
comparison the phase exists for measures nothing. So a null baseline is taken
first: connect to a socket, close it, no HTTP. That bounds what the client costs
per request. If the spread between backends is inside that, the run says
GENERATOR-BOUND rather than publishing a ranking it cannot support.
"""

# `X | None` annotations are evaluated at runtime without this on Python 3.9,
# which is what macOS ships. The build host is newer, but an example that only
# runs on the newer of the two boxes is not much of a tool.
from __future__ import annotations

import argparse
import json
import os
import shutil
import socket
import statistics
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
BACKENDS = REPO / "m6-http/tests/backends"
PAYLOAD = BACKENDS / "status.json"

# Requests issued on one connection each, as the contract requires: the proxy
# opens a connection, writes one request, reads one response and closes.
REQUEST = b"GET /status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"

LANGS = ["c", "cpp", "python", "go", "rust-plain", "rust-m6core"]


def tmpfs_root() -> tuple[Path, bool]:
    """Return a staging directory and whether it is genuinely RAM-backed."""
    shm = Path("/dev/shm")
    if shm.is_dir() and os.access(shm, os.W_OK):
        return shm / f"m6bench-{os.getpid()}", True
    # macOS: no tmpfs by default. Say so rather than quietly measuring the disk.
    return Path(f"/tmp/m6bench-{os.getpid()}"), False


def which(tool: str) -> str | None:
    return shutil.which(tool)


def build(lang: str, stage: Path) -> Path | None:
    """Build or locate the example, placing the runnable binary on tmpfs."""
    src = BACKENDS / lang
    out = stage / lang
    out.mkdir(parents=True, exist_ok=True)

    if lang == "c":
        if not which("cc"):
            return None
        bin_path = out / "ex"
        subprocess.run(
            ["cc", "-std=c11", "-O2", "-pthread", "-o", str(bin_path), str(src / "main.c")],
            check=True,
        )
        return bin_path
    if lang == "cpp":
        if not which("c++"):
            return None
        bin_path = out / "ex"
        subprocess.run(
            ["c++", "-std=c++17", "-O2", "-pthread", "-o", str(bin_path), str(src / "main.cpp")],
            check=True,
        )
        return bin_path
    if lang == "go":
        if not which("go"):
            return None
        bin_path = out / "ex"
        env = dict(os.environ, GOCACHE=str(out / "gocache"))
        subprocess.run(["go", "build", "-o", str(bin_path), "."], cwd=src, check=True, env=env)
        return bin_path
    if lang == "python":
        if not which("python3"):
            return None
        # Copied onto tmpfs so the interpreter reads the source from RAM too.
        dst = out / "main.py"
        shutil.copy2(src / "main.py", dst)
        return dst
    if lang in ("rust-plain", "rust-m6core"):
        name = f"m6-example-{lang}"
        built = REPO / "target/release" / name
        if not built.is_file():
            print(
                f"  {lang}: {built} is missing. Run: cargo build --workspace --release",
                file=sys.stderr,
            )
            return None
        # Copied rather than run in place, so every language is launched from the
        # same filesystem. Otherwise cold start compares a tmpfs read against a
        # disk read and calls the difference a property of the language.
        dst = out / name
        shutil.copy2(built, dst)
        return dst
    raise AssertionError(lang)


def spawn(lang: str, binary: Path, stage: Path, sock: Path) -> subprocess.Popen:
    payload = stage / "status.json"
    if lang == "python":
        cmd = [sys.executable, str(binary), str(sock), str(payload)]
    elif lang == "rust-m6core":
        # A core service is configured rather than argument-driven.
        site = stage / lang / "site"
        site.mkdir(parents=True, exist_ok=True)
        shutil.copy2(payload, site / "status.json")
        (site / "site.toml").write_text(
            '[site]\nname = "backend-example"\ndomain = "localhost"\n\n'
            '[log]\nlevel = "error"\nformat = "text"\n'
        )
        conf = stage / lang / "example.toml"
        conf.write_text('[log]\nlevel = "error"\n\n[server]\nsocket_mode = "0666"\n')
        return subprocess.Popen(
            [str(binary), str(site), str(conf)],
            env=dict(os.environ, M6_SOCKET_OVERRIDE=str(sock)),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    else:
        cmd = [str(binary), str(sock), str(payload)]
    return subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def one_request(path: str) -> int:
    """One connection, one request, one response. Returns bytes read."""
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(path)
    s.sendall(REQUEST)
    n = 0
    while True:
        chunk = s.recv(65536)
        if not chunk:
            break
        n += len(chunk)
    s.close()
    return n


def wait_ready(sock: Path, proc: subprocess.Popen, timeout: float = 20.0) -> float:
    """Time from spawn to the first successful response. §5.2's cold start."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"backend exited early with {proc.returncode}")
        try:
            if one_request(str(sock)) > 0:
                return time.monotonic()
        except (FileNotFoundError, ConnectionRefusedError, OSError):
            time.sleep(0.002)
    raise RuntimeError(f"{sock} never answered within {timeout}s")


def null_baseline(stage: Path, duration: float) -> dict:
    """What the client itself costs: connect, close, no HTTP.

    An accept-and-close listener in this process, so the number is the client's
    syscall and interpreter cost per request and nothing else.
    """
    import threading

    sock = stage / "null.sock"
    if sock.exists():
        sock.unlink()
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(str(sock))
    srv.listen(64)
    stop = threading.Event()

    def accept_loop():
        srv.settimeout(0.2)
        while not stop.is_set():
            try:
                c, _ = srv.accept()
                c.close()
            except (TimeoutError, socket.timeout):
                continue
            except OSError:
                break

    t = threading.Thread(target=accept_loop, daemon=True)
    t.start()

    lat = []
    end = time.monotonic() + duration
    while time.monotonic() < end:
        t0 = time.perf_counter()
        c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            c.connect(str(sock))
            c.close()
        except OSError:
            continue
        lat.append((time.perf_counter() - t0) * 1e6)
    stop.set()
    srv.close()
    t.join(timeout=1)
    sock.unlink(missing_ok=True)
    return summarise(lat, duration)


def summarise(lat_us: list[float], duration: float) -> dict:
    if not lat_us:
        return {"requests": 0, "rps": 0.0, "p50_us": 0.0, "p99_us": 0.0, "max_us": 0.0}
    lat_us.sort()
    return {
        "requests": len(lat_us),
        "rps": len(lat_us) / duration,
        "p50_us": statistics.median(lat_us),
        "p99_us": lat_us[min(len(lat_us) - 1, int(len(lat_us) * 0.99))],
        "max_us": lat_us[-1],
    }


def rss_kb(pid: int) -> int:
    """Resident memory under load. §5.2 asks for it."""
    status = Path(f"/proc/{pid}/status")
    if status.is_file():
        for line in status.read_text().splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
        return 0
    out = subprocess.run(
        ["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True, check=False
    )
    try:
        return int(out.stdout.strip())
    except ValueError:
        return 0


def load(sock: Path, duration: float, workers: int) -> list[float]:
    """Issue requests from `workers` processes for `duration`, pooling latencies.

    Processes rather than threads: the GIL would otherwise make the client's own
    serialisation the thing being measured, which is the failure mode §5.4 warns
    about.
    """
    import multiprocessing

    def worker(path, dur, q):
        lat = []
        end = time.monotonic() + dur
        while time.monotonic() < end:
            t0 = time.perf_counter()
            try:
                one_request(path)
            except OSError:
                continue
            lat.append((time.perf_counter() - t0) * 1e6)
        q.put(lat)

    ctx = multiprocessing.get_context("fork")
    q = ctx.Queue()
    procs = [ctx.Process(target=worker, args=(str(sock), duration, q)) for _ in range(workers)]
    for p in procs:
        p.start()
    out: list[float] = []
    for _ in procs:
        out.extend(q.get())
    for p in procs:
        p.join()
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--duration", type=float, default=3.0, help="seconds of load per backend")
    ap.add_argument("--workers", type=int, default=os.cpu_count() or 2)
    ap.add_argument("--only", default="", help="comma-separated subset of languages")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    langs = [l for l in LANGS if not args.only or l in args.only.split(",")]

    stage, is_ram = tmpfs_root()
    stage.mkdir(parents=True, exist_ok=True)
    shutil.copy2(PAYLOAD, stage / "status.json")

    commit = subprocess.run(
        ["git", "-C", str(REPO), "rev-parse", "--short", "HEAD"],
        capture_output=True,
        text=True,
        check=False,
    ).stdout.strip()

    meta = {
        "commit": commit,
        "staging": str(stage),
        "ram_backed": is_ram,
        "duration_s": args.duration,
        "workers": args.workers,
        "payload_bytes": PAYLOAD.stat().st_size,
        "uname": subprocess.run(
            ["uname", "-srm"], capture_output=True, text=True, check=False
        ).stdout.strip(),
    }

    if not is_ram:
        print(
            "WARNING: no tmpfs available, staging on disk. This run is NOT-RAM and is\n"
            "         not comparable with a build-host run (see §5.4).",
            file=sys.stderr,
        )

    print(f"null baseline: what the client itself costs ({args.workers} worker(s))...")
    baseline = null_baseline(stage, min(args.duration, 2.0))

    results = {}
    for lang in langs:
        print(f"  {lang}: building...", end="", flush=True)
        try:
            binary = build(lang, stage)
        except subprocess.CalledProcessError as e:
            print(f" BUILD FAILED ({e})")
            continue
        if binary is None:
            print(" skipped, toolchain missing")
            continue

        sock = stage / lang / "b.sock"
        sock.unlink(missing_ok=True)

        t_spawn = time.monotonic()
        proc = spawn(lang, binary, stage, sock)
        try:
            t_ready = wait_ready(sock, proc)
            cold_start_ms = (t_ready - t_spawn) * 1000.0

            print(" loading...", end="", flush=True)
            lat = load(sock, args.duration, args.workers)
            rss = rss_kb(proc.pid)
            size = binary.stat().st_size

            r = summarise(lat, args.duration)
            r.update(
                {
                    "cold_start_ms": cold_start_ms,
                    "rss_kb": rss,
                    "artifact_bytes": size,
                }
            )
            results[lang] = r
            print(f" {r['rps']:.0f} rps, p50 {r['p50_us']:.1f}us")
        except RuntimeError as e:
            print(f" FAILED: {e}")
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
            sock.unlink(missing_ok=True)

    shutil.rmtree(stage, ignore_errors=True)

    if args.json:
        print(json.dumps({"meta": meta, "baseline": baseline, "results": results}, indent=2))
        return 0

    report(meta, baseline, results)
    return 0


def report(meta: dict, baseline: dict, results: dict) -> None:
    print()
    print("=" * 78)
    print(f"backend examples, direct to socket   commit {meta['commit']}")
    print(f"{meta['uname']}   payload {meta['payload_bytes']}B   "
          f"{meta['duration_s']}s x {meta['workers']} workers")
    print(f"staging: {meta['staging']}  ({'tmpfs, RAM-backed' if meta['ram_backed'] else 'ON DISK, NOT comparable'})")
    print("=" * 78)
    print()
    print("client's own cost (connect+close, no HTTP), the floor for every row below:")
    print(f"  {baseline['rps']:.0f} rps   p50 {baseline['p50_us']:.1f}us   p99 {baseline['p99_us']:.1f}us")
    print()
    hdr = f"{'language':<12} {'rps':>9} {'p50 us':>8} {'p99 us':>9} {'max us':>9} {'RSS KB':>8} {'artifact':>10} {'cold ms':>8}"
    print(hdr)
    print("-" * len(hdr))
    for lang, r in results.items():
        print(
            f"{lang:<12} {r['rps']:>9.0f} {r['p50_us']:>8.1f} {r['p99_us']:>9.1f} "
            f"{r['max_us']:>9.1f} {r['rss_kb']:>8} {r['artifact_bytes']:>10} {r['cold_start_ms']:>8.1f}"
        )
    print()

    # The comparison the phase exists for, per §5.3.
    plain = results.get("rust-plain")
    core = results.get("rust-m6core")
    if plain and core:
        print("THE MEASUREMENT THIS PHASE EXISTS FOR (§5.3):")
        print("  rust-plain vs rust-m6core: same language, same compiler, same payload,")
        print("  so the difference is what linking m6-core costs.")
        d_rps = (core["rps"] - plain["rps"]) / plain["rps"] * 100.0
        d_p50 = core["p50_us"] - plain["p50_us"]
        print(f"    throughput   {plain['rps']:.0f} -> {core['rps']:.0f} rps  ({d_rps:+.1f}%)")
        print(f"    p50          {plain['p50_us']:.1f} -> {core['p50_us']:.1f} us  ({d_p50:+.1f}us)")
        print(f"    RSS          {plain['rss_kb']} -> {core['rss_kb']} KB")
        print(f"    artifact     {plain['artifact_bytes']} -> {core['artifact_bytes']} bytes "
              f"({core['artifact_bytes'] / plain['artifact_bytes']:.1f}x)")
        print(f"    cold start   {plain['cold_start_ms']:.1f} -> {core['cold_start_ms']:.1f} ms")
        print()

        # Is the result even resolvable? The client's own p50 is the floor.
        spread = abs(d_p50)
        if spread < baseline["p50_us"] * 0.25:
            print("  GENERATOR-BOUND: the difference between the two is smaller than a")
            print("  quarter of the client's own per-request cost, so this run cannot")
            print(f"  resolve it. Client p50 is {baseline['p50_us']:.1f}us against a "
                  f"{spread:.1f}us difference.")
            print("  Read it as 'no measurable overhead at this resolution', not as zero.")
        print()


if __name__ == "__main__":
    sys.exit(main())
