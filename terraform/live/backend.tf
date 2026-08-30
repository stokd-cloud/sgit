# Partial S3 backend. Fill bucket / key / region / dynamodb_table via:
#   - terraform/live/backend.hcl  (copy backend.hcl.example; gitignored), or
#   - TF_STATE_BUCKET / TF_STATE_KEY / TF_STATE_LOCK_TABLE / TF_STATE_REGION
#     (scripts/done.sh passes these as -backend-config).
#
# When stokd-cloud/mono later owns this stack, drop this standalone backend
# and let the resources live in the mono workspace state (see terraform/README.md).
terraform {
  backend "s3" {}
}
