//! Best-effort automatic syntax repair. Every transform here is mechanical
//! and conservative (strip a BOM, straighten smart quotes, drop a trailing
//! comma, expand a leading tab) — this is not a general "guess what the
//! author meant" repair tool, and never invents or removes actual data
//! (keys/values), only formatting mistakes that have exactly one sane
//! reading. `try_fix` returns `None` if none of its passes changed
//! anything, so callers can tell "nothing to try" apart from "tried and
//! the result is still invalid" (the latter is still returned — a partial
//! fix plus a shorter list of remaining errors is still useful to show).

use super::format::Format;

pub fn try_fix(format: Format, content: &str) -> Option<String> {
    let mut s = strip_bom(content);
    s = normalize_smart_quotes(&s);
    s = normalize_line_endings(&s);
    s = match format {
        Format::Json => strip_trailing_commas(&strip_json_comments(&s)),
        Format::Yaml => expand_leading_tabs(&s),
        Format::Xml => escape_bare_ampersands(&s),
        Format::Toml => s,
    };
    if s != content {
        Some(s)
    } else {
        None
    }
}

fn strip_bom(s: &str) -> String {
    s.strip_prefix('\u{FEFF}').unwrap_or(s).to_string()
}

fn normalize_line_endings(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// Straightens curly/smart quotes into plain ASCII ones — a very common
/// source of "invalid syntax" when a config was drafted or pasted from a
/// word processor / chat app that auto-"smartens" quotes.
fn normalize_smart_quotes(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            other => other,
        })
        .collect()
}

/// Removes `//` and `/* */` comments that are outside string literals.
/// JSON has no comment syntax, but "JSON with comments" is a common enough
/// mistake (copy-pasted from JSONC/JS) that it's worth handling explicitly
/// rather than just failing with a confusing parser error pointing at the
/// comment.
fn strip_json_comments(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escape = false;

    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            i += 2;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i = (i + 2).min(chars.len());
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Removes a `,` that (ignoring whitespace) is immediately followed by `}`
/// or `]` and is outside a string literal — the single most common JSON
/// typo (a comma left over from reordering/removing the last field).
fn strip_trailing_commas(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escape = false;

    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
        } else if c == ',' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if !matches!(chars.get(j), Some('}') | Some(']')) {
                out.push(c);
            }
            i += 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// YAML forbids tabs in indentation — a leading tab is one of the most
/// common "why won't this YAML parse" surprises, so it gets a dedicated
/// fix rather than relying on the generic passes above. Each leading tab
/// becomes two spaces; tabs elsewhere on the line (inside a value) are left
/// alone.
fn expand_leading_tabs(s: &str) -> String {
    // `.lines()` drops a final trailing newline, so rebuilding via `.join`
    // would silently strip one if `s` had it — that's a spurious "change"
    // on any file with no tabs at all, which would make `try_fix` return
    // `Some` (and later write + back up the file) for nothing.
    let mut out = s
        .lines()
        .map(|line| {
            let indent_end = line.find(|c: char| c != '\t').unwrap_or(line.len());
            let (indent, rest) = line.split_at(indent_end);
            if indent.is_empty() {
                line.to_string()
            } else {
                format!("{}{}", "  ".repeat(indent.len()), rest)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if s.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Escapes a bare `&` (one not already starting a recognized entity or
/// numeric character reference) as `&amp;` — the single most common way
/// hand-edited XML stops being well-formed (e.g. `<name>Tom & Jerry</name>`).
/// Deliberately doesn't try to repair mismatched/unclosed tags — that has
/// more than one plausible fix and guessing wrong would silently change
/// the document's structure, not just its formatting.
fn escape_bare_ampersands(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let ch = s[i..].chars().next().expect("i is a char boundary");
        if ch == '&' {
            if starts_entity_or_charref(&s[i..]) {
                out.push('&');
            } else {
                out.push_str("&amp;");
            }
        } else {
            out.push(ch);
        }
        i += ch.len_utf8();
    }
    out
}

/// True if `s` (which starts with `&`) begins a recognized XML entity or
/// numeric character reference — `&amp;`, `&lt;`, `&gt;`, `&quot;`,
/// `&apos;`, `&#65;`, `&#x41;`. The `regex` crate can't express this as a
/// negative lookahead (no look-around support), so it's a plain scan.
fn starts_entity_or_charref(s: &str) -> bool {
    let rest = &s[1..];
    if ["amp;", "lt;", "gt;", "quot;", "apos;"].iter().any(|name| rest.starts_with(name)) {
        return true;
    }
    let Some(digits) = rest.strip_prefix('#') else { return false };
    let digits = digits.strip_prefix(['x', 'X']).unwrap_or(digits);
    match digits.find(';') {
        Some(semi) => {
            let num = &digits[..semi];
            !num.is_empty() && num.chars().all(|c| c.is_ascii_hexdigit())
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_check::format;

    #[test]
    fn fixes_json_comments_and_trailing_commas() {
        let bad = "{\n  \"a\": 1, // comment\n  \"b\": [1,2,3,],\n}\n";
        assert!(format::validate(Format::Json, bad).is_err());
        let fixed = try_fix(Format::Json, bad).expect("should change something");
        assert!(format::validate(Format::Json, &fixed).is_ok(), "still invalid:\n{fixed}");
    }

    #[test]
    fn fixes_yaml_leading_tabs() {
        let bad = "top:\n\tchild: 1\n";
        assert!(format::validate(Format::Yaml, bad).is_err());
        let fixed = try_fix(Format::Yaml, bad).expect("should change something");
        assert!(format::validate(Format::Yaml, &fixed).is_ok(), "still invalid:\n{fixed}");
    }

    #[test]
    fn fixes_xml_bare_ampersand() {
        let bad = "<root><name>Tom & Jerry</name></root>";
        assert!(format::validate(Format::Xml, bad).is_err());
        let fixed = try_fix(Format::Xml, bad).expect("should change something");
        assert!(format::validate(Format::Xml, &fixed).is_ok(), "still invalid:\n{fixed}");
    }

    #[test]
    fn xml_fixer_leaves_existing_entities_alone() {
        let ok = "<root><name>Tom &amp; Jerry &#65; &#x41;</name></root>";
        assert_eq!(escape_bare_ampersands(ok), ok);
    }

    #[test]
    fn no_fix_available_returns_none() {
        // Unterminated string: none of the mechanical passes can help.
        let bad = "[section]\nkey = \"unterminated\n";
        assert!(format::validate(Format::Toml, bad).is_err());
        assert!(try_fix(Format::Toml, bad).is_none());
    }

    #[test]
    fn json_comment_inside_string_is_preserved() {
        let content = "{\"url\": \"http://example.com\"}";
        assert!(format::validate(Format::Json, content).is_ok());
        assert!(try_fix(Format::Json, content).is_none());
    }
}
