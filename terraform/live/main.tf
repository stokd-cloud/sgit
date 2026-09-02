locals {
  site_source_dir = var.site_source_dir != "" ? var.site_source_dir : "${path.module}/../../site"
}

module "sgit" {
  source = "./.."

  providers = {
    aws           = aws
    aws.us_east_1 = aws.us_east_1
  }

  hosted_zone_id  = var.hosted_zone_id
  domain_name     = var.domain_name
  environment     = var.environment
  aws_region      = var.aws_region
  aws_account_id  = var.aws_account_id
  site_source_dir = local.site_source_dir
  force_destroy   = var.force_destroy
}
