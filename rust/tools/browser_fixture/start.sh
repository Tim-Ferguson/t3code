#!/bin/sh
# Starts an isolated T3 Code Rust server in a rich, fake-only state. See README.md.
here=$(cd "$(dirname "$0")" && pwd)
exec node "$here/start-main.mjs" "$@"
