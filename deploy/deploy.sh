#!/usr/bin/env bash
# Upgrade a THE MATRIX deployment: source -> build -> staged atomic install ->
# unit restart -> health gate -> (optional) automatic rollback.
#
#   ./deploy/deploy.sh --from array            # the documented path (rsync from build host)
#   ./deploy/deploy.sh --from local --src ~/matrix-build --no-build
#   ./deploy/deploy.sh --from blackie --src /srv/storage/work/the-matrix --build
#
# The script NEVER touches <prefix>/data. It only writes <prefix>/bin (staged,
# then atomically moved into place), <prefix>/bin/prev/<utc>-<sha8>/ (a copy of
# the binaries it replaced, with a manifest) and <prefix>/etc/deploy-log.jsonl.
#
# Exit codes: 0 ok, 2 usage, 3 health gate failed (and rollback failed or was
# not requested), 4 build failed, 5 no previous binary to roll back to.
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

PREFIX="$HOME/the-matrix"
UNIT_PREFIX="matrix"
FROM="array"
SRC=""
WORK=""
DO_BUILD=1
RESTART=1
AUTO_ROLLBACK=0
DRY=0
HEALTH_WAIT=25

while [ $# -gt 0 ]; do
  case "$1" in
    --from) FROM=$2; shift 2 ;;
    --src) SRC=$2; shift 2 ;;
    --work) WORK=$2; shift 2 ;;
    --prefix) PREFIX=$2; shift 2 ;;
    --unit-prefix) UNIT_PREFIX=$2; shift 2 ;;
    --build) DO_BUILD=1; shift ;;
    --no-build) DO_BUILD=0; shift ;;
    --no-restart) RESTART=0; shift ;;
    --auto-rollback) AUTO_ROLLBACK=1; shift ;;
    --dry-run) DRY=1; shift ;;
    --health-wait) HEALTH_WAIT=$2; shift 2 ;;
    -h|--help) sed -n '2,18p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

PREFIX=${PREFIX%/}
WORK=${WORK:-$PREFIX/build}
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
say() { echo "[deploy $(date -u +%H:%M:%SZ)] $*"; }

[ -f "$PREFIX/etc/matrix-install.json" ] || say "warning: no $PREFIX/etc/matrix-install.json (paths from flags/defaults)"

# ---------------------------------------------------------------- 1. source
case "$FROM" in
  local)
    [ -n "$SRC" ] || { echo "--from local needs --src" >&2; exit 2; }
    say "source: local $SRC"
    mkdir -p "$WORK"
    rsync -a --delete --exclude .git --exclude target/ "$SRC"/ "$WORK"/
    ;;
  *)
    say "source: $FROM:$SRC -> $WORK"
    : "${SRC:=$HOME/matrix-build/}"
    mkdir -p "$WORK"
    rsync -a --delete --exclude .git --exclude target/ "$FROM:$SRC" "$WORK"/
    ;;
esac

# ---------------------------------------------------------------- 2. build
if [ "$DO_BUILD" = 1 ]; then
  say "build: cargo build --release in $WORK"
  if ! ( cd "$WORK" && cargo build --release ) > "$PREFIX/etc/deploy-build.log" 2>&1; then
    echo "build FAILED, see $PREFIX/etc/deploy-build.log" >&2
    tail -30 "$PREFIX/etc/deploy-build.log" >&2
    exit 4
  fi
  WARN=$(grep -c '^warning:' "$PREFIX/etc/deploy-build.log" || true)
  say "build ok (warnings=$WARN)"
  BUILT="$WORK/target/release"
else
  say "build skipped"
  BUILT="$WORK/target/release"
fi
for b in matrix-collect matrix-app; do
  [ -x "$BUILT/$b" ] || { echo "missing built binary $BUILT/$b" >&2; exit 4; }
done

# --------------------------------------------------------- 3. stage + swap
STAGE="$PREFIX/bin/.stage"
rm -rf "$STAGE"; mkdir -p "$STAGE"
install -m 0755 "$BUILT/matrix-collect" "$STAGE/matrix-collect"
install -m 0755 "$BUILT/matrix-app" "$STAGE/matrix-app"
NEW_SHA_COLLECT=$(sha256sum "$STAGE/matrix-collect" | cut -d' ' -f1)
NEW_SHA_APP=$(sha256sum "$STAGE/matrix-app" | cut -d' ' -f1)
OLD_SHA_COLLECT=$(sha256sum "$PREFIX/bin/matrix-collect" | cut -d' ' -f1)
OLD_SHA_APP=$(sha256sum "$PREFIX/bin/matrix-app" | cut -d' ' -f1)

say "old collect=${OLD_SHA_COLLECT:0:12} app=${OLD_SHA_APP:0:12}"
say "new collect=${NEW_SHA_COLLECT:0:12} app=${NEW_SHA_APP:0:12}"

PREV="$PREFIX/bin/prev/$STAMP-${OLD_SHA_COLLECT:0:8}"
mkdir -p "$PREV"
cp -a "$PREFIX/bin/matrix-collect" "$PREV/matrix-collect"
cp -a "$PREFIX/bin/matrix-app" "$PREV/matrix-app"
python3 - "$PREV" "$STAMP" "$OLD_SHA_COLLECT" "$OLD_SHA_APP" "$NEW_SHA_COLLECT" "$NEW_SHA_APP" "$PREFIX" <<'PY'
import json, sys, subprocess
prev, stamp, oc, oa, nc, na, prefix = sys.argv[1:8]
def ver(path, flag):
    try:
        return subprocess.run([path, flag], capture_output=True, text=True, timeout=10).stdout.splitlines()[0]
    except Exception as e:
        return "unknown: %s" % e
json.dump({"stamp": stamp, "old_collect_sha": oc, "old_app_sha": oa,
           "new_collect_sha": nc, "new_app_sha": na,
           "old_collect_version": ver(prev + "/matrix-collect", "--version"),
           "old_app_version": ver(prev + "/matrix-app", "--help"),
           "prefix": prefix},
          open(prev + "/manifest.json", "w"), indent=1)
PY

if [ "$DRY" = 1 ]; then
  say "dry run: would have installed ${NEW_SHA_COLLECT:0:12}; previous saved at $PREV"
  rm -rf "$STAGE"
  exit 0
fi

# Same-directory rename: the running processes keep their inode, new starts get
# the new file; there is no window where the path is missing.
mv -f "$STAGE/matrix-collect" "$PREFIX/bin/matrix-collect.new"
mv -f "$STAGE/matrix-app" "$PREFIX/bin/matrix-app.new"
mv -f "$PREFIX/bin/matrix-collect.new" "$PREFIX/bin/matrix-collect"
mv -f "$PREFIX/bin/matrix-app.new" "$PREFIX/bin/matrix-app"
rm -rf "$STAGE"
say "installed; previous binaries saved at $PREV"

# ------------------------------------------------------------ 4. restart
if [ "$RESTART" = 1 ]; then
  systemctl --user daemon-reload >/dev/null 2>&1 || true
  systemctl --user restart "$UNIT_PREFIX-collect.service"
  SOCK=$(python3 -c "import json;print(json.load(open('$PREFIX/etc/matrix-install.json'))['socket'])" 2>/dev/null || echo "$PREFIX/run/collector.sock")
  for _ in $(seq 1 60); do [ -S "$SOCK" ] && break; sleep 0.25; done
  if systemctl --user cat "$UNIT_PREFIX-app.service" >/dev/null 2>&1; then
    systemctl --user restart "$UNIT_PREFIX-app.service"
    sleep 2
    if ! python3 "$HERE/health.py" --prefix "$PREFIX" --wait "$HEALTH_WAIT"; then
      say "HEALTH GATE FAILED on ${NEW_SHA_COLLECT:0:12}"
      if [ "$AUTO_ROLLBACK" = 1 ]; then
        say "auto-rollback to $PREV"
        "$HERE/rollback.sh" --prefix "$PREFIX" --unit-prefix "$UNIT_PREFIX" --to "$PREV" || true
      fi
      exit 3
    fi
  fi
fi

python3 - "$PREFIX" "$FROM" "$OLD_SHA_COLLECT" "$NEW_SHA_COLLECT" "$OLD_SHA_APP" "$NEW_SHA_APP" "$PREV" "$STAMP" <<'PY'
import json, sys
prefix, frm, oc, nc, oa, na, prev, stamp = sys.argv[1:9]
rec = {"ts": stamp, "source": frm, "old_collect_sha": oc, "new_collect_sha": nc,
       "old_app_sha": oa, "new_app_sha": na, "prev_dir": prev}
open(prefix + "/etc/deploy-log.jsonl", "a").write(json.dumps(rec) + "\n")
print(json.dumps({"ok": True, **rec}))
PY
