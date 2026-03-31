use thiserror::Error;

#[derive(Debug, Error)]
pub enum MvnError {
    #[error("POM parse error: {message}")]
    PomParseError { message: String },

    #[error("invalid version '{input}': {message}")]
    VersionParseError { input: String, message: String },

    #[error("invalid coordinate '{input}': {message}")]
    CoordParseError { input: String, message: String },

    #[error("dependency resolution failed: {message}")]
    ResolutionError { message: String },

    #[error("download failed for '{url}': {message}")]
    DownloadError { url: String, message: String },

    #[error("checksum mismatch for '{artifact}': expected {expected}, got {actual}")]
    ChecksumMismatch {
        artifact: String,
        expected: String,
        actual: String,
    },

    #[error("artifact not found: {coord}")]
    ArtifactNotFound { coord: String },

    #[error(transparent)]
    IoError(#[from] std::io::Error),

    #[error("network error: {0}")]
    NetworkError(String),

    #[error(transparent)]
    XmlError(#[from] quick_xml::DeError),

    #[error("settings parse error: {message}")]
    SettingsParseError { message: String },

    #[error("upload failed for '{url}': {message}")]
    UploadError { url: String, message: String },
}

impl From<reqwest::Error> for MvnError {
    fn from(err: reqwest::Error) -> Self {
        MvnError::NetworkError(err.to_string())
    }
}

pub type Result<T> = std::result::Result<T, MvnError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_pom_parse_error() {
        let err = MvnError::PomParseError {
            message: "unexpected tag".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("POM parse error"), "got: {msg}");
        assert!(msg.contains("unexpected tag"), "got: {msg}");
    }

    #[test]
    fn display_version_parse_error() {
        let err = MvnError::VersionParseError {
            input: "abc".into(),
            message: "not numeric".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("invalid version"), "got: {msg}");
        assert!(msg.contains("abc"), "got: {msg}");
        assert!(msg.contains("not numeric"), "got: {msg}");
    }

    #[test]
    fn display_coord_parse_error() {
        let err = MvnError::CoordParseError {
            input: "bad:coord".into(),
            message: "missing version".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("invalid coordinate"), "got: {msg}");
        assert!(msg.contains("bad:coord"), "got: {msg}");
    }

    #[test]
    fn display_resolution_error() {
        let err = MvnError::ResolutionError {
            message: "cycle detected".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("dependency resolution failed"), "got: {msg}");
        assert!(msg.contains("cycle detected"), "got: {msg}");
    }

    #[test]
    fn display_download_error() {
        let err = MvnError::DownloadError {
            url: "https://example.com/foo.jar".into(),
            message: "404".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("download failed"), "got: {msg}");
        assert!(msg.contains("https://example.com/foo.jar"), "got: {msg}");
    }

    #[test]
    fn display_checksum_mismatch() {
        let err = MvnError::ChecksumMismatch {
            artifact: "commons-lang3-3.12.0.jar".into(),
            expected: "aaa".into(),
            actual: "bbb".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("checksum mismatch"), "got: {msg}");
        assert!(msg.contains("aaa"), "got: {msg}");
        assert!(msg.contains("bbb"), "got: {msg}");
    }

    #[test]
    fn display_artifact_not_found() {
        let err = MvnError::ArtifactNotFound {
            coord: "org.example:foo:1.0".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("artifact not found"), "got: {msg}");
        assert!(msg.contains("org.example:foo:1.0"), "got: {msg}");
    }

    #[test]
    fn display_network_error() {
        let err = MvnError::NetworkError("connection refused".into());
        let msg = err.to_string();
        assert!(msg.contains("network error"), "got: {msg}");
        assert!(msg.contains("connection refused"), "got: {msg}");
    }

    #[test]
    fn from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let mvn_err: MvnError = io_err.into();
        match &mvn_err {
            MvnError::IoError(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
            other => panic!("expected IoError, got: {other:?}"),
        }
        // Display should be transparent (show the inner error)
        assert!(mvn_err.to_string().contains("file missing"));
    }

    #[test]
    fn from_xml_de_error() {
        let xml_err = quick_xml::de::from_str::<String>("<bad").unwrap_err();
        let mvn_err: MvnError = xml_err.into();
        match &mvn_err {
            MvnError::XmlError(_) => {}
            other => panic!("expected XmlError, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn from_reqwest_error() {
        // Build a real reqwest error by attempting a connection to a port that won't respond
        let reqwest_err = reqwest::Client::builder()
            .build()
            .unwrap()
            .get("http://[::1]:1")
            .send()
            .await
            .unwrap_err();
        let mvn_err: MvnError = reqwest_err.into();
        match &mvn_err {
            MvnError::NetworkError(msg) => assert!(!msg.is_empty()),
            other => panic!("expected NetworkError, got: {other:?}"),
        }
    }

    #[test]
    fn result_type_alias_ok() {
        let r: Result<i32> = Ok(42);
        assert_eq!(r.unwrap(), 42);
    }

    #[test]
    fn result_type_alias_err() {
        let r: Result<i32> = Err(MvnError::ResolutionError {
            message: "oops".into(),
        });
        assert!(r.is_err());
    }

    #[test]
    fn display_special_chars_in_fields() {
        let err = MvnError::DownloadError {
            url: "https://example.com/path?a=1&b=2".into(),
            message: "error with \"quotes\" and <angle brackets>".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("a=1&b=2"), "got: {msg}");
        assert!(msg.contains("\"quotes\""), "got: {msg}");
        assert!(msg.contains("<angle brackets>"), "got: {msg}");
    }

    #[test]
    fn display_unicode_in_fields() {
        let err = MvnError::PomParseError {
            message: "遇到意外标签 <données>".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("遇到意外标签"), "got: {msg}");
    }

    #[test]
    fn display_settings_parse_error() {
        let err = MvnError::SettingsParseError {
            message: "invalid XML".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("settings parse error"), "got: {msg}");
        assert!(msg.contains("invalid XML"), "got: {msg}");
    }
}
