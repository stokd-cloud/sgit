provider "aws" {
  region = var.aws_region

  default_tags {
    tags = {
      Project     = "sgit"
      ManagedBy   = "terraform"
      Environment = var.environment
    }
  }
}

# ACM certificates for CloudFront must be in us-east-1, even when the
# rest of the stack is in another region.
provider "aws" {
  alias  = "us_east_1"
  region = "us-east-1"

  default_tags {
    tags = {
      Project     = "sgit"
      ManagedBy   = "terraform"
      Environment = var.environment
    }
  }
}
