//! Single-line terminal widgets drawn with raw ANSI escapes: a speed bar and a spinner.
//!
//! Every bar cell is the same glyph `▰`; lit cells are bright white and unlit
//! cells gray, so the bar reads as one shape whose lit portion moves.

use std::fmt::Write as _;
use std::io::{IsTerminal, Write as _};

const CELL: &str = "▰";
const WIDTH: usize = 40;
const LIT: &str = "\x1b[97m";
const GRAY: &str = "\x1b[90m";
const RESET: &str = "\x1b[0m";
const CLEAR_LINE: &str = "\r\x1b[2K";

/// Nice maxima for the scale, in Mbps. The bar starts at the first entry and only grows.
const SCALES: &[u64] = &[
    10, 20, 50, 100, 200, 500, 1000, 2000, 5000, 10_000, 20_000, 50_000, 100_000,
];

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}

pub struct SpeedBar {
    label: &'static str,
    max_mbps: u64,
    tty: bool,
}

impl SpeedBar {
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            max_mbps: SCALES[0],
            tty: stdout_is_tty(),
        }
    }

    /// Redraw the bar in place with the current speed. No-op when stdout is not a terminal.
    pub fn update(&mut self, mbps: f64) {
        self.grow(mbps);
        if self.tty {
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "{CLEAR_LINE}{}", self.render(mbps));
            let _ = out.flush();
        }
    }

    /// Draw the final value and move to the next line.
    pub fn finish(&mut self, mbps: f64) {
        self.grow(mbps);
        let mut out = std::io::stdout().lock();
        if self.tty {
            let _ = write!(out, "{CLEAR_LINE}");
        }
        let _ = writeln!(out, "{}", self.render(mbps));
        let _ = out.flush();
    }

    fn grow(&mut self, mbps: f64) {
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
    }

    fn render(&self, mbps: f64) -> String {
        let lit = lit_cells(mbps / self.max_mbps as f64);
        let mut s = format!("{:<9} ", self.label);
        if self.tty {
            s.push_str(LIT);
            s.push_str(&CELL.repeat(lit));
            s.push_str(GRAY);
            s.push_str(&CELL.repeat(WIDTH - lit));
            s.push_str(RESET);
        } else {
            s.push_str(&CELL.repeat(lit));
            s.push_str(&"▱".repeat(WIDTH - lit));
        }
        let _ = write!(s, "  {mbps:>7.1} Mbps  (0–{})", self.max_mbps);
        s
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
        let mut out = std::io::stdout().lock();
        let _ = write!(
            out,
            "{CLEAR_LINE}{:<9} {GRAY}{glyph} {detail}{RESET}",
            self.label
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
        assert_eq!(line.matches(CELL).count(), 33);
        assert_eq!(line.matches("▱").count(), 7);
        assert!(line.ends_with("412.5 Mbps  (0–500)"), "{line}");
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
