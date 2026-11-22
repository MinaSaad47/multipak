//! A small GLib-style key file ("INI") reader and editor that preserves comments and
//! layout. Used for Flatpak metadata, `.desktop` files and Flatpak override files.

/// One line of a key file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    Group(String),
    Entry { key: String, value: String },
    Other(String),
}

/// A parsed key file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyFile {
    lines: Vec<Line>,
}

impl KeyFile {
    pub fn parse(text: &str) -> Self {
        let lines = text
            .lines()
            .map(|raw| {
                let trimmed = raw.trim();
                if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2 {
                    Line::Group(trimmed[1..trimmed.len() - 1].to_string())
                } else if trimmed.starts_with('#') || trimmed.is_empty() {
                    Line::Other(raw.to_string())
                } else if let Some((k, v)) = raw.split_once('=') {
                    Line::Entry { key: k.trim().to_string(), value: v.trim_start().to_string() }
                } else {
                    Line::Other(raw.to_string())
                }
            })
            .collect();
        Self { lines }
    }

    /// The raw (still escaped) value of `key` in `group`.
    pub fn get(&self, group: &str, key: &str) -> Option<&str> {
        let mut current: Option<&str> = None;
        for line in &self.lines {
            match line {
                Line::Group(g) => current = Some(g),
                Line::Entry { key: k, value } if current == Some(group) && k == key => {
                    return Some(value);
                }
                _ => {}
            }
        }
        None
    }

    /// The value of `key` with string escapes (`\s \n \t \r \\`) decoded.
    pub fn get_string(&self, group: &str, key: &str) -> Option<String> {
        self.get(group, key).map(unescape_value)
    }

    /// A `;`-separated list value.
    pub fn get_list(&self, group: &str, key: &str) -> Vec<String> {
        self.get(group, key)
            .map(|v| v.split(';').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default()
    }

    pub fn get_bool(&self, group: &str, key: &str) -> Option<bool> {
        match self.get(group, key)?.trim() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        }
    }

    pub fn has_group(&self, group: &str) -> bool {
        self.lines.iter().any(|l| matches!(l, Line::Group(g) if g == group))
    }

    /// Sets `key` in `group`, creating the group or key as needed.
    pub fn set(&mut self, group: &str, key: &str, value: &str) {
        let mut current: Option<String> = None;
        let mut group_end: Option<usize> = None;
        for (i, line) in self.lines.iter_mut().enumerate() {
            match line {
                Line::Group(g) => {
                    current = Some(g.clone());
                }
                Line::Entry { key: k, value: v } if current.as_deref() == Some(group) && k == key => {
                    *v = value.to_string();
                    return;
                }
                _ => {}
            }
            if current.as_deref() == Some(group) && !matches!(line, Line::Other(s) if s.trim().is_empty()) {
                group_end = Some(i + 1);
            }
        }
        let entry = Line::Entry { key: key.to_string(), value: value.to_string() };
        match group_end {
            Some(at) => self.lines.insert(at, entry),
            None => {
                if !self.lines.is_empty() && !matches!(self.lines.last(), Some(Line::Other(s)) if s.trim().is_empty()) {
                    self.lines.push(Line::Other(String::new()));
                }
                self.lines.push(Line::Group(group.to_string()));
                self.lines.push(entry);
            }
        }
    }

    /// Removes `key` from `group`. Returns whether it existed.
    pub fn remove(&mut self, group: &str, key: &str) -> bool {
        let mut current: Option<String> = None;
        let mut index = None;
        for (i, line) in self.lines.iter().enumerate() {
            match line {
                Line::Group(g) => current = Some(g.clone()),
                Line::Entry { key: k, .. } if current.as_deref() == Some(group) && k == key => {
                    index = Some(i);
                    break;
                }
                _ => {}
            }
        }
        match index {
            Some(i) => {
                self.lines.remove(i);
                true
            }
            None => false,
        }
    }

    /// Names of all groups, in file order.
    pub fn groups(&self) -> Vec<String> {
        self.lines
            .iter()
            .filter_map(|l| match l {
                Line::Group(g) => Some(g.clone()),
                _ => None,
            })
            .collect()
    }

    /// Removes a whole group (its header, keys and the blank lines before the next group).
    /// Returns whether it existed.
    pub fn remove_group(&mut self, group: &str) -> bool {
        let Some(start) = self.lines.iter().position(|l| matches!(l, Line::Group(g) if g == group)) else {
            return false;
        };
        let end = self.lines[start + 1..]
            .iter()
            .position(|l| matches!(l, Line::Group(_)))
            .map(|i| start + 1 + i)
            .unwrap_or(self.lines.len());
        let was_last = end == self.lines.len();
        self.lines.drain(start..end);
        if was_last {
            // The previous group's trailing blank line would now end the file.
            while matches!(self.lines.last(), Some(Line::Other(s)) if s.trim().is_empty()) {
                self.lines.pop();
            }
        }
        true
    }

    /// Serializes the key file, ending with a newline.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            match line {
                Line::Group(g) => {
                    out.push('[');
                    out.push_str(g);
                    out.push(']');
                }
                Line::Entry { key, value } => {
                    out.push_str(key);
                    out.push('=');
                    out.push_str(value);
                }
                Line::Other(s) => out.push_str(s),
            }
            out.push('\n');
        }
        out
    }
}

/// Decodes the string escapes defined by the Desktop Entry spec.
pub fn unescape_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# comment\n[Application]\nname=com.example.App\nruntime=org.gnome.Platform/x86_64/47\n\n[Context]\nfilesystems=xdg-download;/srv/data;\n";

    #[test]
    fn reads_values() {
        let kf = KeyFile::parse(SAMPLE);
        assert_eq!(kf.get("Application", "runtime"), Some("org.gnome.Platform/x86_64/47"));
        assert_eq!(kf.get_list("Context", "filesystems"), vec!["xdg-download", "/srv/data"]);
        assert_eq!(kf.get("Context", "name"), None);
        assert!(kf.has_group("Context"));
    }

    #[test]
    fn edits_preserve_layout() {
        let mut kf = KeyFile::parse(SAMPLE);
        kf.set("Context", "filesystems", "xdg-download;");
        kf.set("Context", "sockets", "x11;");
        kf.set("Environment", "FOO", "bar");
        let text = kf.to_text();
        assert!(text.starts_with("# comment\n[Application]\n"));
        assert!(text.contains("[Context]\nfilesystems=xdg-download;\nsockets=x11;\n"));
        assert!(text.ends_with("\n[Environment]\nFOO=bar\n"));
        assert!(kf.remove("Context", "sockets"));
        assert!(!kf.remove("Context", "sockets"));
    }

    #[test]
    fn group_removal() {
        let mut kf = KeyFile::parse("[A]\na=1\n\n[B]\nb=2\n\n[C]\nc=3\n");
        assert_eq!(kf.groups(), vec!["A", "B", "C"]);
        assert!(kf.remove_group("B"));
        assert_eq!(kf.to_text(), "[A]\na=1\n\n[C]\nc=3\n");
        assert!(kf.remove_group("C"));
        assert_eq!(kf.to_text(), "[A]\na=1\n");
        assert!(!kf.remove_group("Z"));
    }

    #[test]
    fn unescapes() {
        assert_eq!(unescape_value(r"a\sb\nc\\d\t"), "a b\nc\\d\t");
    }
}
