# SPD Project Memory

## Project: /Users/mgrosvenor/m6/spd

Simple Datagram Protocol (SDP) — HDLC-inspired framing over serial, for embedded MCUs.

### Key files
- `src/sdp.h` — public API (status codes, HAL struct, context struct, all function decls)
- `src/sdp.c` — full implementation
- `src/sdp_config.h` — compile-time config (SDP_WINDOW, SDP_SILENCE_TIMEOUT_MS)
- `CMakeLists.txt` — top-level build
- `tests/CMakeLists.txt` — test targets
- `setup.sh` — toolchain installer (brew/apt/dnf)

### Build
```
cmake -B build -S . && cmake --build build && cd build && ctest
```
cmake is at `/opt/homebrew/bin/cmake` (not in PATH by default).

### Tests (all passing as of implementation)
1. `test_crc` — CRC/framing/stuffing/seq (37 tests)
2. `test_e2e` — loopback pipe handshake + data (37 tests)
3. `test_e2e_socket` — Unix domain socket end-to-end (14 tests)
4-9. `cross_compile_*` — compile-only for ARM-CM4, Apple-M4, AVR-ATtiny1616, PIC-xc8(SKIP), ESP32-U4WDH(SKIP), x86_64-musl(SKIP if no musl-gcc)

### Key design decisions
- `sync_hunt_enabled` flag (set by `sdp_listen`): only device side counts 0xAA bytes;
  host side (via `sdp_connect`) accepts HDLC frames immediately in any state.
- Window tracking uses `seq_unacked(tx_seq, peer_ackseq)` computed from seq numbers,
  NOT a simple counter. This ensures piggybacked ACKSEQs in any frame open the window.
- `peer_ackseq` reset to SDP_SEQ_NONE on LINKED transition to avoid false "14 in flight" calc.
- `connect_on_frame` wrapper intercepts ANNOUNCE during `sdp_connect` blocking loop.
- Cross-compile tests use `tests/cross-headers/` shim (stdint.h, string.h) for bare-metal
  toolchains without libc (e.g., Homebrew arm-none-eabi-gcc configured --without-headers).

### Spec vs implementation notes
- SDP_SLOT_SIZE is 133 (not 132 as listed in spec — 3 header + 128 payload + 2 CRC).
- `sdp.c` never calls stdbool.h; all bool fields use uint8_t.
