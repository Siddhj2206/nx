//! [`Problem`]: an error as nx shows it, and how any error becomes one.

use std::fmt::Display;

/// An error as nx shows it (plan 1.1, *Output* rule 1): what failed in
/// one plain line, the reasons one per line, and the exact commands that
/// fix it. Library errors carry data; the CLI words them into this. It
/// travels as an error (`anyhow`), and `main` draws it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Problem {
    pub headline: String,
    pub reasons: Vec<String>,
    /// Lines only `-v` shows; counted otherwise (rule 2).
    pub more: Vec<String>,
    pub fixes: Vec<Fix>,
}

/// A way out: what it does, and the command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    pub say: String,
    pub run: String,
}

impl Problem {
    pub fn new(headline: impl Into<String>) -> Self {
        Self {
            headline: headline.into(),
            ..Self::default()
        }
    }

    pub fn reason(mut self, r: impl Into<String>) -> Self {
        self.reasons.push(r.into());
        self
    }

    pub fn reasons<S: Into<String>>(mut self, rs: impl IntoIterator<Item = S>) -> Self {
        self.reasons.extend(rs.into_iter().map(Into::into));
        self
    }

    pub fn more<S: Into<String>>(mut self, ds: impl IntoIterator<Item = S>) -> Self {
        self.more.extend(ds.into_iter().map(Into::into));
        self
    }

    /// A fix: `say` (may be empty) and the command to run.
    pub fn fix(mut self, say: impl Into<String>, run: impl Into<String>) -> Self {
        self.fixes.push(Fix {
            say: say.into(),
            run: run.into(),
        });
        self
    }

    /// Any error as a problem. A [`Problem`] in the chain keeps its
    /// reasons and fixes, under the context added on the way out. Without
    /// one, the outermost message is the headline and each cause a reason,
    /// instead of one line joined with `: `; a message that ends with its
    /// cause's text (thiserror's `{source}`) loses the repeat.
    pub fn from_error(err: &anyhow::Error) -> Self {
        let mut messages: Vec<String> = Vec::new();
        let mut found: Option<&Problem> = None;
        for cause in err.chain() {
            if let Some(p) = cause.downcast_ref::<Problem>() {
                found = Some(p);
                break;
            }
            let text = cause.to_string();
            if let Some(last) = messages.last_mut() {
                if *last == text {
                    continue;
                }
                if let Some(head) = last.strip_suffix(&text) {
                    *last = head.trim_end_matches([':', ' ']).to_owned();
                }
            }
            messages.push(text);
        }
        if let Some(p) = found {
            // An error that wraps a problem shows it as its own text.
            messages.retain(|m| *m != p.to_string());
        }
        let mut lines = messages.iter().flat_map(|m| m.lines()).map(str::to_owned);
        let mut out = match (found, lines.next()) {
            (Some(p), None) => return p.clone(),
            (Some(p), Some(head)) => {
                let mut out = Self::new(head).reasons(lines);
                out.reasons.push(p.headline.clone());
                out.reasons.extend(p.reasons.iter().cloned());
                out.more.clone_from(&p.more);
                out.fixes.clone_from(&p.fixes);
                out
            }
            (None, head) => Self::new(head.unwrap_or_else(|| "failed".to_owned())).reasons(lines),
        };
        out.reasons.retain(|r| !r.trim().is_empty());
        out
    }

    /// The headline and its reasons on one line, for a place that holds a
    /// single line: a warning, a notification, the upgrade timer's record.
    pub fn one_line(&self) -> String {
        std::iter::once(&self.headline)
            .chain(&self.reasons)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

impl Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.headline)?;
        for r in self.reasons.iter().chain(&self.more) {
            write!(f, "\n{r}")?;
        }
        for fix in &self.fixes {
            match fix.say.as_str() {
                "" => write!(f, "\n{}", fix.run)?,
                say => write!(f, "\n{say}: {}", fix.run)?,
            }
        }
        Ok(())
    }
}

impl std::error::Error for Problem {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_error_becomes_a_problem() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let e = anyhow::Error::new(io)
            .context("reading /x: no such file")
            .context("loading the repos");
        assert_eq!(
            Problem::from_error(&e),
            Problem::new("loading the repos").reasons(["reading /x", "no such file"])
        );
        // A problem under context keeps its fixes.
        let inner = Problem::new("can't resolve x")
            .reason("nothing provides y")
            .fix("", "nx search x");
        let e = anyhow::Error::new(inner.clone()).context("building the staged image's generation");
        assert_eq!(
            Problem::from_error(&e),
            Problem::new("building the staged image's generation")
                .reasons(["can't resolve x", "nothing provides y"])
                .fix("", "nx search x")
        );
        assert_eq!(
            Problem::from_error(&anyhow::Error::new(inner.clone())),
            inner
        );
        // Lines of a bailed message are reasons.
        let e = anyhow::anyhow!("two problems\nfirst\nsecond");
        assert_eq!(
            Problem::from_error(&e),
            Problem::new("two problems").reasons(["first", "second"])
        );
    }
}
