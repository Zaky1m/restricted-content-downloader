use sqlx::{Executor, PgPool, postgres::PgPoolOptions};

/// A throwaway schema inside TEST_DATABASE_URL; dropped by `finish`. Never touches `public`.
pub struct TempSchema {
    pub pool: PgPool,
    admin: PgPool,
    name: String,
}

impl TempSchema {
    pub async fn create() -> Option<Self> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        let name = format!("rcd_test_{}_{}", std::process::id(), rand_suffix());
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .ok()?;
        drop_leftovers(&admin).await;
        admin
            .execute(format!("CREATE SCHEMA {name}").as_str())
            .await
            .ok()?;
        let schema = name.clone();
        let pool = PgPoolOptions::new()
            .max_connections(3)
            .after_connect(move |conn, _| {
                let s = schema.clone();
                Box::pin(async move {
                    conn.execute(format!("SET search_path TO {s}").as_str())
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .ok()?;
        Some(Self { pool, admin, name })
    }

    #[allow(dead_code)]
    pub fn name(&self) -> &str {
        &self.name
    }

    pub async fn finish(self) {
        self.pool.close().await;
        let _ = self
            .admin
            .execute(format!("DROP SCHEMA {} CASCADE", self.name).as_str())
            .await;
    }
}

/// A test that panics never reaches `finish`; its schema is dropped by the next run once its process is gone.
async fn drop_leftovers(admin: &PgPool) {
    let names: Vec<(String,)> = sqlx::query_as(
        "SELECT nspname::text FROM pg_namespace WHERE nspname LIKE 'rcd\\_test\\_%'",
    )
    .fetch_all(admin)
    .await
    .unwrap_or_default();
    for (name,) in names {
        let pid = name.split('_').nth(2).unwrap_or("");
        if !pid.is_empty() && std::path::Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        let _ = admin
            .execute(format!("DROP SCHEMA IF EXISTS \"{name}\" CASCADE").as_str())
            .await;
    }
}

fn rand_suffix() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0)
}
