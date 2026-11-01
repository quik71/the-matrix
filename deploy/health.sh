#!/usr/bin/env bash
# Health check wrapper: see health.py for the assertions.
#   ./deploy/health.sh --prefix ~/the-matrix --wait 20
set -euo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
exec python3 "$here/health.py" "$@"
