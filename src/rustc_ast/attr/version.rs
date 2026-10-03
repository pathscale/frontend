use core::fmt::{self, Display};

use rustc_macros::{BlobDecodable, Encodable, StableHash};

#[derive(Encodable, BlobDecodable, Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[derive(StableHash)]
pub struct RustcVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl RustcVersion {
    /// Parse a [`RustcVersion`] which is exactly `<major>.<minor>.<patch>`, with no suffix.
    pub fn parse_str_strict(value: &str) -> Option<Self> {
        let mut components = value.splitn(3, '.');
        let major = components.next()?.parse().ok()?;
        let minor = components.next()?.parse().ok()?;
        let patch = components.next()?.parse().ok()?;
        Some(RustcVersion { major, minor, patch })
    }
}

impl Display for RustcVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}
