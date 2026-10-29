#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "${script_dir}/../../.." && pwd)"
target_dir="${PORTABLE_TARGET_DIR:-${repo_root}/target}"
artifact="${target_dir}/x86_64-unknown-linux-musl/release/polaris"

cd -- "${repo_root}"
bash scripts/build-portable-binary.sh
printf '%s\n' "${artifact}"
