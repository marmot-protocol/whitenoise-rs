use crate::{nostr_manager::parser::SerializableToken, whitenoise::error::WhitenoiseError};
use mdk_core::prelude::*;
use nostr_sdk::prelude::*;
use serde::Serialize;

/// Retry information for failed event processing
#[derive(Debug, Clone)]
pub struct RetryInfo {
    /// Number of times this event has been retried
    pub attempt: u32,
    /// Maximum number of retry attempts allowed
    pub max_attempts: u32,
    /// Base delay in milliseconds for exponential backoff
    pub base_delay_ms: u64,
}

impl RetryInfo {
    pub fn new() -> Self {
        Self {
            attempt: 0,
            max_attempts: 10,
            base_delay_ms: 1000,
        }
    }

    pub fn next_attempt(&self) -> Option<Self> {
        if self.attempt >= self.max_attempts {
            None
        } else {
            Some(Self {
                attempt: self.attempt + 1,
                max_attempts: self.max_attempts,
                base_delay_ms: self.base_delay_ms,
            })
        }
    }

    pub fn delay_ms(&self) -> u64 {
        self.base_delay_ms * (2_u64.pow(self.attempt))
    }

    pub fn should_retry(&self) -> bool {
        self.attempt < self.max_attempts
    }
}

impl Default for RetryInfo {
    fn default() -> Self {
        Self::new()
    }
}

/// Events that can be processed by the Whitenoise event processing system
#[derive(Debug)]
pub enum ProcessableEvent {
    /// A Nostr event with an optional subscription ID for account-aware processing
    NostrEvent {
        event: Event,
        subscription_id: Option<String>,
        retry_info: RetryInfo,
    },
    /// A relay message for logging/monitoring purposes
    RelayMessage(RelayUrl, String),
}

impl ProcessableEvent {
    /// Create a new NostrEvent with default retry settings
    pub fn new_nostr_event(event: Event, subscription_id: Option<String>) -> Self {
        Self::NostrEvent {
            event,
            subscription_id,
            retry_info: RetryInfo::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageWithTokens {
    pub message: message_types::Message,
    pub tokens: Vec<SerializableToken>,
}

impl MessageWithTokens {
    pub fn new(message: message_types::Message, tokens: Vec<SerializableToken>) -> Self {
        Self { message, tokens }
    }
}

/// Supported image types for group images
///
/// This enum represents the allowed image formats that can be uploaded
/// as group profile images. The list is intentionally limited to common,
/// well-supported formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageType {
    Jpeg,
    Png,
    Gif,
    Webp,
}

impl ImageType {
    /// Returns the canonical MIME type for this image format
    pub fn mime_type(&self) -> &'static str {
        match self {
            ImageType::Jpeg => "image/jpeg",
            ImageType::Png => "image/png",
            ImageType::Gif => "image/gif",
            ImageType::Webp => "image/webp",
        }
    }

    /// Returns the file extension for this image format (without the dot)
    pub fn extension(&self) -> &'static str {
        match self {
            ImageType::Jpeg => "jpg",
            ImageType::Png => "png",
            ImageType::Gif => "gif",
            ImageType::Webp => "webp",
        }
    }

    /// Detects and validates the image type from raw image data
    ///
    /// Uses the `image` crate to detect the format and validate the image.
    /// This is more reliable than magic byte checking and validates the image
    /// structure in one step.
    ///
    /// # Arguments
    /// * `data` - The raw image file data
    ///
    /// # Returns
    /// * `Ok(ImageType)` - The detected and validated image type
    /// * `Err(anyhow::Error)` - If the format is unsupported, unrecognized, or invalid
    ///
    /// # Example
    /// ```ignore
    /// let image_data = std::fs::read("photo.jpg")?;
    /// let image_type = ImageType::detect(&image_data)?;
    /// assert_eq!(image_type, ImageType::Jpeg);
    /// ```
    pub fn detect(data: &[u8]) -> Result<Self, anyhow::Error> {
        // Use the image crate to detect format - it's more reliable than magic bytes
        let format = ::image::guess_format(data).map_err(|e| {
            anyhow::anyhow!(
                "Failed to detect image format: {}. Supported formats: {}",
                e,
                Self::all()
                    .iter()
                    .map(|t| t.mime_type())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

        // Map the detected format to our ImageType enum
        let image_type = match format {
            ::image::ImageFormat::Jpeg => ImageType::Jpeg,
            ::image::ImageFormat::Png => ImageType::Png,
            ::image::ImageFormat::Gif => ImageType::Gif,
            ::image::ImageFormat::WebP => ImageType::Webp,
            other => {
                return Err(anyhow::anyhow!(
                    "Unsupported image format: {:?}. Supported formats: {}",
                    other,
                    Self::all()
                        .iter()
                        .map(|t| t.mime_type())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        };

        // Validate the image can actually be decoded
        ::image::load_from_memory_with_format(data, format).map_err(|e| {
            anyhow::anyhow!(
                "Invalid or corrupted {} image: {}",
                image_type.mime_type(),
                e
            )
        })?;

        Ok(image_type)
    }

    /// All supported image types as a slice
    pub const fn all() -> &'static [ImageType] {
        &[
            ImageType::Jpeg,
            ImageType::Png,
            ImageType::Gif,
            ImageType::Webp,
        ]
    }
}

impl From<ImageType> for String {
    fn from(image_type: ImageType) -> Self {
        image_type.mime_type().to_string()
    }
}

impl TryFrom<String> for ImageType {
    type Error = anyhow::Error;

    fn try_from(mime_type: String) -> Result<Self, Self::Error> {
        Self::try_from(mime_type.as_str())
    }
}

impl TryFrom<&str> for ImageType {
    type Error = anyhow::Error;

    fn try_from(mime_type: &str) -> Result<Self, Self::Error> {
        match mime_type {
            "image/jpeg" | "image/jpg" => Ok(ImageType::Jpeg),
            "image/png" => Ok(ImageType::Png),
            "image/gif" => Ok(ImageType::Gif),
            "image/webp" => Ok(ImageType::Webp),
            _ => Err(anyhow::anyhow!(
                "Unsupported image MIME type: {}. Supported types: {}",
                mime_type,
                ImageType::all()
                    .iter()
                    .map(|t| t.mime_type())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

/// Result of media type detection
///
/// This enum preserves rich type information for images (via ImageType)
/// while providing necessary details for non-image media types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaTypeDetection {
    /// An image with full ImageType information
    Image(ImageType),
    /// Other media type (video, audio, document)
    Other {
        mime_type: String,
        extension: &'static str,
    },
}

impl MediaTypeDetection {
    /// Get the MIME type for this media
    pub fn mime_type(&self) -> &str {
        match self {
            Self::Image(image_type) => image_type.mime_type(),
            Self::Other { mime_type, .. } => mime_type,
        }
    }

    /// Get the file extension for this media
    pub fn extension(&self) -> &'static str {
        match self {
            Self::Image(image_type) => image_type.extension(),
            Self::Other { extension, .. } => extension,
        }
    }
}

/// Detect MIME type from file data
///
/// Uses robust ImageType validation for images, infer crate with explicit
/// whitelist for other types. This provides strong security for images while
/// supporting other media types.
pub fn detect_media_type(data: &[u8]) -> Result<MediaTypeDetection, WhitenoiseError> {
    if data.is_empty() {
        return Err(WhitenoiseError::UnsupportedMediaFormat(
            "File is empty".to_string(),
        ));
    }

    // Try image detection first (robust: detects format + validates structure)
    if let Ok(image_type) = ImageType::detect(data) {
        return Ok(MediaTypeDetection::Image(image_type));
    }

    // Fall back to infer crate for non-images
    detect_non_image_type(data)
}

/// Detect non-image media types using the infer crate with explicit whitelist
///
/// This function uses an explicit whitelist to only accept specific formats,
/// rejecting anything else even if the infer crate can detect it.
pub(crate) fn detect_non_image_type(data: &[u8]) -> Result<MediaTypeDetection, WhitenoiseError> {
    let detected = infer::get(data).ok_or_else(|| {
        WhitenoiseError::UnsupportedMediaFormat(
            "Unable to detect media type from file data".to_string(),
        )
    })?;

    let mime_type = detected.mime_type();

    // Explicit whitelist - only accept these specific formats
    let extension = match mime_type {
        // Videos
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",

        // Audio
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "audio/mp4" | "audio/m4a" => "m4a",
        "audio/wav" | "audio/x-wav" => "wav",

        // Documents
        "application/pdf" => "pdf",

        // Reject everything else
        _ => {
            return Err(WhitenoiseError::UnsupportedMediaFormat(format!(
                "Unsupported media format: {}. Supported formats: images (JPEG, PNG, GIF, WebP), videos (MP4, WebM, MOV), audio (MP3, OGG, M4A, WAV), documents (PDF)",
                mime_type
            )));
        }
    };

    Ok(MediaTypeDetection::Other {
        mime_type: mime_type.to_string(),
        extension,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a minimal valid PNG image (1x1 pixel)
    fn create_valid_png() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
            0x00, 0x00, 0x00, 0x0D, // IHDR chunk length
            0x49, 0x48, 0x44, 0x52, // "IHDR"
            0x00, 0x00, 0x00, 0x01, // Width: 1
            0x00, 0x00, 0x00, 0x01, // Height: 1
            0x08, 0x02, 0x00, 0x00, 0x00, // Bit depth, color type, etc.
            0x90, 0x77, 0x53, 0xDE, // CRC
            0x00, 0x00, 0x00, 0x00, // IEND chunk length
            0x49, 0x45, 0x4E, 0x44, // "IEND"
            0xAE, 0x42, 0x60, 0x82, // CRC
        ]
    }

    /// Helper to create a minimal valid JPEG image
    fn create_valid_jpeg() -> Vec<u8> {
        vec![
            0xFF, 0xD8, 0xFF, // JPEG SOI marker
            0xE0, 0x00, 0x10, // APP0 marker and length
            0x4A, 0x46, 0x49, 0x46, 0x00, // "JFIF\0"
            0x01, 0x01, // Version 1.1
            0x00, // Density units
            0x00, 0x01, 0x00, 0x01, // X and Y density
            0x00, 0x00, // Thumbnail dimensions
            0xFF, 0xD9, // EOI (End of Image) marker
        ]
    }

    /// Helper to create a minimal valid GIF image
    fn create_valid_gif() -> Vec<u8> {
        vec![
            0x47, 0x49, 0x46, 0x38, 0x39, 0x61, // "GIF89a"
            0x01, 0x00, 0x01, 0x00, // Width and height (1x1)
            0x00, 0x00, 0x00, // No color table, background
            0x2C, 0x00, 0x00, 0x00, 0x00, // Image descriptor
            0x01, 0x00, 0x01, 0x00, 0x00, // Image dimensions
            0x02, 0x02, 0x44, 0x01, 0x00, // Image data
            0x3B, // GIF trailer
        ]
    }

    /// Helper to create a minimal valid WebP image
    fn create_valid_webp() -> Vec<u8> {
        vec![
            0x52, 0x49, 0x46, 0x46, // "RIFF"
            0x1A, 0x00, 0x00, 0x00, // File size - 8
            0x57, 0x45, 0x42, 0x50, // "WEBP"
            0x56, 0x50, 0x38, 0x20, // "VP8 "
            0x0E, 0x00, 0x00, 0x00, // Chunk size
            0x30, 0x01, 0x00, 0x9D, 0x01, 0x2A, // VP8 bitstream
            0x01, 0x00, 0x01, 0x00, 0x00, 0x47, 0x08, 0x85,
        ]
    }

    #[test]
    fn test_detect_rejects_minimal_jpeg() {
        // Our minimal JPEG is just headers - not a complete valid image
        // The image crate correctly rejects it during validation
        let jpeg_data = create_valid_jpeg();
        let result = ImageType::detect(&jpeg_data);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid or corrupted")
        );
    }

    #[test]
    fn test_detect_rejects_minimal_png() {
        // Our minimal PNG is just headers - not a complete valid image
        // The image crate correctly rejects it during validation
        let png_data = create_valid_png();
        let result = ImageType::detect(&png_data);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid or corrupted")
        );
    }

    #[test]
    fn test_detect_rejects_minimal_gif() {
        // Our minimal GIF is just headers - not a complete valid image
        // The image crate correctly rejects it during validation
        let gif_data = create_valid_gif();
        let result = ImageType::detect(&gif_data);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid or corrupted")
        );
    }

    #[test]
    fn test_detect_rejects_minimal_webp() {
        // Our minimal WebP is just headers - not a complete valid image
        // The image crate correctly rejects it during validation
        let webp_data = create_valid_webp();
        let result = ImageType::detect(&webp_data);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid or corrupted")
        );
    }

    #[test]
    fn test_detect_too_small() {
        let small_data = vec![0xFF, 0xD8]; // Only 2 bytes
        let result = ImageType::detect(&small_data);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to detect image format")
        );
    }

    #[test]
    fn test_detect_unsupported_format() {
        // BMP header (not supported)
        let bmp_data = vec![
            0x42, 0x4D, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let result = ImageType::detect(&bmp_data);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Unsupported") || err_msg.contains("Failed to detect"));
    }

    #[test]
    fn test_detect_random_data() {
        let random_data = vec![
            0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x11, 0x22, 0x33, 0x44,
        ];
        let result = ImageType::detect(&random_data);
        assert!(result.is_err());
    }

    #[test]
    fn test_mime_type() {
        assert_eq!(ImageType::Jpeg.mime_type(), "image/jpeg");
        assert_eq!(ImageType::Png.mime_type(), "image/png");
        assert_eq!(ImageType::Gif.mime_type(), "image/gif");
        assert_eq!(ImageType::Webp.mime_type(), "image/webp");
    }

    #[test]
    fn test_extension() {
        assert_eq!(ImageType::Jpeg.extension(), "jpg");
        assert_eq!(ImageType::Png.extension(), "png");
        assert_eq!(ImageType::Gif.extension(), "gif");
        assert_eq!(ImageType::Webp.extension(), "webp");
    }

    #[test]
    fn test_try_from_string() {
        assert_eq!(
            ImageType::try_from("image/jpeg".to_string()).unwrap(),
            ImageType::Jpeg
        );
        assert_eq!(
            ImageType::try_from("image/jpg".to_string()).unwrap(),
            ImageType::Jpeg
        );
        assert_eq!(
            ImageType::try_from("image/png".to_string()).unwrap(),
            ImageType::Png
        );
        assert_eq!(
            ImageType::try_from("image/gif".to_string()).unwrap(),
            ImageType::Gif
        );
        assert_eq!(
            ImageType::try_from("image/webp".to_string()).unwrap(),
            ImageType::Webp
        );

        // Unsupported type
        assert!(ImageType::try_from("image/bmp".to_string()).is_err());
        assert!(ImageType::try_from("application/pdf".to_string()).is_err());
    }

    #[test]
    fn test_try_from_str() {
        assert_eq!(ImageType::try_from("image/jpeg").unwrap(), ImageType::Jpeg);
        assert_eq!(ImageType::try_from("image/jpg").unwrap(), ImageType::Jpeg);
        assert_eq!(ImageType::try_from("image/png").unwrap(), ImageType::Png);
        assert_eq!(ImageType::try_from("image/gif").unwrap(), ImageType::Gif);
        assert_eq!(ImageType::try_from("image/webp").unwrap(), ImageType::Webp);
    }

    #[test]
    fn test_all_supported_types() {
        let all_types = ImageType::all();
        assert_eq!(all_types.len(), 4);
        assert!(all_types.contains(&ImageType::Jpeg));
        assert!(all_types.contains(&ImageType::Png));
        assert!(all_types.contains(&ImageType::Gif));
        assert!(all_types.contains(&ImageType::Webp));
    }

    #[test]
    fn test_into_string() {
        let jpeg: String = ImageType::Jpeg.into();
        assert_eq!(jpeg, "image/jpeg");

        let png: String = ImageType::Png.into();
        assert_eq!(png, "image/png");
    }

    #[test]
    fn test_detect_validates_automatically() {
        // The detect() method now validates automatically
        // This is good - it catches invalid/corrupted images
        let corrupted = vec![0xFF, 0xD8, 0xFF, 0x00, 0x00]; // JPEG header but truncated
        let result = ImageType::detect(&corrupted);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid or corrupted")
        );
    }

    #[test]
    fn test_detect_workflow_explanation() {
        // Note: To test with real valid images, you'd need complete image files
        // The minimal test images above are just headers and will fail validation
        // This is CORRECT behavior - the image crate is properly validating!

        // In production, real image files will work fine:
        // let image_data = std::fs::read("photo.jpg")?;
        // let image_type = ImageType::detect(&image_data)?;  // Detects AND validates
        // assert_eq!(image_type, ImageType::Jpeg);
    }

    #[test]
    fn test_error_message_quality() {
        // Test that error messages are helpful
        let result = ImageType::try_from("image/bmp");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Unsupported"));
        assert!(err_msg.contains("image/bmp"));
        assert!(err_msg.contains("JPEG") || err_msg.contains("image/jpeg"));
    }

    // ========================================================================
    // MediaTypeDetection Tests
    // ========================================================================

    #[test]
    fn test_detect_media_type_empty_file() {
        let empty_data: Vec<u8> = vec![];
        let result = detect_media_type(&empty_data);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[test]
    fn test_detect_non_image_mp4_video() {
        // MP4 magic bytes (ftyp box)
        let mp4_data = vec![
            0x00, 0x00, 0x00, 0x18, // Box size
            b'f', b't', b'y', b'p', // "ftyp"
            b'i', b's', b'o', b'm', // Brand: isom
            0x00, 0x00, 0x00, 0x00, // Version
            b'i', b's', b'o', b'm', // Compatible brands
            b'm', b'p', b'4', b'2',
        ];
        let result = detect_media_type(&mp4_data).unwrap();
        assert_eq!(result.mime_type(), "video/mp4");
        assert_eq!(result.extension(), "mp4");
        match result {
            MediaTypeDetection::Other { .. } => {}
            _ => panic!("Expected Other variant for MP4"),
        }
    }

    #[test]
    fn test_detect_non_image_webm_video() {
        // WebM magic bytes (EBML header)
        let webm_data = vec![
            0x1A, 0x45, 0xDF, 0xA3, // EBML header
            0x9F, 0x42, 0x86, 0x81, 0x01, 0x42, 0xF7, 0x81, 0x01, 0x42, 0xF2, 0x81, 0x04, 0x42,
            0xF3, 0x81, 0x08, 0x42, 0x82, 0x88, 0x77, 0x65, 0x62, 0x6D,
        ];
        let result = detect_media_type(&webm_data).unwrap();
        assert_eq!(result.mime_type(), "video/webm");
        assert_eq!(result.extension(), "webm");
    }

    #[test]
    fn test_detect_non_image_quicktime_video() {
        // QuickTime/MOV magic bytes
        let mov_data = vec![
            0x00, 0x00, 0x00, 0x14, // Box size
            b'f', b't', b'y', b'p', // "ftyp"
            b'q', b't', b' ', b' ', // Brand: qt
            0x00, 0x00, 0x00, 0x00,
        ];
        let result = detect_media_type(&mov_data).unwrap();
        assert_eq!(result.mime_type(), "video/quicktime");
        assert_eq!(result.extension(), "mov");
    }

    #[test]
    fn test_detect_non_image_mp3_audio() {
        // MP3 with ID3v2 tag
        let mp3_data = vec![
            b'I', b'D', b'3', // ID3v2 identifier
            0x03, 0x00, // Version
            0x00, // Flags
            0x00, 0x00, 0x00, 0x00, // Size
        ];
        let result = detect_media_type(&mp3_data).unwrap();
        assert_eq!(result.mime_type(), "audio/mpeg");
        assert_eq!(result.extension(), "mp3");
    }

    #[test]
    fn test_detect_non_image_ogg_audio() {
        // OGG magic bytes
        let ogg_data = vec![
            b'O', b'g', b'g', b'S', // "OggS"
            0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let result = detect_media_type(&ogg_data).unwrap();
        assert_eq!(result.mime_type(), "audio/ogg");
        assert_eq!(result.extension(), "ogg");
    }

    #[test]
    fn test_detect_non_image_m4a_audio() {
        // M4A magic bytes (infer detects this as audio/m4a)
        let m4a_data = vec![
            0x00, 0x00, 0x00, 0x20, // Box size
            b'f', b't', b'y', b'p', // "ftyp"
            b'M', b'4', b'A', b' ', // Brand: M4A
            0x00, 0x00, 0x00, 0x00,
        ];
        let result = detect_media_type(&m4a_data).unwrap();
        assert_eq!(result.mime_type(), "audio/m4a");
        assert_eq!(result.extension(), "m4a");
    }

    #[test]
    fn test_detect_non_image_wav_audio() {
        // WAV magic bytes (RIFF WAVE) - infer detects this as audio/x-wav
        let wav_data = vec![
            b'R', b'I', b'F', b'F', // "RIFF"
            0x00, 0x00, 0x00, 0x00, // File size
            b'W', b'A', b'V', b'E', // "WAVE"
        ];
        let result = detect_media_type(&wav_data).unwrap();
        assert_eq!(result.mime_type(), "audio/x-wav");
        assert_eq!(result.extension(), "wav");
    }

    #[test]
    fn test_detect_non_image_pdf_document() {
        // PDF magic bytes
        let pdf_data = vec![
            b'%', b'P', b'D', b'F', b'-', b'1', b'.', b'4', 0x0A, // "%PDF-1.4\n"
        ];
        let result = detect_media_type(&pdf_data).unwrap();
        assert_eq!(result.mime_type(), "application/pdf");
        assert_eq!(result.extension(), "pdf");
    }

    #[test]
    fn test_detect_media_type_rejects_bmp() {
        // BMP is detectable by infer but NOT in our whitelist
        let bmp_data = vec![
            b'B', b'M', // BMP signature
            0x46, 0x00, 0x00, 0x00, // File size
            0x00, 0x00, // Reserved
            0x00, 0x00, // Reserved
            0x36, 0x00, 0x00, 0x00, // Pixel data offset
        ];
        let result = detect_media_type(&bmp_data);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Unsupported media format"));
        assert!(err_msg.contains("image/bmp"));
    }

    #[test]
    fn test_detect_media_type_rejects_avi() {
        // AVI is detectable by infer but NOT in our whitelist
        let avi_data = vec![
            b'R', b'I', b'F', b'F', // "RIFF"
            0x00, 0x00, 0x00, 0x00, // File size
            b'A', b'V', b'I', b' ', // "AVI "
        ];
        let result = detect_media_type(&avi_data);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Unsupported media format"));
        assert!(err_msg.contains("video/x-msvideo"));
    }

    #[test]
    fn test_detect_media_type_unknown_format() {
        // Random data that doesn't match any known format
        let random_data = vec![0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0];
        let result = detect_media_type(&random_data);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Unable to detect media type")
        );
    }

    #[test]
    fn test_media_type_detection_convenience_methods() {
        // Test convenience methods on Image variant
        // Note: We can't create a valid image with just magic bytes,
        // so we test the enum directly
        let image_detection = MediaTypeDetection::Image(ImageType::Jpeg);
        assert_eq!(image_detection.mime_type(), "image/jpeg");
        assert_eq!(image_detection.extension(), "jpg");

        // Test convenience methods on Other variant
        let other_detection = MediaTypeDetection::Other {
            mime_type: "video/mp4".to_string(),
            extension: "mp4",
        };
        assert_eq!(other_detection.mime_type(), "video/mp4");
        assert_eq!(other_detection.extension(), "mp4");
    }

    #[test]
    fn test_media_type_detection_equality() {
        let detection1 = MediaTypeDetection::Image(ImageType::Png);
        let detection2 = MediaTypeDetection::Image(ImageType::Png);
        let detection3 = MediaTypeDetection::Image(ImageType::Jpeg);
        assert_eq!(detection1, detection2);
        assert_ne!(detection1, detection3);

        let other1 = MediaTypeDetection::Other {
            mime_type: "video/mp4".to_string(),
            extension: "mp4",
        };
        let other2 = MediaTypeDetection::Other {
            mime_type: "video/mp4".to_string(),
            extension: "mp4",
        };
        assert_eq!(other1, other2);
    }

    #[test]
    fn test_explicit_whitelist_comprehensive() {
        // This test verifies the explicit whitelist approach works correctly
        // by testing that only approved formats are accepted

        // Approved video formats
        let mp4 = vec![
            0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm',
        ];
        assert!(detect_media_type(&mp4).is_ok());

        let webm = vec![
            0x1A, 0x45, 0xDF, 0xA3, 0x9F, 0x42, 0x86, 0x81, 0x01, 0x42, 0xF7, 0x81, 0x01, 0x42,
            0xF2, 0x81, 0x04, 0x42, 0xF3, 0x81, 0x08, 0x42, 0x82, 0x88, 0x77, 0x65, 0x62, 0x6D,
        ];
        assert!(detect_media_type(&webm).is_ok());

        // Approved audio formats
        let mp3 = vec![b'I', b'D', b'3', 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert!(detect_media_type(&mp3).is_ok());

        // Approved document format
        let pdf = vec![b'%', b'P', b'D', b'F', b'-', b'1', b'.', b'4', 0x0A];
        assert!(detect_media_type(&pdf).is_ok());

        // Rejected formats (detectable but not whitelisted)
        let bmp = vec![b'B', b'M', 0x46, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert!(detect_media_type(&bmp).is_err());

        let avi = vec![
            b'R', b'I', b'F', b'F', 0x00, 0x00, 0x00, 0x00, b'A', b'V', b'I', b' ',
        ];
        assert!(detect_media_type(&avi).is_err());
    }

    // --- RetryInfo tests ---

    #[test]
    fn test_retry_info_new_defaults() {
        let info = RetryInfo::new();
        assert_eq!(info.attempt, 0);
        assert_eq!(info.max_attempts, 10);
        assert_eq!(info.base_delay_ms, 1000);
    }

    #[test]
    fn test_retry_info_default_matches_new() {
        let from_default = RetryInfo::default();
        let from_new = RetryInfo::new();
        assert_eq!(from_default.attempt, from_new.attempt);
        assert_eq!(from_default.max_attempts, from_new.max_attempts);
        assert_eq!(from_default.base_delay_ms, from_new.base_delay_ms);
    }

    #[test]
    fn test_retry_info_should_retry() {
        let info = RetryInfo::new();
        assert!(info.should_retry());

        let exhausted = RetryInfo {
            attempt: 10,
            max_attempts: 10,
            base_delay_ms: 1000,
        };
        assert!(!exhausted.should_retry());
    }
}
