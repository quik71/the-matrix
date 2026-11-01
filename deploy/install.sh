#!/usr/bin/env bash
# Install (or re-install) a THE MATRIX deployment as systemd *user* units.
#
#   ./deploy/install.sh                       # live layout, ~/the-matrix, units "matrix"
#   ./deploy/install.sh --prefix ~/matrix-tgt --unit-prefix matrixtgt --no-app
#   ./deploy/install.sh --render-only --prefix /tmp/x   # render units, install nothing
#
# What it does, in order:
#   1. creates <prefix>/{bin,data,run,logs} (never deletes anything)
#   2. renders deploy/units/*.service.in with the chosen paths into <unit-dir>
#   3. systemctl --user daemon-reload; enable [--now] the units
#   4. optionally installs the host journald caps (deploy/journald-caps.sh)
#   5. runs deploy/health.sh when it starts the collector
#
# The data directory is only ever created, never truncated: re-running install
# on an existing deployment adopts the existing database.
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

PREFIX="$HOME/the-matrix"
UNIT_PREFIX="matrix"
UNIT_DIR="$HOME/.config/systemd/user"
TRACE="/home/main/strata-0.2.4.2/data/routing-trace.bin"
ENGINE_CONF="/home/main/strata-0.2.4.2/strata-iq3_s.json"
ENGINE="127.0.0.1:8080"
MONITOR="DP-1"
RAIN="on"
VIEW="0"
EXTRA_ARGS=""
BIN_SRC=""
START=1
WITH_APP=1
JOURNALD=0
RENDER_ONLY=0

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX=$2; shift 2 ;;
    --unit-prefix) UNIT_PREFIX=$2; shift 2 ;;
    --unit-dir) UNIT_DIR=$2; shift 2 ;;
    --trace) TRACE=$2; shift 2 ;;
    --engine-conf) ENGINE_CONF=$2; shift 2 ;;
    --engine) ENGINE=$2; shift 2 ;;
    --monitor) MONITOR=$2; shift 2 ;;
    --rain) RAIN=$2; shift 2 ;;
    --view) VIEW=$2; shift 2 ;;
    --extra-args) EXTRA_ARGS=$2; shift 2 ;;
    --bin-src) BIN_SRC=$2; shift 2 ;;
    --no-start) START=0; shift ;;
    --no-app) WITH_APP=0; shift ;;
    --no-journald) JOURNALD=0; shift ;;
    --journald) JOURNALD=1; shift ;;
    --render-only) RENDER_ONLY=1; START=0; shift ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

PREFIX=${PREFIX%/}
SOCKET="$PREFIX/run/collector.sock"
DB="$PREFIX/data/telemetry.sqlite"
STATS="$PREFIX/run/live-stats.json"

mkdir -p "$PREFIX/bin" "$PREFIX/data" "$PREFIX/run" "$PREFIX/logs" "$PREFIX/etc"

if [ -n "$BIN_SRC" ]; then
  for b in matrix-collect matrix-app; do
    install -m 0755 "$BIN_SRC/$b" "$PREFIX/bin/$b"
  done
fi
for b in matrix-collect matrix-app; do
  [ -x "$PREFIX/bin/$b" ] || { echo "missing binary: $PREFIX/bin/$b (use --bin-src)" >&2; exit 1; }
done

render() { # template out-file
  local tpl=$1 dst=$2
  sed -e "s|@PREFIX@|$PREFIX|g" \
      -e "s|@UNIT_PREFIX@|$UNIT_PREFIX|g" \
      -e "s|@SOCKET@|$SOCKET|g" \
      -e "s|@DB@|$DB|g" \
      -e "s|@STATS@|$STATS|g" \
      -e "s|@TRACE@|$TRACE|g" \
      -e "s|@ENGINE_CONF@|$ENGINE_CONF|g" \
      -e "s|@ENGINE@|$ENGINE|g" \
      -e "s|@MONITOR@|$MONITOR|g" \
      -e "s|@RAIN@|$RAIN|g" \
      -e "s|@VIEW@|$VIEW|g" \
      -e "s|@EXTRA_ARGS@|$EXTRA_ARGS|g" \
      "$tpl" > "$dst"
}

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
render "$HERE/units/matrix-collect.service.in" "$TMP/$UNIT_PREFIX-collect.service"
render "$HERE/units/matrix-app.service.in" "$TMP/$UNIT_PREFIX-app.service"

if [ "$RENDER_ONLY" = 1 ]; then
  cat "$TMP/$UNIT_PREFIX-collect.service" "$TMP/$UNIT_PREFIX-app.service"
  exit 0
fi

mkdir -p "$UNIT_DIR"
install -m 0644 "$TMP/$UNIT_PREFIX-collect.service" "$UNIT_DIR/$UNIT_PREFIX-collect.service"
install -m 0644 "$TMP/$UNIT_PREFIX-app.service" "$UNIT_DIR/$UNIT_PREFIX-app.service"

# Ops tools travel with the install: the health check and the runbook (which the
# unit's Documentation= points at).
install -m 0755 "$HERE/health.py" "$PREFIX/bin/health.py"
[ -f "$HERE/OPS-RUNBOOK.md" ] && install -m 0644 "$HERE/OPS-RUNBOOK.md" "$PREFIX/OPS-RUNBOOK.md"

python3 - "$PREFIX" "$UNIT_PREFIX" "$UNIT_DIR" "$SOCKET" "$DB" "$STATS" <<'PY'
import json, sys
prefix, up, ud, sock, db, stats = sys.argv[1:7]
json.dump({"prefix": prefix, "unit_prefix": up, "unit_dir": ud, "socket": sock,
           "db": db, "stats": stats}, open(prefix + "/etc/matrix-install.json", "w"), indent=1)
PY

systemctl --user daemon-reload
systemctl --user enable "$UNIT_PREFIX-collect.service" >/dev/null
if [ "$WITH_APP" = 1 ]; then systemctl --user enable "$UNIT_PREFIX-app.service" >/dev/null; fi

if [ "$START" = 1 ]; then
  systemctl --user restart "$UNIT_PREFIX-collect.service"
  # wait for the socket before the app tries to connect
  for _ in $(seq 1 40); do [ -S "$SOCKET" ] && break; sleep 0.25; done
  if [ "$WITH_APP" = 1 ]; then systemctl --user restart "$UNIT_PREFIX-app.service"; fi
  sleep 2
  if [ "$WITH_APP" = 1 ]; then
    python3 "$HERE/health.py" --prefix "$PREFIX" --wait 20 || {
      echo "health check FAILED after install" >&2; exit 1; }
  else
    systemctl --user is-active "$UNIT_PREFIX-collect.service"
  fi
fi

if [ "$JOURNALD" = 1 ]; then "$HERE/journald-caps.sh" install; fi

echo "installed $UNIT_PREFIX-{collect,app} in $UNIT_DIR (prefix $PREFIX)"
