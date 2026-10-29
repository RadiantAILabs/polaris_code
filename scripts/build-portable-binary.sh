#!/usr/bin/env bash
# Build the benchmark artifact against musl, rather than the host's glibc.
#
# The harness copies this artifact directly into task images, whose glibc can
# be older than the machine that runs the evaluation. Building in the pinned
# Rust Alpine image gives us a libc-independent x86_64 ELF without adding a
# cross compiler to an evaluator's host.
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "${script_dir}/.." && pwd)"
# Set PORTABLE_TARGET_DIR to keep the build output on another volume.
target_dir="${PORTABLE_TARGET_DIR:-${repo_root}/target}"
artifact="${target_dir}/x86_64-unknown-linux-musl/release/polaris-code"

# This digest is the x86_64 image verified for this repository's Rust 1.97
# MSRV. Updating it is an intentional build-environment change.
builder_image="rust@sha256:3c38f3f82c2f3d73da3b38e18d279393a04cb43ddded0e35088a8c3324d40900"

command -v docker >/dev/null || {
    echo "error: docker is required to build the portable binary" >&2
    exit 2
}

mkdir -p "$(dirname -- "${artifact}")"
docker run --rm \
    --user "$(id -u):$(id -g)" \
    --volume "${repo_root}:/workspace" \
    --volume "${target_dir}:/cargo-target" \
    --workdir /workspace \
    --env CARGO_TARGET_DIR=/cargo-target \
    "${builder_image}" \
    cargo build --quiet --locked --release --target x86_64-unknown-linux-musl \
        -p polaris_code_core --bin polaris-code

test -x "${artifact}"
file "${artifact}"
if readelf -d "${artifact}" 2>/dev/null | grep -q '(NEEDED)'; then
    echo "error: portable artifact unexpectedly has dynamic library dependencies" >&2
    exit 1
fi
glibc_symbols="$(objdump -T "${artifact}" 2>/dev/null | \
    sed -n 's/.*\(GLIBC_[0-9][0-9.]*\).*/\1/p' | sort -Vu || true)"
echo "portable binary: ${artifact}"
if [[ -n "${glibc_symbols}" ]]; then
    printf 'required GLIBC symbols:\n%s\n' "${glibc_symbols}"
    echo "error: portable artifact unexpectedly requires glibc" >&2
    exit 1
else
    echo "required GLIBC symbols: none (static musl ELF)"
fi
