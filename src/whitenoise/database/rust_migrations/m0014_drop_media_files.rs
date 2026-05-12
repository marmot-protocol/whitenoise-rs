use async_trait::async_trait;
use sqlx::SqliteConnection;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::GlobalMigration;

pub struct Migration;

#[async_trait]
impl GlobalMigration for Migration {
    fn version(&self) -> u32 {
        14
    }

    fn description(&self) -> &'static str {
        "Drop legacy media_files table after blob/reference split"
    }

    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
        sqlx::query("DROP TABLE IF EXISTS media_files")
            .execute(&mut *tx)
            .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sqlx::SqlitePool;
    use tempfile::TempDir;

    use crate::whitenoise::database::rust_migrations::{Migrator, all_global_migrations};

    async fn create_pool(dir: &TempDir, name: &str) -> SqlitePool {
        let path = dir.path().join(name);
        let url = format!("sqlite://{}?mode=rwc", path.display());
        SqlitePool::connect(&url).await.unwrap()
    }

    #[tokio::test]
    async fn media_files_dropped_after_full_migration() {
        let dir = TempDir::new().unwrap();
        let pool = create_pool(&dir, "drop.db").await;

        // Run all globals through m0014.
        let globals: Vec<Box<dyn crate::whitenoise::database::rust_migrations::GlobalMigration>> =
            all_global_migrations()
                .into_iter()
                .filter(|m| m.version() <= 14)
                .collect();
        let runner = Migrator::new(globals, vec![]);
        runner.run(&pool, None).await.unwrap();

        // media_files should be gone.
        let tables: Vec<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='media_files'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(
            tables.is_empty(),
            "media_files should be dropped after m0014"
        );

        // media_blobs and media_references should still exist.
        for table in &["media_blobs", "media_references"] {
            let exists: Vec<(String,)> =
                sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' AND name = ?")
                    .bind(table)
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert!(!exists.is_empty(), "{table} should still exist after m0014");
        }
    }
}
