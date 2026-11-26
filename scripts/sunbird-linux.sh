#!/bin/sh
set -eu
exec "$(cd "$(dirname -- "$0")" && pwd)/nyaacord-linux.sh" "$@"
