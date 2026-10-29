//! Global hotkey strings ("Alt+Space") parsed and formatted the one way the
//! app and Settings show them.

use std::fmt;

/// The default; the first run lets the user confirm it.
pub const DEFAULT: &str = "Alt+Space";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
}

/// A modifier set plus one key, in the plugin's key-name spelling
/// (`Space`, `KeyF`, `Digit1`, `F5`, ...).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub mods: Mods,
    pub key: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HotkeyError {
    /// Not a valid hotkey: the text says what is wrong.
    Invalid(String),
    /// Windows refused it: another program already owns it.
    InUse(String),
}

impl fmt::Display for HotkeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(why) => write!(f, "not a valid hotkey: {why}"),
            Self::InUse(k) => write!(f, "{k} is already in use by another program"),
        }
    }
}

/// Keys a hotkey may end with, as shown to the user, with the plugin's
/// code for each.
fn key_code(k: &str) -> Option<String> {
    let up = k.to_ascii_uppercase();
    let named = [
        ("SPACE", "Space"),
        ("ENTER", "Enter"),
        ("TAB", "Tab"),
        ("BACKQUOTE", "Backquote"),
        ("`", "Backquote"),
        ("PERIOD", "Period"),
        (".", "Period"),
        ("COMMA", "Comma"),
        (",", "Comma"),
        ("SLASH", "Slash"),
        ("/", "Slash"),
        ("SEMICOLON", "Semicolon"),
        (";", "Semicolon"),
    ];
    if let Some((_, c)) = named.iter().find(|(n, _)| *n == up) {
        return Some((*c).to_string());
    }
    let b = up.as_bytes();
    match b {
        [c] if c.is_ascii_uppercase() => Some(format!("Key{}", *c as char)),
        [c] if c.is_ascii_digit() => Some(format!("Digit{}", *c as char)),
        [b'F', rest @ ..] if !rest.is_empty() && rest.len() <= 2 => {
            let n: u8 = std::str::from_utf8(rest).ok()?.parse().ok()?;
            (1..=24).contains(&n).then(|| format!("F{n}"))
        }
        _ => None,
    }
}

/// How a key code is shown ("KeyF" -> "F").
fn key_label(code: &str) -> String {
    code.strip_prefix("Key")
        .or_else(|| code.strip_prefix("Digit"))
        .unwrap_or(code)
        .to_string()
}

pub fn parse(s: &str) -> Result<Hotkey, HotkeyError> {
    let parts: Vec<&str> = s.split('+').map(str::trim).collect();
    if parts.len() < 2 || parts.iter().any(|p| p.is_empty()) {
        return Err(HotkeyError::Invalid(
            "use one or more of Ctrl, Alt, Shift, Win, then a key".into(),
        ));
    }
    let (key, mods_part) = parts.split_last().expect("at least two parts");
    let mut mods = Mods::default();
    for m in mods_part {
        let slot = match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => &mut mods.ctrl,
            "alt" => &mut mods.alt,
            "shift" => &mut mods.shift,
            "win" | "super" | "meta" => &mut mods.win,
            other => return Err(HotkeyError::Invalid(format!("unknown modifier {other}"))),
        };
        if *slot {
            return Err(HotkeyError::Invalid(format!("{m} appears twice")));
        }
        *slot = true;
    }
    if mods
        == (Mods {
            shift: true,
            ..Mods::default()
        })
    {
        return Err(HotkeyError::Invalid(
            "Shift alone would take a typing key".into(),
        ));
    }
    let key = key_code(key).ok_or_else(|| HotkeyError::Invalid(format!("unknown key {key}")))?;
    Ok(Hotkey { mods, key })
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.mods;
        for (on, name) in [
            (m.ctrl, "Ctrl"),
            (m.win, "Win"),
            (m.alt, "Alt"),
            (m.shift, "Shift"),
        ] {
            if on {
                write!(f, "{name}+")?;
            }
        }
        f.write_str(&key_label(&self.key))
    }
}

impl Hotkey {
    /// The global-shortcut plugin's accelerator string.
    pub fn accelerator(&self) -> String {
        let m = self.mods;
        let mut s = String::new();
        for (on, name) in [
            (m.ctrl, "Control"),
            (m.win, "Super"),
            (m.alt, "Alt"),
            (m.shift, "Shift"),
        ] {
            if on {
                s.push_str(name);
                s.push('+');
            }
        }
        s.push_str(&self.key);
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default() {
        let h = parse(DEFAULT).unwrap();
        assert!(h.mods.alt && !h.mods.ctrl);
        assert_eq!(h.key, "Space");
        assert_eq!(h.accelerator(), "Alt+Space");
    }

    #[test]
    fn parses_variants() {
        assert_eq!(
            parse("Ctrl+Shift+F").unwrap().accelerator(),
            "Control+Shift+KeyF"
        );
        assert_eq!(parse("win+alt+k").unwrap().accelerator(), "Super+Alt+KeyK");
        assert_eq!(parse("Ctrl+F12").unwrap().key, "F12");
        assert_eq!(parse("Alt+1").unwrap().key, "Digit1");
        for bad in [
            "Alt+",
            "Space",
            "Ctrl+Ctrl+A",
            "Hyper+A",
            "Ctrl+F25",
            "Shift+A",
            "Ctrl+Ğ",
        ] {
            assert!(matches!(parse(bad), Err(HotkeyError::Invalid(_))), "{bad}");
        }
    }

    #[test]
    fn format_round_trips() {
        for s in [
            "Alt+Space",
            "Ctrl+Shift+F",
            "Ctrl+Win+Alt+K",
            "Ctrl+F12",
            "Alt+1",
        ] {
            assert_eq!(parse(s).unwrap().to_string(), s);
        }
        assert_eq!(parse("shift+ctrl+f").unwrap().to_string(), "Ctrl+Shift+F");
    }
}
