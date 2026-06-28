//! Interactive voxio waveform viewer. Generates a tunable waveform from an audio
//! file and renders it live, so you can dial in the shaping controls visually.
//!
//!   cargo run --example waveform_compare --features waveform -- <audio-file>
//!
//! Controls:  [ / ] high-pass    up / down treble    m rms/peak
//!            , / . contrast     ; / ' local-norm    q / Esc quit

use std::{io, time::Instant};

use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind},
    layout::{Constraint, Layout, Rect},
    style::{Color, Stylize},
    symbols::Marker,
    widgets::{
        Block, Paragraph,
        canvas::{Canvas, Context, Line as CanvasLine, Rectangle},
    },
};

const WF_LEN: usize = 500;
const WAVEFORM_WIDGET_HEIGHT: f64 = 50.0;
const WAVEFORM_PAD_RATIO: f64 = 0.2; // runout above & below so peaks don't clip the border

const HP_PRESETS: [Option<f32>; 6] = [
    None,
    Some(100.0),
    Some(150.0),
    Some(250.0),
    Some(350.0),
    Some(500.0),
];
const TREBLE_STEP: f32 = 3.0;
const TREBLE_MAX: f32 = 30.0;
const CONTRAST_STEP: f32 = 0.25;
const CONTRAST_MAX: f32 = 3.0;
const LOCALNORM_STEP: f32 = 0.2;
const LOCALNORM_HALF_WINDOW: usize = 24; // starting window radius (bins)
const LOCALNORM_WINDOW_STEP: usize = 4;
const LOCALNORM_WINDOW_MIN: usize = 2;
const LOCALNORM_WINDOW_MAX: usize = 120;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: waveform_compare <audio-file>");
        std::process::exit(2);
    };
    let mut terminal = ratatui::init();
    let res = run(&mut terminal, &path);
    ratatui::restore();
    res.map_err(Into::into)
}

fn run(terminal: &mut ratatui::DefaultTerminal, path: &str) -> io::Result<()> {
    let mut hp_idx = 0usize;
    let mut treble_db = 0.0f32;
    let mut is_peak = false;
    let mut contrast = 1.0f32;
    let mut localnorm = 0.0f32;
    let mut half_window = LOCALNORM_HALF_WINDOW;
    let (mut wf, mut title) = make(
        path,
        HP_PRESETS[hp_idx],
        treble_db,
        is_peak,
        contrast,
        localnorm,
        half_window,
    );

    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            let [wf_area, footer] =
                Layout::vertical([Constraint::Min(2), Constraint::Length(1)]).areas(area);
            draw_pane(frame, wf_area, &wf, title.clone(), Color::Green);
            frame.render_widget(
                Paragraph::new(
                    "  [ / ] high-pass   up / down treble   m rms/peak   , / . contrast   ; / ' local-norm   left / right window   q / Esc quit",
                ).dim(),
                footer,
            );
        })?;

        if let Event::Key(k) = event::read()?
            && k.kind == KeyEventKind::Press
        {
            let mut changed = true;
            match k.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char(']') => hp_idx = (hp_idx + 1) % HP_PRESETS.len(),
                KeyCode::Char('[') => hp_idx = (hp_idx + HP_PRESETS.len() - 1) % HP_PRESETS.len(),
                KeyCode::Up => treble_db = (treble_db + TREBLE_STEP).min(TREBLE_MAX),
                KeyCode::Down => treble_db = (treble_db - TREBLE_STEP).max(0.0),
                KeyCode::Char('m') => is_peak = !is_peak,
                KeyCode::Char('.') => contrast = (contrast + CONTRAST_STEP).min(CONTRAST_MAX),
                KeyCode::Char(',') => contrast = (contrast - CONTRAST_STEP).max(1.0),
                KeyCode::Char('\'') => localnorm = (localnorm + LOCALNORM_STEP).min(1.0),
                KeyCode::Char(';') => localnorm = (localnorm - LOCALNORM_STEP).max(0.0),
                KeyCode::Right => {
                    half_window = (half_window + LOCALNORM_WINDOW_STEP).min(LOCALNORM_WINDOW_MAX)
                }
                KeyCode::Left => {
                    half_window = half_window
                        .saturating_sub(LOCALNORM_WINDOW_STEP)
                        .max(LOCALNORM_WINDOW_MIN)
                }
                //Dev debug
                KeyCode::Char('0') => {
                    hp_idx = 4;
                    treble_db = 9.0;
                    is_peak = false;
                    contrast = 1.5;
                    localnorm = 0.8;
                    half_window = 20;
                }
                _ => changed = false,
            }
            if changed {
                (wf, title) = make(
                    path,
                    HP_PRESETS[hp_idx],
                    treble_db,
                    is_peak,
                    contrast,
                    localnorm,
                    half_window,
                );
            }
        }
    }
}

fn make(
    path: &str,
    highpass_hz: Option<f32>,
    treble_db: f32,
    peak: bool,
    contrast: f32,
    localnorm: f32,
    half_window: usize,
) -> (Vec<f32>, String) {
    let opts = voxio::WaveformOptions {
        bins: WF_LEN,
        metric: if peak {
            voxio::BinMetric::Peak
        } else {
            voxio::BinMetric::Rms
        },
        highpass_hz,
        treble_db,
    };
    let (res, ms) = timed(|| voxio::Waveform::generate(path, &opts).map_err(|e| e.to_string()));

    let hp = match highpass_hz {
        Some(fc) => format!("hp{fc:.0}"),
        None => "raw".to_string(),
    };
    let metric = if peak { "peak" } else { "rms" };
    let con = if contrast > 1.0 {
        format!(" · γ{contrast:.2}")
    } else {
        String::new()
    };
    let ln = if localnorm > 0.0 {
        format!(" · ln{localnorm:.1} w{half_window}")
    } else {
        String::new()
    };
    let label = format!("{metric} · {hp} · +{treble_db:.0}dB{con}{ln}");

    match res {
        Ok(mut wf) => {
            // Library shaping helpers: local-norm → normalize → contrast.
            if localnorm > 0.0 {
                wf.local_normalize(localnorm, half_window);
            }
            wf.normalize();
            if contrast > 1.0 {
                wf.contrast(contrast);
            }
            (wf.bins, format!(" {label} — {ms:.1} ms "))
        }
        Err(e) => (Vec::new(), format!(" {label} — ERROR: {e} ")),
    }
}

fn draw_pane(frame: &mut Frame, area: Rect, wf: &[f32], title: String, color: Color) {
    let wide = area.width >= 170;
    let len = wf.len().max(1);
    let canvas = Canvas::default()
        .block(Block::bordered().title(title))
        .x_bounds([0.0, len as f64])
        .y_bounds([
            -WAVEFORM_WIDGET_HEIGHT * (1.0 + WAVEFORM_PAD_RATIO),
            WAVEFORM_WIDGET_HEIGHT * (1.0 + WAVEFORM_PAD_RATIO),
        ])
        .marker(Marker::Braille)
        .paint(move |ctx: &mut Context| {
            for (idx, &amp) in wf.iter().enumerate() {
                let hgt = (amp as f64 * WAVEFORM_WIDGET_HEIGHT).round();
                match wide {
                    true => ctx.draw(&Rectangle {
                        x: idx as f64,
                        y: -hgt,
                        width: 0.5,
                        height: hgt * 2.0,
                        color,
                    }),
                    false => ctx.draw(&CanvasLine {
                        x1: idx as f64,
                        x2: idx as f64,
                        y1: hgt,
                        y2: -hgt,
                        color,
                    }),
                }
            }
        });
    frame.render_widget(canvas, area);
}

fn timed<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed().as_secs_f64() * 1000.0)
}
