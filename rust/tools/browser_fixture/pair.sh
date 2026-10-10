#!/bin/sh
# Prints a fresh one-use pairing credential (valid five minutes) for the server started under --scratch.
set -eu
scratch=""; server=""
while [ $# -gt 0 ]; do
  case "$1" in
    --scratch) scratch="$2"; shift 2 ;;
    --server) server="$2"; shift 2 ;;
    *) echo "usage: pair.sh --scratch DIR --server T3_SERVER_BINARY" >&2; exit 2 ;;
  esac
done
[ -n "$scratch" ] && [ -n "$server" ] || { echo "usage: pair.sh --scratch DIR --server T3_SERVER_BINARY" >&2; exit 2; }
cd "$scratch/projects"
exec env -i HOME="$scratch/home" PATH="$scratch/bin:/usr/bin:/bin" TMPDIR="$scratch/tmp" "$server" pair --state-dir "$scratch/state"
