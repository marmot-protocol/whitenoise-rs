use std::sync::LazyLock;

use async_trait::async_trait;
use reqwest::Url;

use super::AptabaseAnalyticsConfig;
use super::PRODUCT_ANALYTICS_MAX_BATCH_SIZE;
use super::client::ProductAnalyticsClient;
use super::worker::PreparedProductAnalyticsEvent;
use crate::whitenoise::Result;
use crate::whitenoise::error::WhitenoiseError;

static ANALYTICS_HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    reqwest::Client::builder()
        .use_preconfigured_tls(tls_config)
        .build()
        .expect("Failed to build analytics HTTP client")
});

pub(crate) struct AptabaseProductAnalyticsClient {
    app_key: String,
    endpoint: Url,
}

impl AptabaseProductAnalyticsClient {
    pub(crate) fn new(config: &AptabaseAnalyticsConfig) -> Self {
        let mut endpoint = parse_host(&config.host).expect("analytics host must be validated");
        endpoint.set_path("/api/v0/events");
        Self {
            app_key: config.app_key.clone(),
            endpoint,
        }
    }
}

#[async_trait]
impl ProductAnalyticsClient for AptabaseProductAnalyticsClient {
    async fn send_events(&self, events: &[PreparedProductAnalyticsEvent]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        if events.len() > PRODUCT_ANALYTICS_MAX_BATCH_SIZE {
            return Err(WhitenoiseError::ProductAnalytics(format!(
                "Aptabase batch exceeds max size of {PRODUCT_ANALYTICS_MAX_BATCH_SIZE}"
            )));
        }

        let response = ANALYTICS_HTTP_CLIENT
            .post(self.endpoint.clone())
            .header("App-Key", &self.app_key)
            .json(events)
            .send()
            .await
            .map_err(|e| {
                WhitenoiseError::ProductAnalytics(format!("Aptabase request failed: {e}"))
            })?;

        if !response.status().is_success() {
            return Err(WhitenoiseError::ProductAnalytics(format!(
                "Aptabase request failed with status {}",
                response.status()
            )));
        }

        Ok(())
    }
}

pub(crate) fn validate_host(host: &str) -> Result<()> {
    let _ = parse_host(host)?;
    Ok(())
}

fn parse_host(host: &str) -> Result<Url> {
    let url = Url::parse(host)
        .map_err(|e| WhitenoiseError::ProductAnalytics(format!("Aptabase host is invalid: {e}")))?;

    if url.username() != "" || url.password().is_some() {
        return Err(WhitenoiseError::ProductAnalytics(
            "Aptabase host must not contain credentials".to_string(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(WhitenoiseError::ProductAnalytics(
            "Aptabase host must not contain query or fragment".to_string(),
        ));
    }
    if !matches!(url.path(), "" | "/") {
        return Err(WhitenoiseError::ProductAnalytics(
            "Aptabase host must not contain a path".to_string(),
        ));
    }

    match url.scheme() {
        "https" => Ok(url),
        "http" if is_debug_loopback_host(&url) => Ok(url),
        _ => Err(WhitenoiseError::ProductAnalytics(
            "Aptabase host must use https".to_string(),
        )),
    }
}

fn is_debug_loopback_host(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };

    let is_loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
    is_loopback && (cfg!(debug_assertions) || cfg!(test))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use mockito::Matcher;
    use serde_json::{Map, Value};

    use super::*;
    use crate::whitenoise::product_analytics::worker::SystemProps;

    fn event() -> PreparedProductAnalyticsEvent {
        let mut props = Map::new();
        props.insert("schema_version".to_string(), Value::from(1));
        PreparedProductAnalyticsEvent {
            timestamp: Utc::now(),
            session_id: "171351624706652714".to_string(),
            event_name: "app_started".to_string(),
            system_props: SystemProps {
                locale: "en-US".to_string(),
                os_name: "iOS".to_string(),
                is_debug: true,
                bundle_identifier: "dev.ipf.whitenoise.staging".to_string(),
                device_class: "phone".to_string(),
                app_version: "1.0.0".to_string(),
                sdk_version: "whitenoise-rs@test".to_string(),
            },
            props,
        }
    }

    #[tokio::test]
    async fn sends_json_events_with_app_key_header() {
        let mut server = mockito::Server::new_async().await;
        let event = event();
        let mock = server
            .mock("POST", "/api/v0/events")
            .match_header("app-key", "A-TEST")
            .match_header(
                "content-type",
                Matcher::Regex("application/json.*".to_string()),
            )
            .match_body(Matcher::JsonString(
                serde_json::to_string(&vec![event.clone()]).unwrap(),
            ))
            .with_status(202)
            .create_async()
            .await;
        let client = AptabaseProductAnalyticsClient::new(&AptabaseAnalyticsConfig {
            app_key: "A-TEST".to_string(),
            host: server.url(),
        });

        client.send_events(&[event]).await.unwrap();

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn send_events_accepts_empty_batch_without_network_request() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/v0/events")
            .with_status(500)
            .create_async()
            .await;
        let client = AptabaseProductAnalyticsClient::new(&AptabaseAnalyticsConfig {
            app_key: "A-TEST".to_string(),
            host: server.url(),
        });

        client.send_events(&[]).await.unwrap();

        mock.expect(0).assert_async().await;
    }

    #[tokio::test]
    async fn send_events_rejects_oversized_batch_before_network_request() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/v0/events")
            .with_status(202)
            .create_async()
            .await;
        let client = AptabaseProductAnalyticsClient::new(&AptabaseAnalyticsConfig {
            app_key: "A-TEST".to_string(),
            host: server.url(),
        });
        let events = vec![event(); PRODUCT_ANALYTICS_MAX_BATCH_SIZE + 1];

        assert!(client.send_events(&events).await.is_err());

        mock.expect(0).assert_async().await;
    }

    #[tokio::test]
    async fn send_events_reports_non_success_status() {
        let mut server = mockito::Server::new_async().await;
        let event = event();
        let mock = server
            .mock("POST", "/api/v0/events")
            .with_status(503)
            .create_async()
            .await;
        let client = AptabaseProductAnalyticsClient::new(&AptabaseAnalyticsConfig {
            app_key: "A-TEST".to_string(),
            host: server.url(),
        });

        assert!(client.send_events(&[event]).await.is_err());

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn send_events_reports_transport_failure() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let client = AptabaseProductAnalyticsClient::new(&AptabaseAnalyticsConfig {
            app_key: "A-TEST".to_string(),
            host,
        });

        assert!(client.send_events(&[event()]).await.is_err());
    }

    #[test]
    fn validates_host_shape() {
        assert!(validate_host("https://analytics.example.com").is_ok());
        assert!(validate_host("http://127.0.0.1:1234").is_ok());
        assert!(validate_host("https://analytics.example.com/api").is_err());
        assert!(validate_host("https://user:pass@analytics.example.com").is_err());
        assert!(validate_host("https://analytics.example.com?debug=true").is_err());
        assert!(validate_host("https://analytics.example.com/#events").is_err());
        assert!(validate_host("http://analytics.example.com").is_err());
    }
}
