use cgka_traits::storage::StorageProvider;
use cgka_traits::types::Backend;

use super::WhitenoiseMarmotStorage;
use super::openmls_storage::WhitenoiseOpenMlsStorage;

impl StorageProvider for WhitenoiseMarmotStorage {
    type Mls = WhitenoiseOpenMlsStorage;

    fn mls_storage(&self) -> &Self::Mls {
        &self.openmls
    }

    fn backend(&self) -> Backend {
        Backend::Sqlite
    }
}
