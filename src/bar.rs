//! A single-line speed bar drawn with raw ANSI escapes.
//!
//! Every cell is the same glyph `▰`; lit cells are bright white and unlit
//! cells gray, so the bar reads as one shape whose lit portion moves.

use std::io::{IsTerminal, Write};

const CELL: &str = "▰";
const WIDTH: usize = 40;
const LIT: &str = "\x1b[97m";
const UNLIT: &str = "\x1b[90m";
const RESET: &str = "\x1b[0m";

/// Nice maxima for the scale. The bar starts at the first entry and only grows.
const SCALES: &[f64] = &[
    10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10_000.0, 20_000.0, 50_000.0, 100_000.0,
];

pub struct SpeedBar {
    label: &'static str,
    max_mbps: f64,
    tty: bool,
}

impl SpeedBar {
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            max_mbps: SCALES[0],
            tty: std::io::stdout().is_terminal(),
        }
    }

    /// Redraw the bar in place with the current speed. No-op when stdout is not a terminal.
    pub fn update(&mut self, mbps: f64) {
        self.grow(mbps);
        if self.tty {
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "\r\x1b[2K{}", self.render(mbps));
            let _ = out.flush();
        }
    }

    /// Draw the final value and move to the next line.
    pub fn finish(&mut self, mbps: f64) {
        self.grow(mbps);
        let mut out = std::io::stdout().lock();
        if self.tty {
            let _ = write!(out, "\r\x1b[2K");
        }
        let _ = writeln!(out, "{}", self.render(mbps));
        let _ = out.flush();
    }

    fn grow(&mut self, mbps: f64) {
        if let Some(&max) = SCALES.iter().find(|&&m| m >= self.max_mbps && mbps <= m) {
            self.max_mbps = max;
        } else if mbps > self.max_mbps {
            // Beyond the table: jump to its top, then keep doubling.
            self.max_mbps = self.max_mbps.max(SCALES[SCALES.len() - 1]);
            while mbps > self.max_mbps {
                self.max_mbps *= 2.0;
            }
        }
    }

    fn render(&self, mbps: f64) -> String {
        let lit = ((mbps / self.max_mbps) * WIDTH as f64).round().clamp(0.0, WIDTH as f64) as usize;
        let mut s = format!("{:<9} ", self.label);
        if self.tty {
            s.push_str(LIT);
            s.push_str(&CELL.repeat(lit));
            s.push_str(UNLIT);
            s.push_str(&CELL.repeat(WIDTH - lit));
            s.push_str(RESET);
        } else {
            s.push_str(&CELL.repeat(lit));
            s.push_str(&"▱".repeat(WIDTH - lit));
        }
        s.push_str(&format!("  {mbps:>7.1} Mbps  (0–{})", self.max_mbps as u64));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar() -> SpeedBar {
        SpeedBar { label: "x", max_mbps: SCALES[0], tty: false }
    }

    #[test]
    fn scale_starts_at_10_and_only_grows() {
        let mut b = bar();
        b.grow(4.0);
        assert_eq!(b.max_mbps, 10.0);
        b.grow(40.0);
        assert_eq!(b.max_mbps, 50.0);
        b.grow(150.0);
        assert_eq!(b.max_mbps, 200.0);
        b.grow(10.0);
        assert_eq!(b.max_mbps, 200.0);
        b.grow(950.0);
        assert_eq!(b.max_mbps, 1000.0);
    }

    #[test]
    fn scale_doubles_past_the_table() {
        let mut b = bar();
        b.grow(150_000.0);
        assert_eq!(b.max_mbps, 200_000.0);
        b.grow(150_000.0);
        assert_eq!(b.max_mbps, 200_000.0);
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
}
