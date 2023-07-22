use std::fmt::Debug;
use std::time::Duration;

use log::{debug, trace};

pub const BYTES_PER_CHUNK: usize = 16384;

pub const fn duration_to_chunks(d: Duration) -> u32 {
    const SAMPLES_PER_MS: u64 = 48;
    const BYTES_PER_MS: u64 = SAMPLES_PER_MS * 2;
    (d.as_millis() * BYTES_PER_MS as u128 / BYTES_PER_CHUNK as u128) as u32
}

#[derive(Debug, Clone)]
pub struct Config {
    pub chunk_size: usize,
    pub max_total_chunks: u32,
    pub min_hot_chunks: u32,
    pub max_quiet_chunks: u32,
    pub threshold: i16,
}

pub struct Segmentation {
    config: Config,
    state: State,
    last_chunk: Vec<u8>,
    pending_buf: Vec<u8>,
}

impl Debug for Segmentation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Segmentation")
            .field("config", &self.config)
            .field(
                "last_chunk",
                &format_args!("[len = {}]", self.last_chunk.len()),
            )
            .field("state", &self.state)
            .finish()
    }
}

#[derive(Debug)]
enum State {
    Quiet,
    Pending {
        id: String,
        total_chunks: u32,
        consecutive_hot_chunks: u32,
    },
    Active {
        total_chunks: u32,
        consecutive_quiet_chunks: u32,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Event<'a> {
    Start { id: String },
    Data(&'a [u8]),
    End,
}

impl Segmentation {
    pub fn new(config: Config) -> Self {
        Self {
            pending_buf: Vec::with_capacity(config.chunk_size * config.min_hot_chunks as usize),
            last_chunk: Vec::with_capacity(config.chunk_size),
            state: State::Quiet,
            config,
        }
    }

    pub fn accept<'a, F>(
        &'a mut self,
        chunk: &'a [u8],
        gen_id: F,
    ) -> impl Iterator<Item = Event<'a>>
    where
        F: FnOnce() -> String,
    {
        // TODO: Use or write an iterator implementation that doesn't allocate. We only need to
        // return, like, four events at max.
        let mut events: Vec<Event<'_>> = Vec::new();
        let is_quiet = is_quiet(chunk, self.config.threshold);
        assert!(
            chunk.len() <= self.config.chunk_size,
            "{} > {}",
            chunk.len(),
            self.config.chunk_size
        );

        // Move forward through the `Quiet -> Pending -> Active` state machine, by zero or more
        // steps.

        if let State::Quiet = &self.state {
            if !is_quiet {
                debug!("Mic is hot; segment is now pending");
                let id = gen_id();
                self.pending_buf.clear();
                self.pending_buf.extend_from_slice(&self.last_chunk);
                self.state = State::Pending {
                    id,
                    total_chunks: if self.last_chunk.is_empty() { 0 } else { 1 },
                    consecutive_hot_chunks: 0,
                };
            }
        }

        // If pending, maybe discard this segment, or maybe promote it to active.
        if let State::Pending {
            id,
            total_chunks,
            consecutive_hot_chunks,
        } = &mut self.state
        {
            if is_quiet {
                debug!("Mic is quiet; pending segment discarded");
                self.state = State::Quiet;
            } else {
                *consecutive_hot_chunks += 1;
                if *consecutive_hot_chunks >= self.config.min_hot_chunks {
                    let id = std::mem::take(id);
                    events.push(Event::Start { id });
                    events.push(Event::Data(&self.pending_buf));
                    self.state = State::Active {
                        total_chunks: *total_chunks,
                        consecutive_quiet_chunks: 0,
                    };
                } else {
                    self.pending_buf.extend_from_slice(chunk);
                    *total_chunks += 1;
                }
            }
        }

        // If active, emit this chunk and maybe terminate the segment.
        if let State::Active {
            total_chunks,
            consecutive_quiet_chunks,
        } = &mut self.state
        {
            *total_chunks += 1;
            events.push(Event::Data(chunk));

            if is_quiet {
                if *consecutive_quiet_chunks == 0 {
                    debug!("Mic is quiet; segment is active");
                }
                *consecutive_quiet_chunks += 1;
            } else {
                if *consecutive_quiet_chunks > 0 {
                    debug!("Mic is hot; segment is active");
                }
                *consecutive_quiet_chunks = 0;
            }

            if *total_chunks >= self.config.max_total_chunks
                || *consecutive_quiet_chunks >= self.config.max_quiet_chunks
                || chunk.is_empty()
            {
                events.push(Event::End);
                self.state = State::Quiet;
                // TODO: If we hit the max chunks boundary, start a new segment immediately.
            }
        }

        self.last_chunk.clear();
        self.last_chunk.extend_from_slice(chunk);

        events.into_iter()
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Ids {
        count: AtomicU32,
    }
    impl Ids {
        pub fn new() -> Self {
            Self {
                count: AtomicU32::new(0),
            }
        }
        fn id_at(n: u32) -> String {
            format!("seg{:04}", n)
        }
        pub fn peek(&self) -> String {
            Ids::id_at(self.count.load(Ordering::SeqCst))
        }
        pub fn next(&self) -> String {
            let n = self.count.fetch_add(1, Ordering::SeqCst);
            Ids::id_at(n)
        }
    }

    /// Like `Event,` but the `Data` variant's data is owned, not borrowed, so that we can coalesce
    /// consecutive `Data` events.
    #[derive(Debug, PartialEq, Eq)]
    enum TestEvent {
        Start { id: String },
        Data(Vec<u8>),
        End,
    }

    fn test_events<'a, I: IntoIterator<Item = Event<'a>>>(iter: I) -> Vec<TestEvent> {
        let mut result = vec![];
        for ev in iter {
            match (result.last_mut(), ev) {
                (Some(TestEvent::Data(ref mut buf)), Event::Data(chunk)) => {
                    buf.extend_from_slice(chunk)
                }
                (_, Event::Start { id }) => result.push(TestEvent::Start { id }),
                (_, Event::Data(chunk)) => result.push(TestEvent::Data(chunk.into())),
                (_, Event::End) => result.push(TestEvent::End),
            }
        }
        result
    }

    struct TestBed {
        pub seg: Segmentation,
        pub ids: Ids,
    }

    impl TestBed {
        pub fn new(config: Config) -> Self {
            Self {
                seg: Segmentation::new(config),
                ids: Ids::new(),
            }
        }
        pub fn accept(&mut self, chunk: &[u8]) -> Vec<TestEvent> {
            test_events(self.seg.accept(chunk, || self.ids.next()))
        }
    }

    #[test]
    fn test_simple_on_off() {
        let mut tb = TestBed::new(Config {
            chunk_size: 4,
            max_total_chunks: 10,
            min_hot_chunks: 2,
            max_quiet_chunks: 3,
            threshold: 0x0100,
        });
        let id0 = tb.ids.peek();
        let chunk0 = [0x00, 0x00, 0x00, 0x01]; // quiet
        let chunk1 = [0x00, 0x02, 0x00, 0x03]; // hot
        let chunk2 = [0x00, 0x04, 0x00, 0x05];
        let chunk3 = [0x00, 0x06, 0x00, 0x07]; // hot (active)
        let chunk4 = [0x00, 0x08, 0x08, 0x00];
        let chunk5 = [0x08, 0x00, 0x07, 0x00]; // quiet
        let chunk6 = [0x07, 0x00, 0x06, 0x00];
        let chunk7 = [0x06, 0x00, 0x05, 0x00];
        let chunk8 = [0x06, 0x00, 0x05, 0x00]; // inactive

        assert_eq!(tb.accept(&chunk0), vec![]);
        assert_eq!(tb.accept(&chunk1), vec![]);
        assert_eq!(
            tb.accept(&chunk2),
            test_events([
                Event::Start { id: id0 },
                Event::Data(&chunk0),
                Event::Data(&chunk1),
                Event::Data(&chunk2),
            ]),
        );
        assert_eq!(tb.accept(&chunk3), test_events([Event::Data(&chunk3)]));
        assert_eq!(tb.accept(&chunk4), test_events([Event::Data(&chunk4)]));
        assert_eq!(tb.accept(&chunk5), test_events([Event::Data(&chunk5)])); // first quiet
        assert_eq!(tb.accept(&chunk6), test_events([Event::Data(&chunk6)]));
        assert_eq!(
            tb.accept(&chunk7),
            test_events([Event::Data(&chunk7), Event::End])
        );
        assert_eq!(tb.accept(&chunk8), vec![]);
    }

    #[test]
    fn test_max_chunks() {
        let mut tb = TestBed::new(Config {
            chunk_size: 4,
            max_total_chunks: 10,
            min_hot_chunks: 2,
            max_quiet_chunks: 3,
            threshold: 0x0100,
        });
        let chunk_off = [0x01, 0x00, 0x01, 0x00];
        let chunk_on = [0xcc, 0xcc, 0xcc, 0xcc];

        assert_eq!(tb.accept(&chunk_off), vec![]);
        assert_eq!(tb.accept(&chunk_off), vec![]);

        assert_eq!(tb.accept(&chunk_off), vec![]); // chunk 1

        let id0 = tb.ids.peek();
        assert_eq!(tb.accept(&chunk_on), vec![]); // 2
        assert_eq!(
            tb.accept(&chunk_on), // 3
            test_events([
                Event::Start { id: id0 },
                Event::Data(&chunk_off),
                Event::Data(&chunk_on),
                Event::Data(&chunk_on),
            ])
        );
        for _ in 4..=9 {
            assert_eq!(tb.accept(&chunk_on), test_events([Event::Data(&chunk_on)]));
        }
        assert_eq!(
            tb.accept(&chunk_on), // 10
            test_events([Event::Data(&chunk_on), Event::End])
        );

        // TODO: Fix so that this starts a new segment immediately.
        assert_eq!(tb.accept(&chunk_on), vec![]);
    }

    #[test]
    fn test_max_chunks_from_start() {
        let mut tb = TestBed::new(Config {
            chunk_size: 4,
            max_total_chunks: 10,
            min_hot_chunks: 2,
            max_quiet_chunks: 3,
            threshold: 0x0100,
        });
        let chunk_on = [0xcc, 0xcc, 0xcc, 0xcc];

        // no quiet chunks to start!
        let id0 = tb.ids.peek();
        assert_eq!(tb.accept(&chunk_on), vec![]); // 1
        assert_eq!(
            tb.accept(&chunk_on), // 2
            test_events([
                Event::Start { id: id0 },
                Event::Data(&chunk_on),
                Event::Data(&chunk_on),
            ])
        );
        for _ in 3..=9 {
            assert_eq!(tb.accept(&chunk_on), test_events([Event::Data(&chunk_on)]));
        }
        assert_eq!(
            tb.accept(&chunk_on), // 10
            test_events([Event::Data(&chunk_on), Event::End])
        );

        // TODO: Fix so that this starts a new segment immediately.
        assert_eq!(tb.accept(&chunk_on), vec![]);
    }
}
