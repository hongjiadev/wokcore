use std::fmt;

use wokcore_protocols::canonical::{CanonicalRequest, InputItem};

use super::ClientProtocol;

pub struct DataPlaneRequest {
    protocol: ClientProtocol,
    canonical: CanonicalRequest,
}

impl DataPlaneRequest {
    pub(super) fn new(protocol: ClientProtocol, canonical: CanonicalRequest) -> Self {
        Self {
            protocol,
            canonical,
        }
    }

    pub fn protocol(&self) -> ClientProtocol {
        self.protocol
    }

    pub fn canonical(&self) -> &CanonicalRequest {
        &self.canonical
    }

    pub fn into_canonical(self) -> CanonicalRequest {
        self.canonical
    }

    pub fn summary(&self) -> DataPlaneRequestSummary {
        DataPlaneRequestSummary {
            protocol: self.protocol,
            request_id: self.canonical.request_id.as_str().to_owned(),
            model: self.canonical.model.as_str().to_owned(),
            stream: self.canonical.stream,
            input_items: self.canonical.input.len(),
            image_items: self
                .canonical
                .input
                .iter()
                .filter(|item| matches!(item, InputItem::ImageUrl { .. }))
                .count(),
            tools: self.canonical.tools.len(),
            extension_fields: self.canonical.extensions.len(),
        }
    }
}

impl fmt::Debug for DataPlaneRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.summary().fmt(formatter)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataPlaneRequestSummary {
    pub protocol: ClientProtocol,
    pub request_id: String,
    pub model: String,
    pub stream: bool,
    pub input_items: usize,
    pub image_items: usize,
    pub tools: usize,
    pub extension_fields: usize,
}
