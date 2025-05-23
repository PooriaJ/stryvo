use std::collections::BTreeMap;

use async_trait::async_trait;
use pingora_core::{Error, Result};
use pingora_http::RequestHeader;
use pingora_proxy::Session;
use regex::Regex;

use super::{ensure_empty, extract_val, StryvoContext};

/// This is a single-serving trait for modifiers that provide actions for
/// [ProxyHttp::upstream_request_filter] methods
#[async_trait]
pub trait RequestModifyMod: Send + Sync {
    /// See [ProxyHttp::upstream_request_filter] for more details
    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        header: &mut RequestHeader,
        ctx: &mut StryvoContext,
    ) -> Result<()>;
}

// Remove header by key
//
//

/// Removes a header if the key matches a given regex
pub struct RemoveHeaderKeyRegex {
    regex: Regex,
}

impl RemoveHeaderKeyRegex {
    /// Create from the settings field
    pub fn from_settings(mut settings: BTreeMap<String, String>) -> Result<Self> {
        let mat = extract_val("pattern", &mut settings)?;

        let reg = Regex::new(&mat).map_err(|e| {
            tracing::error!("Bad pattern: '{mat}': {e:?}");
            Error::new_str("Error building regex")
        })?;

        ensure_empty(&settings)?;

        Ok(Self { regex: reg })
    }
}

#[async_trait]
impl RequestModifyMod for RemoveHeaderKeyRegex {
    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        header: &mut RequestHeader,
        _ctx: &mut StryvoContext,
    ) -> Result<()> {
        // Find all the headers that have keys that match the regex...
        let headers = header
            .headers
            .keys()
            .filter_map(|k| {
                if self.regex.is_match(k.as_str()) {
                    tracing::debug!("Removing header: {k:?}");
                    Some(k.to_owned())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        // ... and remove them
        for h in headers {
            assert!(header.remove_header(&h).is_some());
        }

        Ok(())
    }
}

// Upsert Header
//
//

/// Adds or replaces a given header key and value
pub struct UpsertHeader {
    key: String,
    value: String,
}

impl UpsertHeader {
    /// Create from the settings field
    pub fn from_settings(mut settings: BTreeMap<String, String>) -> Result<Self> {
        let key = extract_val("key", &mut settings)?;
        let value = extract_val("value", &mut settings)?;
        Ok(Self { key, value })
    }
}

#[async_trait]
impl RequestModifyMod for UpsertHeader {
    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        header: &mut RequestHeader,
        _ctx: &mut StryvoContext,
    ) -> Result<()> {
        if let Some(h) = header.remove_header(&self.key) {
            tracing::debug!("Removed header: {h:?}");
        }
        header.append_header(self.key.clone(), &self.value)?;
        tracing::debug!("Inserted header: {}: {}", self.key, self.value);
        Ok(())
    }
}
