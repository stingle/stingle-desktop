//! The standard Stingle response envelope:
//! `{ status, parts:{...}, infos:[...], errors:[...] }` with an optional
//! `parts.logout` signalling session expiry.

use serde_json::Value;

use crate::error::{ApiError, Result};

#[derive(Debug)]
pub struct StingleResponse {
    pub status: String,
    pub parts: Value,
    pub infos: Vec<String>,
    pub errors: Vec<String>,
}

impl StingleResponse {
    /// Parse a top-level response JSON value.
    pub fn from_value(v: Value) -> Self {
        let status = v
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("ok")
            .to_string();
        let parts = v.get("parts").cloned().unwrap_or(Value::Null);
        let infos = string_array(v.get("infos"));
        let errors = string_array(v.get("errors"));
        Self {
            status,
            parts,
            infos,
            errors,
        }
    }

    pub fn is_ok(&self) -> bool {
        self.status == "ok"
    }

    /// The server asked us to log out (session expired).
    pub fn logged_out(&self) -> bool {
        self.get("logout").map(|s| !s.is_empty()).unwrap_or(false)
    }

    /// Read a scalar part as a string, mirroring `parts.optString(name)`.
    pub fn get(&self, name: &str) -> Option<String> {
        let val = self.parts.get(name)?;
        Some(match val {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        })
    }

    /// Require a scalar part, erroring if absent or empty.
    pub fn require(&self, name: &'static str) -> Result<String> {
        match self.get(name) {
            Some(s) if !s.is_empty() => Ok(s),
            _ => Err(ApiError::MissingField(name)),
        }
    }

    /// Read an array part. The server may send it as a JSON array or as a
    /// JSON-encoded string (matching `optString` + `new JSONArray(str)`).
    pub fn get_array(&self, name: &str) -> Vec<Value> {
        match self.parts.get(name) {
            Some(Value::Array(a)) => a.clone(),
            Some(Value::String(s)) if !s.is_empty() => {
                serde_json::from_str::<Vec<Value>>(s).unwrap_or_default()
            }
            _ => Vec::new(),
        }
    }

    /// Deserialize an array part into typed items, skipping any that fail.
    pub fn parse_array<T: serde::de::DeserializeOwned>(&self, name: &str) -> Vec<T> {
        self.get_array(name)
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect()
    }

    /// Validate status/logout, converting the envelope into a usable result.
    pub fn into_result(self) -> Result<Self> {
        if self.logged_out() {
            return Err(ApiError::LoggedOut);
        }
        if !self.is_ok() {
            return Err(ApiError::Server {
                errors: self.errors,
                infos: self.infos,
            });
        }
        Ok(self)
    }
}

fn string_array(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| match x {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        _ => Vec::new(),
    }
}
