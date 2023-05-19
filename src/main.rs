use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

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

fn main() {
    let (tx, rx) = mpsc::channel::<ActiveSegment>();
    let mut sp_rec = Command::new("rec")
        .arg("-q")
        .args(RAW_AUDIO_ARGS)
        .arg("-")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn[rec]");
    let mut pipe = sp_rec.stdout.take().unwrap();
    let mut chunk: Vec<u8> = Vec::with_capacity(CHUNK_SIZE);
    let mut seg: Option<ActiveSegment> = None;
    std::thread::spawn(move || {
        while let Ok(mut seg) = rx.recv() {
            seg.encoder.wait().expect("encoder.wait");
            println!("finishing segment {}", seg.id);
            std::fs::rename(seg.part_filename, seg.final_filename).expect("file gone");
        }
    });
    loop {
        chunk.clear();
        (&mut pipe)
            .take(u64::try_from(CHUNK_SIZE).unwrap())
            .read_to_end(&mut chunk)
            .expect("pipe.take(...).read");
        let is_quiet = is_quiet(&chunk);
        let mut cur_seg = match (is_quiet, &mut seg) {
            (true, None) => continue,
            (_, Some(seg)) => seg,
            (false, None) => {
                let now = chrono::Local::now();
                let id = now.format("%Y%m%dT%H%M%S").to_string();
                let date_part = &id[0..8];
                let base_dir = PathBuf::from_iter(["/tmp", date_part]);
                match std::fs::create_dir(&base_dir) {
                    Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
                        panic!("failed to create base directory: {}", e);
                    }
                    _ => {}
                };
                let part_filename = base_dir.join(&format!("recording-{}.flac{}", id, PART_SUFFIX));
                let final_filename = base_dir.join(&format!("recording-{}.flac", id));
                println!("starting segment {}", id);
                let sp_sox = Command::new("sox")
                    .arg("-q")
                    .args(RAW_AUDIO_ARGS)
                    .arg("-")
                    .args(&["-t", "flac"])
                    .arg(&part_filename)
                    .stdin(Stdio::piped())
                    .spawn()
                    .expect("spawn[sox]");
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
        encoder_stdin
            .as_mut()
            .unwrap()
            .write_all(&chunk)
            .expect("write to encoder");
        cur_seg.total_chunks += 1;
        if is_quiet {
            cur_seg.consecutive_quiet_chunks += 1;
        } else {
            cur_seg.consecutive_quiet_chunks = 0;
        }
        if cur_seg.total_chunks >= MAX_TOTAL_CHUNKS
            || cur_seg.consecutive_quiet_chunks >= MAX_QUIET_CHUNKS
            || chunk.is_empty()
        {
            encoder_stdin.take();
            tx.send(seg.take().unwrap()).expect("tx.send");
        }
        if chunk.is_empty() {
            break;
        }
    }
}

fn is_quiet(raw_audio: &[u8]) -> bool {
    let max_sample = raw_audio
        .chunks(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .map(|z| z.abs())
        .max()
        .expect("empty chunk");
    const QUIET_THRESHOLD: i16 = i16::MAX / 4;
    max_sample <= QUIET_THRESHOLD
}
