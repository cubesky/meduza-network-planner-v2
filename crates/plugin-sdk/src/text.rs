//! Small helpers for deterministic native config rendering.

use std::fmt::Write as _;

/// Line-oriented config writer. Rejects values that would inject extra lines or directives.
#[derive(Debug, Default, Clone)]
pub struct ConfigWriter {
    out: String,
    indent: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("value for {key:?} contains a forbidden character")]
pub struct InjectionError {
    pub key: String,
}

impl ConfigWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn comment(&mut self, text: &str) -> &mut Self {
        for line in text.lines() {
            self.raw_line(&format!("# {line}"));
        }
        self
    }

    /// Writes a line verbatim (for generator-owned constant text only).
    pub fn raw_line(&mut self, line: &str) -> &mut Self {
        for _ in 0..self.indent {
            self.out.push(' ');
        }
        self.out.push_str(line);
        self.out.push('\n');
        self
    }

    /// `key value` or just `key` when `value` is empty. Values must be single-line.
    pub fn directive(&mut self, key: &str, value: &str) -> Result<&mut Self, InjectionError> {
        check_single_line(key, value)?;
        if value.is_empty() {
            Ok(self.raw_line(key))
        } else {
            Ok(self.raw_line(&format!("{key} {value}")))
        }
    }

    /// `key=value` (dnsmasq, tinc style).
    pub fn assign(&mut self, key: &str, value: &str) -> Result<&mut Self, InjectionError> {
        check_single_line(key, value)?;
        Ok(self.raw_line(&format!("{key}={value}")))
    }

    /// `key = value` (ini/wireguard style).
    pub fn ini(&mut self, key: &str, value: &str) -> Result<&mut Self, InjectionError> {
        check_single_line(key, value)?;
        Ok(self.raw_line(&format!("{key} = {value}")))
    }

    pub fn blank(&mut self) -> &mut Self {
        self.out.push('\n');
        self
    }

    pub fn indent(&mut self, n: usize) -> &mut Self {
        self.indent = n;
        self
    }

    pub fn finish(self) -> String {
        self.out
    }

    pub fn as_str(&self) -> &str {
        &self.out
    }

    /// Appends preformatted text (e.g. a PEM block inside `<ca>` tags).
    pub fn block(&mut self, text: &str) -> &mut Self {
        let _ = write!(self.out, "{text}");
        if !text.ends_with('\n') {
            self.out.push('\n');
        }
        self
    }
}

fn check_single_line(key: &str, value: &str) -> Result<(), InjectionError> {
    if key.contains(['\n', '\r', '\0']) || value.contains(['\n', '\r', '\0']) {
        Err(InjectionError { key: key.into() })
    } else {
        Ok(())
    }
}

/// Quotes a string for OpenVPN-style configs, which accept double quotes with backslash escapes.
pub fn quote(value: &str) -> String {
    let mut s = String::with_capacity(value.len() + 2);
    s.push('"');
    for c in value.chars() {
        if c == '"' || c == '\\' {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('"');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_blocks_injection() {
        let mut w = ConfigWriter::new();
        w.directive("dev", "tun0").unwrap();
        assert!(w.directive("remote", "a\nscript-security 3").is_err());
        w.assign("port", "53").unwrap();
        assert_eq!(w.finish(), "dev tun0\nport=53\n");
        assert_eq!(quote(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }
}
