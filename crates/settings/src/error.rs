use std::path::PathBuf;
use std::sync::Arc;

/// Everything that can go wrong loading, saving or watching settings.
///
/// Cheap to clone (I/O errors are shared behind an `Arc`) so it can be fanned
/// out to UI observers. `Display` is suitable for showing in the app.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SettingsError {
    /// The file exists but could not be read.
    #[error("could not read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: Arc<std::io::Error>,
    },

    /// The file is not valid TOML, or a value has the wrong type.
    #[error("{}{}: {message}", path.display(), location(*line, *column))]
    Parse {
        path: PathBuf,
        /// The parser's description, without the source snippet.
        message: String,
        /// 1-based line of the problem, when the parser reported a span.
        line: Option<usize>,
        /// 1-based column of the problem, when the parser reported a span.
        column: Option<usize>,
    },

    /// The file parsed, but a value is outside its allowed range.
    #[error("{}: invalid `{field}`: {reason}", path.display())]
    Invalid {
        path: PathBuf,
        field: &'static str,
        reason: String,
    },

    /// Settings could not be serialised to TOML (a bug, not user error).
    #[error("could not serialise settings: {0}")]
    Serialize(String),

    /// Writing the file (temp file, fsync or rename) failed.
    #[error("could not write {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: Arc<std::io::Error>,
    },

    /// The file watcher could not be started or failed.
    #[error("could not watch {}: {message}", path.display())]
    Watch { path: PathBuf, message: String },
}

fn location(line: Option<usize>, column: Option<usize>) -> String {
    match (line, column) {
        (Some(l), Some(c)) => format!(":{l}:{c}"),
        (Some(l), None) => format!(":{l}"),
        _ => String::new(),
    }
}
