#!/usr/bin/env bash
# Uninstall a THE MATRIX deployment.
#
#   ./deploy/uninstall.sh                 # stop+disable+remove units, KEEP data
#   ./deploy/uninstall.sh --purge --confirm-data-dir ~/matrix-tgt/data
#
# Data policy: the telemetry database is NEVER deleted implicitly. Purging
# needs both --purge and --confirm-data-dir <exact path>; the path must be the
# data directory recorded by install.sh and it must not be the live deployment
# directory unless --purge-live is passed as well. Nothing outside that
# directory is removed; bin/prev/ (the rollback copies) and etc/ are kept
# unless --remove-bin is given.
set -euo pipefail
PREFIX="$HOME/the-matrix"
UNIT_PREFIX="matrix"
UNIT_DIR="$HOME/.config/systemd/user"
PURGE=0
CONFIRM=""
PURGE_LIVE=0
REMOVE_BIN=0

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX=$2; shift 2 ;;
    --unit-prefix) UNIT_PREFIX=$2; shift 2 ;;
    --unit-dir) UNIT_DIR=$2; shift 2 ;;
    --purge) PURGE=1; shift ;;
    --confirm-data-dir) CONFIRM=$2; shift 2 ;;
    --purge-live) PURGE_LIVE=1; shift ;;
    --remove-bin) REMOVE_BIN=1; shift ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
PREFIX=${PREFIX%/}
INSTALL_JSON="$PREFIX/etc/matrix-install.json"

if [ -f "$INSTALL_JSON" ]; then
  IFS='|' read -r UNIT_PREFIX UNIT_DIR DB < <(python3 - "$INSTALL_JSON" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
print("|".join([d.get("unit_prefix", "matrix"), d.get("unit_dir", ""), d.get("db", "")]))
PY
)
  DATA_DIR=$(dirname "$DB")
else
  DATA_DIR="$PREFIX/data"
fi
UNIT_DIR=${UNIT_DIR:-$HOME/.config/systemd/user}

echo "[uninstall] prefix=$PREFIX unit-prefix=$UNIT_PREFIX data=$DATA_DIR"

# 1. stop + disable + remove units
for u in "$UNIT_PREFIX-app" "$UNIT_PREFIX-collect"; do
  if systemctl --user cat "$u.service" >/dev/null 2>&1; then
    systemctl --user stop "$u.service" || true
    systemctl --user disable "$u.service" >/dev/null 2>&1 || true
    rm -f "$UNIT_DIR/$u.service"
    echo "[uninstall] removed $UNIT_DIR/$u.service"
  fi
done
systemctl --user daemon-reload
systemctl --user reset-failed "$UNIT_PREFIX-app.service" "$UNIT_PREFIX-collect.service" 2>/dev/null || true

# 2. data: keep unless explicitly confirmed
KEPT="kept"
if [ "$PURGE" = 1 ]; then
  [ -n "$CONFIRM" ] || { echo "--purge requires --confirm-data-dir <path>" >&2; exit 2; }
  case "$CONFIRM" in "~/"*) CONFIRM="$HOME/${CONFIRM#\~/}" ;; esac
  CONFIRM=${CONFIRM%/}
  RDIR=$(realpath -m "$DATA_DIR")
  RCONF=$(realpath -m "$CONFIRM")
  LIVE=$(realpath -m "$HOME/the-matrix/data")
  [ "$RDIR" = "$RCONF" ] || { echo "confirm mismatch: '$RCONF' != data dir '$RDIR'" >&2; exit 2; }
  if [ "$RDIR" = "$LIVE" ] && [ "$PURGE_LIVE" != 1 ]; then
    echo "refusing to purge the LIVE data dir ($LIVE); pass --purge-live if that is really intended" >&2
    exit 2
  fi
  [ "$RDIR" != "/" ] || { echo "refusing to purge /" >&2; exit 2; }
  # sanity: it must look like a matrix data dir
  if [ -e "$RDIR" ] && ! ls "$RDIR" | grep -q '\.sqlite\|\.sqlite-wal\|\.sqlite-shm'; then
    echo "refusing to purge '$RDIR': no sqlite files inside (not a matrix data dir?)" >&2
    exit 2
  fi
  BYTES=$(du -sb "$RDIR" 2>/dev/null | cut -f1 || echo 0)
  rm -rf -- "$RDIR"
  echo "[uninstall] purged $RDIR ($BYTES bytes)"
  KEPT="purged"
fi

# 3. binaries (optional) and the install record
if [ "$REMOVE_BIN" = 1 ]; then rm -rf "$PREFIX/bin"; echo "[uninstall] removed $PREFIX/bin"; fi
rm -f "$INSTALL_JSON"

python3 - "$PREFIX" "$UNIT_PREFIX" "$DATA_DIR" "$KEPT" <<'PY'
import json, os, sys
prefix, up, data, kept = sys.argv[1:5]
size = 0
if os.path.isdir(data):
    for root, _, files in os.walk(data):
        for f in files:
            try: size += os.path.getsize(os.path.join(root, f))
            except OSError: pass
print(json.dumps({"ok": True, "prefix": prefix, "units_removed": [up + "-collect", up + "-app"],
                  "data_dir": data, "data": kept, "data_bytes_present": size,
                  "data_exists": os.path.isdir(data)}))
PY
