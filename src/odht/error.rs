use alloc::string::String;

#[derive(Eq, PartialEq, Debug)]
pub(crate) struct Error(pub String);

// `core::error::Error` since 1.81, so this survives the no_std conversion.
impl core::error::Error for Error {}
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}
