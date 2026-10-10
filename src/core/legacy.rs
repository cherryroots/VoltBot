//! Importing voltgpt's database (`old.db`).
//!
//! On startup, if `old.db` exists, every feature that declares a [`LegacyImport`] gets to
//! copy its data over, each in its own transaction. `legacy_imports` records the parts that
//! are done, so a restart never imports twice. Once every part in [`PARTS`] is done, the file
//! is renamed to `old.db.imported`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use super::Feature;
use super::config::Config;
use super::db::Db;

/// Every part Vivy will import. A part whose feature isn't ported yet stays pending, and
/// `old.db` is kept until it is done.
pub const PARTS: &[&str] = &["reminders", "wheel"];

/// What happened to one part on this start.
#[derive(Debug)]
pub enum Outcome {
    Imported { part: &'static str, rows: usize },
    Failed { part: &'static str, error: String },
}

/// Imports what hasn't been imported yet. Returns `None` when there is no `old.db`.
pub async fn import(
    db: &Db,
    features: &[Arc<dyn Feature>],
    path: &Path,
    config: Arc<Config>,
) -> anyhow::Result<Option<Vec<Outcome>>> {
    if !path.exists() {
        return Ok(None);
    }
    let features = features.to_vec();
    let path = path.to_path_buf();
    let outcomes = db
        .call(move |conn| import_sync(conn, &features, &path, &config))
        .await?;
    Ok(Some(outcomes))
}

fn import_sync(
    conn: &mut Connection,
    features: &[Arc<dyn Feature>],
    path: &Path,
    config: &Config,
) -> anyhow::Result<Vec<Outcome>> {
    let old = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", path.display()))?;

    let mut outcomes = Vec::new();
    for import in features.iter().filter_map(|f| f.legacy_import()) {
        if is_done(conn, import.part)? {
            continue;
        }
        let tx = conn.transaction()?;
        match (import.run)(&old, &tx, config) {
            Ok(rows) => {
                tx.execute(
                    "INSERT INTO legacy_imports (part, imported_at, rows) VALUES (?1, unixepoch(), ?2)",
                    (import.part, rows),
                )?;
                tx.commit()?;
                outcomes.push(Outcome::Imported {
                    part: import.part,
                    rows,
                });
            }
            // Dropping `tx` without committing rolls the part back.
            Err(err) => outcomes.push(Outcome::Failed {
                part: import.part,
                error: format!("{err:#}"),
            }),
        }
    }
    drop(old);

    let mut all_done = true;
    for part in PARTS {
        all_done &= is_done(conn, part)?;
    }
    if all_done {
        let mut imported = PathBuf::from(path);
        imported.as_mut_os_string().push(".imported");
        std::fs::rename(path, &imported)
            .with_context(|| format!("renaming {} to {}", path.display(), imported.display()))?;
    }
    Ok(outcomes)
}

fn is_done(conn: &Connection, part: &str) -> anyhow::Result<bool> {
    let found = conn
        .query_row(
            "SELECT 1 FROM legacy_imports WHERE part = ?1",
            [part],
            |_| Ok(()),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Whether voltgpt's database has a table. Old copies may predate some features.
pub fn table_exists(old: &Connection, table: &str) -> anyhow::Result<bool> {
    let found = old
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .optional()?;
    Ok(found.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::LegacyImport;
    use crate::core::db::{CORE_MIGRATIONS, migrate};
    use async_trait::async_trait;

    struct Importer(&'static str);

    #[async_trait]
    impl Feature for Importer {
        fn name(&self) -> &'static str {
            self.0
        }
        fn legacy_import(&self) -> Option<LegacyImport> {
            Some(LegacyImport {
                part: self.0,
                run: |old, _new, _config| {
                    Ok(old.query_row("SELECT count(*) FROM things", [], |r| r.get(0))?)
                },
            })
        }
    }

    fn setup() -> (Connection, PathBuf) {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, "core", CORE_MIGRATIONS).unwrap();
        let dir = std::env::temp_dir().join(format!("vivy-legacy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("old-{:?}.db", std::thread::current().id()));
        let _ = std::fs::remove_file(&path);
        let old = Connection::open(&path).unwrap();
        old.execute_batch("CREATE TABLE things (x); INSERT INTO things VALUES (1), (2);")
            .unwrap();
        (conn, path)
    }

    #[test]
    fn imports_each_part_once_and_renames_when_all_done() {
        let (mut conn, path) = setup();
        let reminders: Vec<Arc<dyn Feature>> = vec![Arc::new(Importer("reminders"))];

        let outcomes = import_sync(&mut conn, &reminders, &path, &Config::default()).unwrap();
        assert!(matches!(
            outcomes[..],
            [Outcome::Imported {
                part: "reminders",
                rows: 2
            }]
        ));
        // "wheel" isn't done yet, so the file stays.
        assert!(path.exists());
        assert!(
            import_sync(&mut conn, &reminders, &path, &Config::default())
                .unwrap()
                .is_empty()
        );

        let both: Vec<Arc<dyn Feature>> =
            vec![Arc::new(Importer("reminders")), Arc::new(Importer("wheel"))];
        let outcomes = import_sync(&mut conn, &both, &path, &Config::default()).unwrap();
        assert!(matches!(
            outcomes[..],
            [Outcome::Imported { part: "wheel", .. }]
        ));
        assert!(!path.exists());
        let imported = PathBuf::from(format!("{}.imported", path.display()));
        assert!(imported.exists());
        std::fs::remove_file(imported).unwrap();
    }
}
