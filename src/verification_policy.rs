//! Share checker identity and bounded-log policy between admission and execution.
//!
//! This contract depends on content resolution, not evidence admission or workers.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::content::ContentResolver;

pub(crate) const MAX_LOG_BYTES: u64 = 384 * 1024;

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub(crate) fn sandbox_digest(resolver: &ContentResolver) -> Result<String> {
    Ok(digest(
        resolver
            .resolve_content("sandbox/common.sb")
            .context("verification checker requires common sandbox definition")?
            .as_bytes(),
    ))
}
