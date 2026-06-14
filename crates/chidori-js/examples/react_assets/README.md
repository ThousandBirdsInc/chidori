# Vendored React (for the chidori-js demo/tests)

These are unmodified UMD builds fetched from npm, used to prove that real React
executes on the pure-Rust engine and to drive `examples/react_agent_demo.rs`:

- `react.js`              — react@18.3.1 `umd/react.production.min.js`
- `react-dom-server.js`   — react-dom@18.3.1 `umd/react-dom-server-legacy.browser.production.min.js`

React is MIT licensed (© Meta Platforms, Inc. and affiliates). The legacy
server build is used because `renderToStaticMarkup` / `renderToString` are
synchronous and need no scheduler/streams.
