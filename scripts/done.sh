#!/usr/bin/env bash
# Deploy entrypoint. Names match stokd-cloud/mono done.sh vocabulary
# (prod / stage / local / plan) without reproducing the full mono script.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENV="${1:-prod}"
TF_DIR="$ROOT/terraform/live"

usage() {
  cat <<'EOF'
Usage: bash scripts/done.sh [prod|stage|local]

  prod   terraform apply for sgit.selfactor.io (default)
  stage  terraform apply for sgit-stage.selfactor.io
  local  build the static site and serve it; no AWS apply

Environment:
  STOKD_DONE_DRY_RUN=1   plan only (pnpm done:plan)
  STOKD_DONE_FORCE=1     apply -auto-approve (pnpm done / done:force)
  SGIT_HOSTED_ZONE_ID    existing selfactor.io Route53 zone ID
  SGIT_DOMAIN            override FQDN
  SGIT_AWS_REGION        default us-east-1
  SGIT_AWS_ACCOUNT_ID    optional account guard
  TF_STATE_BUCKET        S3 state bucket
  TF_STATE_KEY           default sgit/<env>/terraform.tfstate
  TF_STATE_LOCK_TABLE    DynamoDB lock table
  TF_STATE_REGION        default SGIT_AWS_REGION / us-east-1
  SGIT_LOCAL_PORT        local preview port (default 4173)
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

ACTION="apply"
if [[ "${STOKD_DONE_DRY_RUN:-}" == "1" ]]; then
  ACTION="plan"
fi

build_site() {
  bash "$ROOT/scripts/build-site.sh"
}

if [[ "$ENV" == "local" ]]; then
  build_site
  PORT="${SGIT_LOCAL_PORT:-4173}"
  echo "done:local — serving $ROOT/site at http://127.0.0.1:${PORT} (no AWS apply)"
  cd "$ROOT/site"
  exec python3 -m http.server "$PORT" --bind 127.0.0.1
fi

if [[ "$ENV" != "prod" && "$ENV" != "stage" ]]; then
  echo "error: unknown environment '$ENV' (expected prod, stage, or local)" >&2
  usage >&2
  exit 1
fi

if [[ "$ENV" == "prod" ]]; then
  DOMAIN="${SGIT_DOMAIN:-sgit.selfactor.io}"
  FORCE_DESTROY="false"
else
  DOMAIN="${SGIT_DOMAIN:-sgit-stage.selfactor.io}"
  FORCE_DESTROY="true"
fi

AWS_REGION_VALUE="${SGIT_AWS_REGION:-${AWS_REGION:-us-east-1}}"

build_site

if ! command -v terraform >/dev/null 2>&1; then
  echo "error: terraform is not on PATH" >&2
  exit 1
fi

# Provider install + validate never needs a remote backend or AWS creds.
terraform -chdir="$TF_DIR" init -backend=false -input=false >/dev/null
terraform -chdir="$TF_DIR" validate

if [[ -z "${SGIT_HOSTED_ZONE_ID:-}" ]]; then
  if [[ "$ACTION" == "plan" ]]; then
    echo "done:plan — terraform validate ok; skipping remote plan (SGIT_HOSTED_ZONE_ID unset)"
    exit 0
  fi
  echo "error: set SGIT_HOSTED_ZONE_ID to the existing selfactor.io hosted zone" >&2
  exit 1
fi

BACKEND_ARGS=()
if [[ -f "$TF_DIR/backend.hcl" ]]; then
  BACKEND_ARGS+=(-backend-config="$TF_DIR/backend.hcl")
elif [[ -n "${TF_STATE_BUCKET:-}" ]]; then
  BACKEND_ARGS+=(
    -backend-config="bucket=${TF_STATE_BUCKET}"
    -backend-config="key=${TF_STATE_KEY:-sgit/${ENV}/terraform.tfstate}"
    -backend-config="region=${TF_STATE_REGION:-$AWS_REGION_VALUE}"
    -backend-config="encrypt=true"
  )
  if [[ -n "${TF_STATE_LOCK_TABLE:-}" ]]; then
    BACKEND_ARGS+=(-backend-config="dynamodb_table=${TF_STATE_LOCK_TABLE}")
  fi
else
  if [[ "$ACTION" == "plan" ]]; then
    echo "done:plan — terraform validate ok; skipping remote plan (no backend.hcl or TF_STATE_BUCKET)"
    exit 0
  fi
  echo "error: configure remote state (copy terraform/live/backend.hcl.example to backend.hcl, or set TF_STATE_BUCKET)" >&2
  exit 1
fi

TFVARS=(
  -var="environment=${ENV}"
  -var="domain_name=${DOMAIN}"
  -var="hosted_zone_id=${SGIT_HOSTED_ZONE_ID}"
  -var="aws_region=${AWS_REGION_VALUE}"
  -var="force_destroy=${FORCE_DESTROY}"
  -var="site_source_dir=${ROOT}/site"
)

if [[ -n "${SGIT_AWS_ACCOUNT_ID:-}" ]]; then
  TFVARS+=(-var="aws_account_id=${SGIT_AWS_ACCOUNT_ID}")
fi

terraform -chdir="$TF_DIR" init -reconfigure -input=false "${BACKEND_ARGS[@]}"

if [[ "$ACTION" == "plan" ]]; then
  terraform -chdir="$TF_DIR" plan -input=false "${TFVARS[@]}"
  exit 0
fi

APPROVE=(-auto-approve)
if [[ "${STOKD_DONE_FORCE:-}" == "1" ]]; then
  APPROVE=(-auto-approve)
fi

terraform -chdir="$TF_DIR" apply -input=false "${APPROVE[@]}" "${TFVARS[@]}"
