use core::str::FromStr;

use gcp_auth::AuthenticationManager;

pub struct GcsContext {
    pub path: GcsPath,
    pub auth: AuthenticationManager,
}

#[derive(Debug)]
pub struct GcsPath {
    pub bucket: String,
    pub prefix: String,
}

impl FromStr for GcsPath {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s
            .strip_prefix("gs://")
            .ok_or_else(|| anyhow::anyhow!("GCS path must start with \"gs://\", but got {s:?}"))?;
        let (bucket, prefix) = match s.split_once('/') {
            None => {
                return Ok(GcsPath {
                    bucket: s.to_string(),
                    prefix: String::new(),
                })
            }
            Some(bp) => bp,
        };
        if !prefix.is_empty() && !prefix.ends_with('/') {
            anyhow::bail!("Non-empty GCS prefix must end with slash, but got {prefix:?}");
        }
        Ok(GcsPath {
            bucket: bucket.into(),
            prefix: prefix.into(),
        })
    }
}
