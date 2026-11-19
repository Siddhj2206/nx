//! The small slice of shell that scriptlets are made of: joining
//! continuation lines, dropping comments, splitting words, and removing the
//! redirections and `|| :` that macros tack onto every command. Anything
//! that needs more than this is not an idiom nx knows.

/// A scriptlet body as logical lines: continuations joined, comments and
/// blank lines gone, each line trimmed.
pub fn lines(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending = String::new();
    for raw in body.lines() {
        let line = raw.trim();
        if let Some(head) = line.strip_suffix('\\') {
            pending.push_str(head.trim_end());
            pending.push(' ');
            continue;
        }
        pending.push_str(line);
        let joined = std::mem::take(&mut pending);
        let joined = joined.trim();
        if joined.is_empty() || joined.starts_with('#') {
            continue;
        }
        out.push(joined.to_owned());
    }
    let rest = pending.trim();
    if !rest.is_empty() {
        out.push(rest.to_owned());
    }
    out
}

/// Removes what macros append to a command and which changes nothing about
/// what it does here: output redirections to `/dev/null` and `|| :`.
pub fn strip_noise(line: &str) -> &str {
    const NOISE: [&str; 12] = [
        "||:",
        "|| :",
        "|| true",
        "||true",
        ">/dev/null 2>&1",
        "> /dev/null 2>&1",
        "&>/dev/null",
        "&> /dev/null",
        "2>/dev/null",
        "2> /dev/null",
        ">/dev/null",
        "> /dev/null",
    ];
    let mut line = line.trim();
    loop {
        let before = line;
        for noise in NOISE {
            if let Some(rest) = line.strip_suffix(noise) {
                line = rest.trim_end();
            }
        }
        if line == before {
            return line;
        }
    }
}

/// Splits a command into words, honouring single and double quotes.
/// `None` when it uses anything a plain word list can't express: variable
/// or command substitution, pipes, redirections, grouping.
pub fn words(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                started = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => cur.push(c),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '$' | '`' | '\\' => return None,
                        c => cur.push(c),
                    }
                }
            }
            c if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            '$' | '`' | '\\' | ';' | '&' | '|' | '<' | '>' | '(' | ')' | '{' | '}' | '!' => {
                return None;
            }
            c => {
                started = true;
                cur.push(c);
            }
        }
    }
    if started {
        words.push(cur);
    }
    Some(words)
}

/// Splits `line` at the first `||` outside quotes.
pub fn split_or(line: &str) -> Option<(&str, &str)> {
    let mut quote = None;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match (quote, bytes[i]) {
            (None, b'\'' | b'"') => quote = Some(bytes[i]),
            (Some(q), c) if c == q => quote = None,
            (None, b'|') if bytes.get(i + 1) == Some(&b'|') => {
                return Some((line[..i].trim(), line[i + 2..].trim()));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_continuations_and_drops_comments() {
        let body = "# a comment\n\nfoo \\\n  bar\n  baz # not a comment, kept\n";
        assert_eq!(lines(body), ["foo bar", "baz # not a comment, kept"]);
    }

    #[test]
    fn strips_macro_noise() {
        assert_eq!(
            strip_noise("systemctl stop x.service > /dev/null 2>&1 || :"),
            "systemctl stop x.service"
        );
        assert_eq!(strip_noise("ldconfig"), "ldconfig");
    }

    #[test]
    fn splits_words_with_quotes_and_refuses_expansion() {
        assert_eq!(
            words("useradd -c \"A B\" 'x y' z"),
            Some(vec![
                "useradd".into(),
                "-c".into(),
                "A B".into(),
                "x y".into(),
                "z".into()
            ])
        );
        assert_eq!(words("echo $HOME"), None);
        assert_eq!(words("cat x | sh"), None);
        assert_eq!(words("echo \"$x\""), None);
    }

    #[test]
    fn splits_at_a_top_level_or() {
        assert_eq!(
            split_or("getent group x || groupadd x"),
            Some(("getent group x", "groupadd x"))
        );
        assert_eq!(split_or("echo \"a || b\""), None);
    }
}
