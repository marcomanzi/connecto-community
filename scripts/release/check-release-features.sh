#!/usr/bin/env bash
# Fails the release build if the feature graph of the shipped programs turns on
# a feature that switches off TLS checks: connecto-net's `test-relay`, or iroh's
# `test-utils` (which only `test-relay` turns on).
#
#   scripts/release/check-release-features.sh TARGET   check the release graph for TARGET
#   scripts/release/check-release-features.sh --self-test
#                                                      prove the check trips when the feature is on
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FORBIDDEN='feature "(test-relay|test-utils)"'

# The lines of a `cargo tree` text that name a forbidden feature; status 1 if any.
forbidden_lines() {
  printf '%s\n' "$1" | grep -E "$FORBIDDEN" || return 1
}

if [ "${1:-}" = "--self-test" ]; then
  # 1. A tree that names the feature is refused (the text cargo prints for it).
  canned='connecto-net v0.1.5
└── connecto-net feature "test-relay"
    └── iroh feature "test-utils"'
  if forbidden_lines "$canned" >/dev/null; then :; else
    echo "self-test FAILED: the canned forbidden feature was not seen" >&2; exit 1
  fi
  # 2. Real cargo output with the feature forced on is refused too.
  forced="$(cd "$ROOT" && cargo tree --edges features --locked -p connecto-net --features test-relay 2>&1)"
  if forbidden_lines "$forced" >/dev/null; then :; else
    echo "self-test FAILED: cargo's own output with test-relay was not seen" >&2; exit 1
  fi
  echo "self-test ok: the check trips on test-relay (canned and real cargo output)"
  exit 0
fi

TARGET="${1:?usage: check-release-features.sh TARGET | --self-test}"
TREE="$(cd "$ROOT" && cargo tree --edges features --locked --target "$TARGET" -p connecto-app -p connecto-hook 2>&1)" \
  || { echo "error: cargo tree failed for $TARGET" >&2; printf '%s\n' "$TREE" >&2; exit 1; }
if hits="$(forbidden_lines "$TREE")"; then
  echo "error: the release feature graph for $TARGET turns on a feature that disables TLS checks:" >&2
  printf '%s\n' "$hits" >&2
  exit 1
fi
echo "==> release feature graph for $TARGET has no test-relay or test-utils"
