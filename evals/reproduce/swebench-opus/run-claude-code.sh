#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "${script_dir}/../../.." && pwd)"

die() {
    echo "error: $*" >&2
    exit 2
}

absolute_path() {
    # Absolute, but symlinks kept: a venv's python only finds its packages
    # when run through the venv's own bin/ link.
    case "$1" in
        /*) printf '%s\n' "$1" ;;
        *) printf '%s/%s\n' "$(pwd)" "$1" ;;
    esac
}

resolve_aws_credentials() {
    if [[ -n "${AWS_ACCESS_KEY_ID:-}" && -n "${AWS_SECRET_ACCESS_KEY:-}" ]]; then
        return
    fi
    command -v aws >/dev/null || die \
        "AWS credentials are not exported and the AWS CLI is unavailable; configure env credentials, a profile, or SSO"
    local credentials
    credentials="$(aws configure export-credentials --format env-no-export)" || die \
        "the AWS CLI could not resolve credentials; authenticate the selected profile or SSO session"
    while IFS='=' read -r name value; do
        case "${name}" in
            AWS_ACCESS_KEY_ID|AWS_SECRET_ACCESS_KEY|AWS_SESSION_TOKEN)
                export "${name}=${value}"
                ;;
        esac
    done <<<"${credentials}"
    [[ -n "${AWS_ACCESS_KEY_ID:-}" && -n "${AWS_SECRET_ACCESS_KEY:-}" ]] || die \
        "the standard AWS credential chain returned no usable credentials"
}

CLAUDE_BIN="${CLAUDE_BIN:-claude}"
CLAUDE_BIN="$(command -v -- "${CLAUDE_BIN}")" || die "CLAUDE_BIN is not executable or on PATH: ${CLAUDE_BIN}"
CLAUDE_BIN="$(absolute_path "${CLAUDE_BIN}")"

[[ -n "${GRADER_PYTHON:-}" ]] || die "GRADER_PYTHON must name a Python interpreter with the swebench package"
GRADER_PYTHON="$(command -v -- "${GRADER_PYTHON}")" || die "GRADER_PYTHON is not executable: ${GRADER_PYTHON}"
GRADER_PYTHON="$(absolute_path "${GRADER_PYTHON}")"

AWS_REGION="${AWS_REGION:-}"
[[ -n "${AWS_REGION}" ]] || die "AWS_REGION must name a region that serves the global Claude Opus 5.5 inference profile"
export AWS_REGION
resolve_aws_credentials

INSTANCES_FILE="${INSTANCES_FILE:-${script_dir}/instances.txt}"
[[ -f "${INSTANCES_FILE}" ]] || die "INSTANCES_FILE does not exist: ${INSTANCES_FILE}"
instances="$(tr -d '[:space:]' <"${INSTANCES_FILE}")"
[[ -n "${instances}" ]] || die "INSTANCES_FILE is empty: ${INSTANCES_FILE}"

OUT_DIR="$(absolute_path "${OUT_DIR:-${repo_root}/out/claude-code}")"
MAX_WORKERS="${MAX_WORKERS:-2}"
EPISODE_WORKERS="${EPISODE_WORKERS:-1}"
MODEL="${MODEL:-global.anthropic.claude-opus-5-5}"

export CLAUDE_CODE_USE_BEDROCK=1
unset ANTHROPIC_API_KEY

cd -- "${repo_root}"
exec python3 -m evals.external.claude_code_swebench \
    --bedrock \
    --workspace-rule \
    --instances "${instances}" \
    --model "${MODEL}" \
    --effort medium \
    --claude-binary "${CLAUDE_BIN}" \
    --episode-timeout-seconds 2400 \
    --grade-timeout-seconds 1800 \
    --concurrency "${EPISODE_WORKERS}" \
    --max-workers "${MAX_WORKERS}" \
    --grader-python "${GRADER_PYTHON}" \
    --output "${OUT_DIR}"
