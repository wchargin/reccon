use core::str::FromStr;

use anyhow::Context as _;
use gcp_auth::AuthenticationManager;

pub struct Client {
    pub http: reqwest::Client,
    pub path: Path,
    pub auth: AuthenticationManager,
}

#[derive(Debug)]
pub struct Path {
    pub bucket: String,
    pub prefix: String,
}

impl FromStr for Path {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s
            .strip_prefix("gs://")
            .ok_or_else(|| anyhow::anyhow!("GCS path must start with \"gs://\", but got {s:?}"))?;
        let (bucket, prefix) = match s.split_once('/') {
            None => {
                return Ok(Path {
                    bucket: s.to_string(),
                    prefix: String::new(),
                })
            }
            Some(bp) => bp,
        };
        if !prefix.is_empty() && !prefix.ends_with('/') {
            anyhow::bail!("Non-empty GCS prefix must end with slash, but got {prefix:?}");
        }
        Ok(Path {
            bucket: bucket.into(),
            prefix: prefix.into(),
        })
    }
}

impl Client {
    pub async fn put(
        &self,
        name: &str,
        contents: Vec<u8>,
        content_type: &str,
    ) -> Result<(), anyhow::Error> {
        const SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";
        let token = self
            .auth
            .get_token(&[SCOPE])
            .await
            .context("Failed to get GCS auth token")?;

        let object_name = format!("{}{}", &self.path.prefix, name);
        let url = format!(
            "https://storage.googleapis.com/upload/storage/v1/b/{}/o?uploadType=media&name={}",
            &self.path.bucket,
            urlencoding::encode(&object_name)
        );
        self.http
            .post(url)
            .header("Authorization", format!("Bearer {}", token.as_str()))
            .header("Content-Type", content_type)
            .body(contents)
            .send()
            .await
            .and_then(|res| res.error_for_status())
            .context("Failed to upload to GCS")?;
        Ok(())
    }
}