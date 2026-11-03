//! Permission prompts read off an agent's terminal.
//!
//! Neither Claude Code nor Codex writes a pending approval to its transcript
//! — the question exists only on screen — so lama reads the session's kitty
//! window and recognises the dialog there: a question, then a run of
//! numbered choices (`❯ 1. Yes`, `2. Yes, and don't ask again …`, `3. No …`),
//! possibly drawn inside a box. Answering sends the key the dialog itself
//! offers: a shortcut shown in parentheses (`(y)`, `(esc)`), else the digit.

/// A permission dialog on an agent's screen.
#[derive(Debug, Clone, PartialEq)]
pub struct Ask {
    /// The question, e.g. "Do you want to proceed?".
    pub question: String,
    /// Up to three lines of what it is about (the command, the file).
    pub detail: Vec<String>,
    pub options: Vec<Choice>,
    /// Which choice the dialog has highlighted, if it shows one.
    pub selected: Option<usize>,
}

/// One answer a dialog offers.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    /// As the dialog numbers it (1-based).
    pub number: u8,
    pub label: String,
    pub key: Key,
}

/// The key that picks a choice.
#[derive(Debug, Clone, PartialEq)]
pub enum Key {
    /// A character to type: the choice's digit or its letter shortcut.
    Char(char),
    Escape,
}

/// Box drawing and padding around a dialog line.
fn unbox(line: &str) -> &str {
    line.trim_matches(|c: char| c.is_whitespace() || matches!(c, '│' | '┃' | '║' | '|'))
}

/// `❯ 2. Yes, and don't ask again (a)` → (highlighted, 2, label, key).
fn option(line: &str) -> Option<(bool, u8, String, Key)> {
    let s = unbox(line);
    let (marked, s) = match s.chars().next()? {
        '❯' | '›' | '>' | '▶' | '→' => (true, s[s.char_indices().nth(1)?.0..].trim_start()),
        _ => (false, s),
    };
    let dot = s.find(". ")?;
    let number: u8 = s[..dot].parse().ok()?;
    if !(1..=9).contains(&number) {
        return None;
    }
    let label = s[dot + 2..].trim().to_string();
    if label.is_empty() {
        return None;
    }
    // A trailing `(y)` / `(esc)` is the dialog's own shortcut.
    let key = label
        .rsplit_once('(')
        .and_then(|(_, tail)| tail.strip_suffix(')'))
        .and_then(|hint| match hint {
            "esc" | "Esc" | "ESC" => Some(Key::Escape),
            h if h.chars().count() == 1 && h.chars().all(|c| c.is_ascii_alphabetic()) => {
                h.chars().next().map(Key::Char)
            }
            _ => None,
        })
        .unwrap_or_else(|| Key::Char((b'0' + number) as char));
    Some((marked, number, label, key))
}

/// How far a line is indented inside any box border.
fn indent(line: &str) -> usize {
    let s = line.trim_start_matches(['│', '┃', '║', '|']);
    s.len() - s.trim_start().len()
}

/// Find a permission dialog in the bottom part of a terminal screen.
pub fn parse(screen: &str) -> Option<Ask> {
    let lines: Vec<&str> = screen.lines().collect();
    let tail = lines.len().saturating_sub(60);
    let lines = &lines[tail..];
    // The bottom-most run of choices numbered 1, 2, 3 … A choice too long
    // for the screen wraps onto lines indented past the choices' own (Codex
    // spells out the whole command in "don't ask again for …").
    let end = (0..lines.len())
        .rev()
        .find(|&i| option(lines[i]).is_some())?;
    let wrap = indent(lines[end]);
    let mut start = end;
    let mut run: Vec<(bool, u8, String, Key)> = Vec::new();
    let mut more: Vec<&str> = Vec::new();
    let mut i = end + 1;
    while i > 0 {
        let line = lines[i - 1];
        if let Some((marked, number, mut label, key)) = option(line) {
            if !more.is_empty() {
                more.reverse();
                label = std::iter::once(label.as_str())
                    .chain(more.drain(..).map(unbox))
                    .collect::<Vec<_>>()
                    .join(" ");
            }
            // The shortcut may only show at the end of the wrapped label.
            let key = match key {
                Key::Char(c) if c.is_ascii_digit() => option(&format!("{number}. {label}"))
                    .map(|o| o.3)
                    .unwrap_or(key),
                k => k,
            };
            run.push((marked, number, label, key));
            start = i - 1;
        } else if i - 1 < end && !unbox(line).is_empty() && indent(line) > wrap {
            more.push(line);
        } else {
            break;
        }
        i -= 1;
    }
    run.reverse();
    let in_order = run.iter().enumerate().all(|(k, o)| o.1 as usize == k + 1);
    if run.len() < 2 || !in_order {
        return None;
    }
    // The question: the nearest line above that asks one — not a labelled
    // field like Codex's "Reason: May I …?".
    let labelled = |l: &str| {
        l.split_once(": ")
            .is_some_and(|(k, _)| !k.is_empty() && k.chars().all(char::is_alphabetic))
    };
    let q = (start.saturating_sub(24)..start).rev().find(|&i| {
        let l = unbox(lines[i]);
        l.ends_with('?') && !labelled(l)
    })?;
    let shown = |l: &&&str| {
        let l = unbox(l);
        !l.is_empty() && !l.chars().all(|c| "─━═╭╮╰╯┌┐└┘".contains(c))
    };
    // What it is about: Codex lists it under the question (reason, command,
    // the command wrapped over several lines), Claude above it.
    let mut below: Vec<String> = Vec::new();
    for l in lines[q + 1..start].iter().filter(shown).map(|l| unbox(l)) {
        match below.last_mut() {
            Some(cmd) if cmd.starts_with("$ ") && !l.contains(": ") => {
                cmd.push(' ');
                cmd.push_str(l);
            }
            _ if l.starts_with("Environment:") => {}
            _ => below.push(l.to_string()),
        }
    }
    let detail: Vec<String> = if below.is_empty() {
        let above: Vec<String> = lines[q.saturating_sub(6)..q]
            .iter()
            .filter(shown)
            .map(|l| unbox(l).to_string())
            .collect();
        above[above.len().saturating_sub(3)..].to_vec()
    } else {
        below.into_iter().take(3).collect()
    };
    Some(Ask {
        question: unbox(lines[q]).to_string(),
        detail,
        selected: run.iter().position(|o| o.0),
        options: run
            .into_iter()
            .map(|(_, number, label, key)| Choice { number, label, key })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_permission_box() {
        let screen = "\
some earlier output
╭───────────────────────────────────────────────╮
│ Bash command                                  │
│                                               │
│   npm test                                    │
│   Run the test suite                          │
│                                               │
│ Do you want to proceed?                       │
│ ❯ 1. Yes                                      │
│   2. Yes, and don't ask again for npm commands │
│   3. No, and tell Claude what to do differently (esc) │
╰───────────────────────────────────────────────╯
";
        let ask = parse(screen).expect("a dialog");
        assert_eq!(ask.question, "Do you want to proceed?");
        assert_eq!(
            ask.detail,
            ["Bash command", "npm test", "Run the test suite"]
        );
        assert_eq!(ask.options.len(), 3);
        assert_eq!(ask.options[0].key, Key::Char('1'));
        assert_eq!(ask.options[2].key, Key::Escape);
        assert_eq!(ask.selected, Some(0));
    }

    #[test]
    fn codex_approval_with_letter_shortcuts() {
        let screen = "\
  Would you like to run the following command?
  $ cargo test
› 1. Yes, proceed (y)
  2. Yes, and don't ask again for this command (a)
  3. No, and tell Codex what to do differently (esc)
  Press enter to confirm or esc to cancel
";
        let ask = parse(screen).expect("a dialog");
        assert_eq!(ask.question, "Would you like to run the following command?");
        assert_eq!(ask.options[0].key, Key::Char('y'));
        assert_eq!(ask.options[1].key, Key::Char('a'));
        assert_eq!(ask.options[2].key, Key::Escape);
    }

    #[test]
    fn codex_approval_with_a_wrapped_choice() {
        // A helper agent's request: the reason and a long command push the
        // question far up, and choice 2 wraps the whole command.
        let screen = "\
• Finished waiting
  └ No agents completed yet


  Would you like to run the following command?

  Thread: Agent (01a18d40)

  Environment: local

  Reason: May I rerun the controller suite?

  $ docker compose run --rm -v /w/sp:/shop -v /w/.bundle:/bundle:ro -v
  /w/locale:/shop/locale:ro -v /w/views/DE:/shop/app/views/DE:ro -v
  /w/views/EN:/shop/app/views/EN:ro -e BUNDLE_PATH=/bundle shop_app bundle
  exec rspec spec/controllers > /tmp/out.log 2>&1


› 1. Yes, proceed (y)
  2. Yes, and don't ask again for commands that start with `docker compose run --rm -v /w/sp:/shop -v /w/.bundle:/
     bundle:ro -v /w/locale:/shop/locale:ro -v /w/views/DE:/shop/app/views/DE:ro -v /w/views/EN:/shop/app/
     views/EN:ro -e BUNDLE_PATH=/bundle shop_app bundle exec rspec spec/controllers > /tmp/out.log 2>&1` (p)
  3. No, and tell Codex what to do differently (esc)

  Press enter to confirm or esc to cancel or o to open thread
";
        let ask = parse(screen).expect("a dialog");
        assert_eq!(ask.question, "Would you like to run the following command?");
        assert_eq!(ask.options.len(), 3);
        assert_eq!(ask.options[0].key, Key::Char('y'));
        assert_eq!(ask.options[1].key, Key::Char('p'), "{:?}", ask.options[1]);
        assert_eq!(ask.options[2].key, Key::Escape);
        assert_eq!(ask.selected, Some(0));
        assert_eq!(ask.detail[0], "Thread: Agent (01a18d40)");
        assert_eq!(ask.detail[1], "Reason: May I rerun the controller suite?");
        assert!(ask.detail[2].starts_with("$ docker compose run"));
        assert!(ask.detail[2].ends_with("> /tmp/out.log 2>&1"));
    }

    #[test]
    fn a_numbered_list_without_a_question_is_not_a_dialog() {
        assert_eq!(parse("Steps:\n1. build\n2. test\n"), None);
        assert_eq!(
            parse("Plan?\n2. later\n3. never\n"),
            None,
            "must start at 1"
        );
        assert_eq!(parse("plain output\nno choices here\n"), None);
    }
}
