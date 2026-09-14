use crate::error::{Error, Result};
use crate::util;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

// Both embedded vocabularies encode at most 128 bytes in one ordinary token.
// The vocabulary test below checks this bound when the tokenizer is updated.
pub(crate) const MAX_TOKEN_BYTES: usize = 128;

#[derive(Clone, Debug)]
pub struct Budget {
    pub kind: String,
    pub encoding: String,
    pub limit: usize,
}

impl Budget {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.kind.as_str(), "tokens" | "bytes")
            || !matches!(self.encoding.as_str(), "o200k_base" | "cl100k_base")
        {
            return Err(Error::new(
                "invalid_request",
                "unsupported budget kind or tokenizer",
            ));
        }
        Ok(())
    }

    pub fn count(&self, text: &str) -> usize {
        if self.kind == "bytes" {
            text.len()
        } else if self.encoding == "cl100k_base" {
            tiktoken_rs::cl100k_base_singleton()
                .encode_ordinary(text)
                .len()
        } else {
            tiktoken_rs::o200k_base_singleton()
                .encode_ordinary(text)
                .len()
        }
    }

    /// Counts the same compact JSON serialization used by CLI and MCP.
    pub fn settle(&self, value: &mut Value) -> Result<usize> {
        value["budget"] = json!({"kind":self.kind,"encoding":if self.kind=="tokens"{json!(self.encoding)}else{Value::Null},"limit":self.limit,"used":0});
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..32 {
            let used = self.count(&serde_json::to_string(value)?);
            if value["budget"]["used"] == used {
                return Ok(used);
            }
            if !seen.insert(used) {
                return Err(Error::new(
                    "budget_accounting_error",
                    "token count did not settle",
                ));
            }
            value["budget"]["used"] = json!(used);
        }
        Err(Error::new(
            "budget_accounting_error",
            "token count did not settle",
        ))
    }

    pub fn require(&self, value: &mut Value) -> Result<()> {
        let minimum = self.settle(value)?;
        if minimum > self.limit {
            return Err(Error::new(
                "budget_below_minimum",
                json!({"minimum_budget":minimum,"kind":self.kind,"limit":self.limit}),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Continuation {
    pub format: u32,
    pub view_id: String,
    pub request_hash: String,
    pub stream: String,
    pub offset: usize,
}

impl Continuation {
    pub fn encode(&self) -> Result<String> {
        Ok(URL_SAFE_NO_PAD.encode(util::canonical(self)?))
    }
    pub fn decode(token: &str) -> Result<Self> {
        let error = || Error::new("stale_continuation", "invalid continuation");
        if token.len() > 8192 {
            return Err(error());
        }
        let bytes = URL_SAFE_NO_PAD.decode(token).map_err(|_| error())?;
        let value: Self = serde_json::from_slice(&bytes).map_err(|_| error())?;
        if value.format != 1
            || ulid::Ulid::from_string(&value.view_id).is_err()
            || value.request_hash.len() != 64
            || value.encode()? != token
        {
            return Err(error());
        }
        Ok(value)
    }
}

pub fn utf8_end(text: &str, mut end: usize) -> usize {
    end = end.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::MAX_TOKEN_BYTES;

    #[test]
    fn supported_vocabularies_obey_the_ordinary_token_byte_bound() {
        // Ordinary ranks are contiguous in the two bundled base vocabularies.
        // Decode bytes directly: individual tokens need not be valid UTF-8.
        for (encoding, count) in [
            (tiktoken_rs::cl100k_base_singleton(), 100_256),
            (tiktoken_rs::o200k_base_singleton(), 199_998),
        ] {
            let maximum = encoding
                ._decode_native_and_split((0..count).collect())
                .map(|bytes| bytes.len())
                .max()
                .unwrap();
            assert_eq!(maximum, MAX_TOKEN_BYTES);
        }
    }
}
