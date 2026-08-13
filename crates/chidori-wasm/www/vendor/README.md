# Vendored React (demo page only)

`react.production.min.js` / `react-dom.production.min.js` are the React
18.3.1 UMD builds, vendored so `www/react.html` runs fully offline from a
static file server (same policy as the engine's vendored React bundles in
`crates/chidori-js/examples/react_assets/`). `react-shim.js` re-exports the
UMD global as an ES module for the page's import map.
