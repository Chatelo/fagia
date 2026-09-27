//! Path presentation: `~` for home, escaped control characters, and a
//! lossless JSON form for paths that are not valid UTF-8.

use schemars::JsonSchema;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Human form of a path. Control characters are escaped so a file name can
/// never inject terminal escape sequences.
pub fn display_path(path: &Path, home: Option<&Path>) -> String {
    let shown = match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.to_string_lossy()),
        None => path.to_string_lossy().into_owned(),
    };
    escape_control(&shown)
}

pub fn escape_control(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// JSON form of a path: `path` is always present (lossy when needed) and
/// `path_bytes` carries the exact bytes, base64, when the path is not UTF-8.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct JsonPath {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_bytes: Option<String>,
}

impl JsonPath {
    pub fn new(path: &Path) -> Self {
        match path.to_str() {
            Some(s) => Self {
                path: s.to_string(),
                path_bytes: None,
            },
            None => Self {
                path: path.to_string_lossy().into_owned(),
                path_bytes: Some(base64(path.as_os_str().as_encoded_bytes())),
            },
        }
    }
}

pub fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let bytes = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

impl JsonPath {
    /// The exact path back, using `path_bytes` when present.
    pub fn to_path(&self) -> PathBuf {
        use std::os::unix::ffi::OsStringExt;
        match self.path_bytes.as_deref().and_then(base64_decode) {
            Some(b) => PathBuf::from(std::ffi::OsString::from_vec(b)),
            None => PathBuf::from(&self.path),
        }
    }
}

/// Expands a leading `~` against `home`.
pub fn expand_tilde(raw: &str, home: &Path) -> PathBuf {
    if raw == "~" {
        home.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(raw)
    }
}

/// Lexical containment check; callers canonicalize first when symlinks matter.
pub fn is_within(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn shortens_home_and_escapes() {
        let home = Path::new("/home/u");
        assert_eq!(display_path(Path::new("/home/u"), Some(home)), "~");
        assert_eq!(display_path(Path::new("/home/u/a b"), Some(home)), "~/a b");
        assert_eq!(
            display_path(Path::new("/home/user2"), Some(home)),
            "/home/user2"
        );
        assert_eq!(
            display_path(Path::new("/tmp/x\x1b[2Jy"), None),
            "/tmp/x\\u{1b}[2Jy"
        );
    }

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn non_utf8_paths_keep_bytes() {
        let p = Path::new(OsStr::from_bytes(b"/tmp/\xff"));
        let j = JsonPath::new(p);
        assert_eq!(j.path, "/tmp/\u{fffd}");
        assert_eq!(j.path_bytes.as_deref(), Some("L3RtcC//"));
        assert_eq!(j.to_path(), p);
        assert_eq!(base64_decode("Zm9vYmE="), Some(b"fooba".to_vec()));
    }
}
