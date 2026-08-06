#!/usr/bin/env bash
# Vendor the curated Node.js core test subset for the node-compat harness.
#
# Downloads each listed test/parallel file from the pinned Node release into
# crates/chidori/tests/node_compat/suite/. Candidates that 404 (renamed or
# absent in the pinned release) are reported and skipped, so the list can
# carry best guesses. Re-run after editing CANDIDATES or bumping NODE_VERSION;
# then run the harness with NODE_COMPAT_UPDATE=1 to refresh expectations and
# the report.
set -euo pipefail

NODE_VERSION="v22.12.0"
DEST="$(cd "$(dirname "$0")/.." && pwd)/crates/chidori/tests/node_compat/suite"
BASE="https://raw.githubusercontent.com/nodejs/node/${NODE_VERSION}/test/parallel"

CANDIDATES=(
  # querystring
  test-querystring.js
  test-querystring-escape.js
  test-querystring-maxKeys-non-finite.js
  test-querystring-multichar-separator.js
  # punycode
  test-punycode.js
  # path
  test-path.js
  test-path-basename.js
  test-path-dirname.js
  test-path-extname.js
  test-path-isabsolute.js
  test-path-join.js
  test-path-normalize.js
  test-path-parse-format.js
  test-path-relative.js
  test-path-resolve.js
  test-path-zero-length-strings.js
  # events
  test-event-emitter-add-listeners.js
  test-event-emitter-emit-context.js
  test-event-emitter-errors.js
  test-event-emitter-get-max-listeners.js
  test-event-emitter-listener-count.js
  test-event-emitter-listeners.js
  test-event-emitter-max-listeners.js
  test-event-emitter-num-args.js
  test-event-emitter-once.js
  test-event-emitter-prepend.js
  test-event-emitter-remove-all-listeners.js
  test-event-emitter-remove-listeners.js
  test-event-emitter-special-event-names.js
  test-event-emitter-subclass.js
  test-event-emitter-symbols.js
  test-events-once.js
  # string_decoder
  test-string-decoder.js
  test-string-decoder-end.js
  # url (legacy API)
  test-url-parse-format.js
  test-url-relative.js
  test-url-format-invalid-input.js
  test-url-fileurltopath.js
  test-url-pathtofileurl.js
  # buffer
  test-buffer-concat.js
  test-buffer-tojson.js
  test-buffer-isencoding.js
  test-buffer-from.js
  test-buffer-compare.js
  # zlib
  test-zlib-convenience-methods.js
  test-zlib-sync-no-event.js
  test-zlib-empty-buffer.js
  test-zlib-zero-byte.js
  test-zlib-brotli.js
  test-zlib-brotli-from-string.js
  test-zlib-deflate-constructors.js
  test-zlib-not-string-or-buffer.js
  # util
  test-util-inherits.js
  test-util-promisify.js
  test-util-format.js
  test-util-types.js
  test-util-deprecate.js
  test-util-isDeepStrictEqual.js
  # assert
  test-assert.js
  test-assert-async.js
  test-assert-fail.js
  # net helpers
  test-net-isip.js
  test-net-isipv4.js
  test-net-isipv6.js
  # timers
  test-timers-zero-timeout.js
  test-timers-clearImmediate.js
  test-timers-same-timeout-wrong-list-deleted.js
  test-timers-immediate-queue.js
  # diagnostics_channel
  test-diagnostics-channel-pub-sub.js
  test-diagnostics-channel-has-subscribers.js
  test-diagnostics-channel-symbol-named.js
  test-diagnostics-channel-safe-subscriber-errors.js
  # async_hooks (AsyncLocalStorage)
  test-async-local-storage-snapshot.js
  test-async-local-storage-bind.js
  # streams
  test-stream-push-strings.js
  test-stream-writable-finished-state.js
  test-stream-writable-ended-state.js
  test-stream-readable-data.js
  test-stream-transform-callback-twice.js
  test-stream-duplex-writable-finished.js
  test-stream-passthrough-drain.js
  # module
  test-module-isBuiltin.js
  # process surface
  test-process-env-allowed-flags.js
)

mkdir -p "$DEST"
echo "$NODE_VERSION" > "$(dirname "$DEST")/NODE_VERSION"

kept=0
skipped=0
for name in "${CANDIDATES[@]}"; do
  case "$name" in *\#*|*\?*) continue;; esac
  code=$(curl -s -o "$DEST/$name" -w "%{http_code}" "$BASE/$name")
  if [ "$code" = "200" ]; then
    kept=$((kept + 1))
  else
    rm -f "$DEST/$name"
    skipped=$((skipped + 1))
    echo "skip ($code): $name"
  fi
done
echo "vendored $kept files into $DEST ($skipped candidates absent in $NODE_VERSION)"
