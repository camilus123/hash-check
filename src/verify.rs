//! Verification logic that needs no I/O: classifying a re-hashed file and
//! working out which volume a path lives on.

/// What is known about a file at one point in time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint<'a> {
    pub hash: &'a [u8],
    pub size: u64,
    pub mtime_ns: i64,
}

/// Outcome of comparing a file with its stored record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The hash matches.
    Ok,
    /// The hash differs and so does the size and/or modified time:
    /// probably a legitimate edit.
    Changed {
        size_differs: bool,
        mtime_differs: bool,
    },
    /// The hash differs although size and modified time are identical:
    /// the signature of bit rot.
    PossiblyCorrupted,
}

pub fn classify(stored: Fingerprint<'_>, current: Fingerprint<'_>) -> Verdict {
    if stored.hash == current.hash {
        return Verdict::Ok;
    }
    let size_differs = stored.size != current.size;
    let mtime_differs = stored.mtime_ns != current.mtime_ns;
    if size_differs || mtime_differs {
        Verdict::Changed {
            size_differs,
            mtime_differs,
        }
    } else {
        Verdict::PossiblyCorrupted
    }
}

/// The part of a path that names its volume: `C:\` for `C:\dir\file`,
/// `\\server\share\` for a network path, `/` for a Unix path. Used to tell
/// "this drive is not connected" apart from "this file is missing".
pub fn volume_root(path: &str) -> Option<&str> {
    let b = path.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
    {
        return Some(&path[..3]);
    }
    if let Some(rest) = path.strip_prefix(r"\\") {
        // \\server\share\...
        let server_end = rest.find('\\')?;
        let after_server = &rest[server_end + 1..];
        if server_end == 0 || after_server.is_empty() {
            return None;
        }
        let share_len = after_server.find('\\').unwrap_or(after_server.len());
        if share_len == 0 {
            return None;
        }
        let end = (2 + server_end + 1 + share_len + 1).min(path.len());
        return Some(&path[..end]);
    }
    if path.starts_with('/') {
        return Some("/");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(hash: &'static [u8], size: u64, mtime_ns: i64) -> Fingerprint<'static> {
        Fingerprint {
            hash,
            size,
            mtime_ns,
        }
    }

    #[test]
    fn same_hash_is_ok_whatever_the_metadata() {
        assert_eq!(classify(fp(b"aa", 10, 5), fp(b"aa", 10, 5)), Verdict::Ok);
        // Touched (or copied back from backup) without a content change.
        assert_eq!(classify(fp(b"aa", 10, 5), fp(b"aa", 10, 999)), Verdict::Ok);
    }

    #[test]
    fn different_hash_with_different_metadata_is_changed() {
        assert_eq!(
            classify(fp(b"aa", 10, 5), fp(b"bb", 10, 6)),
            Verdict::Changed {
                size_differs: false,
                mtime_differs: true
            }
        );
        assert_eq!(
            classify(fp(b"aa", 10, 5), fp(b"bb", 11, 5)),
            Verdict::Changed {
                size_differs: true,
                mtime_differs: false
            }
        );
        assert_eq!(
            classify(fp(b"aa", 10, 5), fp(b"bb", 11, 6)),
            Verdict::Changed {
                size_differs: true,
                mtime_differs: true
            }
        );
    }

    #[test]
    fn different_hash_with_identical_metadata_is_possible_corruption() {
        assert_eq!(
            classify(fp(b"aa", 10, 5), fp(b"ab", 10, 5)),
            Verdict::PossiblyCorrupted
        );
        // One nanosecond of difference is already a real timestamp change.
        assert_ne!(
            classify(fp(b"aa", 10, 5), fp(b"ab", 10, 6)),
            Verdict::PossiblyCorrupted
        );
        // Pre-1970 and zero-size files are classified the same way.
        assert_eq!(
            classify(fp(b"aa", 0, -7), fp(b"ab", 0, -7)),
            Verdict::PossiblyCorrupted
        );
    }

    #[test]
    fn volume_roots() {
        assert_eq!(volume_root(r"L:\media\film.mkv"), Some(r"L:\"));
        assert_eq!(volume_root(r"c:\x"), Some(r"c:\"));
        assert_eq!(volume_root(r"C:\"), Some(r"C:\"));
        assert_eq!(volume_root(r"\\nas\share\dir\f.txt"), Some(r"\\nas\share\"));
        assert_eq!(volume_root(r"\\nas\share"), Some(r"\\nas\share"));
        assert_eq!(volume_root(r"\\nas\"), None);
        assert_eq!(volume_root(r"\\"), None);
        assert_eq!(volume_root("/home/m/a"), Some("/"));
        assert_eq!(volume_root("relative/path"), None);
        assert_eq!(volume_root(""), None);
        assert_eq!(volume_root("Ž:\\x"), None);
    }
}
