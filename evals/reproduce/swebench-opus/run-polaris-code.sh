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

[[ -n "${POLARIS_CODE_BIN:-}" ]] || die "POLARIS_CODE_BIN must name the portable polaris-code binary"
[[ -x "${POLARIS_CODE_BIN}" ]] || die "POLARIS_CODE_BIN is not executable: ${POLARIS_CODE_BIN}"
POLARIS_CODE_BIN="$(absolute_path "${POLARIS_CODE_BIN}")"

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
IFS=',' read -r -a instance_ids <<<"${instances}"

OUT_DIR="$(absolute_path "${OUT_DIR:-${repo_root}/out/polaris-code}")"
MAX_WORKERS="${MAX_WORKERS:-2}"
EPISODE_WORKERS="${EPISODE_WORKERS:-1}"
API_COST_CEILING_USD="${API_COST_CEILING_USD:-${#instance_ids[@]}}"
MODEL="${MODEL:-aws.bedrock/global.anthropic.claude-opus-5-5}"
ARM="${ARM:-loop}"

export POLARIS_CODE_PROVIDER=bedrock
export POLARIS_CODE_MODELS="${MODEL}"
export POLARIS_CODE_TESTENV=1
unset ANTHROPIC_API_KEY

cd -- "${repo_root}"
exec python3 -m evals.packs.swebench_verified run \
    --model "${MODEL}" \
    --repetitions 1 \
    --max-workers "${MAX_WORKERS}" \
    --episode-workers "${EPISODE_WORKERS}" \
    --episode-timeout-seconds 2400 \
    --grade-timeout-seconds 1800 \
    --grader-python "${GRADER_PYTHON}" \
    --allow-edit \
    --allow-command "python *" \
    --allow-command "pytest *" \
    --keep-transcripts \
    --instances "${instances}" \
    --api-cost-ceiling-usd "${API_COST_CEILING_USD}" \
    --experiment-axis composition \
    --composition-policies gates \
    --topology-intent careful \
    --domain none \
    --arm "${ARM}" \
    --polaris-code "${POLARIS_CODE_BIN}" \
    --output "${OUT_DIR}"
