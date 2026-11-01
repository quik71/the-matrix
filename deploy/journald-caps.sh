#!/usr/bin/env bash
# Bound the journal for the THE MATRIX units. Host-wide journald caps are the
# only ones systemd offers for *total* size (per-user journals are managed by
# the same journald instance), so this drop-in is installed with sudo; the
# per-unit ingest limit lives in the unit files themselves (RateLimit*).
#
#   ./deploy/journald-caps.sh            # install + verify (needs sudo)
#   ./deploy/journald-caps.sh --show     # show effective [Journal] config
#   ./deploy/journald-caps.sh --remove   # remove the drop-in
#
# Chosen values (measured in Stage 8, see artifacts/log-bounds.json):
#   SystemMaxUse=512M   -> hard ceiling for every journal file on this host
#   SystemMaxFileSize=64M -> no single journal file can exceed 64 MiB
#   MaxFileSec=1day     -> daily rotation even when nothing is quiet
#   SystemKeepFree=4G   -> never take the last 4 GiB of the filesystem
#   MaxRetentionSec=14day
set -euo pipefail
DROPIN=/etc/systemd/journald.conf.d/60-matrix-bounds.conf
CONTENT='[Journal]
SystemMaxUse=512M
SystemMaxFileSize=64M
SystemKeepFree=4G
MaxFileSec=1day
MaxRetentionSec=14day
'

show() {
  echo "--- effective [Journal] (systemd-analyze cat-config) ---"
  systemd-analyze cat-config systemd/journald.conf | grep -vE '^\s*#|^\s*$'
  echo "--- journal size ---"
  journalctl --disk-usage
}

case "${1:-install}" in
  --show|show) show; exit 0 ;;
  --remove|remove)
    sudo rm -f "$DROPIN"
    sudo systemctl restart systemd-journald
    echo "removed $DROPIN"
    show
    exit 0 ;;
  install|--install) ;;
  *) echo "usage: $0 [--install|--show|--remove]" >&2; exit 2 ;;
esac

sudo install -d -m 0755 /etc/systemd/journald.conf.d
echo "$CONTENT" | sudo tee "$DROPIN" >/dev/null
sudo systemctl restart systemd-journald
sleep 1
show
