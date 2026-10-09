#!/usr/bin/env bash
# Publish a package's demo module and run its test suite against it.
#
#   scripts/e2e.sh <crypto|retry|rate-limit|cron|presence|api-keys|grid|lobby> <rust|csharp>     (needs `spacetime start`)
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PKG=${1:?usage: $0 <package> <rust|csharp>}
LANG_=${2:?usage: $0 <package> <rust|csharp>}
SERVER=${SERVER:-local}
DB=port-$PKG-$LANG_

cd "$ROOT"
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
