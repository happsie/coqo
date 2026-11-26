#!/usr/bin/env bash
# Regression test for check-mod-boundary.sh: run it against
# fixtures/mod-boundary and expect it to flag bad-mod -> coqo-kernel only.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fixture="$here/../fixtures/mod-boundary/Cargo.toml"

if output="$("$here/check-mod-boundary.sh" "$fixture" 2>&1)"; then
  echo "FAIL: check-mod-boundary.sh passed on a fixture with a violation:" >&2
  echo "$output" >&2
  exit 1
fi

fail=0
grep -q 'bad-mod -> coqo-kernel' <<<"$output" || { echo "FAIL: bad-mod -> coqo-kernel not reported" >&2; fail=1; }
if grep -q 'good-mod ->' <<<"$output"; then echo "FAIL: good-mod wrongly reported" >&2; fail=1; fi
if grep -q 'coqo-mod-api (' <<<"$output"; then echo "FAIL: coqo-mod-api wrongly reported" >&2; fail=1; fi

if [[ $fail -ne 0 ]]; then
  echo "$output" >&2
  exit 1
fi
echo "check-mod-boundary.sh fixture test OK"
