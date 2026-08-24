use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use chrono::DateTime;
use serde_json::Value;

use crate::{Error, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Identity {
    pub email: String,
    pub account_id: String,
    pub user_id: String,
}

#[derive(Clone, Debug)]
pub struct AuthDocument {
    pub path: PathBuf,
    pub raw: Value,
    pub identity: Identity,
    pub refresh_ns: i64,
}

impl AuthDocument {
    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = fs::read(path).map_err(|error| Error::io(path, error))?;
        let raw: Value =
            serde_json::from_slice(&bytes).map_err(|error| Error::json(path, error))?;
        let tokens = raw
            .get("tokens")
            .and_then(Value::as_object)
            .ok_or_else(|| Error::InvalidAuth(path.to_owned()))?;
        let id_token = tokens
            .get("id_token")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::InvalidAuth(path.to_owned()))?;
        let claims = decode_claims(id_token).ok_or_else(|| Error::InvalidAuth(path.to_owned()))?;
        let profile = claims
            .get("https://api.openai.com/profile")
            .and_then(Value::as_object);
        let auth = claims
            .get("https://api.openai.com/auth")
            .and_then(Value::as_object);
        let email = claims.get("email").and_then(Value::as_str).or_else(|| {
            profile
                .and_then(|value| value.get("email"))
                .and_then(Value::as_str)
        });
        let account_id = tokens
            .get("account_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let user_id = auth
            .and_then(|value| {
                value
                    .get("chatgpt_user_id")
                    .and_then(Value::as_str)
                    .or_else(|| value.get("user_id").and_then(Value::as_str))
            })
            .map(ToOwned::to_owned);
        let email = email.map(ToOwned::to_owned);
        let last_refresh = raw.get("last_refresh").and_then(Value::as_str);
        let refresh_ns = last_refresh
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .and_then(|value| value.timestamp_nanos_opt())
            .ok_or_else(|| Error::InvalidAuth(path.to_owned()))?;
        Ok(Self {
            path: path.to_owned(),
            raw,
            identity: Identity {
                email: email.ok_or_else(|| Error::InvalidAuth(path.to_owned()))?,
                account_id: account_id.ok_or_else(|| Error::InvalidAuth(path.to_owned()))?,
                user_id: user_id.ok_or_else(|| Error::InvalidAuth(path.to_owned()))?,
            },
            refresh_ns,
        })
    }

    pub fn same_credentials(&self, other: &Self) -> bool {
        self.raw.get("tokens") == other.raw.get("tokens")
    }
}

fn decode_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    #[test]
    fn decodes_namespaced_profile_email_and_identity() {
        let claims = serde_json::json!({
            "https://api.openai.com/profile": {"email": "one@example.com"},
            "https://api.openai.com/auth": {"chatgpt_user_id": "user-one"}
        });
        let token = format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        let claims = decode_claims(&token).unwrap();
        assert_eq!(
            claims["https://api.openai.com/profile"]["email"],
            "one@example.com"
        );
    }
}
