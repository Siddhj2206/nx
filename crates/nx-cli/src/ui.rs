//! nx's output, ported from nx 0.x: colour that turns itself off for pipes,
//! the symbols every command shares, and padding that ignores colour codes.
//!
//! Results go to stdout and progress to stderr, so `nx list | grep` sees only
//! results. Every line has a two-space indent and one symbol for its kind:
//!
//! ```text
//! › step   ↑ upgraded   + added   - removed   ↓ fetched
//! ⚙ building   ✓ done   ! warning   ✗ error
//! ```
//!
//! Symbols and helpers land here as commands need them. Downloads print one
//! permanent `↓` line per package; the live region with progress bars
//! below them is still to come.

use std::fmt::Display;
use std::io::{self, IsTerminal, Write};

pub const SYM_STEP: &str = "›";
pub const SYM_ADDED: &str = "+";
pub const SYM_REMOVED: &str = "-";
pub const SYM_FETCH: &str = "↓";
pub const SYM_DONE: &str = "✓";
pub const SYM_WARN: &str = "!";
pub const SYM_ERROR: &str = "✗";

/// Colours text for one stream, or leaves it plain.
#[derive(Debug, Clone, Copy, Default)]
pub struct Style {
    on: bool,
}

impl Style {
    fn wrap(self, code: &str, x: &str) -> String {
        if !self.on || x.is_empty() {
            return x.to_owned();
        }
        format!("\x1b[{code}m{x}\x1b[0m")
    }

    pub fn bold(self, x: &str) -> String {
        self.wrap("1", x)
    }
    pub fn dim(self, x: &str) -> String {
        self.wrap("2", x)
    }
    pub fn green(self, x: &str) -> String {
        self.wrap("32", x)
    }
    pub fn cyan(self, x: &str) -> String {
        self.wrap("36", x)
    }
    pub fn red(self, x: &str) -> String {
        self.wrap("31", x)
    }
    pub fn yellow(self, x: &str) -> String {
        self.wrap("33", x)
    }
}

/// stdout or stderr, with its own colour decision.
pub struct Stream {
    w: Box<dyn Write>,
    pub style: Style,
}

impl Stream {
    /// Colour follows anstream's rules, the same ones clap uses: on for a
    /// terminal, off for pipes, `NO_COLOR` and `TERM=dumb`, forced on by
    /// `CLICOLOR_FORCE`.
    fn terminal<W: Write + IsTerminal + anstream::stream::RawStream + 'static>(w: W) -> Self {
        let colour = anstream::AutoStream::choice(&w) != anstream::ColorChoice::Never;
        Self {
            w: Box::new(w),
            style: Style { on: colour },
        }
    }

    /// Writes one indented line. A closed pipe (`nx status | head -1`) is not
    /// an error worth reporting, so write failures are dropped.
    pub fn line(&mut self, text: impl Display) {
        let _ = writeln!(self.w, "  {text}");
    }

    pub fn blank(&mut self) {
        let _ = writeln!(self.w);
    }

    /// A text block that already carries its own indentation (help).
    pub fn raw(&mut self, text: impl Display) {
        let _ = write!(self.w, "{text}");
    }

    /// An `info`-style row: the key dim and padded to 11 columns. Empty
    /// values are skipped, as in nx 0.x.
    pub fn row(&mut self, key: &str, value: &str) {
        if !value.is_empty() {
            let key = self.style.dim(&pad(key, 11));
            self.line(format_args!("{key} {value}"));
        }
    }
}

/// Both streams.
pub struct Ui {
    /// Results.
    pub out: Stream,
    /// Progress, steps, warnings, errors.
    pub err: Stream,
}

impl Ui {
    /// The process's own stdout and stderr.
    pub fn new() -> Self {
        Self {
            out: Stream::terminal(io::stdout()),
            err: Stream::terminal(io::stderr()),
        }
    }

    /// Uncoloured output into buffers, for tests.
    #[cfg(test)]
    pub fn plain(out: SharedBuf, err: SharedBuf) -> Self {
        Self {
            out: Stream {
                w: Box::new(out),
                style: Style::default(),
            },
            err: Stream {
                w: Box::new(err),
                style: Style::default(),
            },
        }
    }

    /// Announces something about to take a while, or a fact to check
    /// before anything changes.
    pub fn step(&mut self, msg: impl Display) {
        let sym = self.err.style.cyan(SYM_STEP);
        self.err.line(format_args!("{sym} {msg}"));
    }

    /// Reports success, on stdout.
    pub fn done(&mut self, msg: impl Display) {
        let sym = self.out.style.green(SYM_DONE);
        self.out.line(format_args!("{sym} {msg}"));
    }

    /// Reports something odd that didn't stop the work.
    pub fn warn(&mut self, msg: impl Display) {
        let sym = self.err.style.yellow(SYM_WARN);
        self.err.line(format_args!("{sym} {msg}"));
    }

    /// Reports a failure. Continuation lines get the same indent.
    pub fn error(&mut self, msg: impl Display) {
        let sym = self.err.style.red(SYM_ERROR);
        let msg = msg.to_string().replace('\n', "\n    ");
        self.err.line(format_args!("{sym} {msg}"));
    }
}

/// `2026-11-19 07:53` (UTC) for seconds since the epoch.
pub fn date(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        rem / 3_600,
        rem % 3_600 / 60
    )
}

/// A string's width on screen, ignoring colour codes. Every char counts as
/// one column.
pub fn visible_len(s: &str) -> usize {
    let mut n = 0;
    let mut esc = false;
    for c in s.chars() {
        if esc {
            if ('@'..='~').contains(&c) && c != '[' {
                esc = false;
            }
        } else if c == '\x1b' {
            esc = true;
        } else {
            n += 1;
        }
    }
    n
}

/// Right-pads `s` with spaces to `w` visible columns.
pub fn pad(s: &str, w: usize) -> String {
    let n = visible_len(s);
    if n >= w {
        return s.to_owned();
    }
    format!("{s}{}", " ".repeat(w - n))
}

/// A byte count for people: `1.4 MiB`.
pub fn size(n: u64) -> String {
    const KIB: u64 = 1 << 10;
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    match n {
        n if n >= GIB => format!("{:.1} GiB", n as f64 / GIB as f64),
        n if n >= MIB => format!("{:.1} MiB", n as f64 / MIB as f64),
        n if n >= KIB => format!("{} KiB", n / KIB),
        n => format!("{n} B"),
    }
}

/// `plural(1, "package")` is "1 package", `plural(2, "match")` "2 matches".
pub fn plural(n: usize, word: &str) -> String {
    let suffix = match n {
        1 => "",
        _ if ["s", "x", "ch", "sh"].iter().any(|e| word.ends_with(e)) => "es",
        _ => "s",
    };
    format!("{n} {word}{suffix}")
}

/// The mark on a package the image already has.
pub fn in_image(style: Style) -> String {
    style.green(&format!("{SYM_DONE} in the image"))
}

/// Cuts plain text to `w` columns with an ellipsis.
pub fn truncate(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(w.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The terminal's width, from `COLUMNS`, else 100 as in nx 0.x.
pub fn width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .filter(|&w| w > 20)
        .unwrap_or(100)
}

/// A cloneable in-memory writer for tests.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct SharedBuf(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

#[cfg(test)]
impl SharedBuf {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.borrow()).into_owned()
    }
}

#[cfg(test)]
impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_len_skips_colour() {
        let s = Style { on: true };
        assert_eq!(visible_len(&s.bold("nx")), 2);
        assert_eq!(visible_len("› step"), 6);
    }

    #[test]
    fn pad_counts_visible_columns() {
        let s = Style { on: true };
        assert_eq!(visible_len(&pad(&s.dim("Booted"), 11)), 11);
        assert_eq!(pad("longer than this", 4), "longer than this");
    }

    #[test]
    fn sizes_and_plurals() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(124 * 1024), "124 KiB");
        assert_eq!(size(3 * 1024 * 1024 / 2), "1.5 MiB");
        assert_eq!(plural(1, "package"), "1 package");
        assert_eq!(plural(3, "package"), "3 packages");
        assert_eq!(plural(2, "match"), "2 matches");
        assert_eq!(truncate("abcdef", 4), "abc…");
    }

    #[test]
    fn plain_style_adds_nothing() {
        assert_eq!(Style::default().red("x"), "x");
        assert_eq!(Style { on: true }.red(""), "");
    }

    #[test]
    fn error_indents_continuation_lines() {
        let err = SharedBuf::default();
        let mut ui = Ui::plain(SharedBuf::default(), err.clone());
        ui.error("first\nsecond");
        assert_eq!(err.text(), "  ✗ first\n    second\n");
    }

    #[test]
    fn formats_dates_in_utc() {
        assert_eq!(date(0), "1970-01-01 00:00");
        assert_eq!(date(951_782_400), "2000-02-29 00:00");
        assert_eq!(date(1_795_076_040), "2026-11-19 08:14");
    }
}
