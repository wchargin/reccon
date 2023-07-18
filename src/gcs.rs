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
    /// Writes an object to GCS and sets its metadata.
    ///
    /// The `content_type` argument should be suitable for raw inclusion in an HTTP header.
    pub async fn put_meta(
        &self,
        name: &str,
        contents: &[u8],
        content_type: &str,
        metadata: &serde_json::Value,
    ) -> Result<(), anyhow::Error> {
        const SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";
        let token = self
            .auth
            .get_token(&[SCOPE])
            .await
            .context("Failed to get GCS auth token")?;

        let object_name = format!("{}{}", &self.path.prefix, name);

        let metadata = serde_json::json!({
            "name": object_name,
            "metadata": metadata,
        });
        let metadata =
            serde_json::to_string(&metadata).context("Failed to serialize metadata to JSON")?;

        let boundary: String = loop {
            let boundary = multipart_boundary();
            use memchr::memmem::Finder;
            let finder = Finder::new(boundary.as_bytes());
            if finder.find(metadata.as_bytes()).is_some() {
                continue;
            }
            if finder.find(content_type.as_bytes()).is_some() {
                continue;
            }
            if finder.find(contents).is_some() {
                continue;
            }
            break boundary;
        };
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(b"--");
        body.extend_from_slice(boundary.as_bytes());
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(b"Content-Type: application/json; charset=UTF-8\r\n\r\n");
        body.extend_from_slice(metadata.as_bytes());
        body.extend_from_slice(b"\r\n\r\n");
        body.extend_from_slice(b"--");
        body.extend_from_slice(boundary.as_bytes());
        body.extend_from_slice(b"Content-Type: ");
        body.extend_from_slice(content_type.as_bytes());
        body.extend_from_slice(b"\r\n\r\n");
        body.extend_from_slice(contents);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(b"--");
        body.extend_from_slice(boundary.as_bytes());
        body.extend_from_slice(b"--\r\n");

        let url = format!(
            "https://storage.googleapis.com/upload/storage/v1/b/{}/o?uploadType=multipart",
            urlencoding::encode(&self.path.bucket)
        );
        self.http
            .post(url)
            .header("Authorization", format!("Bearer {}", token.as_str()))
            .header(
                "Content-Type",
                format!("multipart/related; boundary={}", boundary),
            )
            .body(body)
            .send()
            .await
            .and_then(|res| res.error_for_status())
            .context("Failed to upload to GCS")?;
        Ok(())
    }
}

/// Generates a random boundary for a `multipart/related` (or similar) form.
///
/// This contains at least 128 bits of entropy, but the caller may still want to ensure that it
/// doesn't happen to appear in the rest of the body.
fn multipart_boundary() -> String {
    let [a, b, c, d] = rand::random::<[u32; 4]>();
    format!("{:08x}-{:08x}-{:08x}-{:08x}", a, b, c, d)
}
