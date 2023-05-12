use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;

fn main() {
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(16);
    let mut sp = Command::new("sh")
        .args(&["-c", "rec -q -L -t raw -c 1 -e signed -b 16 -r 48k -"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn[yes]");
    let mut pipe = sp.stdout.take().unwrap();
    std::thread::spawn(move || loop {
        const CHUNK_SIZE: usize = 8192;
        let mut vec = Vec::with_capacity(CHUNK_SIZE); // wasteful realloc
        (&mut pipe)
            .take(u64::try_from(CHUNK_SIZE).unwrap())
            .read_to_end(&mut vec)
            .expect("pipe.take(...).read");
        if vec.is_empty() {
            break;
        }
        tx.send(vec).expect("tx.send");
    });
    let mut was_on = false;
    while let Ok(vec) = rx.recv() {
        let (mut min, mut max) = (i16::MAX, i16::MIN);
        for sample in vec.chunks(2) {
            let sample = i16::from_le_bytes([sample[0], sample[1]]);
            if sample > max {
                max = sample;
            }
            if sample < min {
                min = sample;
            }
        }
        let on = max >= 8192;
        let sigil = match (on, was_on) {
            (true, true) => "|",
            (true, false) => "<",
            (false, true) => ">",
            (false, false) => " ",
        };
        was_on = on;
        println!(
            "{}|{:16}|",
            sigil,
            String::from(if on { ":" } else { "." }).repeat((max as u64 * 16 / 32768) as usize)
        );
    }
}
