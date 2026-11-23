# Test fixtures

`d3.v7.9.0.js` is the unminified d3 7.9.0 build, fetched from
`https://cdn.jsdelivr.net/npm/d3@7.9.0/dist/d3.js`. It is ISC licensed,
copyright Mike Bostock and the d3 contributors.

It is a third-party-sized classic script for
`tests/js_minify.rs`: the minifier must round-trip it with identical behaviour
on a small harness, and it must still parse after minification. Nothing here is
served by m6.
