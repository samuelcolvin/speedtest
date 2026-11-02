//! Single-line terminal widgets drawn with raw ANSI escapes: a speed bar and a spinner.
//!
//! Every bar cell is the same glyph `▰`; lit cells are bright white and unlit
//! cells gray, so the bar reads as one shape whose lit portion moves.

use std::fmt::Write as _;
use std::io::{IsTerminal, Write as _};

const CELL: &str = "▰";
const WIDTH: usize = 50;
const LIT: &str = "\x1b[97m";
const GRAY: &str = "\x1b[90m";
const RESET: &str = "\x1b[0m";
const CLEAR_LINE: &str = "\r\x1b[2K";
const CURSOR_UP: &str = "\x1b[1A";
const HIDE_CURSOR: &str = "\x1b[?25l";
pub const SHOW_CURSOR: &str = "\x1b[?25h";

/// Nice maxima for the scale, in Mbps. The bar starts at the first entry and only grows.
const SCALES: &[u64] = &[
    10, 20, 50, 100, 200, 500, 1000, 2000, 5000, 10_000, 20_000, 50_000, 100_000,
];

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}

/// Redraws the current line with `text` and leaves a blank line below it, returning the cursor
/// to the start of the redrawn line so the next redraw replaces it.
fn draw_live(out: &mut impl std::io::Write, text: &str) {
    let _ = write!(out, "{CLEAR_LINE}{text}\n{CLEAR_LINE}{CURSOR_UP}");
}

/// Draws the final version of the current line and moves to the blank line below it.
fn draw_final(out: &mut impl std::io::Write, tty: bool, text: &str) {
    if tty {
        let _ = write!(out, "{CLEAR_LINE}");
    }
    let _ = writeln!(out, "{text}");
}

/// A bar that has stopped updating. A bar drawn directly below it shares its scale, and
/// redraws it if that scale has to grow.
pub struct Finished {
    label: &'static str,
    mbps: f64,
    max_mbps: u64,
}

pub struct SpeedBar {
    label: &'static str,
    max_mbps: u64,
    tty: bool,
    above: Option<Finished>,
}

impl SpeedBar {
    /// A new bar. With `above`, the bar starts on that bar's scale and keeps the two in step.
    pub fn new(label: &'static str, above: Option<Finished>) -> Self {
        Self {
            label,
            max_mbps: above.as_ref().map_or(SCALES[0], |a| a.max_mbps),
            tty: stdout_is_tty(),
            above,
        }
    }

    /// Redraw the bar in place with the current speed. No-op when stdout is not a terminal.
    pub fn update(&mut self, mbps: f64) {
        let grew = self.grow(mbps);
        if self.tty {
            let mut out = std::io::stdout().lock();
            self.redraw_above(&mut out, grew);
            draw_live(&mut out, &self.render(mbps));
            let _ = out.flush();
        }
    }

    /// Draw the final value and move to the next line.
    pub fn finish(mut self, mbps: f64) -> Finished {
        let grew = self.grow(mbps);
        let mut out = std::io::stdout().lock();
        if self.tty {
            self.redraw_above(&mut out, grew);
        }
        draw_final(&mut out, self.tty, &self.render(mbps));
        let _ = out.flush();
        Finished {
            label: self.label,
            mbps,
            max_mbps: self.max_mbps,
        }
    }

    /// Redraw the finished bar on the line above when the shared scale has grown.
    fn redraw_above(&self, out: &mut impl std::io::Write, grew: bool) {
        if let (true, Some(above)) = (grew, &self.above) {
            let line = render(above.label, above.mbps, self.max_mbps, self.tty);
            let _ = writeln!(out, "{CURSOR_UP}{CLEAR_LINE}{line}");
        }
    }

    /// Grow the scale to fit `mbps`, returning whether it changed.
    fn grow(&mut self, mbps: f64) -> bool {
        let before = self.max_mbps;
        let fits = |m: u64| mbps <= m as f64;
        if let Some(&max) = SCALES.iter().find(|&&m| m >= self.max_mbps && fits(m)) {
            self.max_mbps = max;
        } else if !fits(self.max_mbps) {
            // Beyond the table: jump to its top, then keep doubling.
            self.max_mbps = self.max_mbps.max(SCALES[SCALES.len() - 1]);
            while !fits(self.max_mbps) {
                self.max_mbps *= 2;
            }
        }
        self.max_mbps != before
    }

    fn render(&self, mbps: f64) -> String {
        render(self.label, mbps, self.max_mbps, self.tty)
    }
}

fn render(label: &str, mbps: f64, max_mbps: u64, tty: bool) -> String {
    let lit = lit_cells(mbps / max_mbps as f64);
    let mut s = format!("{label:<9} ");
    if tty {
        s.push_str(LIT);
        s.push_str(&CELL.repeat(lit));
        s.push_str(GRAY);
        s.push_str(&CELL.repeat(WIDTH - lit));
        s.push_str(RESET);
    } else {
        s.push_str(&CELL.repeat(lit));
        s.push_str(&"▱".repeat(WIDTH - lit));
    }
    let _ = write!(s, "  {:>7} Mbps  (0–{max_mbps})", format_mbps(mbps));
    s
}

/// More decimals as the speed drops, so slow links still show a meaningful figure:
/// one place from 10 Mbps up, two below 10, three below 1.
fn format_mbps(mbps: f64) -> String {
    if mbps >= 10.0 {
        format!("{mbps:.1}")
    } else if mbps >= 1.0 {
        format!("{mbps:.2}")
    } else {
        format!("{mbps:.3}")
    }
}

/// Number of lit cells for a fill fraction, rounded to nearest and clamped to the bar.
fn lit_cells(fraction: f64) -> usize {
    let cells = (fraction * WIDTH as f64).round();
    // Clamped to 0..=WIDTH, so the conversion is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lit = cells.clamp(0.0, WIDTH as f64) as usize;
    lit
}

/// Hides the terminal cursor for as long as the guard lives, so it doesn't blink at the end
/// of the live-updating lines. Does nothing when stdout is not a terminal.
pub struct HiddenCursor {
    tty: bool,
}

impl HiddenCursor {
    pub fn new() -> Self {
        let tty = stdout_is_tty();
        if tty {
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "{HIDE_CURSOR}");
            let _ = out.flush();
        }
        Self { tty }
    }
}

impl Drop for HiddenCursor {
    fn drop(&mut self) {
        if self.tty {
            // The cursor is on the blank line below the output; step past it so the blank
            // line stays between the output and whatever comes next.
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "\n{SHOW_CURSOR}");
            let _ = out.flush();
        }
    }
}

/// A line of text redrawn in place, for results that refine as they arrive.
pub struct LiveLine {
    tty: bool,
}

impl LiveLine {
    pub fn new() -> Self {
        Self {
            tty: stdout_is_tty(),
        }
    }

    /// Replace the current line. No-op when stdout is not a terminal.
    pub fn update(&self, text: &str) {
        if self.tty {
            let mut out = std::io::stdout().lock();
            draw_live(&mut out, text);
            let _ = out.flush();
        }
    }

    /// Replace the current line for the last time and move to the next.
    pub fn finish(&self, text: &str) {
        let mut out = std::io::stdout().lock();
        draw_final(&mut out, self.tty, text);
        let _ = out.flush();
    }
}

/// An animated spinner shown while a phase has nothing else to display yet.
pub struct Spinner {
    label: &'static str,
    frame: usize,
    tty: bool,
}

impl Spinner {
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            frame: 0,
            tty: stdout_is_tty(),
        }
    }

    /// Advance the animation and redraw with `detail` after the spinner glyph.
    pub fn tick(&mut self, detail: &str) {
        if !self.tty {
            return;
        }
        let glyph = SPINNER_FRAMES[self.frame % SPINNER_FRAMES.len()];
        self.frame += 1;
        let detail = if detail.is_empty() {
            String::new()
        } else {
            format!(" {detail}")
        };
        let mut out = std::io::stdout().lock();
        draw_live(
            &mut out,
            &format!("{:<9} {GRAY}{glyph}{detail}{RESET}", self.label),
        );
        let _ = out.flush();
    }

    /// Erase the spinner line so the final output can take its place.
    pub fn clear(&self) {
        if self.tty {
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "{CLEAR_LINE}");
            let _ = out.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar() -> SpeedBar {
        SpeedBar {
            label: "x",
            max_mbps: SCALES[0],
            tty: false,
            above: None,
        }
    }

    #[test]
    fn scale_starts_at_10_and_only_grows() {
        let mut b = bar();
        b.grow(4.0);
        assert_eq!(b.max_mbps, 10);
        b.grow(40.0);
        assert_eq!(b.max_mbps, 50);
        b.grow(150.0);
        assert_eq!(b.max_mbps, 200);
        b.grow(10.0);
        assert_eq!(b.max_mbps, 200);
        b.grow(950.0);
        assert_eq!(b.max_mbps, 1000);
    }

    #[test]
    fn bar_below_starts_on_the_finished_scale() {
        let mut download = bar();
        download.grow(150.0);
        let finished = download.finish(150.0);
        let mut upload = SpeedBar::new("Upload", Some(finished));
        assert_eq!(upload.max_mbps, 200);
        assert!(!upload.grow(20.0));
        assert!(upload.grow(300.0));
        assert_eq!(upload.max_mbps, 500);
    }

    #[test]
    fn scale_doubles_past_the_table() {
        let mut b = bar();
        b.grow(150_000.0);
        assert_eq!(b.max_mbps, 200_000);
        b.grow(150_000.0);
        assert_eq!(b.max_mbps, 200_000);
    }

    #[test]
    fn render_lights_proportional_cells() {
        let mut b = bar();
        b.grow(412.5);
        let line = b.render(412.5);
        assert_eq!(line.matches(CELL).count(), 41);
        assert_eq!(line.matches("▱").count(), 9);
        assert!(line.ends_with("412.5 Mbps  (0–500)"), "{line}");
    }

    #[test]
    fn decimals_increase_as_speed_drops() {
        assert_eq!(format_mbps(412.53), "412.5");
        assert_eq!(format_mbps(10.0), "10.0");
        assert_eq!(format_mbps(9.876), "9.88");
        assert_eq!(format_mbps(1.0), "1.00");
        assert_eq!(format_mbps(0.1234), "0.123");
        assert_eq!(format_mbps(0.0), "0.000");
    }

    #[test]
    fn lit_cells_is_clamped() {
        assert_eq!(lit_cells(-1.0), 0);
        assert_eq!(lit_cells(0.0), 0);
        assert_eq!(lit_cells(0.5), WIDTH / 2);
        assert_eq!(lit_cells(1.0), WIDTH);
        assert_eq!(lit_cells(7.0), WIDTH);
    }
}
