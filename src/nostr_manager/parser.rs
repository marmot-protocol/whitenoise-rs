//! This module contains the logic for parsing Nostr events into tokens.

use nostr_sdk::parser::{NostrParser, Token};
use serde::{Deserialize, Serialize};

use crate::nostr_manager::NostrManager;

/// Parser trait for parsing content into tokens
/// This trait is designed to be thread-safe for use with Flutter Rust Bridge (FRB)
pub trait Parser: Send + Sync {
    fn parse(
        &self,
        content: &str,
    ) -> Result<Vec<SerializableToken>, Box<dyn std::error::Error + Send + Sync>>;
}

/// Serializable Token
/// This is a parallel of the `Token` enum from the `nostr` crate, modified so that we can serialize it for use in commands and the DB
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SerializableToken {
    /// Nostr URI converted to a string
    Nostr(String),
    /// Url converted to a string
    Url(String),
    /// Hashtag
    Hashtag(String),
    /// Other text
    ///
    /// Spaces at the beginning or end of a text are parsed as [`Token::Whitespace`].
    Text(String),
    /// Line break
    LineBreak,
    /// A whitespace
    Whitespace,
}

// We use From instead of TryFrom because we want to show an error if the underlying token enum changes.
impl<'a> From<Token<'a>> for SerializableToken {
    fn from(value: Token<'a>) -> Self {
        match value {
            Token::Nostr(n) => SerializableToken::Nostr(match n.to_nostr_uri() {
                Ok(uri) => uri,
                Err(e) => {
                    // handle or return a default/fallback
                    format!("invalid_nostr_uri:{}", e)
                }
            }),
            Token::Url(u) => SerializableToken::Url(u.to_string()),
            Token::Hashtag(h) => SerializableToken::Hashtag(h.to_string()),
            Token::Text(t) => SerializableToken::Text(t.to_string()),
            Token::LineBreak => SerializableToken::LineBreak,
            Token::Whitespace => SerializableToken::Whitespace,
        }
    }
}

impl NostrManager {
    /// Parses a string into a vector of serializable tokens.
    ///
    /// This function takes a string content and returns a vector of `SerializableToken`s,
    /// which can be used for database storage or frontend communication.
    ///
    /// # Arguments
    /// * `content` - The string content to parse
    ///
    /// # Returns
    /// A vector of `SerializableToken`s representing the parsed content
    pub fn parse(&self, content: &str) -> Vec<SerializableToken> {
        let parser = NostrParser::new();
        parser.parse(content).map(SerializableToken::from).collect()
    }
}

impl Parser for NostrManager {
    fn parse(
        &self,
        content: &str,
    ) -> Result<Vec<SerializableToken>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(self.parse(content))
    }
}

#[cfg(test)]
pub struct MockParser;

#[cfg(test)]
impl MockParser {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(test)]
impl Parser for MockParser {
    fn parse(
        &self,
        content: &str,
    ) -> Result<Vec<SerializableToken>, Box<dyn std::error::Error + Send + Sync>> {
        // Simple mock that just treats everything as text for testing
        if content.is_empty() {
            Ok(vec![])
        } else {
            Ok(vec![SerializableToken::Text(content.to_string())])
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::whitenoise::event_tracker::NoEventTracker;
    use tokio::sync::mpsc;

    async fn setup_nostr_manager() -> NostrManager {
        let (event_sender, _event_receiver) = mpsc::channel(500);
        // Use NoEventTracker for parser tests since we don't need event tracking
        let event_tracker = Arc::new(NoEventTracker);
        NostrManager::new(event_sender, event_tracker)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_parse_basic_text() {
        let content = "Hello, world!";
        let nostr = setup_nostr_manager().await;
        let tokens = nostr.parse(content);
        assert_eq!(
            tokens,
            vec![SerializableToken::Text("Hello, world!".to_string())]
        );
    }

    #[tokio::test]
    async fn test_parse_with_whitespace() {
        let content = "  Hello  world  ";
        let nostr = setup_nostr_manager().await;
        let tokens = nostr.parse(content);
        assert_eq!(
            tokens,
            vec![
                SerializableToken::Whitespace,
                SerializableToken::Whitespace,
                SerializableToken::Text("Hello  world ".to_string()),
                SerializableToken::Whitespace,
            ]
        );
    }

    #[tokio::test]
    async fn test_parse_with_line_breaks() {
        let content = "Hello\nworld";
        let nostr = setup_nostr_manager().await;
        let tokens = nostr.parse(content);
        assert_eq!(
            tokens,
            vec![
                SerializableToken::Text("Hello".to_string()),
                SerializableToken::LineBreak,
                SerializableToken::Text("world".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn test_parse_with_hashtags() {
        let content = "Hello #nostr world";
        let nostr = setup_nostr_manager().await;
        let tokens = nostr.parse(content);
        assert_eq!(
            tokens,
            vec![
                SerializableToken::Text("Hello".to_string()),
                SerializableToken::Whitespace,
                SerializableToken::Hashtag("nostr".to_string()),
                SerializableToken::Whitespace,
                SerializableToken::Text("world".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn test_parse_with_nostr_uri() {
        let content =
            "Check out nostr:npub1l2vyh47mk2p0qlsku7hg0vn29faehy9hy34ygaclpn66ukqp3afqutajft";
        let nostr = setup_nostr_manager().await;
        let tokens = nostr.parse(content);
        assert_eq!(
            tokens,
            vec![
                SerializableToken::Text("Check out".to_string()),
                SerializableToken::Whitespace,
                SerializableToken::Nostr(
                    "nostr:npub1l2vyh47mk2p0qlsku7hg0vn29faehy9hy34ygaclpn66ukqp3afqutajft"
                        .to_string()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn test_parse_with_url() {
        let content = "Visit https://example.com";
        let nostr = setup_nostr_manager().await;
        let tokens = nostr.parse(content);
        assert_eq!(
            tokens,
            vec![
                SerializableToken::Text("Visit".to_string()),
                SerializableToken::Whitespace,
                SerializableToken::Url("https://example.com/".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn test_parse_empty_string() {
        let content = "";
        let nostr = setup_nostr_manager().await;
        let tokens = nostr.parse(content);
        assert_eq!(tokens, vec![]);
    }

    #[tokio::test]
    async fn test_parse_complex_content() {
        let content = "Hello #nostr! Check out https://example.com and nostr:npub1l2vyh47mk2p0qlsku7hg0vn29faehy9hy34ygaclpn66ukqp3afqutajft\n\nBye!";
        let nostr = setup_nostr_manager().await;
        let tokens = nostr.parse(content);
        assert_eq!(
            tokens,
            vec![
                SerializableToken::Text("Hello".to_string()),
                SerializableToken::Whitespace,
                SerializableToken::Hashtag("nostr".to_string()),
                SerializableToken::Text("! Check out".to_string()),
                SerializableToken::Whitespace,
                SerializableToken::Url("https://example.com/".to_string()),
                SerializableToken::Whitespace,
                SerializableToken::Text("and".to_string()),
                SerializableToken::Whitespace,
                SerializableToken::Nostr(
                    "nostr:npub1l2vyh47mk2p0qlsku7hg0vn29faehy9hy34ygaclpn66ukqp3afqutajft"
                        .to_string()
                ),
                SerializableToken::LineBreak,
                SerializableToken::LineBreak,
                SerializableToken::Text("Bye!".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn test_url_edge_cases() {
        let nostr = setup_nostr_manager().await;
        let test_cases = vec![
            (
                "https://example.com?param=value",
                vec![SerializableToken::Url(
                    "https://example.com/?param=value".to_string(),
                )],
            ),
            (
                "https://example.com#fragment",
                vec![SerializableToken::Url(
                    "https://example.com/#fragment".to_string(),
                )],
            ),
            (
                "https://example.com/path/to/resource",
                vec![SerializableToken::Url(
                    "https://example.com/path/to/resource".to_string(),
                )],
            ),
            (
                "not a valid url",
                vec![SerializableToken::Text("not a valid url".to_string())],
            ),
            (
                "https://example.com with text",
                vec![
                    SerializableToken::Url("https://example.com/".to_string()),
                    SerializableToken::Whitespace,
                    SerializableToken::Text("with text".to_string()),
                ],
            ),
        ];

        for (input, expected) in test_cases {
            let tokens = nostr.parse(input);
            assert_eq!(tokens, expected, "Failed for input: {}", input);
        }
    }

    #[tokio::test]
    async fn test_whitespace_edge_cases() {
        let nostr = setup_nostr_manager().await;
        let test_cases = vec![
            (
                "\t\t",
                vec![
                    SerializableToken::Text("\t\t".to_string()), // TODO: This should be updated upstream to handle tabs as whitespace
                ],
            ),
            (
                "  \t  ",
                vec![
                    SerializableToken::Whitespace,
                    SerializableToken::Whitespace,
                    SerializableToken::Text("\t ".to_string()), // TODO: This should be updated upstream to handle tabs as whitespace
                    SerializableToken::Whitespace,
                ],
            ),
            (
                "\n\t",
                vec![
                    SerializableToken::LineBreak,
                    SerializableToken::Text("\t".to_string()), // TODO: This should be updated upstream to handle tabs as whitespace
                ],
            ),
            (
                "text\ttext",
                vec![SerializableToken::Text("text\ttext".to_string())], // TODO: This should be updated upstream to handle tabs as whitespace
            ),
        ];

        for (input, expected) in test_cases {
            let tokens = nostr.parse(input);
            assert_eq!(tokens, expected, "Failed for input: {:?}", input);
        }
    }

    #[tokio::test]
    async fn test_text_edge_cases() {
        let nostr = setup_nostr_manager().await;
        let test_cases = vec![
            (
                "Hello, 世界!",
                vec![SerializableToken::Text("Hello, 世界!".to_string())],
            ),
            (
                "Text with emoji 😊",
                vec![SerializableToken::Text("Text with emoji 😊".to_string())],
            ),
            (
                "Text with \"quotes\"",
                vec![SerializableToken::Text("Text with \"quotes\"".to_string())],
            ),
            (
                "Text with \\escaped\\ chars",
                vec![SerializableToken::Text(
                    "Text with \\escaped\\ chars".to_string(),
                )],
            ),
        ];

        for (input, expected) in test_cases {
            let tokens = nostr.parse(input);
            assert_eq!(tokens, expected, "Failed for input: {}", input);
        }
    }

    #[tokio::test]
    async fn test_error_cases() {
        // Test with a very long string
        let long_string = "a".repeat(10000);
        let nostr = setup_nostr_manager().await;
        let tokens = nostr.parse(&long_string);
        assert!(!tokens.is_empty(), "Should handle long strings");

        // Test with a string containing null bytes
        let null_string = "text\0text";
        let tokens = nostr.parse(null_string);
        assert_eq!(
            tokens,
            vec![SerializableToken::Text("text\0text".to_string())],
            "Should handle null bytes"
        );

        // Test with invalid UTF-8 (this will panic if not handled properly)
        // let invalid_utf8 = unsafe { String::from_utf8_unchecked(vec![0xFF, 0xFF]) };
        // let tokens = nostr.parse(&invalid_utf8);
        // assert!(!tokens.is_empty(), "Should handle invalid UTF-8");
    }
}
