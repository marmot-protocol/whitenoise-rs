# Media Files Storage Architecture

## Overview

The media files system is organized into three distinct layers, each with a specific responsibility:

```
┌─────────────────────────────────────────────────────────────┐
│                    whitenoise/media_files                   │
│                   (High-level orchestration)                │
│  - Coordinates between storage, database, and Blossom       │
│  - Handles encryption/decryption through Marmot media APIs  │
│  - Manages business logic                                   │
└────────────┬──────────────┬──────────────┬──────────────────┘
             │              │              │
             ▼              ▼              ▼
    ┌────────────┐  ┌──────────────┐  ┌──────────────┐
    │     DB     │  │      FS      │  │   Blossom    │
    └────────────┘  └──────────────┘  └──────────────┘
    database/       storage/          BlossomClient
    media_files     media_files       (nostr_sdk)
```

## Layer Responsibilities

### 1. `whitenoise/media_files.rs` (Orchestration Layer)

**Purpose:** High-level coordination between storage and database

**Responsibilities:**

- Coordinate between storage layer and database layer
- Provide convenience methods for common operations
- Combine filesystem and database operations atomically
- Business logic and validation

**Does NOT:**

- Touch the filesystem directly (delegates to storage layer)
- Execute SQL directly (delegates to database layer)
- Handle network operations (use BlossomClient)
- Perform encryption/decryption directly (delegates to Marmot media helpers
  via callers)

**Key Types:**

- `MediaFiles<'a>` - Orchestrator with lifetime-bound references to storage and database

**Key Methods:**

- `store_and_record()` - Store file and record metadata in one operation
- `record_in_database()` - Record existing file metadata
- `find_file_with_prefix()` - Delegate to storage layer
- `get_file_path()` - Delegate to storage layer

### 2. `database/media_files.rs` (Left Branch)

**Purpose:** Database operations for media file metadata

**Responsibilities:**

- Store and retrieve media file records
- Track file metadata (hash, mime type, dimensions, blurhash)
- Query files by group and hash

**Does NOT:**

- Touch the filesystem
- Download or upload files
- Handle encryption/decryption

**Key Types:**

- `MediaFile` - Database record representation
- `MediaFileParams` - Parameters for saving records
- `MediaFileRow` - Internal database row type

### 3. `storage/media_files.rs` (Middle Branch)

**Purpose:** Content-addressed filesystem storage for media files

**Responsibilities:**

- Create the cache directory
- Store files atomically (write to `.tmp`, then rename)
- Deduplicate files by content (same hash = same file on disk)
- Retrieve file paths
- Check file existence
- Find files by prefix (useful when extension is unknown)

**Does NOT:**

- Interact with the database
- Download or upload files
- Handle encryption/decryption
- Know about Blossom URLs
- Track group/file relationships (database handles this)
- Classify media types (database handles this via `media_type` column)

**Key Types:**

- `MediaFileStorage` - Filesystem storage manager
- `Storage` - Wrapper struct for all storage subsystems

**Directory Structure:**

```
<data_dir>/media_cache/
  <hash>.<ext>    # All files in flat structure
  <hash>.<ext>    # Deduplicated by content hash
  <hash>.<ext>    # Database tracks media type and relationships
```

**Deduplication Strategy:**
Files are stored in a flat structure, identified solely by their content hash. If a file
with the same hash is forwarded from group X to groups Y and Z, it's stored only once
on disk. The database maintains separate records linking each group to the shared file,
as well as metadata like media type ("group_image", "chat_media", etc.).

### 4. `BlossomClient` (Right Branch)

**Purpose:** Network operations with Blossom servers

**Provided by:** `nostr_sdk` crate

**Responsibilities:**

- Upload blobs to Blossom servers
- Download blobs from Blossom servers
- Handle authentication with Nostr keys

**Does NOT:**

- Touch the filesystem directly
- Manage database records
- Handle caching

## Usage Patterns

### Group Images (Marmot Data Extension)

Group images are part of the Marmot protocol's data extension. They're stored in the flat
cache structure and identified by the `media_type: "group_image"` field in the database.

**Using the orchestration layer (recommended):**

```rust
use whitenoise::media_files::MediaFileUpload;

let upload = MediaFileUpload {
    data: &decrypted_data,
    file_hash: hash,
    mime_type: "image/jpeg",
    media_type: "group_image",  // Database tracks the type
    blossom_url: Some("https://..."),
    file_metadata: None,
};

// Store and record in one operation
// Files are deduplicated - same hash = same file on disk
let path = whitenoise
    .media_files()
    .store_and_record(
        &account_pubkey,
        &group_id,
        "abc123.jpg",  // Typically <hash>.<ext>
        upload,
    )
    .await?;
```

**Using layers directly (for more control):**

Note: This is intended to be used only on whitenoise/media_files.rs

```rust
// Store file to filesystem (deduplicated by content)
let path = whitenoise.storage.media_files
    .store_file("abc123.jpg", &decrypted_data)
    .await?;

// Record in database (links this group to the file and tracks type)
use whitenoise::database::media_files::{MediaFile, MediaFileParams};
MediaFile::save(
    &whitenoise.database,
    &group_id,
    &account_pubkey,
    MediaFileParams {
        file_path: &path,
        file_hash: &hash,
        mime_type: "image/jpeg",
        media_type: "group_image",  // Type stored in database
        blossom_url: Some("https://..."),
        file_metadata: None,
    },
).await?;
```

### Chat Media Files

Regular chat media (images, videos, etc.) are stored in the same flat cache structure,
identified by `media_type: "chat_media"` in the database.

**Using the orchestration layer (recommended):**

```rust
use whitenoise::database::media_files::FileMetadata;
use whitenoise::media_files::MediaFileUpload;

let metadata = FileMetadata::new()
    .with_dimensions("1920x1080".to_string())
    .with_blurhash("LKO2?U%2Tw=w]~RBVZRi};RPxuwH".to_string());

let upload = MediaFileUpload {
    data: &decrypted_data,
    file_hash: hash,
    mime_type: "video/mp4",
    media_type: "chat_media",  // Database tracks the type
    blossom_url: Some("https://..."),
    file_metadata: Some(&metadata),
};

// Store and record in one operation
let path = whitenoise
    .media_files()
    .store_and_record(
        &account_pubkey,
        &group_id,
        "xyz789.mp4",  // Typically <hash>.<ext>
        upload,
    )
    .await?;
```
