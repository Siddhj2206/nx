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
//! On a terminal, steps (`›`, `↓`, `⚙ building`) share one status line on
//! stderr that each replaces, and only results, warnings and errors stay;
//! piped, in the journal or with `-v`, every step prints. An error is a
//! [`Problem`]: a headline, the reasons, and the commands that fix it.

use std::cell::{Cell, RefCell};
use std::fmt::Display;
use std::io::{self, IsTerminal, Write};
use std::rc::Rc;

mod problem;
pub use problem::Problem;

pub const SYM_STEP: &str = "›";
pub const SYM_UPGRADED: &str = "↑";
pub const SYM_ADDED: &str = "+";
pub const SYM_REMOVED: &str = "-";
pub const SYM_FETCH: &str = "↓";
pub const SYM_DONE: &str = "✓";
pub const SYM_WARN: &str = "!";
pub const SYM_ERROR: &str = "✗";
pub const SYM_SERVICE: &str = "⚙";

/// Starts every escape code; only [`Style`] writes them, and the width
/// helpers below skip them.
const ESC: char = '\x1b';

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
        format!("{ESC}[{code}m{x}{}", Self::reset())
    }

    /// The code that ends a colour [`Style::wrap`] began.
    const fn reset() -> &'static str {
        "\x1b[0m"
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

    /// Back to the start of the line and erase it: the status line's
    /// reset. Written only when stderr is a terminal ([`Live`]).
    const fn clear_line() -> &'static str {
        "\r\x1b[2K"
    }
}

/// The status line steps share on a terminal. Both streams hold it, so
/// whatever either prints first erases the step it would otherwise land
/// beside.
struct Live {
    /// stderr is a terminal and `-v` wasn't given.
    on: Cell<bool>,
    /// A step is on screen, without its newline.
    shown: Cell<bool>,
    /// Where it is drawn: stderr.
    w: RefCell<Box<dyn Write>>,
    width: usize,
}

impl Live {
    #[cfg(test)]
    fn off() -> Self {
        Self {
            on: Cell::new(false),
            shown: Cell::new(false),
            w: RefCell::new(Box::new(io::sink())),
            width: 0,
        }
    }

    fn clear(&self) {
        if self.shown.replace(false) {
            let mut w = self.w.borrow_mut();
            let _ = write!(w, "{}", Style::clear_line());
            let _ = w.flush();
        }
    }

    /// Replaces the status line with `text`, cut to the terminal.
    fn draw(&self, text: &str) {
        let mut w = self.w.borrow_mut();
        let text = truncate(text, self.width.saturating_sub(3));
        let _ = write!(w, "{}  {text}", Style::clear_line());
        let _ = w.flush();
        self.shown.set(true);
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.clear();
    }
}

/// stdout or stderr, with its own colour decision.
pub struct Stream {
    w: Box<dyn Write>,
    pub style: Style,
    /// `-v`: lists whole, generation numbers, every step kept.
    pub verbose: bool,
    live: Rc<Live>,
}

impl Stream {
    /// Colour follows anstream's rules, the same ones clap uses: on for a
    /// terminal, off for pipes, `NO_COLOR` and `TERM=dumb`, forced on by
    /// `CLICOLOR_FORCE`.
    fn terminal<W: Write + IsTerminal + anstream::stream::RawStream + 'static>(
        w: W,
        live: Rc<Live>,
    ) -> Self {
        let colour = anstream::AutoStream::choice(&w) != anstream::ColorChoice::Never;
        Self {
            w: Box::new(w),
            style: Style { on: colour },
            verbose: false,
            live,
        }
    }

    /// Writes one indented line. A closed pipe (`nx status | head -1`) is not
    /// an error worth reporting, so write failures are dropped.
    pub fn line(&mut self, text: impl Display) {
        self.live.clear();
        let _ = writeln!(self.w, "  {text}");
    }

    /// A step: on the status line when there is one, else a line of its
    /// own.
    pub fn status(&mut self, text: impl Display) {
        if self.live.on.get() {
            self.live.draw(&text.to_string());
        } else {
            self.line(text);
        }
    }

    /// `↓ what  detail`, something fetched: a step.
    pub fn fetched(&mut self, what: &str, detail: &str) {
        let st = self.style;
        let detail = if detail.is_empty() {
            String::new()
        } else {
            format!("  {}", st.dim(detail))
        };
        self.status(format_args!("{} {what}{detail}", st.cyan(SYM_FETCH)));
    }

    /// A line that starts with its kind's symbol. Continuation lines are
    /// indented past the symbol, so a message of several lines stays one
    /// block.
    pub fn symbol(&mut self, sym: &str, msg: impl Display) {
        let msg = msg.to_string().replace('\n', "\n    ");
        self.line(format_args!("{sym} {msg}"));
    }

    pub fn blank(&mut self) {
        self.live.clear();
        let _ = writeln!(self.w);
    }

    /// A text block that already carries its own indentation (help).
    pub fn raw(&mut self, text: impl Display) {
        self.live.clear();
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
    /// The process's own stdout and stderr. Steps share a status line when
    /// stderr is a terminal that can erase one.
    pub fn new() -> Self {
        let stderr = io::stderr();
        let dumb = std::env::var_os("TERM").is_none_or(|t| t == "dumb");
        let live = Rc::new(Live {
            on: Cell::new(stderr.is_terminal() && !dumb),
            shown: Cell::new(false),
            w: RefCell::new(Box::new(io::stderr())),
            width: width(),
        });
        Self {
            out: Stream::terminal(io::stdout(), Rc::clone(&live)),
            err: Stream::terminal(stderr, live),
        }
    }

    /// `-v`: every step on a line of its own, lists whole, generation
    /// numbers in the results.
    pub fn set_verbose(&mut self, verbose: bool) {
        self.out.verbose = verbose;
        self.err.verbose = verbose;
        if verbose {
            self.err.live.clear();
            self.err.live.on.set(false);
        }
    }

    pub fn verbose(&self) -> bool {
        self.out.verbose
    }

    /// Uncoloured output into buffers, for tests.
    #[cfg(test)]
    pub fn plain(out: SharedBuf, err: SharedBuf) -> Self {
        let live = Rc::new(Live::off());
        Self {
            out: Stream {
                w: Box::new(out),
                style: Style::default(),
                verbose: false,
                live: Rc::clone(&live),
            },
            err: Stream {
                w: Box::new(err),
                style: Style::default(),
                verbose: false,
                live,
            },
        }
    }

    /// [`Ui::plain`] with a status line drawn into `err`, as on a terminal
    /// `width` columns wide.
    #[cfg(test)]
    pub fn live(out: SharedBuf, err: SharedBuf, width: usize) -> Self {
        let ui = Self::plain(out, err.clone());
        let live = Rc::new(Live {
            on: Cell::new(true),
            shown: Cell::new(false),
            w: RefCell::new(Box::new(err)),
            width,
        });
        Self {
            out: Stream {
                live: Rc::clone(&live),
                ..ui.out
            },
            err: Stream { live, ..ui.err },
        }
    }

    /// Announces something about to take a while: a step, replaced by the
    /// next one on a terminal.
    pub fn step(&mut self, msg: impl Display) {
        let sym = self.err.style.cyan(SYM_STEP);
        let msg = msg.to_string().replace('\n', "\n    ");
        self.err.status(format_args!("{sym} {msg}"));
    }

    /// A step only `-v` shows.
    pub fn detail(&mut self, msg: impl Display) {
        if self.err.verbose {
            self.step(msg);
        }
    }

    /// A build that takes a while: a step.
    pub fn building(&mut self, msg: impl Display) {
        let sym = self.err.style.cyan(SYM_SERVICE);
        self.err.status(format_args!("{sym} {msg}"));
    }

    /// Reports success, on stdout.
    pub fn done(&mut self, msg: impl Display) {
        let sym = self.out.style.green(SYM_DONE);
        self.out.symbol(&sym, msg);
    }

    /// Reports something odd that didn't stop the work.
    pub fn warn(&mut self, msg: impl Display) {
        let sym = self.err.style.yellow(SYM_WARN);
        self.err.symbol(&sym, msg);
    }

    /// Reports something done to a unit, user or file.
    pub fn service(&mut self, msg: impl Display) {
        let sym = self.err.style.cyan(SYM_SERVICE);
        self.err.symbol(&sym, msg);
    }

    /// Reports a failure.
    pub fn error(&mut self, msg: impl Display) {
        let sym = self.err.style.red(SYM_ERROR);
        self.err.symbol(&sym, msg);
    }

    /// Reports a failure as a block: `✗` headline, reasons, fixes.
    pub fn problem(&mut self, p: &Problem) {
        let sym = self.err.style.red(SYM_ERROR);
        self.block(&sym, p);
    }

    /// An error that didn't stop the work: `headline`, then the error's
    /// own headline and reasons.
    pub fn warn_error(&mut self, headline: impl Into<String>, e: &anyhow::Error) {
        let why = Problem::from_error(e);
        let mut p = Problem::new(headline)
            .reason(why.headline)
            .reasons(why.reasons);
        p.fixes = why.fixes;
        self.warn_problem(&p);
    }

    /// A [`Problem`] that didn't stop the work (another image's, say): the
    /// same block under `!`.
    pub fn warn_problem(&mut self, p: &Problem) {
        let sym = self.err.style.yellow(SYM_WARN);
        self.block(&sym, p);
    }

    fn block(&mut self, sym: &str, p: &Problem) {
        let s = &mut self.err;
        let st = s.style;
        s.symbol(sym, &p.headline);
        for r in &p.reasons {
            s.line(format_args!("  {}", r.replace('\n', "\n    ")));
        }
        if !p.more.is_empty() {
            if s.verbose {
                for d in &p.more {
                    s.line(format_args!("    {}", st.dim(d)));
                }
            } else {
                s.line(format_args!(
                    "  {}",
                    st.dim(&format!("… {} more (-v shows them)", p.more.len()))
                ));
            }
        }
        if p.fixes.is_empty() {
            return;
        }
        if !p.reasons.is_empty() || !p.more.is_empty() {
            s.blank();
        }
        let w = p
            .fixes
            .iter()
            .map(|f| f.say.chars().count())
            .max()
            .unwrap_or(0);
        for f in &p.fixes {
            if f.say.is_empty() {
                s.line(format_args!("  {}", st.bold(&f.run)));
            } else {
                s.line(format_args!(
                    "  {}  {}",
                    st.dim(&pad(&f.say, w)),
                    st.bold(&f.run)
                ));
            }
        }
    }
}

/// When something happened, for people: `just now`, `5 min ago`, `2 h
/// ago`, `yesterday`, `4 days ago`, then the date (rule 6).
pub fn ago(secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    ago_from(secs, now, &jiff::tz::TimeZone::system())
}

fn ago_from(secs: u64, now: u64, tz: &jiff::tz::TimeZone) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    let Some(age) = now.checked_sub(secs) else {
        return date_in(secs, tz);
    };
    match age {
        a if a < MIN => "just now".to_owned(),
        a if a < HOUR => format!("{} min ago", a / MIN),
        a if a < DAY => format!("{} h ago", a / HOUR),
        a if a < 2 * DAY => "yesterday".to_owned(),
        a if a < 14 * DAY => format!("{} days ago", a / DAY),
        _ => date_in(secs, tz)
            .split(' ')
            .next()
            .unwrap_or_default()
            .to_owned(),
    }
}

/// `2026-11-19 13:23` for seconds since the epoch, in the system's time
/// zone (`TZ`, else `/etc/localtime`).
pub fn date(secs: u64) -> String {
    date_in(secs, &jiff::tz::TimeZone::system())
}

fn date_in(secs: u64, tz: &jiff::tz::TimeZone) -> String {
    i64::try_from(secs)
        .ok()
        .and_then(|s| jiff::Timestamp::from_second(s).ok())
        .map_or_else(
            || "unknown".to_owned(),
            |t| {
                t.to_zoned_datetime(tz.clone())
                    .strftime("%Y-%m-%d %H:%M")
                    .to_string()
            },
        )
}

/// `sha256:92409d3fcf8435…` → `sha256:92409d3fcf84`, as short as nx 0.x
/// showed nixpkgs revisions, long enough to tell images apart.
pub fn short_digest(digest: &str) -> &str {
    let end = digest.find(':').map_or(12, |colon| colon + 13);
    digest.get(..end).unwrap_or(digest)
}

/// A string's width on screen in columns, ignoring colour codes; wide
/// characters (CJK, most emoji) count two.
pub fn visible_len(s: &str) -> usize {
    let mut n = 0;
    let mut esc = false;
    for c in s.chars() {
        if esc {
            esc = !ends_escape(c);
        } else if c == ESC {
            esc = true;
        } else {
            n += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        }
    }
    n
}

/// The final byte of a CSI escape code (`\x1b[1m`).
fn ends_escape(c: char) -> bool {
    ('@'..='~').contains(&c) && c != '['
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

/// The word that agrees with a count: `agree(1, "has", "have")` is "has".
pub fn agree<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

/// The mark on a package the image already has.
pub fn in_image(style: Style) -> String {
    style.green(&format!("{SYM_DONE} in the image"))
}

/// Cuts text that may carry colour codes to `w` visible columns, with an
/// ellipsis, and resets the colour if the cut fell inside one.
pub fn truncate(s: &str, w: usize) -> String {
    if visible_len(s) <= w {
        return s.to_owned();
    }
    let (mut out, mut n, mut esc, mut coloured) = (String::new(), 0, false, false);
    for c in s.chars() {
        if esc {
            out.push(c);
            esc = !ends_escape(c);
            continue;
        }
        if c == ESC {
            (esc, coloured) = (true, true);
            out.push(c);
            continue;
        }
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        // One column stays for the ellipsis.
        if n + cw >= w {
            break;
        }
        out.push(c);
        n += cw;
    }
    out.push('…');
    if coloured {
        out.push_str(Style::reset());
    }
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
        // Wide characters take two columns, so fewer of them fit.
        assert_eq!(visible_len("日本語"), 6);
        assert_eq!(truncate("日本語です", 5), "日本…");
        let s = Style { on: true };
        assert_eq!(
            truncate(&s.dim("abcdef"), 4),
            format!("\x1b[2mabc…{}", Style::reset())
        );
    }

    #[test]
    fn plain_style_adds_nothing() {
        assert_eq!(Style::default().red("x"), "x");
        assert_eq!(Style { on: true }.red(""), "");
    }

    #[test]
    fn every_kind_indents_continuation_lines() {
        let (out, err) = (SharedBuf::default(), SharedBuf::default());
        let mut ui = Ui::plain(out.clone(), err.clone());
        ui.error("first\nsecond");
        ui.warn("advice\n  nx add memcached");
        ui.done("one\ntwo");
        assert_eq!(
            err.text(),
            "  ✗ first\n    second\n  ! advice\n      nx add memcached\n"
        );
        assert_eq!(out.text(), "  ✓ one\n    two\n");
    }

    #[test]
    fn steps_share_one_status_line_on_a_terminal() {
        let (out, err) = (SharedBuf::default(), SharedBuf::default());
        let mut ui = Ui::live(out.clone(), err.clone(), 30);
        ui.step("resolving");
        ui.err.fetched("wayland-devel", "1.26.0-3.fc45  161 KiB");
        ui.warn("stays");
        ui.step("building");
        ui.done("added");
        assert_eq!(
            err.text(),
            "\r\x1b[2K  › resolving\
             \r\x1b[2K  ↓ wayland-devel  1.26.0-3.…\
             \r\x1b[2K  ! stays\n\
             \r\x1b[2K  › building\
             \r\x1b[2K"
        );
        assert_eq!(out.text(), "  ✓ added\n");
    }

    #[test]
    fn an_error_mid_step_clears_the_status_line_first() {
        let err = SharedBuf::default();
        let mut ui = Ui::live(SharedBuf::default(), err.clone(), 80);
        ui.step("downloading 3 packages");
        ui.problem(&Problem::new("can't reach the mirror").fix("Try again", "nx add jq"));
        assert_eq!(
            err.text(),
            "\r\x1b[2K  › downloading 3 packages\
             \r\x1b[2K  ✗ can't reach the mirror\n\
             \x20   Try again  nx add jq\n"
        );
    }

    #[test]
    fn verbose_and_pipes_keep_every_step() {
        let (out, err) = (SharedBuf::default(), SharedBuf::default());
        let mut ui = Ui::live(out, err.clone(), 30);
        ui.set_verbose(true);
        ui.step("resolving");
        ui.detail("rooted generation 3");
        assert_eq!(err.text(), "  › resolving\n  › rooted generation 3\n");
        let err = SharedBuf::default();
        let mut ui = Ui::plain(SharedBuf::default(), err.clone());
        ui.step("one");
        ui.detail("hidden");
        assert_eq!(err.text(), "  › one\n");
    }

    #[test]
    fn a_problem_is_a_headline_reasons_and_fixes() {
        let err = SharedBuf::default();
        let mut ui = Ui::plain(SharedBuf::default(), err.clone());
        let p = Problem::new("memcached-selinux has scriptlets nx can't translate")
            .reason("%pre relabels files for SELinux")
            .more(["fixfiles -R memcached restore"])
            .fix(
                "Put it in the image",
                "RUN dnf install -y memcached-selinux",
            )
            .fix(
                "Or skip them",
                "nx add memcached --allow-scriptlets memcached-selinux=0123",
            );
        ui.problem(&p);
        assert_eq!(
            err.text(),
            "  ✗ memcached-selinux has scriptlets nx can't translate\n\
             \x20   %pre relabels files for SELinux\n\
             \x20   … 1 more (-v shows them)\n\
             \n\
             \x20   Put it in the image  RUN dnf install -y memcached-selinux\n\
             \x20   Or skip them         nx add memcached --allow-scriptlets memcached-selinux=0123\n"
        );
    }

    #[test]
    fn times_are_relative() -> Result<(), jiff::Error> {
        let utc = jiff::tz::TimeZone::UTC;
        let now = 1_795_076_040;
        assert_eq!(ago_from(now - 5, now, &utc), "just now");
        assert_eq!(ago_from(now - 300, now, &utc), "5 min ago");
        assert_eq!(ago_from(now - 2 * 3600 - 10, now, &utc), "2 h ago");
        assert_eq!(ago_from(now - 30 * 3600, now, &utc), "yesterday");
        assert_eq!(ago_from(now - 4 * 86400, now, &utc), "4 days ago");
        assert_eq!(ago_from(now - 30 * 86400, now, &utc), "2026-10-20");
        Ok(())
    }

    #[test]
    fn formats_dates_in_the_local_zone() -> Result<(), jiff::Error> {
        let utc = jiff::tz::TimeZone::UTC;
        assert_eq!(date_in(0, &utc), "1970-01-01 00:00");
        assert_eq!(date_in(951_782_400, &utc), "2000-02-29 00:00");
        // 08:14 UTC is 13:44 in India, and 09:14 in Berlin in winter (POSIX
        // rules: the test needs no zoneinfo).
        let kolkata = jiff::tz::TimeZone::posix("IST-5:30")?;
        assert_eq!(date_in(1_795_076_040, &kolkata), "2026-11-19 13:44");
        let berlin = jiff::tz::TimeZone::posix("CET-1CEST,M3.5.0,M10.5.0/3")?;
        assert_eq!(date_in(1_795_076_040, &berlin), "2026-11-19 09:14");
        assert_eq!(date_in(u64::MAX, &utc), "unknown");
        Ok(())
    }
}
