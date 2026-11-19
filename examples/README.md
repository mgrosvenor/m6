# m6-examples

Eleven examples for the [m6](https://github.com/mgrosvenor/m6) web server framework, each building on the last.

## Prerequisites

**Clone m6 beside this repository, not inside it.** The custom renderer crates
reach m6-core by relative path (`../../../../m6/m6-core`), so the two checkouts
have to be siblings:

```
~/code/
├── m6/
└── m6-examples/
```

```bash
# m6's binaries, from source. They are not on crates.io.
git clone https://github.com/mgrosvenor/m6
cd m6 && cargo build --release --workspace
export PATH="$PWD/target/release:$PATH"

# TLS for development, once. m6-http is HTTPS only.
mkcert -install
```

Each example's `dev.sh` runs `examples/dev-keys.sh`, which writes the development
certificate and auth keys to `dev/` in a private per-user directory on first run,
so there is nothing to place or symlink by hand. The directory is
`${XDG_RUNTIME_DIR:-${TMPDIR:-$HOME/.cache}}/m6`, created with mode 700; set
`M6_DEV_HOME` to use another. Each example's configs name it as `@M6_DEV_HOME@`,
and its `dev.sh` writes a copy with the real path for m6 to read.

### Check the install

Example 05 runs the whole stack, and its test suite makes 96 checks against it.
Run that before anything else, and a failure is then a real answer about your
install rather than a puzzle several examples later:

```bash
cargo build --release        # the custom renderers
cd examples/05-cms
./dev.sh                     # one terminal
./test.sh                    # another
#   96 passed  0 failed  (96 checks)
```

## Examples

| # | Directory | What it covers |
|---|-----------|---------------|
| 01 | `examples/01-static/` | Static site: templates, assets, hand-authored JSON |
| 02 | `examples/02-blog/` | Blog: Markdown posts via `m6-md`, single JSON params file |
| 03 | `examples/03-contact/` | Contact form: custom Rust renderer, SMTP email, POST handling |
| 04 | `examples/04-auth/` | Login and protected pages: JWT auth, HttpOnly cookies, `require` |
| 05 | `examples/05-cms/` | CMS blog: full system with publish flow, route invalidation |
| 06 | `examples/06-systemd/` | Production: systemd units, hardening, scaling, deploy workflow |
| 07 | `examples/07-dev-to-production/` | Dev → production: single site.toml, system config split |
| 08 | `examples/08-logviewer/` | Log viewer: real-time log browsing, search, and filtering |
| 09 | `examples/09-global-deployment/` | Global deployment: multi-region Vultr/Render configuration |
| 10 | `examples/10-api-tokens/` | API tokens: long-lived Bearer JWTs for scripts and services |
| 11 | `examples/11-admin-dashboard/` | Admin API: perf, system, bench, logs, config, service restart |

## Quick start

```bash
cd examples/01-static
./dev.sh
# open https://localhost:8443
```

Each example has its own `dev.sh`. Examples with a custom Rust renderer need a build step first:

```bash
cd examples/03-contact
cargo build --release -p render-contact
./dev.sh
```

`dev.sh` stops only the processes belonging to its own example, so another m6
site on the same machine keeps running.

## Running the tests

Examples 05, 08, 10 and 11 have a `test.sh` that runs against a started stack.
`./m6-test-eg` starts each example, runs its tests, and stops it again:

```bash
./m6-test-eg          # every example that can run locally
./m6-test-eg 5 10     # just these
```

Example 05's is the one that covers the whole system, and m6's own build checks
run it on every change to m6. If you are changing m6 itself,
`M6_BUILD_HOST=... m6/tools/build-host-tests.sh` builds this repository against
your m6 tree and runs that suite, which is what catches an m6 interface change
breaking the code that uses it.

## Building custom renderers

The workspace `Cargo.toml` at the repo root includes all custom renderer crates so they share a
`target/` directory:

```bash
# Build all renderers
cargo build --release

# Or build one
cargo build --release -p render-contact
cargo build --release -p render-cms
cargo build --release -p render-admin
```
