<div align="center">
<h1>Voxio</h1>

[![Crates.io Version](https://img.shields.io/crates/v/voxio?labelColor=444&color=A3079C)](https://crates.io/crates/voxio)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)

**A lightweight, Rust-powered audio playback library**
</div>

---

Originally designed as the tailor-made backend for
[Noctavox](https://crates.io/crates/noctavox) (a TUI music player), Voxio is a
batteries-included audio playback library for Rust applications. Built to be
responsive, lightweight, dependable, and simple.

## Quick Start

```toml
# Cargo.toml
[dependencies]
voxio = "0.2"
```

```rust
// main.rs
use voxio::{Vox, VoxEvent};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (vox, events) = Vox::new()?;
    vox.play("track.mp3")?;
    vox.set_next("track2.flac")?; // Prime next track for gapless transition

    while let Some(event) = events.recv_active(Duration::from_millis(50)) {
        match event {
            VoxEvent::TrackStarted { path, .. } => println!("Now playing: {}", path.display()),
            VoxEvent::DeviceChanged { .. } => println!("Device change detected!"),
            VoxEvent::Stopped => println!("Playback stopped. Engine closing."),
            _ => {}
        }
    }
    Ok(())
}
```

## Features

- **Gapless by design** — Prime the next track and Voxio hands off seamlessly
  when the current one ends. 
- **Plays everything** — MP3, AAC, FLAC, ALAC, Ogg Vorbis, WAV, AIFF, and Opus*
  out of the box, via [Symphonia](https://github.com/pdeljanov/Symphonia).
- **Just works across devices** — Automatic resampling to your output device,
  plus transparent recovery when a device disconnects or the system default
  changes mid-playback.
- **Loudness normalization** — Built-in ReplayGain (track or album mode) with
  peak clamping to keep volume consistent and clip-free.
- **Push-based events** — `VoxEvents` delivers playback events the moment they
  happen — no polling. Match on `TrackStarted`, `TrackEnded`, `Stopped`, and
  more for instant UI reactions.
- **Visualization tap** — A lock-free sample tap for meters, waveforms, and FFT
  displays.
- **Stays out of your way** — All audio work runs on its own threads behind a
  `Send` handle. Control it from anywhere; drop it to shut down cleanly.

> ***Note:** Opus is feature gated, but enabled by default

## What Voxio is Not

- Not capable of playing multiple sources together
- Not capable of augmenting playback (volume, speed, etc)
- Not designed to play sound from online sources (subject to change??)

## Examples

```bash
# Interactive terminal player — playback control, seeking, and a live level
meter cargo run --example interactive -- song.mp3

# Gapless transition demo
cargo run --example gapless -- track1.flac track2.flac
```

## Feature flags

**Opus** support is enabled by default. Drop it if you don't need it:

```toml
voxio = { version = "0.2", default-features = false }
```

## Disclaimer

**Testing tracks supplied from [https://musopen.org/]()**

Voxio is a work in progress. Please share your experiences and usecases so we
can improve Voxio.
