#!/usr/bin/env bash
# Roll a THE MATRIX deployment back to a previously deployed binary pair.
#
#   ./deploy/rollback.sh                 # most recent previous pair
#   ./deploy/rollback.sh --list          # show what is available
#   ./deploy/rollback.sh --to ~/the-matrix/bin/prev/20261031T221500Z-abcdef12
#
# Restores <prefix>/bin/{matrix-collect,matrix-app} from the chosen prev dir and
# restarts the units, then runs the health gate. Data is never touched.
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
PREFIX="$HOME/the-matrix"
UNIT_PREFIX="matrix"
TO=""
LIST=0
RESTART=1
HEALTH_WAIT=25

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX=$2; shift 2 ;;
    --unit-prefix) UNIT_PREFIX=$2; shift 2 ;;
    --to) TO=$2; shift 2 ;;
    --list) LIST=1; shift ;;
    --no-restart) RESTART=0; shift ;;
    --health-wait) HEALTH_WAIT=$2; shift 2 ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
PREFIX=${PREFIX%/}
PREVROOT="$PREFIX/bin/prev"
UNIT_DIR="$HOME/.config/systemd/user"

if [ "$LIST" = 1 ]; then
  for d in "$PREVROOT"/*/; do
    [ -f "${d}manifest.json" ] && python3 -c "
import json,sys
m=json.load(open(sys.argv[1]))
print(m['stamp'], m['old_collect_sha'][:12], m['old_app_sha'][:12], sys.argv[2])" "${d}manifest.json" "${d%/}"
  done
  exit 0
fi

if [ -n "$TO" ]; then
  PREV=${TO%/}
else
  PREV=$(ls -1d "$PREVROOT"/*/ 2>/dev/null | sort | tail -1)
  PREV=${PREV%/}
fi
[ -f "$PREV/manifest.json" ] || { echo "no previous deployment at '$PREV' (see --list)" >&2; exit 5; }

for b in matrix-collect matrix-app; do
  [ -x "$PREV/$b" ] || { echo "missing $PREV/$b" >&2; exit 5; }
done

echo "[rollback] restoring $PREV"
install -m 0755 "$PREV/matrix-collect" "$PREFIX/bin/.matrix-collect.rb"
install -m 0755 "$PREV/matrix-app" "$PREFIX/bin/.matrix-app.rb"
mv -f "$PREFIX/bin/.matrix-collect.rb" "$PREFIX/bin/matrix-collect"
mv -f "$PREFIX/bin/.matrix-app.rb" "$PREFIX/bin/matrix-app"
RESTORED_COLLECT=$(sha256sum "$PREFIX/bin/matrix-collect" | cut -d' ' -f1)
RESTORED_APP=$(sha256sum "$PREFIX/bin/matrix-app" | cut -d' ' -f1)

# Restore the unit files that were in force for this pair, when the deploy that
# saved it kept them. Without this a rollback can leave a unit asking for a flag
# the restored binary does not know (Stage 8: --max-db-mb), and the "restored"
# collector never starts. Reported, not assumed.
RESTORED_UNITS=""
if [ -d "$PREV/units" ] && [ -n "$(ls -A "$PREV/units" 2>/dev/null)" ]; then
  install -m 0644 "$PREV/units/"*.service "$UNIT_DIR/"
  RESTORED_UNITS=$(cd "$PREV/units" && ls -1 *.service | tr '\n' ' ')
  systemctl --user daemon-reload >/dev/null 2>&1 || true
  echo "[rollback] units restored: $RESTORED_UNITS"
fi

if [ "$RESTART" = 1 ] && systemctl --user cat "$UNIT_PREFIX-collect.service" >/dev/null 2>&1; then
  systemctl --user restart "$UNIT_PREFIX-collect.service"
  SOCK=$(python3 -c "import json;print(json.load(open('$PREFIX/etc/matrix-install.json'))['socket'])" 2>/dev/null || echo "$PREFIX/run/collector.sock")
  for _ in $(seq 1 60); do [ -S "$SOCK" ] && break; sleep 0.25; done
  if systemctl --user cat "$UNIT_PREFIX-app.service" >/dev/null 2>&1; then
    systemctl --user restart "$UNIT_PREFIX-app.service"
    sleep 2
    python3 "$HERE/health.py" --prefix "$PREFIX" --wait "$HEALTH_WAIT" || { echo "[rollback] health gate FAILED" >&2; exit 3; }
  fi
fi

python3 - "$PREFIX" "$PREV" "$RESTORED_COLLECT" "$RESTORED_APP" "$RESTORED_UNITS" <<'PY'
import json, sys
prefix, prev, c, a, units = sys.argv[1:6]
rec = {"ts": __import__("time").strftime("%Y%m%dT%H%M%SZ", __import__("time").gmtime()),
       "restored_from": prev, "collect_sha": c, "app_sha": a,
       "units_restored": units.split()}
open(prefix + "/etc/deploy-log.jsonl", "a").write(json.dumps({"rollback": rec}) + "\n")
print(json.dumps({"ok": True, "restored_from": prev, "collect_sha": c, "app_sha": a,
                  "units_restored": rec["units_restored"]}))
PY
