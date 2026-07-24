use serde::{Deserialize, Serialize};

use super::{GatewayError, Usage};

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum CanonicalEvent {
    Created {
        response_id: String,
    },
    OutputTextDelta {
        item_id: String,
        delta: String,
    },
    ReasoningDelta {
        item_id: String,
        delta: String,
    },
    ToolCallDelta {
        item_id: String,
        call_id: String,
        name: String,
        delta: String,
    },
    Usage(Usage),
    Completed,
    Failed(GatewayError),
}
