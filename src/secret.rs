//! A string that never prints itself.

use std::fmt;

/// A credential value. `Debug` and `Display` print a placeholder, so a secret
/// cannot reach a log line, an error message or a tool result by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value itself. Call this only where it goes on the wire.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_formats_its_value() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s}"), "[redacted]");
        assert_eq!(format!("{s:?}"), "[redacted]");
        assert_eq!(s.expose(), "hunter2");
    }
}
