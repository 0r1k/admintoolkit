//! Minimal round-tripping parser/writer for the OpenSSH client config file
//! (`~/.ssh/config`). Untouched `Host` blocks (and everything before the
//! first one — global directives, header comments) are kept byte-identical
//! on save by storing their raw source text and only re-serializing a block
//! once something in it has actually been edited through this app. Edited
//! blocks are regenerated cleanly from their parsed directives, which loses
//! any inline comments *within that one block* — an accepted trade-off for
//! not needing a full comment-preserving AST parser.

/// One `Host <patterns...>` block.
pub struct HostBlock {
    pub patterns: Vec<String>,
    /// Ordered `(Key, Value)` pairs; keys are matched case-insensitively.
    /// May contain duplicate keys (`IdentityFile`, `LocalForward`, ...).
    pub directives: Vec<(String, String)>,
    /// Exact original source text (header + body), used verbatim when
    /// `dirty` is false.
    raw: String,
    /// Set once this block has been rebuilt in-memory (added, or edited) —
    /// forces `render` to regenerate it instead of reusing `raw`.
    pub dirty: bool,
}

impl HostBlock {
    pub fn new(patterns: Vec<String>, directives: Vec<(String, String)>) -> Self {
        Self { patterns, directives, raw: String::new(), dirty: true }
    }

    pub fn get_first(&self, key: &str) -> Option<&str> {
        self.directives.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str())
    }

    pub fn get_all(&self, key: &str) -> Vec<String> {
        self.directives.iter().filter(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.clone()).collect()
    }

    /// The first pattern that isn't a glob (`!*?[]`) — the alias this block
    /// is treated as, or `None` for wildcard-only blocks like
    /// `Host *` (those round-trip but are never shown as a manageable
    /// server).
    pub fn primary_alias(&self) -> Option<String> {
        self.patterns.iter().find(|p| !has_wildcard(p)).cloned()
    }

    fn render_dirty(&self) -> String {
        let mut s = format!("Host {}\n", self.patterns.join(" "));
        for (k, v) in &self.directives {
            s.push_str(&format!("    {k} {v}\n"));
        }
        s.push('\n');
        s
    }
}

fn has_wildcard(s: &str) -> bool {
    s.chars().any(|c| "!*?[]".contains(c))
}

pub struct SshConfigFile {
    /// Raw text before the first `Host` line (global settings, header
    /// comments) — always preserved verbatim.
    prefix: String,
    pub hosts: Vec<HostBlock>,
}

impl SshConfigFile {
    pub fn parse(text: &str) -> Self {
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let mut idx = 0;
        let mut prefix = String::new();
        while idx < lines.len() && !is_host_line(lines[idx]) {
            prefix.push_str(lines[idx]);
            idx += 1;
        }

        let mut hosts = Vec::new();
        while idx < lines.len() {
            let header = lines[idx];
            let patterns = parse_host_patterns(header);
            let mut raw = String::from(header);
            idx += 1;

            let mut body: Vec<&str> = Vec::new();
            while idx < lines.len() && !is_host_line(lines[idx]) {
                raw.push_str(lines[idx]);
                body.push(lines[idx]);
                idx += 1;
            }

            let directives = parse_directives(&body);
            hosts.push(HostBlock { patterns, directives, raw, dirty: false });
        }

        Self { prefix, hosts }
    }

    pub fn render(&self) -> String {
        let mut out = self.prefix.clone();
        for h in &self.hosts {
            if h.dirty {
                out.push_str(&h.render_dirty());
            } else {
                out.push_str(&h.raw);
            }
        }
        out
    }

    pub fn find_index(&self, alias: &str) -> Option<usize> {
        self.hosts.iter().position(|h| h.patterns.iter().any(|p| p == alias))
    }

    pub fn alias_exists(&self, alias: &str) -> bool {
        self.find_index(alias).is_some()
    }
}

fn is_host_line(line: &str) -> bool {
    line.trim_start().split_whitespace().next().is_some_and(|w| w.eq_ignore_ascii_case("host"))
}

fn parse_host_patterns(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let rest = trimmed.splitn(2, char::is_whitespace).nth(1).unwrap_or("").trim_start();
    rest.split_whitespace().map(|s| s.to_string()).collect()
}

fn parse_directives(body: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in body {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (key, rest) = match trimmed.find(char::is_whitespace) {
            Some(pos) => (&trimmed[..pos], trimmed[pos..].trim_start()),
            None => match trimmed.find('=') {
                Some(pos) => (&trimmed[..pos], trimmed[pos + 1..].trim_start()),
                None => (trimmed, ""),
            },
        };
        let key = key.trim_end_matches('=').to_string();
        let mut value = rest.trim().to_string();
        if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
            value = value[1..value.len() - 1].to_string();
        }
        out.push((key, value));
    }
    out
}
