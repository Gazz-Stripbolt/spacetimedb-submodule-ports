#!/usr/bin/env bash
# Publish a package's demo module and run its test suite against it.
#
#   scripts/e2e.sh <package> <rust|csharp>     (needs `spacetime start`)
#
# resend, stripe, posthog, auth (OAuth) and agents send HTTP to the suites' mocks on 127.0.0.1, which
# needs the loopback-enabled server from scripts/dev-server.sh.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PKG=${1:?usage: $0 <package> <rust|csharp>}
LANG_=${2:?usage: $0 <package> <rust|csharp>}
SERVER=${SERVER:-local}
DB=port-$PKG-$LANG_

cd "$ROOT"
# Build-time API bases for the mocks (see resend/README.md and stripe/README.md).
export RESEND_API_BASE=${RESEND_API_BASE:-http://127.0.0.1:4201}
export STRIPE_API_BASE=${STRIPE_API_BASE:-http://127.0.0.1:4202}
echo "== publish $PKG/demo/$LANG_ as $DB"
spacetime publish -s "$SERVER" -y "$DB" -p "$PKG/demo/$LANG_" --delete-data >/dev/null
case "$PKG" in
  cron) suites=(cron-vectors cron) ;;
  *) suites=("$PKG") ;;
esac
for suite in "${suites[@]}"; do
  echo "== tests/$suite.test.mjs"
  DB=$DB node "tests/$suite.test.mjs"
done
