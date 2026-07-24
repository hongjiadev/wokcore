use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{InputItem, ReasoningOptions, ToolDefinition};

macro_rules! string_newtype {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

string_newtype!(RequestId);
string_newtype!(PublicModelId);
string_newtype!(ThreadKey);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterKind {
    OpenAiResponses,
    OpenAiChat,
    Anthropic,
    Gemini,
    AzureOpenAi,
    Cursor,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct CanonicalRequest {
    pub request_id: RequestId,
    pub model: PublicModelId,
    pub thread_key: Option<ThreadKey>,
    pub input: Vec<InputItem>,
    pub tools: Vec<ToolDefinition>,
    pub stream: bool,
    pub reasoning: Option<ReasoningOptions>,
    pub extensions: BTreeMap<String, Value>,
}

impl CanonicalRequest {
    pub fn with_extension(mut self, key: impl Into<String>, value: Value) -> Self {
        self.extensions.insert(key.into(), value);
        self
    }
}
