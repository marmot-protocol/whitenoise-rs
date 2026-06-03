use cgka_traits::storage::{StorageError, StorageResult};

pub(super) trait RusqliteResultExt<T> {
    fn storage(self) -> StorageResult<T>;
}

impl<T> RusqliteResultExt<T> for rusqlite::Result<T> {
    fn storage(self) -> StorageResult<T> {
        self.map_err(|err| StorageError::Backend(err.to_string()))
    }
}
