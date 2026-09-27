// PrintPuck's own screen design: a "dial" language for the round panel.
//
// This is deliberately NOT the assistant's card-stack / ring-plus-glyph
// language and not any reference project's layout. The organizing idea is a
// physical puck: a thin outer track that fills clockwise as the print
// progresses, with content zones sized to the inscribed circle (r=116).

extern crate alloc;
use alloc::{format, string::String};

use micromath::F32Ext;

use embedded_graphics::{
    mono_font::{ascii::FONT_10X20, ascii::FONT_6X10, MonoTextStyle},
    pixelcolor::Rgb565,
    prelude::*,
    text::Text,
};

use crate::model::{fmt_remaining, GcodeState, Status};
use crate::pins::{DISPLAY_HEIGHT, DISPLAY_WIDTH};

const W: i32 = DISPLAY_WIDTH as i32;
const H: i32 = DISPLAY_HEIGHT as i32;
const CENTER: Point = Point::new(W / 2, H / 2);
/// Usable radius inside the bezel.
const R: i32 = 116;

// Palette: deep ink background, ice-white primary text, one accent per
// semantic role. Nothing here matches the assistant's palette or any
// reference project's screenshots.
const BG: Rgb565 = Rgb565::new(2, 5, 9);
const TRACK: Rgb565 = Rgb565::new(6, 13, 19);
const INK: Rgb565 = Rgb565::new(31, 63, 31);
const MUTED: Rgb565 = Rgb565::new(16, 30, 20);
const ACCENT: Rgb565 = Rgb565::new(0, 40, 31);
const HOT: Rgb565 = Rgb565::new(31, 20, 0);
const WARM: Rgb565 = Rgb565::new(31, 38, 6);
const ERR: Rgb565 = Rgb565::new(31, 8, 6);
const OK: Rgb565 = Rgb565::new(4, 40, 16);

/// What the main loop currently shows. Ordered by escalation: later variants
/// override earlier ones when drawn as a full frame.
pub enum Screen {
    Boot { step: &'static str, progress: u8 },
    Portal { clients: u8 },
    Offline { reason: &'static str },
    Dashboard,
}

/// Draws `screen` into `frame` (a full 240*240*2 Rgb565 buffer).
pub fn draw(frame: &mut [u8], screen: &Screen, status: &Status, anim: u32) {
    let mut fb = Frame::new(frame);
    fb.clear(BG);
    match screen {
        Screen::Boot { step, progress } => fb.boot(*step, *progress),
        Screen::Portal { clients } => fb.portal(*clients),
        Screen::Offline { reason } => fb.offline(reason),
        Screen::Dashboard => fb.dashboard(status, anim),
    }
}

struct Frame<'a> {
    buf: &'a mut [u8],
}

impl<'a> Frame<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf }
    }

    /// Paints one pixel. Coordinates outside the panel are dropped; the
    /// inscribed-circle rule is the caller's job.
    fn px(&mut self, x: i32, y: i32, c: Rgb565) {
        if x < 0 || y < 0 || x >= W || y >= H {
            return;
        }
        let off = (y as usize * W as usize + x as usize) * 2;
        let b = c.to_be_bytes();
        if off + 1 < self.buf.len() {
            self.buf[off] = b[0];
            self.buf[off + 1] = b[1];
        }
    }

    fn clear(&mut self, c: Rgb565) {
        let b = c.to_be_bytes();
        for i in (0..self.buf.len()).step_by(2) {
            self.buf[i] = b[0];
            self.buf[i + 1] = b[1];
        }
    }

    fn filled_circle(&mut self, center: Point, r: i32, c: Rgb565) {
        for y in -r..=r {
            for x in -r..=r {
                if x * x + y * y <= r * r {
                    self.px(center.x + x, center.y + y, c);
                }
            }
        }
    }

    fn ring(&mut self, center: Point, r: i32, width: i32, from_deg: f32, sweep_deg: f32, c: Rgb565) {
        // Filled band between r-width..r, angles in degrees, 0 = 12 o'clock,
        // sweeping clockwise.
        let steps = ((sweep_deg.abs() / 2.0).ceil() as i32).max(2);
        for s in 0..steps {
            let t = s as f32 / (steps - 1) as f32;
            let deg = from_deg + sweep_deg * t;
            let rad = deg.to_radians() - core::f32::consts::FRAC_PI_2;
            let (sin, cos) = (rad.sin(), rad.cos());
            for rr in (r - width + 1)..=r {
                let x = center.x + (rr as f32 * cos).round() as i32;
                let y = center.y + (rr as f32 * sin).round() as i32;
                self.px(x, y, c);
            }
        }
    }

    /// Text drawn with a mono font, clipped to the panel.
    fn text(&mut self, t: &str, x: i32, y: i32, style: MonoTextStyle<Rgb565>, center: bool) {
        let w = t.chars().count() as i32 * style.font.character_size.width as i32;
        let x0 = if center { x - w / 2 } else { x };
        Text::new(t, Point::new(x0, y), style)
            .draw(self)
            .ok();
    }

    fn boot(&mut self, step: &str, progress: u8) {
        self.ring(CENTER, R - 12, 6, 0.0, 360.0 * progress as f32 / 100.0, ACCENT);
        self.ring(CENTER, R - 12, 6, 0.0, 360.0, TRACK);
        self.text("PRINTPUCK", CENTER.x, CENTER.y - 8, small_style(MUTED), true);
        self.text(step, CENTER.x, CENTER.y + 10, small_style(INK), true);
    }

    fn portal(&mut self, clients: u8) {
        self.ring(CENTER, R - 12, 6, 0.0, 360.0, TRACK);
        self.text("SETUP", CENTER.x, CENTER.y - 20, big_style(ACCENT), true);
        self.text("Join PrintPuck-Setup", CENTER.x, CENTER.y, small_style(INK), true);
        self.text("then open 192.168.4.1", CENTER.x, CENTER.y + 14, small_style(MUTED), true);
        if clients > 0 {
            let s = alloc::format!("{clients} client{}", if clients == 1 { "" } else { "s" });
            self.text(&s, CENTER.x, CENTER.y + 30, small_style(OK), true);
        }
    }

    fn offline(&mut self, reason: &str) {
        self.ring(CENTER, R - 12, 6, 0.0, 360.0, TRACK);
        self.text("OFFLINE", CENTER.x, CENTER.y - 10, big_style(MUTED), true);
        self.text(reason, CENTER.x, CENTER.y + 10, small_style(MUTED), true);
    }

    fn dashboard(&mut self, status: &Status, anim: u32) {
        // Outer progress track.
        self.ring(CENTER, R - 10, 8, 0.0, 360.0, TRACK);
        let (state_color, state_label): (Rgb565, String) = match status.state {
            Some(GcodeState::Failed) => (ERR, "FAILED".into()),
            Some(GcodeState::Finish) => (OK, "DONE".into()),
            Some(GcodeState::Pause) => (WARM, "PAUSED".into()),
            Some(GcodeState::Prepare) => (ACCENT, stage_or(status, "PREP")),
            Some(GcodeState::Running) => (ACCENT, stage_or(status, "PRINTING")),
            Some(GcodeState::Idle) => (MUTED, "IDLE".into()),
            _ => (MUTED, "WAITING".into()),
        };
        let pct = status.progress.unwrap_or(0.0);
        if pct > 0.0 {
            self.ring(CENTER, R - 10, 8, 0.0, 360.0 * pct / 100.0, state_color);
        }

        // Error flag overrides content with a plain error card.
        if status.error_code.is_some() {
            self.text("ERROR", CENTER.x, CENTER.y - 26, big_style(ERR), true);
            let code = status.error_code.unwrap();
            let s = alloc::format!("code {code:08X}");
            self.text(&s, CENTER.x, CENTER.y - 4, small_style(INK), true);
            self.text("check printer", CENTER.x, CENTER.y + 14, small_style(MUTED), true);
            return;
        }

        // Center: big percentage.
        let pct_s = alloc::format!("{:.0}%", pct);
        self.text(&pct_s, CENTER.x, CENTER.y - 30, big_style(INK), true);

        // State label under the percentage.
        self.text(&state_label, CENTER.x, CENTER.y - 10, small_style(state_color), true);

        // Remaining time.
        if let Some(min) = status.remaining_min {
            let s = fmt_remaining(min);
            self.text(&s, CENTER.x, CENTER.y + 8, small_style(INK), true);
            self.text("left", CENTER.x, CENTER.y + 20, small_style(MUTED), true);
        }

        // Bottom: layer + temps in one row.
        if let Some(layer) = status.layer {
            let s = if let Some(total) = status.layer_total {
                alloc::format!("L{layer}/{total}")
            } else {
                alloc::format!("L{layer}")
            };
            self.text(&s, CENTER.x, CENTER.y + 38, small_style(MUTED), true);
        }

        // Left/right: nozzle and bed temps.
        if let Some(t) = status.nozzle_temp {
            let s = alloc::format!("{t:.0}°");
            self.text(&s, CENTER.x - 62, CENTER.y + 8, small_style(HOT), false);
            self.text("noz", CENTER.x - 62, CENTER.y + 20, small_style(MUTED), false);
        }
        if let Some(t) = status.bed_temp {
            let s = alloc::format!("{t:.0}°");
            self.text(&s, CENTER.x + 44, CENTER.y + 8, small_style(WARM), false);
            self.text("bed", CENTER.x + 44, CENTER.y + 20, small_style(MUTED), false);
        }

        // AMS dots at the bottom arc (4 dots along the curve).
        if status.ams_active {
            let base = CENTER.y + 56;
            for (i, tray) in status.ams_trays.iter().enumerate() {
                let Some(tray) = tray else { continue };
                let x = CENTER.x + (i as i32 - 1) * 18 + 9;
                // Tray colors are stored as raw RGB565 u16; convert once.
                let c = tray
                    .color
                    .map(|raw| Rgb565::from(Rgb565::new((raw >> 11) as u8, ((raw >> 5) & 0x3F) as u8, (raw & 0x1F) as u8)))
                    .unwrap_or(MUTED);
                let r = if tray.active { 6 } else { 4 };
                self.filled_circle(Point::new(x, base), r, c);
            }
        }

        // Breathing accent dot while a print runs (anim = tick counter).
        if status.state == Some(GcodeState::Running) {
            let phase = (anim % 60) as f32 / 60.0;
            let breathe = ((phase * core::f32::consts::PI).sin() * 0.5 + 0.5) * 3.0 + 2.0;
            self.filled_circle(
                Point::new(CENTER.x, CENTER.y + 68),
                breathe as i32,
                state_color,
            );
        }
    }
}

fn stage_or(status: &Status, fallback: &str) -> String {
    match &status.stage {
        Some(s) => s.to_uppercase(),
        None => fallback.into(),
    }
}

fn small_style(c: Rgb565) -> MonoTextStyle<'static, Rgb565> {
    MonoTextStyle::new(&FONT_6X10, c)
}

fn big_style(c: Rgb565) -> MonoTextStyle<'static, Rgb565> {
    MonoTextStyle::new(&FONT_10X20, c)
}

/// embedded-graphics DrawTarget impl over the raw framebuffer.
impl DrawTarget for Frame<'_> {
    type Color = Rgb565;
    type Error = core::convert::Infallible;

    fn draw_iter<I: IntoIterator<Item = Pixel<Self::Color>>>(
        &mut self,
        pixels: I,
    ) -> Result<(), Self::Error> {
        for Pixel(p, c) in pixels {
            self.px(p.x, p.y, c);
        }
        Ok(())
    }
}

impl OriginDimensions for Frame<'_> {
    fn size(&self) -> Size {
        Size::new(W as u32, H as u32)
    }
}
