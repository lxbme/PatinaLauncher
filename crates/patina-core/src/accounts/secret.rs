use std::fmt;
use zeroize::Zeroizing;

/// Secret material is only readable at explicit protocol/storage boundaries.
pub(crate) struct SecretString(Zeroizing<String>);

impl SecretString {
    #[allow(dead_code, reason = "Constructed by Task 2 MsaClient")]
    pub(crate) fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}
