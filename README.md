# reccon [🦝][racc]

Continuously record audio, segmenting against silence and (optionally)
uploading audio to GCS.

[racc]: https://www.gstatic.com/android/keyboard/emojikitchen/20211115/u1f99d/u1f99d_u1f3a7.png

## Motivation

I created this to continuously record the output from my piano, which is
useful to me for a number of reasons:

-   If I improvise something interesting, but then can't quite remember
    exactly what it was that I played, I can look back at my "replay
    buffer" to hear it again. Particularly useful for dense jazz
    voicings, extensions, and alterations.

-   If I'm practicing a section of a piece of music, and I'm especially
    satisfied with one playthrough, I can check back to analyze exactly
    what it was in my playing that sounded good to me. Particularly
    useful when I'm working on interpretation more than technique, and
    when the piece has flexibility in terms of rubato, dynamics,
    articulations, or even local rhythms.

-   If I do specifically intend to play and record something, I can just
    sit down and play it and grab the recording later, which is easier
    than setting up and managing a recording session.

I recommend connecting your computer's audio line-in directly to the
line-out of an electronic instrument, so that you're only recording that
instrument's output and not any conversations or other ambient sounds.
For privacy reasons, I recommend against using a microphone. If you do
choose to use a microphone, be sure to communicate to anyone in the area
that their voice will be recorded.

## Runtime dependencies

You must have [SoX][] installed; the `sox`, `soxi`, and `rec` binaries
should be on your path. Most Linux-based distributions provide SoX under
the `sox` package.

[SoX]: https://sox.sourceforge.net/

## Building

This project uses the standard [Rust][] toolchain:

```
$ cargo test
$ cargo build --release
```

[Rust]: https://www.rust-lang.org/

## Usage

In simplest form, just run the program with no arguments. It'll record
FLAC files into `/tmp/recordings` (or your system's `$TMPDIR`).

You can add a configuration file `reccon.toml` in the [TOML][] format to
customize behavior:

-   Set `storage_dir` to change the directory into which recordings will
    be stored. **Note:** `reccon` assumes that it owns this directory
    and will manage all its contents. If you have unrelated files in
    this directory, they may be overwritten or deleted.

-   Set `threshold` to a float between 0.0 and 1.0 to specify how loud
    the audio needs to be to start recording. Audio below this threshold
    counts as silence. This is a linear value, so (e.g.) use `0.01` for
    a power measurement of −40 dBFS.

-   Set `gcs_bucket` to a string like `gs://my-bucket` or
    `gs://my-bucket/my-prefix/` to automatically upload recorded
    segments to Google Cloud Storage. If this is set, then the host
    machine must have GCS credentials, either ADC credentials at the
    well-known path or service account credentials pointed to by the
    `GOOGLE_APPLICATION_CREDENTIALS` environment variable.

To use a configuration file other than `./reccon.toml`, pass its path as
the sole command-line argument.

[TOML]: https://toml.io/

## Installation on a dedicated system

This is my best-effort recollection of how to get this thing working on
a dedicated Raspberry Pi (I use a first-generation Zero W). I'm assuming
that the Pi is reachable as `raspiano` on your network.

First, build for ARM (not ARMv7) and ship over the binary:

```
cross build --release --target arm-unknown-linux-gnueabihf
rsync --info=progress2 target/arm-unknown-linux-gnueabihf/release/reccon pi@raspiano:~/reccon
```

(I use `rsync` and not `scp` because it works better even when the
remote program is currently being executed, instead of failing with
`ETXTBSY`.)

Follow the instructions at <https://superuser.com/a/1045885> to disable
the on-board sound card and use the USB DAC by default:

```
# follow instructions at https://superuser.com/a/1045885
echo blacklist snd_bcm2835 | sudo tee -a /etc/modprobe.d/blacklist-snd_bcm2835.conf
sudo sed -i -e '/snd-usb-audio index=-2/ s/^/#/' /lib/modprobe.d/aliases.conf
```

Create a systemd unit file in `/etc/systemd/system/reccon.service`:

```
[Unit]
Description=Continuous audio recorder
After=network-online.target sound.target
Wants=network-online.target sound.target

[Service]
ExecStart=/home/pi/start.sh
WorkingDirectory=/home/pi
User=pi
Group=pi

[Install]
WantedBy=multi-user.target
```

Create the systemd symlinks:

```
sudo systemctl enable reccon
```

It should now launch on boot. Use `systemctl restart reccon` to poke it.
I've had lots of issues with it nondeterministically sometimes not
starting or starting but then failing because it doesn't see the sound
card. I don't know if that's because ALSA doesn't recognize the sound
card if it was plugged in at boot (sometimes), or because `reccon` /
`rec(1)` starts before ALSA. I just recently added the `sound.target`
unit to the `After=` and `Wants=` and am hoping that this helps.

## Etymology

It's called `reccon` because it **rec**ords **con**tinuously.
