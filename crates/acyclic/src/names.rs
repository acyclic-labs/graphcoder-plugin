//! Host names ↔ engine names, in this platform's volume profile encoding.
//!
//! [`crate::store::volume_config`] picks a [`FilesystemProfile`] per platform,
//! and every layer that names a file has to agree with it: capture and
//! restore, the exclusion rules, the diff walk, and the fork mount router.
//! The mount router is the unforgiving one — the `ProjFS` provider decodes
//! every entry name it is handed as UTF-16LE, so a name that reaches it in
//! any other encoding is *projected as mojibake* rather than rejected.
//!
//! The byte form of a name is therefore platform-defined, and this module is
//! the only place that defines it: raw bytes on Unix, UTF-16LE on Windows.
//! Everywhere else in the engine a name is opaque bytes, compared and stored
//! but never interpreted. Anything that mints name bytes from host paths or
//! from configured text must come through here, or it will disagree with the
//! volume on the first non-ASCII name — and on Windows, on every name.

use std::ffi::{OsStr, OsString};

use acyclic_fs::kernel::NameEncoding;
use acyclic_fs::model::FilesystemProfile;

/// The volume profile for this host.
#[must_use]
pub const fn profile() -> FilesystemProfile {
    #[cfg(windows)]
    {
        FilesystemProfile::Windows
    }
    #[cfg(unix)]
    {
        FilesystemProfile::Posix
    }
    #[cfg(not(any(unix, windows)))]
    {
        FilesystemProfile::Portable
    }
}

/// The name encoding that matches [`profile`].
#[must_use]
pub const fn encoding() -> NameEncoding {
    #[cfg(windows)]
    {
        NameEncoding::WindowsUtf16Le
    }
    #[cfg(unix)]
    {
        NameEncoding::PosixBytes
    }
    #[cfg(not(any(unix, windows)))]
    {
        NameEncoding::Utf8
    }
}

/// Upper bound on one name component, in the bytes [`encoding`] produces.
///
/// Both families cap a component at 255 *characters*; UTF-16LE spends two
/// bytes per unit, so the byte budget has to double on Windows or a legal
/// 200-character filename is refused as too long.
#[must_use]
pub const fn maximum_component_bytes() -> u32 {
    #[cfg(windows)]
    {
        510
    }
    #[cfg(not(windows))]
    {
        255
    }
}

/// A host name in this platform's engine byte form.
///
/// Lossless in both directions: Unix names are arbitrary bytes and Windows
/// names are arbitrary UTF-16, and neither is forced through UTF-8 on the
/// way past. [`bytes_to_os`] is the exact inverse.
#[cfg(unix)]
#[must_use]
pub fn os_to_bytes(name: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    name.as_bytes().to_vec()
}

#[cfg(windows)]
#[must_use]
pub fn os_to_bytes(name: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    name.encode_wide().flat_map(u16::to_le_bytes).collect()
}

#[cfg(not(any(unix, windows)))]
#[must_use]
pub fn os_to_bytes(name: &OsStr) -> Vec<u8> {
    name.to_string_lossy().into_owned().into_bytes()
}

/// Inverse of [`os_to_bytes`].
#[cfg(unix)]
#[must_use]
pub fn bytes_to_os(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(bytes.to_vec())
}

/// Inverse of [`os_to_bytes`]. A trailing odd byte cannot occur in a name
/// this module produced, and is dropped rather than failing the whole walk.
#[cfg(windows)]
#[must_use]
pub fn bytes_to_os(bytes: &[u8]) -> OsString {
    use std::os::windows::ffi::OsStringExt;
    let (pairs, _odd_trailing_byte) = bytes.as_chunks::<2>();
    let units: Vec<u16> = pairs.iter().copied().map(u16::from_le_bytes).collect();
    OsString::from_wide(&units)
}

#[cfg(not(any(unix, windows)))]
#[must_use]
pub fn bytes_to_os(bytes: &[u8]) -> OsString {
    String::from_utf8_lossy(bytes).into_owned().into()
}

/// UTF-8 text as engine name bytes: configured exclude rules and the fork
/// route ids the mount router projects as directory names.
#[must_use]
pub fn str_to_bytes(text: &str) -> Vec<u8> {
    os_to_bytes(OsStr::new(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_bytes_round_trip_through_the_host_encoding() {
        for name in ["a.txt", "sub", "ünïcøde", "日本語", ".git"] {
            let bytes = os_to_bytes(OsStr::new(name));
            assert_eq!(bytes_to_os(&bytes), OsString::from(name), "{name}");
        }
    }

    #[test]
    fn str_bytes_agree_with_os_bytes() {
        assert_eq!(str_to_bytes("fork-id"), os_to_bytes(OsStr::new("fork-id")));
    }

    /// The encoding and the profile are one decision; a host that reported
    /// mismatched halves would capture names the mount layer cannot project.
    #[test]
    fn encoding_matches_profile() {
        let expected = match profile() {
            FilesystemProfile::Posix => NameEncoding::PosixBytes,
            FilesystemProfile::Windows => NameEncoding::WindowsUtf16Le,
            FilesystemProfile::Portable | FilesystemProfile::Browser => NameEncoding::Utf8,
        };
        assert_eq!(encoding(), expected);
    }

    /// An ASCII name costs one byte per character on Unix and two on
    /// Windows; the budget has to cover 255 characters either way.
    #[test]
    fn component_budget_covers_a_maximal_host_name() {
        let longest = "x".repeat(255);
        let bytes = os_to_bytes(OsStr::new(&longest));
        assert!(
            u32::try_from(bytes.len()).is_ok_and(|len| len <= maximum_component_bytes()),
            "255-character name needs {} bytes, budget is {}",
            bytes.len(),
            maximum_component_bytes()
        );
    }
}
