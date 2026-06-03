//! WhiteNoise-owned media validation, sanitization, and preview metadata.

use std::io::Cursor;

use blurhash::encode as encode_blurhash;
use exif::{In, Reader as ExifReader, Tag};
use fast_thumbhash::rgba_to_thumb_hash_b91;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{DynamicImage, ImageEncoder, ImageReader};

use crate::marmot::media::canonical_media_type;
use crate::whitenoise::error::{Result, WhitenoiseError};

pub const MAX_FILE_SIZE: usize = 100 * 1024 * 1024;
pub const MAX_FILENAME_LENGTH: usize = 210;
pub const MAX_IMAGE_DIMENSION: u32 = 16_384;
pub const MAX_IMAGE_PIXELS: u64 = 50_000_000;
pub const MAX_IMAGE_MEMORY_MB: u64 = 256;

const SUPPORTED_MIME_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "video/mp4",
    "video/quicktime",
    "video/webm",
    "audio/ogg",
    "audio/mp4",
    "audio/m4a",
    "audio/mpeg",
    "audio/wav",
    "audio/x-wav",
    "application/pdf",
];

/// Options for chat-media validation, sanitization, and preview metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaProcessingOptions {
    /// Sanitize EXIF and other metadata for privacy.
    pub sanitize_exif: bool,
    /// Generate blurhash preview metadata for images.
    pub generate_blurhash: bool,
    /// Generate thumbhash preview metadata for images.
    pub generate_thumbhash: bool,
    /// Maximum allowed image width or height.
    pub max_dimension: Option<u32>,
    /// Maximum allowed file size in bytes.
    pub max_file_size: Option<usize>,
    /// Maximum allowed filename length in bytes.
    pub max_filename_length: Option<usize>,
}

impl Default for MediaProcessingOptions {
    fn default() -> Self {
        Self {
            sanitize_exif: true,
            generate_blurhash: true,
            generate_thumbhash: true,
            max_dimension: Some(MAX_IMAGE_DIMENSION),
            max_file_size: Some(MAX_FILE_SIZE),
            max_filename_length: Some(MAX_FILENAME_LENGTH),
        }
    }
}

impl MediaProcessingOptions {
    pub fn validation_only() -> Self {
        Self {
            sanitize_exif: false,
            generate_blurhash: false,
            generate_thumbhash: false,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessedMedia {
    pub(crate) data: Vec<u8>,
    pub(crate) mime_type: String,
    pub(crate) dimensions: Option<(u32, u32)>,
    pub(crate) blurhash: Option<String>,
    pub(crate) thumbhash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageMetadata {
    dimensions: Option<(u32, u32)>,
    blurhash: Option<String>,
    thumbhash: Option<String>,
}

pub(crate) fn process_for_chat_media_encryption(
    data: &[u8],
    mime_type: &str,
    filename: &str,
    options: &MediaProcessingOptions,
) -> Result<ProcessedMedia> {
    validate_file_size(data, options)?;
    validate_filename(filename, options)?;
    let mime_type = canonical_media_type(mime_type)?;
    validate_supported_mime_type(&mime_type)?;

    if mime_type.starts_with("image/") {
        return process_image_for_chat_media(data, mime_type, options);
    }

    Ok(ProcessedMedia {
        data: data.to_vec(),
        mime_type,
        dimensions: None,
        blurhash: None,
        thumbhash: None,
    })
}

fn validate_file_size(data: &[u8], options: &MediaProcessingOptions) -> Result<()> {
    let max_size = options.max_file_size.unwrap_or(MAX_FILE_SIZE);
    if data.len() > max_size {
        return Err(WhitenoiseError::UnsupportedMediaFormat(format!(
            "File size {} exceeds maximum allowed size {}",
            data.len(),
            max_size
        )));
    }
    Ok(())
}

fn validate_supported_mime_type(mime_type: &str) -> Result<()> {
    if SUPPORTED_MIME_TYPES.contains(&mime_type) {
        return Ok(());
    }

    Err(WhitenoiseError::UnsupportedMediaFormat(format!(
        "Unsupported media format: {mime_type}. Supported formats: images (JPEG, PNG, GIF, WebP), videos (MP4, WebM, MOV), audio (MP3, OGG, M4A, WAV), documents (PDF)"
    )))
}

fn validate_filename(filename: &str, options: &MediaProcessingOptions) -> Result<()> {
    let max_length = options.max_filename_length.unwrap_or(MAX_FILENAME_LENGTH);
    if filename.is_empty() {
        return Err(WhitenoiseError::InvalidInput(
            "chat media filename cannot be empty".to_string(),
        ));
    }
    if filename.len() > max_length {
        return Err(WhitenoiseError::InvalidInput(format!(
            "chat media filename length {} exceeds maximum {}",
            filename.len(),
            max_length
        )));
    }
    if filename.contains('/') || filename.contains('\\') || filename.chars().any(char::is_control) {
        return Err(WhitenoiseError::InvalidInput(
            "chat media filename cannot contain path separators or control characters".to_string(),
        ));
    }
    Ok(())
}

fn process_image_for_chat_media(
    data: &[u8],
    mime_type: String,
    options: &MediaProcessingOptions,
) -> Result<ProcessedMedia> {
    if options.sanitize_exif && is_safe_raster_format(&mime_type) {
        preflight_dimension_check(data, options)?;
        match strip_exif_and_return_image(data, &mime_type) {
            Ok((cleaned_data, decoded_image)) => {
                let metadata = extract_metadata_from_decoded_image(&decoded_image, options)?;
                return Ok(processed_image(cleaned_data, mime_type, metadata));
            }
            Err(error) => {
                return Err(error);
            }
        }
    }

    let metadata = extract_metadata_from_encoded_image(data, options)?;
    Ok(processed_image(data.to_vec(), mime_type, metadata))
}

fn processed_image(data: Vec<u8>, mime_type: String, metadata: ImageMetadata) -> ProcessedMedia {
    ProcessedMedia {
        data,
        mime_type,
        dimensions: metadata.dimensions,
        blurhash: metadata.blurhash,
        thumbhash: metadata.thumbhash,
    }
}

fn is_safe_raster_format(mime_type: &str) -> bool {
    matches!(mime_type, "image/jpeg" | "image/png")
}

fn preflight_dimension_check(data: &[u8], options: &MediaProcessingOptions) -> Result<()> {
    let reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(media_metadata_error)?;
    let (width, height) = reader.into_dimensions().map_err(media_metadata_error)?;
    validate_image_dimensions(width, height, options)
}

fn strip_exif_and_return_image(data: &[u8], mime_type: &str) -> Result<(Vec<u8>, DynamicImage)> {
    let reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(media_metadata_error)?;
    let image = reader.decode().map_err(media_metadata_error)?;
    let image = apply_exif_orientation(data, image);
    let mut output = Cursor::new(Vec::new());

    match mime_type {
        "image/jpeg" => {
            let mut encoder = JpegEncoder::new_with_quality(&mut output, 100);
            encoder
                .encode(
                    image.as_bytes(),
                    image.width(),
                    image.height(),
                    image.color().into(),
                )
                .map_err(media_metadata_error)?;
        }
        "image/png" => {
            let encoder = PngEncoder::new(&mut output);
            encoder
                .write_image(
                    image.as_bytes(),
                    image.width(),
                    image.height(),
                    image.color().into(),
                )
                .map_err(media_metadata_error)?;
        }
        _ => {
            return Err(WhitenoiseError::UnsupportedMediaFormat(format!(
                "Unsupported image format for EXIF sanitization: {mime_type}"
            )));
        }
    }

    Ok((output.into_inner(), image))
}

fn apply_exif_orientation(data: &[u8], image: DynamicImage) -> DynamicImage {
    let exif = match ExifReader::new().read_from_container(&mut Cursor::new(data)) {
        Ok(exif) => exif,
        Err(_) => return image,
    };
    let orientation = match exif.get_field(Tag::Orientation, In::PRIMARY) {
        Some(field) => match field.value.get_uint(0) {
            Some(value) => value,
            None => return image,
        },
        None => return image,
    };

    match orientation {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.fliph().rotate270(),
        6 => image.rotate90(),
        7 => image.fliph().rotate90(),
        8 => image.rotate270(),
        _ => image,
    }
}

fn extract_metadata_from_encoded_image(
    data: &[u8],
    options: &MediaProcessingOptions,
) -> Result<ImageMetadata> {
    let reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(media_metadata_error)?;
    let (width, height) = reader.into_dimensions().map_err(media_metadata_error)?;
    validate_image_dimensions(width, height, options)?;

    let mut metadata = ImageMetadata {
        dimensions: Some((width, height)),
        blurhash: None,
        thumbhash: None,
    };

    if options.generate_blurhash || options.generate_thumbhash {
        let reader = ImageReader::new(Cursor::new(data))
            .with_guessed_format()
            .map_err(media_metadata_error)?;
        let image = reader.decode().map_err(media_metadata_error)?;
        fill_preview_hashes(&mut metadata, &image, options);
    }

    Ok(metadata)
}

fn extract_metadata_from_decoded_image(
    image: &DynamicImage,
    options: &MediaProcessingOptions,
) -> Result<ImageMetadata> {
    validate_image_dimensions(image.width(), image.height(), options)?;
    let mut metadata = ImageMetadata {
        dimensions: Some((image.width(), image.height())),
        blurhash: None,
        thumbhash: None,
    };
    fill_preview_hashes(&mut metadata, image, options);
    Ok(metadata)
}

fn fill_preview_hashes(
    metadata: &mut ImageMetadata,
    image: &DynamicImage,
    options: &MediaProcessingOptions,
) {
    if options.generate_blurhash {
        metadata.blurhash = generate_blurhash(image);
    }
    if options.generate_thumbhash {
        metadata.thumbhash = Some(generate_thumbhash(image));
    }
}

fn generate_blurhash(image: &DynamicImage) -> Option<String> {
    let small_image = image.resize(32, 32, image::imageops::FilterType::Lanczos3);
    let rgba_image = small_image.to_rgba8();
    encode_blurhash(
        4,
        3,
        rgba_image.width(),
        rgba_image.height(),
        rgba_image.as_raw(),
    )
    .ok()
}

fn generate_thumbhash(image: &DynamicImage) -> String {
    let small_image = image.resize(100, 100, image::imageops::FilterType::Triangle);
    let rgba_image = small_image.to_rgba8();
    rgba_to_thumb_hash_b91(
        rgba_image.width() as usize,
        rgba_image.height() as usize,
        rgba_image.as_raw(),
    )
}

fn validate_image_dimensions(
    width: u32,
    height: u32,
    options: &MediaProcessingOptions,
) -> Result<()> {
    if let Some(max_dimension) = options.max_dimension
        && (width > max_dimension || height > max_dimension)
    {
        return Err(WhitenoiseError::UnsupportedMediaFormat(format!(
            "Image dimensions {width}x{height} exceed maximum {max_dimension}"
        )));
    }

    let total_pixels = width as u64 * height as u64;
    if total_pixels > MAX_IMAGE_PIXELS {
        return Err(WhitenoiseError::UnsupportedMediaFormat(format!(
            "Image has {total_pixels} pixels, exceeding maximum {MAX_IMAGE_PIXELS}"
        )));
    }

    let estimated_mb = total_pixels.saturating_mul(4).div_ceil(1024 * 1024);
    if estimated_mb > MAX_IMAGE_MEMORY_MB {
        return Err(WhitenoiseError::UnsupportedMediaFormat(format!(
            "Decoded image would require approximately {estimated_mb} MB, exceeding maximum {MAX_IMAGE_MEMORY_MB} MB"
        )));
    }

    Ok(())
}

fn media_metadata_error(error: impl std::fmt::Display) -> WhitenoiseError {
    WhitenoiseError::UnsupportedMediaFormat(format!("Failed to process image metadata: {error}"))
}

#[cfg(test)]
mod tests {
    use image::ImageFormat;

    use super::{MediaProcessingOptions, process_for_chat_media_encryption};

    fn png_image(width: u32, height: u32) -> Vec<u8> {
        let mut data = Vec::new();
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([0, 128, 255, 255]));
        image
            .write_to(&mut std::io::Cursor::new(&mut data), ImageFormat::Png)
            .unwrap();
        data
    }

    #[test]
    fn default_processing_sanitizes_png_and_generates_preview_metadata() {
        let data = png_image(32, 24);

        let processed = process_for_chat_media_encryption(
            &data,
            "IMAGE/PNG; charset=binary",
            "photo.png",
            &MediaProcessingOptions::default(),
        )
        .unwrap();

        assert_eq!(processed.mime_type, "image/png");
        assert_eq!(processed.dimensions, Some((32, 24)));
        assert!(processed.blurhash.is_some());
        assert!(processed.thumbhash.is_some());
        assert!(!processed.data.is_empty());
    }

    #[test]
    fn validation_rejects_path_like_filenames() {
        let data = png_image(8, 8);
        let error = process_for_chat_media_encryption(
            &data,
            "image/png",
            "../photo.png",
            &MediaProcessingOptions::validation_only(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("path separators"));
    }
}
