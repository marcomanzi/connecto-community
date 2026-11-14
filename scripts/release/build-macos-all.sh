#!/usr/bin/env bash
# All three macOS artifacts: aarch64, x86_64 and universal.
set -euo pipefail
here="$(dirname "${BASH_SOURCE[0]}")"
for arch in aarch64 x86_64 universal; do "$here/build-macos.sh" "$arch"; done
