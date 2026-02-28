use serde::{Deserialize, Serialize};

/// A request from the CLI client to the daemon.
///
/// Each variant maps to one daemon method. The JSON wire format uses
/// `{"method": "variant_name", "params": {...}}` via serde's tagged enum.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "method", content = "params")]
pub enum Request {
    // Daemon management
    #[serde(rename = "ping")]
    Ping,

    // Identity & auth
    #[serde(rename = "create_identity")]
    CreateIdentity,
    #[serde(rename = "login_start")]
    LoginStart { nsec: String },
    #[serde(rename = "login_publish_default_relays")]
    LoginPublishDefaultRelays { pubkey: String },
    #[serde(rename = "login_with_custom_relay")]
    LoginWithCustomRelay { pubkey: String, relay_url: String },
    #[serde(rename = "login_cancel")]
    LoginCancel { pubkey: String },
    #[serde(rename = "logout")]
    Logout { pubkey: String },

    // Accounts
    #[serde(rename = "all_accounts")]
    AllAccounts,
    #[serde(rename = "export_nsec")]
    ExportNsec { pubkey: String },
}

/// A response from the daemon to the CLI client.
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub stream_end: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub message: String,
}

impl Response {
    pub fn ok(result: serde_json::Value) -> Self {
        Self {
            result: Some(result),
            error: None,
            stream_end: false,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            result: None,
            error: Some(ErrorPayload {
                message: message.into(),
            }),
            stream_end: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Unit variants must serialize without a "params" key.
    #[test]
    fn unit_variant_roundtrip() {
        let req = Request::Ping;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"method":"ping"}"#);

        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::Ping));
    }

    #[test]
    fn create_identity_roundtrip() {
        let req = Request::CreateIdentity;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"method":"create_identity"}"#);

        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::CreateIdentity));
    }

    #[test]
    fn login_start_roundtrip() {
        let req = Request::LoginStart {
            nsec: "nsec1abc".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::LoginStart { nsec } if nsec == "nsec1abc"));
    }

    #[test]
    fn login_with_custom_relay_roundtrip() {
        let req = Request::LoginWithCustomRelay {
            pubkey: "npub1xyz".into(),
            relay_url: "wss://relay.example.com".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(parsed, Request::LoginWithCustomRelay { pubkey, relay_url }
                if pubkey == "npub1xyz" && relay_url == "wss://relay.example.com")
        );
    }

    #[test]
    fn all_accounts_roundtrip() {
        let req = Request::AllAccounts;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"method":"all_accounts"}"#);

        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::AllAccounts));
    }

    #[test]
    fn logout_roundtrip() {
        let req = Request::Logout {
            pubkey: "npub1abc".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::Logout { pubkey } if pubkey == "npub1abc"));
    }

    #[test]
    fn export_nsec_roundtrip() {
        let req = Request::ExportNsec {
            pubkey: "npub1abc".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::ExportNsec { pubkey } if pubkey == "npub1abc"));
    }

    #[test]
    fn unknown_method_is_deser_error() {
        let json = r#"{"method":"nonexistent"}"#;
        assert!(serde_json::from_str::<Request>(json).is_err());
    }

    #[test]
    fn response_ok_skips_error_and_stream_end() {
        let resp = Response::ok(json!({"npub": "npub1abc"}));
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("error").is_none());
        assert!(parsed.get("stream_end").is_none());
        assert_eq!(parsed["result"]["npub"], "npub1abc");
    }

    #[test]
    fn response_err_skips_result_and_stream_end() {
        let resp = Response::err("something went wrong");
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("result").is_none());
        assert!(parsed.get("stream_end").is_none());
        assert_eq!(parsed["error"]["message"], "something went wrong");
    }

    /// Verify that JSON sent from the wire (e.g. via socat) deserializes correctly.
    #[test]
    fn wire_format_compat() {
        let wire = r#"{"method":"login_start","params":{"nsec":"nsec1test"}}"#;
        let parsed: Request = serde_json::from_str(wire).unwrap();
        assert!(matches!(parsed, Request::LoginStart { nsec } if nsec == "nsec1test"));
    }
}
