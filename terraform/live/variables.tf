variable "hosted_zone_id" {
  description = "Existing Route53 hosted zone ID for selfactor.io."
  type        = string
}

variable "domain_name" {
  description = "FQDN to publish."
  type        = string
  default     = "sgit.selfactor.io"
}

variable "environment" {
  description = "prod or stage."
  type        = string
  default     = "prod"
}

variable "aws_region" {
  description = "Primary AWS region."
  type        = string
  default     = "us-east-1"
}

variable "aws_account_id" {
  description = "Optional expected AWS account ID."
  type        = string
  default     = null
}

variable "site_source_dir" {
  description = "Static site directory to upload. Default is the repo site/ folder."
  type        = string
  default     = ""
}

variable "force_destroy" {
  description = "Allow destroying a non-empty site bucket."
  type        = bool
  default     = false
}
