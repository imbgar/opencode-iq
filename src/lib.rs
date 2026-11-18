//! opencode-iq: OpenCode on Isoquant with zero data retention (ZDR) forced.
//!
//! The binary proves ZDR, starts a local proxy that alone holds the Isoquant
//! key, points OpenCode at that proxy, and checks the proxy's receipts when
//! OpenCode exits.

pub mod isoquant;
pub mod keychain;
pub mod opencode;
pub mod proxy;
pub mod receipt;
