//! What a template database should contain.

use std::hash::{Hash, Hasher};

use sqlx::PgPool;
use sqlx::migrate::Migrator;

/// A schema definition that a template database is built from.
///
/// Two variants, both wanted: [`Schema::migrations`] for real schemas that ship,
/// and [`Schema::sql`] for tests of the harness itself and for small fixtures
/// that don't warrant a migration directory.
#[derive(Debug)]
pub struct Schema {
    label: &'static str,
    source: Source,
}

#[derive(Debug)]
enum Source {
    Migrations(&'static Migrator),
    Sql(&'static [&'static str]),
}

impl Schema {
    /// A schema built from a `sqlx::migrate!` migrator — the production path.
    #[must_use]
    pub const fn migrations(label: &'static str, migrator: &'static Migrator) -> Self {
        Self {
            label,
            source: Source::Migrations(migrator),
        }
    }

    /// A schema built from literal statements, applied in order.
    #[must_use]
    pub const fn sql(label: &'static str, statements: &'static [&'static str]) -> Self {
        Self {
            label,
            source: Source::Sql(statements),
        }
    }

    #[must_use]
    pub const fn label(&self) -> &'static str {
        self.label
    }

    /// Identifies the template database for this exact schema content.
    ///
    /// Changing a migration changes the fingerprint, so the next test run builds
    /// a new template instead of silently testing against the old schema. That
    /// failure mode — tests passing against a stale template — is the one thing
    /// a template-based harness must not have.
    pub(crate) fn fingerprint(&self) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.label.hash(&mut hasher);
        match self.source {
            Source::Migrations(migrator) => {
                for migration in migrator.iter() {
                    migration.version.hash(&mut hasher);
                    migration.checksum.hash(&mut hasher);
                }
            }
            Source::Sql(statements) => {
                for statement in statements {
                    statement.hash(&mut hasher);
                }
            }
        }
        // The label is sanitized for use in a database name, but the *original*
        // is hashed above — so two labels differing only in punctuation still
        // get distinct templates rather than colliding after normalization.
        //
        // Hex of a 64-bit hash. `DefaultHasher` is not stable across Rust
        // releases, which is harmless here: an unexpected change just rebuilds
        // the template once.
        format!("{}_{:016x}", Self::sanitize(self.label), hasher.finish())
    }

    /// Labels are written for humans (`"control-plane"`), but the fingerprint
    /// becomes part of a database name, which admits only lowercase ASCII,
    /// digits and underscores. Truncated so a long label can't push the full
    /// name past Postgres's 63-byte identifier limit.
    fn sanitize(label: &str) -> String {
        label
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .take(24)
            .collect()
    }

    pub(crate) async fn apply(&self, pool: &PgPool) -> anyhow::Result<()> {
        match self.source {
            Source::Migrations(migrator) => migrator.run(pool).await?,
            Source::Sql(statements) => {
                // `&'static str` satisfies `SqlSafeStr` directly — these come
                // from source code, never from input, so no assertion is needed.
                for statement in statements {
                    sqlx::raw_sql(*statement).execute(pool).await?;
                }
            }
        }
        Ok(())
    }
}
