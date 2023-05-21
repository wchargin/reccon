use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::Context;
use log::{debug, error, info, warn};

mod config;

struct ActiveSegment {
    /// Unique ID for this segment, for logging/etc. purposes.
    id: String,
    /// Filename used while this segment is still being actively recorded.
    part_filename: PathBuf,
    /// Filename used once this segment has finished recording.
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
const MAX_QUIET_CHUNKS: u32 = duration_to_chunks(Duration::from_secs(5));

const PART_SUFFIX: &str = ".part";

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
        .as_ref()
        .map(|s| s.as_os_str())
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

async fn finish_segment(mut seg: ActiveSegment) {
    info!("Finishing segment {}", seg.id);
    match tokio::task::spawn_blocking(move || seg.encoder.wait())
        .await
        .unwrap()
    {
        Ok(st) if st.success() => {}
        Ok(st) => error!("Encoder for segment {} exited unhealthy: {}", seg.id, st),
        Err(e) => error!("Failed to reap encoder for segment {}: {}", seg.id, e),
    }
    if let Err(e) = std::fs::rename(seg.part_filename, seg.final_filename) {
        warn!("Failed to finalize filename for segment {}: {}", seg.id, e);
    }
}

fn main() -> anyhow::Result<()> {
    use env_logger::{Builder, Env};
    Builder::from_env(Env::default().default_filter_or(log::LevelFilter::Info.to_string())).init();

    let config = read_config()?;
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

    let mut sp_rec = Command::new("rec")
        .arg("-q")
        .args(RAW_AUDIO_ARGS)
        .arg("-")
        .stdout(Stdio::piped())
        .spawn()
        .context("Failed to spawn rec(1); is SoX installed?")?;
    let mut pipe = sp_rec.stdout.take().unwrap();
    let mut chunk: Vec<u8> = Vec::with_capacity(CHUNK_SIZE);
    let mut seg: Option<ActiveSegment> = None;
    loop {
        chunk.clear();
        (&mut pipe)
            .take(u64::try_from(CHUNK_SIZE).unwrap())
            .read_to_end(&mut chunk)
            .context("Failed to read chunk from rec(1) pipe")?;
        let is_quiet = is_quiet(&chunk, threshold);
        let mut cur_seg = match (is_quiet, &mut seg) {
            (true, None) => continue,
            (_, Some(seg)) => seg,
            (false, None) => {
                let now = chrono::Local::now();
                let id = now.format("%Y%m%dT%H%M%S").to_string();
                let part_filename =
                    storage_dir.join(&format!("recording-{}.flac{}", id, PART_SUFFIX));
                let final_filename = storage_dir.join(&format!("recording-{}.flac", id));
                info!("Starting segment {}", id);
                let sp_sox = Command::new("sox")
                    .arg("-q")
                    .args(RAW_AUDIO_ARGS)
                    .arg("-")
                    .args(&["-t", "flac"])
                    .arg(&part_filename)
                    .stdin(Stdio::piped())
                    .spawn()
                    .context("Failed to spawn sox(1)")?;
                seg = Some(ActiveSegment {
                    id,
                    encoder: sp_sox,
                    part_filename,
                    final_filename,
                    total_chunks: 0,
                    consecutive_quiet_chunks: 0,
                });
                seg.as_mut().unwrap()
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
                debug!("Mic is quiet");
            }
            cur_seg.consecutive_quiet_chunks += 1;
        } else {
            if cur_seg.consecutive_quiet_chunks > 0 {
                debug!("Mic is hot");
            }
            cur_seg.consecutive_quiet_chunks = 0;
        }
        if cur_seg.total_chunks >= MAX_TOTAL_CHUNKS
            || cur_seg.consecutive_quiet_chunks >= MAX_QUIET_CHUNKS
            || chunk.is_empty()
            || write_failed
        {
            encoder_stdin.take();
            rt.spawn(finish_segment(seg.take().unwrap()));
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
        .expect("empty chunk");
    max_sample <= threshold
}
