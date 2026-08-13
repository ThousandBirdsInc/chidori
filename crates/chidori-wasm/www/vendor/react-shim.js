// ESM view of the vendored React UMD globals, for the import map in
// react.html: `@1kbirds/chidori-browser/react` does `import ... from 'react'`,
// and this shim resolves it to the UMD build already on the page — so the
// demo stays fully offline, no bundler, no CDN.
const R = window.React;
export default R;
export const {
  useState, useEffect, useRef, useCallback, useMemo, useReducer,
  createElement, Fragment, StrictMode,
} = R;
