use std::path::PathBuf;

use serde::Deserialize;

pub const DEFAULT_FILENAME: &str = "reccon.toml";

#[derive(Debug, Deserialize)]
pub struct Config {
    pub storage_dir: Option<PathBuf>,
    pub threshold: Option<f64>,
    pub gcs_bucket: Option<String>,
}
