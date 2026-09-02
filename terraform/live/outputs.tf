output "site_url" {
  value = module.sgit.site_url
}

output "domain_name" {
  value = module.sgit.domain_name
}

output "s3_bucket_name" {
  value = module.sgit.s3_bucket_name
}

output "cloudfront_distribution_id" {
  value = module.sgit.cloudfront_distribution_id
}

output "cloudfront_domain_name" {
  value = module.sgit.cloudfront_domain_name
}

output "certificate_arn" {
  value = module.sgit.certificate_arn
}
