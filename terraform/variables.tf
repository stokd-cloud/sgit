variable "hosted_zone_id" {
  description = "Existing Route53 hosted zone ID for selfactor.io. Do not create a second zone; this stack only adds records."
  type        = string
}

variable "domain_name" {
  description = "FQDN to publish (prod default is sgit.selfactor.io)."
  type        = string
  default     = "sgit.selfactor.io"
}

variable "environment" {
  description = "Deploy environment. Matches mono done.sh vocabulary: prod or stage."
  type        = string
  default     = "prod"

  validation {
    condition     = contains(["prod", "stage"], var.environment)
    error_message = "environment must be \"prod\" or \"stage\"."
  }
}

variable "aws_region" {
  description = "Primary AWS region for S3 and supporting resources. CloudFront ACM is always issued in us-east-1 via the aws.us_east_1 provider alias."
  type        = string
  default     = "us-east-1"
}

variable "aws_account_id" {
  description = "Optional expected AWS account ID. When set, apply fails if the caller is a different account. Used so mono can pass the SST account later."
  type        = string
  default     = null
}

variable "site_source_dir" {
  description = "Directory of static files to upload. Empty skips object upload (infra-only apply)."
  type        = string
  default     = ""
}

variable "name_prefix" {
  description = "Prefix for AWS resource names."
  type        = string
  default     = "sgit"
}

variable "price_class" {
  description = "CloudFront price class."
  type        = string
  default     = "PriceClass_100"
}

variable "force_destroy" {
  description = "Allow Terraform to delete the site bucket even if it still has objects. Stage may want this; prod should leave it false."
  type        = bool
  default     = false
}
