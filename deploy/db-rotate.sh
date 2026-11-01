#!/usr/bin/env bash
# Rotate the telemetry database. Routing rows are never pruned by retention, so
# the only real bound on the file is rotation: archive it with the documented
# `VACUUM INTO` snapshot, then let the collector recreate an empty schema.
# The database is regenerable telemetry (a projection of the routing trace), so
# deleting the live file after a verified archive loses no authoritative state.
#
#   ./deploy/db-rotate.sh --check            # report only (size, threshold)
#   ./deploy/db-rotate.sh                    # rotate if >= threshold
#   ./deploy/db-rotate.sh --force            # rotate regardless
#
# Archives are kept in <prefix>/data/archive/, newest <keep> retained (default 2).
set -euo pipefail
PREFIX="$HOME/the-matrix"
UNIT_PREFIX="matrix"
THRESH_GB=8
KEEP=2
CHECK=0
FORCE=0

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX=$2; shift 2 ;;
    --unit-prefix) UNIT_PREFIX=$2; shift 2 ;;
    --threshold-gb) THRESH_GB=$2; shift 2 ;;
    --keep) KEEP=$2; shift 2 ;;
    --check) CHECK=1; shift ;;
    --force) FORCE=1; shift ;;
    -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
PREFIX=${PREFIX%/}
DB="$PREFIX/data/telemetry.sqlite"
ARCH_DIR="$PREFIX/data/archive"
THRESH=$((THRESH_GB * 1024 * 1024 * 1024))
SIZE=$(stat -c %s "$DB" 2>/dev/null || echo 0)
ARCH_N=$(ls -1 "$ARCH_DIR" 2>/dev/null | grep -c "\.sqlite$" || true)
UNIT="$UNIT_PREFIX-collect.service"
ACTIVE=$(systemctl --user is-active "$UNIT" 2>/dev/null || echo inactive)

report() { # $1 = action
  python3 - "$1" "$DB" "$SIZE" "$THRESH" "$ARCH_N" "$ACTIVE" <<'PY'
import json, sys
action, db, size, thresh, arch_n, active = sys.argv[1:7]
print(json.dumps({"action": action, "db": db, "db_bytes": int(size),
                  "threshold_bytes": int(thresh), "over_threshold": int(size) >= int(thresh),
                  "archives": int(arch_n), "collector_active": active}))
PY
}

if [ "$CHECK" = 1 ]; then report check; exit 0; fi
if [ "$SIZE" -lt "$THRESH" ] && [ "$FORCE" != 1 ]; then report below-threshold; exit 0; fi

STAMP=$(date -u +%Y%m%dT%H%M%SZ)
mkdir -p "$ARCH_DIR"
ARCHIVE="$ARCH_DIR/telemetry-$STAMP.sqlite"
systemctl --user stop "$UNIT"
# Same command as the runbook's documented snapshot (measured: seconds, not a
# .backup livelock); the collector is stopped so this is trivially consistent.
sqlite3 "$DB" "VACUUM INTO '$ARCHIVE'"
INTEG=$(sqlite3 "$ARCHIVE" "pragma integrity_check;")
[ "$INTEG" = "ok" ] || { echo "archive integrity_check failed: $INTEG" >&2; systemctl --user start "$UNIT"; exit 4; }
ARCH_BYTES=$(stat -c %s "$ARCHIVE")
rm -f "$DB" "$DB-wal" "$DB-shm"
systemctl --user start "$UNIT"
# keep the newest $KEEP archives
find "$ARCH_DIR" -maxdepth 1 -name "*.sqlite" -printf "%T@ %p\n" 2>/dev/null | sort -rn | tail -n +$((KEEP + 1)) | cut -d" " -f2- | xargs -r rm -f || true
python3 - "$ARCHIVE" "$ARCH_BYTES" "$STAMP" <<'PY'
import json, sys
import os
archive, n, stamp = sys.argv[1:4]
print(json.dumps({"action": "rotated", "archive": archive, "archive_bytes": int(n),
                  "stamp": stamp, "integrity": "ok",
                  "archives_kept": len([f for f in os.listdir(os.path.dirname(archive)) if f.endswith(".sqlite")])}))
PY
