#!/usr/bin/env bash
# Publish both ports of a package and check they expose the same schema.
#
#   scripts/parity.sh <package>
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PKG=${1:?usage: $0 <package>}
cd "$ROOT"
for lang in rust csharp; do
  spacetime publish -s "${SERVER:-local}" -y "port-$PKG-$lang" -p "$PKG/demo/$lang" --delete-data >/dev/null
done
node tests/schema-parity.mjs "port-$PKG-rust" "port-$PKG-csharp"
