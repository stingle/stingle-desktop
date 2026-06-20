//! Query layer — mirrors the Android `Db/Query/*` interfaces.

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::error::Result;
use crate::models::{DbAlbum, DbContact, DbFile, FileSet, Sort};
use crate::Db;

const FILE_COLS: &str =
    "_id, filename, is_local, is_remote, version, reupload, date_created, date_modified, headers";
const ALBUM_FILE_COLS: &str = "_id, album_id, filename, is_local, is_remote, version, reupload, date_created, date_modified, headers";

fn map_file(row: &Row) -> rusqlite::Result<DbFile> {
    Ok(DbFile {
        id: row.get(0)?,
        album_id: None,
        filename: row.get(1)?,
        is_local: row.get(2)?,
        is_remote: row.get(3)?,
        version: row.get(4)?,
        reupload: row.get(5)?,
        date_created: row.get(6)?,
        date_modified: row.get(7)?,
        headers: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
    })
}

fn map_album_file(row: &Row) -> rusqlite::Result<DbFile> {
    Ok(DbFile {
        id: row.get(0)?,
        album_id: Some(row.get(1)?),
        filename: row.get(2)?,
        is_local: row.get(3)?,
        is_remote: row.get(4)?,
        version: row.get(5)?,
        reupload: row.get(6)?,
        date_created: row.get(7)?,
        date_modified: row.get(8)?,
        headers: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
    })
}

fn map_album(row: &Row) -> rusqlite::Result<DbAlbum> {
    Ok(DbAlbum {
        album_id: row.get(0)?,
        enc_private_key: row.get(1)?,
        public_key: row.get(2)?,
        metadata: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
        is_shared: row.get(4)?,
        is_hidden: row.get(5)?,
        is_owner: row.get(6)?,
        members: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
        permissions: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
        sync_local: row.get(9)?,
        is_locked: row.get(10)?,
        cover: row.get::<_, Option<String>>(11)?.unwrap_or_default(),
        date_created: row.get(12)?,
        date_modified: row.get(13)?,
    })
}

fn map_contact(row: &Row) -> rusqlite::Result<DbContact> {
    Ok(DbContact {
        user_id: row.get(0)?,
        email: row.get(1)?,
        public_key: row.get(2)?,
        date_used: row.get(3)?,
        date_modified: row.get(4)?,
    })
}

/// Gallery/Trash tables only (panics for Album — use the album-file methods).
fn table(set: FileSet) -> &'static str {
    assert!(set != FileSet::Album, "use album-file methods for the album set");
    set.table()
}

impl Db {
    // ============================ Gallery / Trash ============================

    pub fn get_file(&self, set: FileSet, filename: &str) -> Result<Option<DbFile>> {
        self.with_conn(|c| {
            let sql = format!("SELECT {FILE_COLS} FROM {} WHERE filename = ?1", table(set));
            Ok(c.query_row(&sql, params![filename], map_file).optional()?)
        })
    }

    pub fn insert_file(&self, set: FileSet, f: &DbFile) -> Result<()> {
        self.with_conn(|c| {
            let sql = format!(
                "INSERT OR REPLACE INTO {} (filename, is_local, is_remote, version, reupload, date_created, date_modified, headers) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                table(set)
            );
            c.execute(
                &sql,
                params![
                    f.filename, f.is_local, f.is_remote, f.version, f.reupload,
                    f.date_created, f.date_modified, f.headers
                ],
            )?;
            Ok(())
        })
    }

    pub fn update_file_meta(
        &self,
        set: FileSet,
        filename: &str,
        version: i64,
        headers: &str,
        date_created: i64,
        date_modified: i64,
    ) -> Result<()> {
        self.with_conn(|c| {
            let sql = format!(
                "UPDATE {} SET version=?2, headers=?3, date_created=?4, date_modified=?5 WHERE filename=?1",
                table(set)
            );
            c.execute(&sql, params![filename, version, headers, date_created, date_modified])?;
            Ok(())
        })
    }

    pub fn set_local(&self, set: FileSet, filename: &str, is_local: bool) -> Result<()> {
        self.exec_flag(table(set), "is_local", filename, is_local)
    }

    pub fn set_remote(&self, set: FileSet, filename: &str, is_remote: bool) -> Result<()> {
        self.exec_flag(table(set), "is_remote", filename, is_remote)
    }

    /// `markFileAsRemote`: is_remote=1, reupload=0.
    pub fn mark_remote(&self, set: FileSet, filename: &str) -> Result<()> {
        self.with_conn(|c| {
            let sql = format!(
                "UPDATE {} SET is_remote=1, reupload=0 WHERE filename=?1",
                table(set)
            );
            c.execute(&sql, params![filename])?;
            Ok(())
        })
    }

    pub fn set_reupload(&self, set: FileSet, filename: &str, reupload: bool) -> Result<()> {
        self.exec_flag(table(set), "reupload", filename, reupload)
    }

    pub fn delete_file(&self, set: FileSet, filename: &str) -> Result<()> {
        self.with_conn(|c| {
            let sql = format!("DELETE FROM {} WHERE filename=?1", table(set));
            c.execute(&sql, params![filename])?;
            Ok(())
        })
    }

    pub fn list_files(
        &self,
        set: FileSet,
        sort: Sort,
        limit: Option<i64>,
        offset: i64,
    ) -> Result<Vec<DbFile>> {
        self.with_conn(|c| {
            let lim = limit.unwrap_or(-1);
            let sql = format!(
                "SELECT {FILE_COLS} FROM {} ORDER BY date_created {} LIMIT ?1 OFFSET ?2",
                table(set),
                sort.sql()
            );
            collect(c, &sql, params![lim, offset], map_file)
        })
    }

    pub fn count_files(&self, set: FileSet) -> Result<i64> {
        self.with_conn(|c| {
            let sql = format!("SELECT COUNT(*) FROM {}", table(set));
            Ok(c.query_row(&sql, [], |r| r.get(0))?)
        })
    }

    /// Files present locally but not yet uploaded (`is_local=1 AND is_remote=0`).
    pub fn list_only_local(&self, set: FileSet, sort: Sort) -> Result<Vec<DbFile>> {
        self.with_conn(|c| {
            let sql = format!(
                "SELECT {FILE_COLS} FROM {} WHERE is_local=1 AND is_remote=0 ORDER BY date_created {}",
                table(set),
                sort.sql()
            );
            collect(c, &sql, [], map_file)
        })
    }

    /// Files flagged for re-upload.
    pub fn list_reupload(&self, set: FileSet) -> Result<Vec<DbFile>> {
        self.with_conn(|c| {
            let sql = format!(
                "SELECT {FILE_COLS} FROM {} WHERE reupload=1 ORDER BY date_created DESC",
                table(set)
            );
            collect(c, &sql, [], map_file)
        })
    }

    /// Distinct `date_created` values (for date-grouped UI), newest first.
    pub fn distinct_dates(&self, set: FileSet) -> Result<Vec<i64>> {
        self.with_conn(|c| {
            let sql = format!(
                "SELECT DISTINCT date_created FROM {} ORDER BY date_created DESC",
                table(set)
            );
            collect(c, &sql, [], |r| r.get::<_, i64>(0))
        })
    }

    // ============================ Album files ============================

    pub fn get_album_file(&self, album_id: &str, filename: &str) -> Result<Option<DbFile>> {
        self.with_conn(|c| {
            let sql = format!(
                "SELECT {ALBUM_FILE_COLS} FROM album_files WHERE album_id=?1 AND filename=?2"
            );
            Ok(c.query_row(&sql, params![album_id, filename], map_album_file)
                .optional()?)
        })
    }

    /// Look up an album file by filename alone (any album).
    pub fn get_album_file_any(&self, filename: &str) -> Result<Option<DbFile>> {
        self.with_conn(|c| {
            let sql = format!("SELECT {ALBUM_FILE_COLS} FROM album_files WHERE filename=?1");
            Ok(c.query_row(&sql, params![filename], map_album_file)
                .optional()?)
        })
    }

    pub fn insert_album_file(&self, f: &DbFile) -> Result<()> {
        let album_id = f.album_id.clone().unwrap_or_default();
        self.with_conn(|c| {
            c.execute(
                "INSERT OR REPLACE INTO album_files (album_id, filename, is_local, is_remote, version, reupload, headers, date_created, date_modified) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    album_id, f.filename, f.is_local, f.is_remote, f.version, f.reupload,
                    f.headers, f.date_created, f.date_modified
                ],
            )?;
            Ok(())
        })
    }

    pub fn list_album_files(
        &self,
        album_id: &str,
        sort: Sort,
        limit: Option<i64>,
        offset: i64,
    ) -> Result<Vec<DbFile>> {
        self.with_conn(|c| {
            let lim = limit.unwrap_or(-1);
            let sql = format!(
                "SELECT {ALBUM_FILE_COLS} FROM album_files WHERE album_id=?1 ORDER BY date_created {} LIMIT ?2 OFFSET ?3",
                sort.sql()
            );
            collect(c, &sql, params![album_id, lim, offset], map_album_file)
        })
    }

    pub fn count_album_files(&self, album_id: &str) -> Result<i64> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM album_files WHERE album_id=?1",
                params![album_id],
                |r| r.get(0),
            )?)
        })
    }

    pub fn mark_album_file_remote(&self, album_id: &str, filename: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE album_files SET is_remote=1, reupload=0 WHERE album_id=?1 AND filename=?2",
                params![album_id, filename],
            )?;
            Ok(())
        })
    }

    pub fn delete_album_file(&self, album_id: &str, filename: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "DELETE FROM album_files WHERE album_id=?1 AND filename=?2",
                params![album_id, filename],
            )?;
            Ok(())
        })
    }

    pub fn delete_all_files_in_album(&self, album_id: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute("DELETE FROM album_files WHERE album_id=?1", params![album_id])?;
            Ok(())
        })
    }

    /// Every album file row (all albums) — used by the thumbnail prefetcher.
    pub fn list_all_album_files(&self) -> Result<Vec<DbFile>> {
        self.with_conn(|c| {
            let sql = format!(
                "SELECT {ALBUM_FILE_COLS} FROM album_files ORDER BY date_created DESC"
            );
            collect(c, &sql, [], map_album_file)
        })
    }

    /// Album files present locally but not uploaded, across all albums.
    pub fn list_album_files_only_local(&self) -> Result<Vec<DbFile>> {
        self.with_conn(|c| {
            let sql = format!(
                "SELECT {ALBUM_FILE_COLS} FROM album_files WHERE is_local=1 AND is_remote=0 ORDER BY date_created DESC"
            );
            collect(c, &sql, [], map_album_file)
        })
    }

    // ============================ Albums ============================

    pub fn get_album(&self, album_id: &str) -> Result<Option<DbAlbum>> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT album_id, album_sk, album_pk, metadata, is_shared, is_hidden, is_owner, members, permissions, sync_local, is_locked, cover, date_created, date_modified FROM albums WHERE album_id=?1",
                params![album_id],
                map_album,
            )
            .optional()?)
        })
    }

    /// Insert or update an album, preserving the local-only `sync_local` flag.
    pub fn upsert_album(&self, a: &DbAlbum) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO albums (album_id, album_sk, album_pk, metadata, is_shared, is_hidden, is_owner, members, permissions, sync_local, is_locked, cover, date_created, date_modified) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14) \
                 ON CONFLICT(album_id) DO UPDATE SET \
                   album_sk=excluded.album_sk, album_pk=excluded.album_pk, metadata=excluded.metadata, \
                   is_shared=excluded.is_shared, is_hidden=excluded.is_hidden, is_owner=excluded.is_owner, \
                   members=excluded.members, permissions=excluded.permissions, is_locked=excluded.is_locked, \
                   cover=excluded.cover, date_created=excluded.date_created, date_modified=excluded.date_modified",
                params![
                    a.album_id, a.enc_private_key, a.public_key, a.metadata, a.is_shared, a.is_hidden,
                    a.is_owner, a.members, a.permissions, a.sync_local, a.is_locked, a.cover,
                    a.date_created, a.date_modified
                ],
            )?;
            Ok(())
        })
    }

    pub fn set_album_sync_local(&self, album_id: &str, sync_local: bool) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE albums SET sync_local=?2 WHERE album_id=?1",
                params![album_id, sync_local],
            )?;
            Ok(())
        })
    }

    pub fn delete_album(&self, album_id: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute("DELETE FROM albums WHERE album_id=?1", params![album_id])?;
            Ok(())
        })
    }

    pub fn list_albums(&self, include_hidden: bool) -> Result<Vec<DbAlbum>> {
        self.with_conn(|c| {
            let sql = if include_hidden {
                "SELECT album_id, album_sk, album_pk, metadata, is_shared, is_hidden, is_owner, members, permissions, sync_local, is_locked, cover, date_created, date_modified FROM albums ORDER BY date_created DESC"
            } else {
                "SELECT album_id, album_sk, album_pk, metadata, is_shared, is_hidden, is_owner, members, permissions, sync_local, is_locked, cover, date_created, date_modified FROM albums WHERE is_hidden=0 ORDER BY date_created DESC"
            };
            collect(c, sql, [], map_album)
        })
    }

    // ============================ Contacts ============================

    pub fn upsert_contact(&self, ct: &DbContact) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO contacts (user_id, email, pk, date_used, date_modified) VALUES (?1,?2,?3,?4,?5) \
                 ON CONFLICT(user_id) DO UPDATE SET email=excluded.email, pk=excluded.pk, date_used=excluded.date_used, date_modified=excluded.date_modified",
                params![ct.user_id, ct.email, ct.public_key, ct.date_used, ct.date_modified],
            )?;
            Ok(())
        })
    }

    pub fn get_contact_by_user_id(&self, user_id: i64) -> Result<Option<DbContact>> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT user_id, email, pk, date_used, date_modified FROM contacts WHERE user_id=?1",
                params![user_id],
                map_contact,
            )
            .optional()?)
        })
    }

    pub fn get_contact_by_email(&self, email: &str) -> Result<Option<DbContact>> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT user_id, email, pk, date_used, date_modified FROM contacts WHERE email=?1",
                params![email],
                map_contact,
            )
            .optional()?)
        })
    }

    pub fn list_contacts(&self) -> Result<Vec<DbContact>> {
        self.with_conn(|c| {
            collect(
                c,
                "SELECT user_id, email, pk, date_used, date_modified FROM contacts ORDER BY email ASC",
                [],
                map_contact,
            )
        })
    }

    pub fn delete_contact(&self, user_id: i64) -> Result<()> {
        self.with_conn(|c| {
            c.execute("DELETE FROM contacts WHERE user_id=?1", params![user_id])?;
            Ok(())
        })
    }

    // ============================ Imported ids ============================

    pub fn mark_imported(&self, media_id: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT OR IGNORE INTO imported_ids (media_id) VALUES (?1)",
                params![media_id],
            )?;
            Ok(())
        })
    }

    pub fn is_imported(&self, media_id: &str) -> Result<bool> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT 1 FROM imported_ids WHERE media_id=?1",
                params![media_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
        })
    }

    // ============================ Key/value ============================

    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        self.with_conn(|c| {
            Ok(c.query_row("SELECT v FROM kv WHERE k=?1", params![key], |r| r.get(0))
                .optional()?)
        })
    }

    pub fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO kv (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
                params![key, value],
            )?;
            Ok(())
        })
    }

    pub fn kv_get_i64(&self, key: &str) -> Result<Option<i64>> {
        Ok(self.kv_get(key)?.and_then(|s| s.parse().ok()))
    }

    pub fn kv_set_i64(&self, key: &str, value: i64) -> Result<()> {
        self.kv_set(key, &value.to_string())
    }

    // ---- internal helpers ----

    fn exec_flag(&self, table: &str, col: &str, filename: &str, val: bool) -> Result<()> {
        self.with_conn(|c| {
            let sql = format!("UPDATE {table} SET {col}=?2 WHERE filename=?1");
            c.execute(&sql, params![filename, val])?;
            Ok(())
        })
    }
}

fn collect<T>(
    c: &Connection,
    sql: &str,
    p: impl rusqlite::Params,
    f: impl Fn(&Row) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = c.prepare(sql)?;
    let rows = stmt.query_map(p, |r| f(r))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}
