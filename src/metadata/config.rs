//! The `[metadata]` table of the config file.

/// Where song metadata comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    None,
    /// AudioControl, at `url`.
    Acr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataConfig {
    pub source: Source,
    /// ACR's API base URL.
    pub url: String,
}

/// ACR's API on the same device.
pub const DEFAULT_URL: &str = "http://127.0.0.1:1080/api";

impl Default for MetadataConfig {
    fn default() -> Self {
        MetadataConfig {
            source: Source::None,
            url: DEFAULT_URL.into(),
        }
    }
}

/// The `[metadata]` table, checked; no table is no source.
pub fn from_table(table: Option<toml::Table>) -> Result<MetadataConfig, String> {
    let mut config = MetadataConfig::default();
    let Some(table) = table else {
        return Ok(config);
    };
    if let Some(key) = table.keys().find(|k| *k != "source" && *k != "url") {
        return Err(format!("metadata: unknown key {key}"));
    }
    if let Some(v) = table.get("source") {
        config.source = match v.as_str() {
            Some("acr") => Source::Acr,
            Some("none") => Source::None,
            _ => return Err("metadata: source must be \"acr\" or \"none\"".into()),
        };
    }
    if let Some(v) = table.get("url") {
        match v.as_str() {
            Some(url) if url.starts_with("http://") => config.url = url.into(),
            _ => return Err("metadata: url must start with http://".into()),
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<MetadataConfig, String> {
        from_table(Some(toml::from_str(text).unwrap()))
    }

    #[test]
    fn no_table_is_no_source() {
        assert_eq!(
            from_table(None).unwrap(),
            MetadataConfig {
                source: Source::None,
                url: "http://127.0.0.1:1080/api".into()
            }
        );
        assert_eq!(parse("").unwrap(), MetadataConfig::default());
    }

    #[test]
    fn acr_is_read_with_its_url() {
        assert_eq!(
            parse("source = \"acr\"\nurl = \"http://192.0.2.1:1080/api\"\n").unwrap(),
            MetadataConfig {
                source: Source::Acr,
                url: "http://192.0.2.1:1080/api".into()
            }
        );
        assert_eq!(parse("source = \"none\"\n").unwrap().source, Source::None);
    }

    #[test]
    fn an_unknown_source_is_an_error() {
        assert_eq!(
            parse("source = \"mpd\"\n").unwrap_err(),
            "metadata: source must be \"acr\" or \"none\""
        );
        assert_eq!(
            parse("source = 1\n").unwrap_err(),
            "metadata: source must be \"acr\" or \"none\""
        );
    }

    #[test]
    fn an_unknown_key_is_an_error() {
        let e = parse("colour = 1\n").unwrap_err();
        assert!(e.contains("metadata: unknown key colour"), "{e}");
    }

    #[test]
    fn a_url_that_is_not_http_is_an_error() {
        assert_eq!(
            parse("url = \"ftp://x\"\n").unwrap_err(),
            "metadata: url must start with http://"
        );
        assert_eq!(
            parse("url = 5\n").unwrap_err(),
            "metadata: url must start with http://"
        );
    }
}
