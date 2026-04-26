#!/bin/bash
# Build agent-browser and ad-hoc codesign the resulting binary so the macOS
# Accessibility (TCC) grant survives recompiles. Without ad-hoc signing,
# every `cargo build` produces a new binary identity and the AX grant
# revokes — Chrome's autofill-picker AX walk will start failing silently.
#
# Usage:
#   ./build-signed.sh             # debug build (default)
#   ./build-signed.sh --release   # release build
#
# After the first run on a fresh machine the system will prompt for the AX
# grant. Approve in System Settings → Privacy & Security → Accessibility,
# then re-run. The grant persists across rebuilds because the ad-hoc
# signature keeps the binary identity stable.

set -euo pipefail
cd "$(dirname "$0")"

PROFILE="debug"
if [[ "${1:-}" == "--release" ]]; then
  PROFILE="release"
  cargo build --release
else
  cargo build
fi

BIN="target/${PROFILE}/agent-browser"
if [[ ! -x "$BIN" ]]; then
  echo "build-signed.sh: expected binary at $BIN but did not find it" >&2
  exit 1
fi

codesign --force --sign - "$BIN"
echo "Built + ad-hoc signed: $(pwd)/$BIN"

# Stale daemon serves OLD code. Reap it so the next AB call re-spawns
# against the freshly compiled binary.
pkill -f "agent-browser/cli/target/${PROFILE}/agent-browser" 2>/dev/null || true
