//! Format detection and syntax validation for JSON/TOML/YAML/XML. Each
//! parser is only ever asked "is this well-formed?" — we throw away the
//! parsed value and keep the parser's own error, since `serde_json`,
//! `toml`, `serde_yaml`, and `roxmltree` all already produce a readable,
//! position-annotated message on their own (`toml` and `roxmltree` even
//! include a little code excerpt) — reformatting that ourselves would only
//! throw information away.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Toml,
    Yaml,
    Xml,
}

impl Format {
    pub const ALL: [Format; 4] = [Format::Json, Format::Toml, Format::Yaml, Format::Xml];

    pub fn label(self) -> &'static str {
        match self {
            Format::Json => "JSON",
            Format::Toml => "TOML",
            Format::Yaml => "YAML",
            Format::Xml => "XML",
        }
    }
}

/// Guesses a format from a path's extension. `None` means the caller
/// should ask the user (the Check tab's Format field can force one).
pub fn detect(path: &str) -> Option<Format> {
    let lower = path.to_lowercase();
    if lower.ends_with(".json") {
        Some(Format::Json)
    } else if lower.ends_with(".toml") {
        Some(Format::Toml)
    } else if lower.ends_with(".yaml") || lower.ends_with(".yml") {
        Some(Format::Yaml)
    } else if lower.ends_with(".xml") {
        Some(Format::Xml)
    } else {
        None
    }
}

/// `Ok(())` if `content` is well-formed `format`; `Err(message)` with the
/// underlying parser's own (often multi-line) error text otherwise.
pub fn validate(format: Format, content: &str) -> Result<(), String> {
    match format {
        Format::Json => serde_json::from_str::<serde_json::Value>(content).map(|_| ()).map_err(|e| e.to_string()),
        Format::Toml => content.parse::<toml::Value>().map(|_| ()).map_err(|e| e.to_string()),
        Format::Yaml => serde_yaml::from_str::<serde_yaml::Value>(content).map(|_| ()).map_err(|e| e.to_string()),
        Format::Xml => roxmltree::Document::parse(content).map(|_| ()).map_err(|e| e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_extension_case_insensitively() {
        assert_eq!(detect("/etc/app/Config.JSON"), Some(Format::Json));
        assert_eq!(detect("settings.toml"), Some(Format::Toml));
        assert_eq!(detect("compose.yml"), Some(Format::Yaml));
        assert_eq!(detect("values.yaml"), Some(Format::Yaml));
        assert_eq!(detect("pom.xml"), Some(Format::Xml));
        assert_eq!(detect("README"), None);
    }

    #[test]
    fn validates_each_format() {
        assert!(validate(Format::Json, r#"{"a": 1}"#).is_ok());
        assert!(validate(Format::Json, "{a: 1}").is_err());

        assert!(validate(Format::Toml, "a = 1\n").is_ok());
        assert!(validate(Format::Toml, "a = \n").is_err());

        assert!(validate(Format::Yaml, "a: 1\n").is_ok());
        assert!(validate(Format::Yaml, "a:\n\tb: 1\n").is_err());

        assert!(validate(Format::Xml, "<a><b/></a>").is_ok());
        assert!(validate(Format::Xml, "<a><b></a>").is_err());
    }

    #[test]
    fn json_error_includes_line_info() {
        let err = validate(Format::Json, "{\n  \"a\": ,\n}").unwrap_err();
        assert!(err.contains("line 2"), "expected a line number in: {err}");
    }
}
