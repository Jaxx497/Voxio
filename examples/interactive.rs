//! Interactive player demonstrating the voxio API end to end.
//!
//! Usage: cargo run --example interactive -- <audio-file>
//!
//! Keys:
//!   space  toggle playback (replays the track if playback has stopped)
//!   n      seek forward 5s
//!   p      seek backward 5s
//!   ↑/+    volume up
//!   ↓/-    volume down
//!   s      stop playback
//!   q/Esc  quit
//!
//! The status pane shows live position and a level meter fed by the sample
//! tap; the log pane prints every event the engine emits.

use std::collections::VecDeque;
use std::env;
use std::io::{self, Write};
use std::time::Duration;

use crossterm::{
    cursor,
    event::{Event, KeyCode, KeyEventKind, KeyModifiers, poll, read},
    execute,
    terminal::{self, Clear, ClearType},
};
use voxio::{TapHandle, Vox};

const SEEK_STEP: f64 = 5.0;
const VOLUME_STEP: f32 = 0.05;
const VOLUME_MAX: f32 = 1.5; // matches Vox::set_volume's clamp ceiling
const LOG_LINES: usize = 10;
const BAR_WIDTH: usize = 40;

fn main() -> io::Result<()> {
    let path = env::args()
        .nth(1)
        .unwrap_or_else(|| "tests/test_suite/Preludes, Op. 28 - No. 16 'Hades'.mp3".to_string());

    // Capture panics from any thread (engine threads run in the background) before
    // the alternate screen eats them: restore the terminal so the message is visible.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), cursor::Show, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
        default_hook(info);
    }));

    let (mut vox, events) = Vox::new().expect("failed to start engine");
    let mut tap = vox.take_tap().expect("tap handle");
    vox.play(&path).expect("failed to play");

    terminal::enable_raw_mode()?;
    execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;

    let result = run(&mut vox, &events, &mut tap, &path);

    execute!(io::stdout(), cursor::Show, terminal::LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;
    result
}

fn run(
    vox: &mut Vox,
    events: &voxio::VoxEvents,
    tap: &mut voxio::TapHandle,
    path: &str,
) -> io::Result<()> {
    let mut log: VecDeque<String> = VecDeque::new();
    let mut out = io::stdout();

    loop {
        // Keyboard input
        if poll(Duration::from_millis(50))?
            && let Event::Key(key) = read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(());
                }
                KeyCode::Char(' ') => {
                    if vox.is_active() {
                        match vox.is_paused() {
                            true => vox.resume(),
                            false => vox.pause(),
                        }
                    } else {
                        let _ = vox.play(path);
                    }
                }
                KeyCode::Char('n') => {
                    vox.seek_relative(SEEK_STEP);
                }
                KeyCode::Char('p') => {
                    vox.seek_relative(-SEEK_STEP);
                }
                KeyCode::Up | KeyCode::Char('+') | KeyCode::Char('=') => {
                    vox.set_volume(vox.volume() + VOLUME_STEP);
                }
                KeyCode::Down | KeyCode::Char('-') | KeyCode::Char('_') => {
                    vox.set_volume(vox.volume() - VOLUME_STEP);
                }
                KeyCode::Char('s') => {
                    vox.stop();
                }
                _ => {}
            }
        }

        // Engine events → log pane
        while let Some(event) = events.try_recv() {
            let line = format!("{event:?}");
            log.push_back(line);
            if log.len() > LOG_LINES {
                log.pop_front();
            }
        }

        draw(&mut out, vox, tap, &log)?;
    }
}

fn draw(
    out: &mut io::Stdout,
    vox: &mut Vox,
    tap: &mut TapHandle,
    log: &VecDeque<String>,
) -> io::Result<()> {
    let pos = vox.position().as_secs_f64();
    let dur = vox.duration().as_secs_f64();

    let state = match (vox.is_active(), vox.is_paused()) {
        (false, _) => "STOPPED",
        (true, true) => "PAUSED ",
        (true, false) => "PLAYING",
    };

    // Progress bar
    let progress = if dur > 0.0 { (pos / dur).min(1.0) } else { 0.0 };
    let filled = (progress * BAR_WIDTH as f64) as usize;
    let bar: String = (0..BAR_WIDTH)
        .map(|i| if i < filled { '█' } else { '░' })
        .collect();

    // Level meter from the sample tap
    let level = tap.latest(1024).iter().fold(0f32, |m, s| m.max(s.abs()));
    let lit = (level.min(1.0) * BAR_WIDTH as f32) as usize;
    let meter: String = (0..BAR_WIDTH)
        .map(|i| if i < lit { '▮' } else { '·' })
        .collect();

    // Volume knob, scaled across the 0..=VOLUME_MAX range
    let volume = vox.volume();
    let knob_at = ((volume / VOLUME_MAX) * BAR_WIDTH as f32).round() as usize;
    let knob: String = (0..BAR_WIDTH)
        .map(|i| if i == knob_at { '●' } else { '─' })
        .collect();

    execute!(out, cursor::MoveTo(0, 0), Clear(ClearType::All))?;
    write!(
        out,
        "voxio interactive — space: play/pause  n/p: seek  ↑/↓: volume  s: stop  q: quit\r\n\
         \r\n\
         [{state}]  {bar}  {pos:6.1}s / {dur:.1}s\r\n\
         level    {meter}\r\n\
         volume   {knob}  {vol_pct:3.0}%\r\n\
         output   {} Hz, {} ch\r\n\
         \r\n\
         events:\r\n",
        vox.sample_rate(),
        vox.channels(),
        vol_pct = volume * 100.0,
    )?;
    for line in log {
        write!(out, "  {line}\r\n")?;
    }
    out.flush()
}
