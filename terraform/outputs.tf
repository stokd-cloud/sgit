output "domain_name" {
  description = "Public hostname this stack publishes."
  value       = var.domain_name
}

output "site_url" {
  description = "HTTPS URL of the published site."
  value       = "https://${var.domain_name}"
}

output "s3_bucket_name" {
  description = "Origin bucket name."
  value       = aws_s3_bucket.site.id
}

output "s3_bucket_arn" {
  description = "Origin bucket ARN."
  value       = aws_s3_bucket.site.arn
}

output "cloudfront_distribution_id" {
  description = "CloudFront distribution ID (for invalidations)."
  value       = aws_cloudfront_distribution.site.id
}

output "cloudfront_domain_name" {
  description = "CloudFront domain (dxxx.cloudfront.net)."
  value       = aws_cloudfront_distribution.site.domain_name
}

output "cloudfront_hosted_zone_id" {
  description = "CloudFront hosted zone ID for Route53 aliases."
  value       = aws_cloudfront_distribution.site.hosted_zone_id
}

output "certificate_arn" {
  description = "ACM certificate ARN in us-east-1."
  value       = aws_acm_certificate.site.arn
}
