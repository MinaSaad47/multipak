//! Freedesktop `.desktop` launchers for instances.
//!
//! Escaping follows the Desktop Entry Specification: each `Exec` argument is quoted
//! when it contains reserved characters (with `"`, `` ` ``, `$` and `\` escaped inside the
//! quotes), `%` becomes `%%`, and finally the general string escapes (`\\`, `\n`, `\t`,
//! `\r`) are applied to the whole value.

use crate::instance::Instance;

/// Characters that force an `Exec` argument to be quoted.
const RESERVED: &[char] =
    &[' ', '\t', '\n', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(', ')', '`'];

/// Quotes one argument for an `Exec` key (before string escaping).
pub fn quote_exec_arg(arg: &str) -> String {
    let arg = arg.replace('%', "%%");
    if !arg.is_empty() && !arg.contains(RESERVED) {
        return arg;
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Applies the general string escapes for a key file value.
pub fn escape_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for (i, c) in value.chars().enumerate() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            ' ' if i == 0 => out.push_str("\\s"),
            c => out.push(c),
        }
    }
    out
}

/// The complete, escaped `Exec` value.
pub fn exec_value(argv: &[String]) -> String {
    let joined = argv.iter().map(|a| quote_exec_arg(a)).collect::<Vec<_>>().join(" ");
    escape_value(&joined)
}

/// Splits an `Exec` value back into arguments (used by tests and the doctor).
pub fn parse_exec_value(raw: &str) -> Vec<String> {
    let unescaped = crate::keyfile::unescape_value(raw);
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has_token = false;
    let mut chars = unescaped.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            '\\' if in_quotes => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ' ' if !in_quotes => {
                if has_token {
                    args.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            '%' if chars.peek() == Some(&'%') => {
                chars.next();
                cur.push('%');
                has_token = true;
            }
            c => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        args.push(cur);
    }
    args
}

/// Everything needed to render a launcher.
#[derive(Debug, Clone)]
pub struct LauncherInfo<'a> {
    pub instance: &'a Instance,
    pub argv: &'a [String],
    /// An icon name (`com.slack.Slack`) or an absolute path to a badge icon.
    pub icon: &'a str,
    pub categories: &'a [String],
    pub autostart: bool,
}

/// Renders the `.desktop` file text.
pub fn render(info: &LauncherInfo<'_>) -> String {
    let i = info.instance;
    let mut lines = vec![
        "[Desktop Entry]".to_string(),
        "Type=Application".to_string(),
        "Version=1.5".to_string(),
        format!("Name={}", escape_value(&i.display_name())),
        format!("Comment={}", escape_value(&format!("{} instance \"{}\", managed by Multipak", i.app_name, i.name))),
        format!("Exec={}", exec_value(info.argv)),
        format!("Icon={}", escape_value(info.icon)),
        "Terminal=false".to_string(),
        "StartupNotify=true".to_string(),
        format!("StartupWMClass={}", i.wm_class),
    ];
    let cats: Vec<&String> = info
        .categories
        .iter()
        .filter(|c| !c.is_empty() && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
        .collect();
    if !cats.is_empty() {
        lines.push(format!("Categories={};", cats.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(";")));
    }
    if info.autostart {
        lines.push("X-GNOME-Autostart-enabled=true".to_string());
    }
    lines.push(format!("X-Multipak-App={}", i.app_id));
    lines.push(format!("X-Multipak-Instance={}", i.slug));
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apptype::Strategy;
    use crate::instance::{DraftContext, InstanceDraft};
    use crate::keyfile::KeyFile;
    use crate::paths::Paths;

    #[test]
    fn quoting_rules() {
        assert_eq!(quote_exec_arg("plain-arg"), "plain-arg");
        assert_eq!(quote_exec_arg("a b"), "\"a b\"");
        assert_eq!(quote_exec_arg("say \"hi\""), r#""say \"hi\"""#);
        assert_eq!(quote_exec_arg("$HOME"), r#""\$HOME""#);
        assert_eq!(quote_exec_arg("back\\slash"), r#""back\\slash""#);
        assert_eq!(quote_exec_arg("100%"), "100%%");
        assert_eq!(quote_exec_arg(""), "\"\"");
        assert_eq!(quote_exec_arg("~/x"), "\"~/x\"");
    }

    #[test]
    fn backslash_becomes_four_in_file() {
        // The spec example: a literal backslash inside a quoted argument needs four.
        assert_eq!(exec_value(&["a\\b c".to_string()]), r#""a\\\\b c""#);
    }

    #[test]
    fn exec_round_trip() {
        let argv: Vec<String> = [
            "/usr/bin/flatpak",
            "run",
            "--env=A=has space",
            "--env=B=$(rm -rf ~)",
            "com.x.Y",
            "quote\"d",
            "tab\there",
            "100%",
            "back\\slash",
            "",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(parse_exec_value(&exec_value(&argv)), argv);
    }

    #[test]
    fn renders_full_entry() {
        let paths = Paths::for_home("/home/u");
        let mut d = InstanceDraft::new("com.slack.Slack", "Slack", "Work\\Team", Strategy::Electron, &[]);
        d.slug = "work".into();
        d.wm_class = "multipak-slack-work".into();
        let inst = d.validate(DraftContext { paths: &paths, existing: &[], editing: None }).unwrap().instance;
        let argv: Vec<String> = vec!["/usr/bin/flatpak".into(), "run".into(), "com.slack.Slack".into()];
        let cats = vec!["Network".to_string(), "InstantMessaging".to_string(), "bad;cat".to_string()];
        let text = render(&LauncherInfo {
            instance: &inst,
            argv: &argv,
            icon: "com.slack.Slack",
            categories: &cats,
            autostart: true,
        });
        let kf = KeyFile::parse(&text);
        let g = "Desktop Entry";
        assert_eq!(kf.get_string(g, "Name").unwrap(), "Slack (Work\\Team)");
        assert_eq!(kf.get(g, "Exec").unwrap(), "/usr/bin/flatpak run com.slack.Slack");
        assert_eq!(kf.get(g, "StartupWMClass").unwrap(), "multipak-slack-work");
        assert_eq!(kf.get(g, "Categories").unwrap(), "Network;InstantMessaging;");
        assert_eq!(kf.get(g, "X-GNOME-Autostart-enabled").unwrap(), "true");
        assert_eq!(kf.get(g, "X-Multipak-Instance").unwrap(), "work");
        assert!(!text.contains("$HOME"));
    }
}
