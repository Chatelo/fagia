use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid size {0:?}: expected a number with an optional K, M, G or T suffix")]
    InvalidSize(String),
    #[error("invalid duration {0:?}: expected a number with an s, m, h, d or w suffix")]
    InvalidDuration(String),
    #[error("config {}: {message}", path.display())]
    Config { path: PathBuf, message: String },
    #[error("rule {id}: {message}")]
    Rule { id: String, message: String },
    #[error("history store: {0}")]
    Store(#[from] rusqlite::Error),
    #[error("{0}")]
    Platform(String),
    #[error("refused: {0}")]
    Refused(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) trait IoContext<T> {
    fn at(self, path: &Path) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn at(self, path: &Path) -> Result<T> {
        self.map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}
