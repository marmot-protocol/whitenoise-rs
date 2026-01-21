use chrono::{DateTime, Utc};
use nostr_sdk::{PublicKey, Timestamp, ToBech32};

use crate::whitenoise::{Whitenoise, error::WhitenoiseError};

impl Whitenoise {
    /// Converts a Nostr public key to its bech32-encoded npub representation.
    ///
    /// # Arguments
    ///
    /// * `public_key` - A reference to the `PublicKey` to convert
    pub fn npub_from_public_key(public_key: &PublicKey) -> Result<String, WhitenoiseError> {
        public_key
            .to_bech32()
            .map_err(|_| WhitenoiseError::InvalidPublicKey)
    }

    /// Converts a hex-encoded public key string to its bech32-encoded npub representation.
    ///
    /// This is a convenience method that first parses the hex string into a `PublicKey`
    /// and then converts it to the npub format.
    ///
    /// # Arguments
    ///
    /// * `hex_pubkey` - A hex-encoded public key string (64 characters)
    pub fn npub_from_hex_pubkey(hex_pubkey: &str) -> Result<String, WhitenoiseError> {
        let public_key =
            PublicKey::parse(hex_pubkey).map_err(|_| WhitenoiseError::InvalidPublicKey)?;
        Self::npub_from_public_key(&public_key)
    }

    /// Converts a bech32-encoded npub string to its hex representation.
    ///
    /// This method parses an npub string and returns the underlying public key
    /// as a hex-encoded string.
    ///
    /// # Arguments
    ///
    /// * `npub` - A bech32-encoded npub string (starts with "npub1")
    pub fn hex_pubkey_from_npub(npub: &str) -> Result<String, WhitenoiseError> {
        let public_key = PublicKey::parse(npub).map_err(|_| WhitenoiseError::InvalidPublicKey)?;
        Ok(public_key.to_hex())
    }
}

/// Converts a Nostr timestamp to a DateTime<Utc> with proper error handling.
///
/// Nostr event timestamps are in seconds since Unix epoch. This function handles
/// the conversion safely to prevent overflow issues that could lead to incorrect timestamps.
///
/// # Arguments
/// * `timestamp` - The Nostr timestamp to convert
///
/// # Returns
/// * `Ok(DateTime<Utc>)` if the timestamp is valid
/// * `Err(WhitenoiseError)` if the timestamp is invalid or would overflow
pub(crate) fn timestamp_to_datetime(
    timestamp: Timestamp,
) -> Result<DateTime<Utc>, WhitenoiseError> {
    let timestamp_secs = timestamp.as_u64();

    // Check if timestamp fits in i64
    if timestamp_secs > i64::MAX as u64 {
        return Err(WhitenoiseError::InvalidTimestamp);
    }

    DateTime::from_timestamp(timestamp_secs as i64, 0)
        .ok_or_else(|| WhitenoiseError::InvalidTimestamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

    #[test]
    fn test_hex_pubkey_from_npub() {
        let npub = "npub1jgm0ntzjr03wuzj5788llhed7l6fst05um4ej2r86ueaa08etv6sgd669p";
        let result = Whitenoise::hex_pubkey_from_npub(npub);

        assert!(result.is_ok());
        let hex = result.unwrap();
        assert_eq!(
            hex,
            "9236f9ac521be2ee0a54f1cfffdf2df7f4982df4e6eb992867d733debcf95b35"
        );

        let invalid_npub = "definitely not a valid npub";
        let invalid_result = Whitenoise::hex_pubkey_from_npub(invalid_npub);
        assert!(invalid_result.is_err());

        let empty_npub = "";
        let empty_result = Whitenoise::hex_pubkey_from_npub(empty_npub);
        assert!(empty_result.is_err());
    }

    #[test]
    fn test_npub_from_hex_pubkey() {
        let hex_pubkey = "9236f9ac521be2ee0a54f1cfffdf2df7f4982df4e6eb992867d733debcf95b35";
        let result = Whitenoise::npub_from_hex_pubkey(hex_pubkey);

        assert!(result.is_ok());
        let npub = result.unwrap();
        assert_eq!(
            npub,
            "npub1jgm0ntzjr03wuzj5788llhed7l6fst05um4ej2r86ueaa08etv6sgd669p"
        );

        let invalid_hex = "invalid_hex_string";
        let invalid_result = Whitenoise::npub_from_hex_pubkey(invalid_hex);
        assert!(invalid_result.is_err());

        let empty_hex = "";
        let empty_result = Whitenoise::npub_from_hex_pubkey(empty_hex);
        assert!(empty_result.is_err());
    }

    #[test]
    fn test_npub_from_public_key() {
        let hex_pubkey = "9236f9ac521be2ee0a54f1cfffdf2df7f4982df4e6eb992867d733debcf95b35";
        let public_key = PublicKey::parse(hex_pubkey).unwrap();
        let result = Whitenoise::npub_from_public_key(&public_key);

        assert!(result.is_ok());
        let npub = result.unwrap();
        assert_eq!(
            npub,
            "npub1jgm0ntzjr03wuzj5788llhed7l6fst05um4ej2r86ueaa08etv6sgd669p"
        );
    }

    #[test]
    fn test_timestamp_to_datetime() {
        // Test valid timestamp
        let timestamp = Timestamp::from(1609459200); // 2021-01-01 00:00:00 UTC
        let result = timestamp_to_datetime(timestamp);
        assert!(result.is_ok());

        let datetime = result.unwrap();
        assert_eq!(datetime.timestamp(), 1609459200);

        // Test that the year is correct (2021)
        assert_eq!(datetime.format("%Y").to_string(), "2021");
        assert_eq!(datetime.format("%m-%d").to_string(), "01-01");
    }

    #[test]
    fn test_timestamp_to_datetime_current_time() {
        // Test with current timestamp
        let now = Timestamp::now();
        let result = timestamp_to_datetime(now);
        assert!(result.is_ok());

        let datetime = result.unwrap();
        // Should be close to current time (within reasonable bounds)
        let current_year = chrono::Utc::now().year();
        assert_eq!(datetime.year(), current_year);
    }

    #[test]
    fn test_timestamp_to_datetime_overflow() {
        // Test that very large timestamps that would overflow i64 are rejected

        // Test with a reasonable future timestamp - year 2100
        let year_2100_timestamp = Timestamp::from(4102444800u64); // Jan 1, 2100
        let result = timestamp_to_datetime(year_2100_timestamp);
        assert!(result.is_ok());

        // Test with u64::MAX (definitely should fail our overflow check)
        let max_timestamp = Timestamp::from(u64::MAX);
        let result = timestamp_to_datetime(max_timestamp);
        assert!(result.is_err());

        // Test with a value just over i64::MAX
        let just_over_i64_max = Timestamp::from((i64::MAX as u64) + 1);
        let result = timestamp_to_datetime(just_over_i64_max);
        assert!(result.is_err());
    }

    #[test]
    fn test_timestamp_to_datetime_with_nostr_timestamp() {
        // Test the main function with a Nostr Timestamp
        let timestamp = Timestamp::from(1609459200); // 2021-01-01 00:00:00 UTC
        let result = timestamp_to_datetime(timestamp);
        assert!(result.is_ok());

        let datetime = result.unwrap();
        assert_eq!(datetime.timestamp(), 1609459200);
        assert_eq!(datetime.format("%Y").to_string(), "2021");
        assert_eq!(datetime.format("%m-%d").to_string(), "01-01");
    }
}
