use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use log::{debug, error, info, trace, warn};

mod config;
mod gcs;

struct PendingSegment {
    id: String,
    consecutive_hot_chunks: u32,
    // buffer of hot chunks is persisted externally
}
struct ActiveSegment {
    /// Unique ID for this segment, for logging/etc. purposes.
    id: String,
    /// Filename used while this segment is still being actively recorded.
    part_filename: PathBuf,
    /// Filename used once this segment has finished recording but not been uploaded to GCS.
    local_filename: PathBuf,
    /// Filename used once this segment in its terminal state.
    final_filename: PathBuf,
    /// `sox(1)` subprocess writing to the file at `part_filename`.
    encoder: Child,
    /// Number of chunks that have been fed to `encoder` to far.
    total_chunks: u32,
    /// Length of the longest suffix of chunks below the quiet threshold.
    consecutive_quiet_chunks: u32,
}
const CHUNK_SIZE: usize = 16384;
const MAX_TOTAL_CHUNKS: u32 = duration_to_chunks(Duration::from_secs(60 * 10));
const MIN_HOT_CHUNKS: u32 = duration_to_chunks(Duration::from_secs(1));
const MAX_QUIET_CHUNKS: u32 = duration_to_chunks(Duration::from_secs(5));

const PART_SUFFIX: &str = ".part";
const LOCAL_SUFFIX: &str = ".local";

const RAW_AUDIO_ARGS: &[&str] = &[
    "-L", "-t", "raw", "-c", "1", "-e", "signed", "-b", "16", "-r", "48k",
];

const fn duration_to_chunks(d: Duration) -> u32 {
    const SAMPLES_PER_MS: u64 = 48;
    const BYTES_PER_MS: u64 = SAMPLES_PER_MS * 2;
    (d.as_millis() * BYTES_PER_MS as u128 / CHUNK_SIZE as u128) as u32
}

fn read_config() -> anyhow::Result<config::Config> {
    let arg = std::env::args_os().nth(1);
    let config_file = arg
        .as_deref()
        .unwrap_or_else(|| std::ffi::OsStr::new(config::DEFAULT_FILENAME));
    let config_file = Path::new(config_file);
    let contents = match std::fs::read(config_file) {
        Ok(c) => c,
        // If no config file was explicitly given and the default wasn't found, behave as if the
        // config file were empty, producing a "default" config.
        Err(e) if e.kind() == io::ErrorKind::NotFound && arg.is_none() => Vec::new(),
        Err(e) => {
            return Err(e).with_context(|| {
                format!("Failed to read config file from {}", config_file.display())
            })
        }
    };
    let contents = String::from_utf8(contents).context("Invalid UTF-8 in config file")?;
    toml::from_str(&contents)
        .with_context(|| format!("Invalid config in {}", config_file.display()))
}

async fn finish_segment(mut seg: ActiveSegment, gcs: Option<Arc<gcs::Client>>) {
    info!("Finishing segment {}", seg.id);
    match tokio::task::spawn_blocking(move || seg.encoder.wait())
        .await
        .unwrap()
    {
        Ok(st) if st.success() => {}
        Ok(st) => error!("Encoder for segment {} exited unhealthy: {}", seg.id, st),
        Err(e) => error!("Failed to reap encoder for segment {}: {}", seg.id, e),
    }
    if let Err(e) = tokio::fs::rename(&seg.part_filename, &seg.local_filename).await {
        error!(
            "Failed to mark segment {} as locally finished: {:#}",
            seg.id, e
        );
        return;
    }
    if let Some(gcs) = gcs {
        let res = upload_segment(&seg.id, &seg.local_filename, &seg.final_filename, &gcs).await;
        if let Err(e) = res {
            error!("Failed to upload segment {} to GCS: {:#}", seg.id, e);
        }
    } else if let Err(e) = tokio::fs::rename(&seg.local_filename, &seg.final_filename).await {
        error!(
            "Failed to finalize filename for segment {}: {:#}",
            seg.id, e
        );
    }
}

/// Runs `soxi $query $file` and returns the output (with trailing whitespace trimmed).
async fn soxi(query: &str, file: &Path) -> anyhow::Result<String> {
    let output = tokio::process::Command::new("soxi")
        .arg(query)
        .arg(file)
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "Failed to query soxi {:?} for {} ({}): {}",
            query,
            file.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let mut s: String = match String::from_utf8(output.stdout) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(&e.into_bytes()).into_owned(),
    };
    s.truncate(s.trim_end().len());
    Ok(s)
}

async fn upload_segment(
    id: &str,
    local_name: &Path,
    final_name: &Path,
    gcs: &gcs::Client,
) -> anyhow::Result<()> {
    let contents = tokio::fs::read(local_name);
    let samples = soxi("-s", local_name);
    let sample_rate = soxi("-r", local_name);
    let (contents, samples, sample_rate) = tokio::join!(contents, samples, sample_rate);

    let contents = contents
        .with_context(|| format!("Failed to read segment from {}", local_name.display()))?;

    let mut metadata = serde_json::Map::new();
    match samples {
        Ok(v) => drop(metadata.insert("samples".to_string(), v.into())),
        Err(e) => warn!("Couldn't measure sample count: {}", e),
    };
    match sample_rate {
        Ok(v) => drop(metadata.insert("sample-rate".to_string(), v.into())),
        Err(e) => warn!("Couldn't measure sample rate: {}", e),
    };
    let metadata = metadata.into();

    let object_name = &format!("{}.flac", id);
    gcs.put_meta(object_name, &contents, "audio/flac", &metadata)
        .await?;

    tokio::fs::rename(local_name, final_name)
        .await
        .context("Failed to finalize filename")?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    use env_logger::{Builder, Env};
    Builder::from_env(Env::default().default_filter_or(log::LevelFilter::Info.to_string())).init();

    let mut config = read_config()?;
    let threshold = (config.threshold.unwrap_or(0.25).clamp(0.0, 1.0) * f64::from(i16::MAX)) as i16;
    let storage_dir = config
        .storage_dir
        .unwrap_or_else(|| std::env::temp_dir().join("recordings"));
    match std::fs::create_dir(&storage_dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
            anyhow::bail!(
                "Failed to create storage directory {}: {}",
                storage_dir.display(),
                e
            );
        }
        _ => {}
    };

    let num_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(
            num_cpus.saturating_sub(2) + 1, // leave one for the main thread
        )
        .build()
        .context("Failed to start async runtime")?;

    let gcs = match config.gcs_bucket.take() {
        None => None,
        Some(bucket) => Some(Arc::new(rt.block_on(async {
            let http = reqwest::Client::new();
            let path: gcs::Path = bucket.parse()?;
            log::debug!("Attempting to authenticate to GCS");
            let auth = gcp_auth::AuthenticationManager::new()
                .await
                .with_context(|| {
                    format!("GCS bucket specified ({bucket}) but no valid credentials found")
                })?;
            log::info!("Authenticated to GCS");
            Ok::<_, anyhow::Error>(gcs::Client { http, path, auth })
        })?)),
    };

    let mut sp_rec = Command::new("rec")
        .arg("-q")
        .args(RAW_AUDIO_ARGS)
        .arg("-")
        .stdout(Stdio::piped())
        .spawn()
        .context("Failed to spawn rec(1); is SoX installed?")?;
    let mut pipe = sp_rec.stdout.take().unwrap();
    let mut chunk: Vec<u8> = Vec::with_capacity(CHUNK_SIZE);
    let mut last_chunk: Vec<u8> = Vec::with_capacity(CHUNK_SIZE);
    let mut pending_buf: Vec<u8> = Vec::with_capacity(CHUNK_SIZE * MIN_HOT_CHUNKS as usize);
    // invariant: `pending_buf` is non-empty iff `state` is `Pending(_)`
    enum State {
        Quiet,
        Pending(PendingSegment),
        Active(ActiveSegment),
    }
    let mut state = State::Quiet;
    loop {
        last_chunk.clear();
        last_chunk.extend(&chunk);
        chunk.clear();
        (&mut pipe)
            .take(u64::try_from(CHUNK_SIZE).unwrap())
            .read_to_end(&mut chunk)
            .context("Failed to read chunk from rec(1) pipe")?;
        let is_quiet = is_quiet(&chunk, threshold);
        let mut cur_seg = match (is_quiet, &mut state) {
            (true, State::Quiet) if chunk.is_empty() => break,
            (_, State::Active(seg)) => seg,
            (true, State::Quiet) => continue,
            (false, State::Quiet) => {
                debug!("Mic is hot; segment is now pending");
                let now = chrono::Utc::now();
                let id = now.format("%Y%m%dT%H%M%S").to_string();
                assert!(pending_buf.is_empty());
                pending_buf.extend(&last_chunk);
                pending_buf.extend(&chunk);
                state = State::Pending(PendingSegment {
                    id,
                    consecutive_hot_chunks: 1,
                });
                continue;
            }
            (true, State::Pending(_)) => {
                debug!("Mic is quiet; pending segment discarded");
                pending_buf.clear();
                state = State::Quiet;
                continue;
            }
            (false, State::Pending(ref mut pending)) => {
                pending.consecutive_hot_chunks += 1;
                pending_buf.extend(&chunk);
                if pending.consecutive_hot_chunks < MIN_HOT_CHUNKS {
                    continue;
                }
                let id = std::mem::take(&mut pending.id);
                let part_filename =
                    storage_dir.join(&format!("recording-{}.flac{}", id, PART_SUFFIX));
                let local_filename =
                    storage_dir.join(&format!("recording-{}.flac{}", id, LOCAL_SUFFIX));
                let final_filename = storage_dir.join(&format!("recording-{}.flac", id));
                info!("Starting segment {}", id);
                let mut sp_sox = Command::new("sox")
                    .arg("-q")
                    .args(RAW_AUDIO_ARGS)
                    .arg("-")
                    .args(["-t", "flac", "--comment", ""])
                    .arg(&part_filename)
                    .stdin(Stdio::piped())
                    .spawn()
                    .context("Failed to spawn sox(1)")?;
                let encoder_stdin = sp_sox.stdin.as_mut().unwrap();
                if let Err(e) = encoder_stdin.write_all(&pending_buf) {
                    error!(
                        "Failed to write initial backlog of segment {} to encoder: {}",
                        id, e
                    );
                    // but carry on
                }
                state = State::Active(ActiveSegment {
                    id,
                    encoder: sp_sox,
                    part_filename,
                    local_filename,
                    final_filename,
                    total_chunks: pending.consecutive_hot_chunks + 1, /* for initial last_chunk */
                    consecutive_quiet_chunks: 0,
                });
                pending_buf.clear();
                continue;
            }
        };
        let encoder_stdin = &mut cur_seg.encoder.stdin;
        let write_failed = match encoder_stdin.as_mut().unwrap().write_all(&chunk) {
            Ok(_) => {
                cur_seg.total_chunks += 1;
                false
            }
            Err(e) => {
                error!(
                    "Failed to write chunk {} to encoder: {}",
                    cur_seg.total_chunks + 1,
                    e
                );
                true
            }
        };
        if is_quiet {
            if cur_seg.consecutive_quiet_chunks == 0 {
                debug!("Mic is quiet; segment is active");
            }
            cur_seg.consecutive_quiet_chunks += 1;
        } else {
            if cur_seg.consecutive_quiet_chunks > 0 {
                debug!("Mic is hot; segment is active");
            }
            cur_seg.consecutive_quiet_chunks = 0;
        }
        if cur_seg.total_chunks >= MAX_TOTAL_CHUNKS
            || cur_seg.consecutive_quiet_chunks >= MAX_QUIET_CHUNKS
            || chunk.is_empty()
            || write_failed
        {
            encoder_stdin.take();
            rt.spawn(finish_segment(
                match std::mem::replace(&mut state, State::Quiet) {
                    State::Active(seg) => seg,
                    _ => unreachable!(),
                },
                gcs.clone(),
            ));
        }
        if chunk.is_empty() {
            break;
        }
    }

    Ok(())
}

fn is_quiet(raw_audio: &[u8], threshold: i16) -> bool {
    let max_sample = raw_audio
        .chunks(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .map(|z| z.abs())
        .max()
        .unwrap_or(0);
    trace!("Max sample: {} <=> {}", max_sample, threshold);
    max_sample <= threshold
}
