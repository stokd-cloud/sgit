# sgit Terraform module

Reusable module that publishes a static site at `sgit.selfactor.io` (or another FQDN) using S3 + CloudFront + ACM + Route53.

This directory is the module. The standalone root used by `pnpm done` lives in [`live/`](live/).

## What it creates

- Private S3 bucket for the static origin
- CloudFront distribution with origin access control
- ACM certificate **in us-east-1** (required by CloudFront), DNS-validated
- `A` / `AAAA` aliases on an **existing** Route53 hosted zone

It does **not** create a hosted zone for `selfactor.io`. Pass `hosted_zone_id` for the zone that already exists in the SST account.

## Inputs

| Variable | Required | Default | Purpose |
|----------|----------|---------|---------|
| `hosted_zone_id` | yes | — | Existing `selfactor.io` zone ID |
| `domain_name` | no | `sgit.selfactor.io` | FQDN to publish |
| `environment` | no | `prod` | `prod` or `stage` |
| `aws_region` | no | `us-east-1` | Region for S3 (ACM stays in us-east-1) |
| `aws_account_id` | no | `null` | Optional account guard |
| `site_source_dir` | no | `""` | Directory of files to upload; empty skips upload |
| `name_prefix` | no | `sgit` | Resource name prefix |
| `price_class` | no | `PriceClass_100` | CloudFront price class |
| `force_destroy` | no | `false` | Allow deleting a non-empty bucket |

## Outputs

`site_url`, `domain_name`, `s3_bucket_name`, `s3_bucket_arn`, `cloudfront_distribution_id`, `cloudfront_domain_name`, `cloudfront_hosted_zone_id`, `certificate_arn`.

## Standalone deploy (this repo)

From the repo root, after AWS credentials and DNS vars are set:

```bash
pnpm done          # prod apply
pnpm done:plan     # prod plan
pnpm done:stage    # stage apply (default hostname sgit-stage.selfactor.io)
pnpm done:local    # local preview, no AWS
```

See the root [README](../README.md#deploy-sgitselfactorio).

Remote state for the standalone root is an S3 backend with a DynamoDB lock (`terraform/live/backend.hcl.example`). Bucket, key, and table are not hardcoded so they can move into a larger workspace later.

## Roll-in from stokd-cloud/mono

When mono is converted from SST to Terraform, consume this directory as a module. No rewrite of the resources is required.

```hcl
provider "aws" {
  region = var.aws_region
}

provider "aws" {
  alias  = "us_east_1"
  region = "us-east-1"
}

module "sgit" {
  source = "./apps/sgit/terraform"

  providers = {
    aws           = aws
    aws.us_east_1 = aws.us_east_1
  }

  hosted_zone_id  = var.selfactor_hosted_zone_id
  domain_name     = "sgit.selfactor.io"
  environment     = "prod"
  aws_account_id  = var.aws_account_id
  site_source_dir = "${path.root}/apps/sgit/site"
}
```

`hosted_zone_id` / region / account stay variables so mono can pass the values it already owns. Do not create a second `selfactor.io` zone.

### State migration

The standalone root stores state at (by default) `s3://$TF_STATE_BUCKET/sgit/prod/terraform.tfstate` with lock table `$TF_STATE_LOCK_TABLE`.

When mono takes ownership:

1. Leave this module's resource addresses as `module.sgit.*` (the standalone root already uses that name).
2. Either `terraform state mv` those addresses into the mono workspace, or change mono's backend key to the existing `sgit/prod/terraform.tfstate` for a first apply and then split later.
3. Stop applying `terraform/live/` from this repo so two roots do not fight over the same records.

The GitHub repo can stay private. The published site is a static snapshot of install + docs; it does not clone or proxy GitHub.
