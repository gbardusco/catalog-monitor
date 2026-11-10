//! Banco SQLite: abertura, migrações versionadas (`PRAGMA user_version`) e helpers.

use crate::matching::{fold, split_credits};
use crate::scraper::normalize;
use rusqlite::{params, Connection};

pub const LATEST: i64 = 2;

pub fn open(path: &str) -> rusqlite::Result<Connection> {
    let existed = std::fs::metadata(path)
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    let conn = Connection::open(path)?;
    migrate(&conn, existed.then_some(path))?;
    Ok(conn)
}

pub fn open_memory() -> rusqlite::Result<Connection> {
    let conn = Connection::open_in_memory()?;
    migrate(&conn, None)?;
    Ok(conn)
}

fn has_column(c: &Connection, table: &str, col: &str) -> bool {
    c.prepare(&format!("PRAGMA table_info({table})"))
        .and_then(|mut st| {
            let names = st
                .query_map([], |r| r.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(names.iter().any(|n| n == col))
        })
        .unwrap_or(false)
}

fn has_table(c: &Connection, table: &str) -> bool {
    c.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
        [table],
        |_| Ok(()),
    )
    .is_ok()
}

fn add_column(c: &Connection, table: &str, col: &str, decl: &str) -> rusqlite::Result<()> {
    if !has_column(c, table, col) {
        c.execute(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"), [])?;
    }
    Ok(())
}

fn migrate(conn: &Connection, backup_of: Option<&str>) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version >= LATEST {
        return Ok(());
    }
    // Backup antes de mexer num banco que já tem dados.
    if let Some(path) = backup_of {
        if has_table(conn, "composers") {
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
            let bak = format!("{path}.bak-v{version}");
            if !std::path::Path::new(&bak).exists() {
                match std::fs::copy(path, &bak) {
                    Ok(_) => println!("backup do banco antes da migração: {bak}"),
                    Err(e) => eprintln!("aviso: não foi possível criar o backup {bak}: {e}"),
                }
            }
        }
    }
    let tx = conn.unchecked_transaction()?;
    if version < 1 {
        v1(&tx)?;
    }
    if version < 2 {
        v2(&tx)?;
    }
    tx.execute_batch(&format!("PRAGMA user_version = {LATEST}"))?;
    tx.commit()
}

/// Esquema original (idempotente: bancos antigos já têm parte das colunas).
fn v1(c: &Connection) -> rusqlite::Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS composers(
            id INTEGER PRIMARY KEY, name TEXT NOT NULL, created_at INTEGER NOT NULL,
            last_ok_at INTEGER, last_attempt_at INTEGER, last_error TEXT);
        CREATE TABLE IF NOT EXISTS terms(
            id INTEGER PRIMARY KEY,
            composer_id INTEGER NOT NULL REFERENCES composers(id) ON DELETE CASCADE,
            term TEXT NOT NULL, norm TEXT NOT NULL UNIQUE,
            is_primary INTEGER NOT NULL DEFAULT 0, baselined INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS songs(
            catalog_id TEXT PRIMARY KEY, title TEXT, show_name TEXT, artist TEXT, credits TEXT,
            date_added TEXT, medley INTEGER, uploads INTEGER, copyright TEXT, url TEXT);
        CREATE TABLE IF NOT EXISTS hits(
            composer_id INTEGER NOT NULL REFERENCES composers(id) ON DELETE CASCADE,
            catalog_id TEXT NOT NULL REFERENCES songs(catalog_id),
            found_at INTEGER NOT NULL, is_new INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(composer_id, catalog_id));",
    )?;
    add_column(c, "terms", "searchable", "INTEGER NOT NULL DEFAULT 1")?;
    add_column(c, "terms", "resume_page", "INTEGER NOT NULL DEFAULT 0")?;
    add_column(c, "terms", "total_expected", "INTEGER NOT NULL DEFAULT 0")?;
    Ok(())
}

/// Casamento por nome inteiro, links entre registros, auditoria, configurações e índices.
fn v2(c: &Connection) -> rusqlite::Result<()> {
    add_column(c, "songs", "title_fold", "TEXT NOT NULL DEFAULT ''")?;
    add_column(c, "songs", "artist_fold", "TEXT NOT NULL DEFAULT ''")?;
    add_column(c, "songs", "search_fold", "TEXT NOT NULL DEFAULT ''")?;
    add_column(c, "terms", "fold", "TEXT NOT NULL DEFAULT ''")?;
    add_column(c, "hits", "last_seen_at", "INTEGER")?;
    add_column(c, "hits", "removed_at", "INTEGER")?;
    // registros que já existiam contam como já avisados
    add_column(c, "hits", "notified", "INTEGER NOT NULL DEFAULT 1")?;
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS song_credits(
            catalog_id TEXT NOT NULL REFERENCES songs(catalog_id) ON DELETE CASCADE,
            name TEXT NOT NULL, name_fold TEXT NOT NULL,
            PRIMARY KEY(catalog_id, name_fold));
        CREATE TABLE IF NOT EXISTS runs(
            id INTEGER PRIMARY KEY, composer_id INTEGER REFERENCES composers(id) ON DELETE CASCADE,
            started_at INTEGER NOT NULL, ended_at INTEGER, trigger TEXT NOT NULL, full INTEGER NOT NULL DEFAULT 0,
            fetched INTEGER NOT NULL DEFAULT 0, new_count INTEGER NOT NULL DEFAULT 0,
            ok INTEGER, detail TEXT NOT NULL DEFAULT '');
        CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS idx_hits_composer_new ON hits(composer_id, is_new);
        CREATE INDEX IF NOT EXISTS idx_hits_catalog ON hits(catalog_id);
        CREATE INDEX IF NOT EXISTS idx_songs_date ON songs(date_added);
        CREATE INDEX IF NOT EXISTS idx_terms_composer ON terms(composer_id);
        CREATE INDEX IF NOT EXISTS idx_terms_fold ON terms(fold);
        CREATE INDEX IF NOT EXISTS idx_credits_fold ON song_credits(name_fold);
        CREATE INDEX IF NOT EXISTS idx_runs_started ON runs(started_at);
        DROP TABLE IF EXISTS log;",
    )?;

    // retro-preenchimento
    let songs: Vec<(String, String, String, String, String)> = {
        let mut st = c.prepare(
            "SELECT catalog_id, COALESCE(title,''), COALESCE(artist,''), COALESCE(show_name,''), COALESCE(credits,'') FROM songs",
        )?;
        let rows = st.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        rows.collect::<Result<_, _>>()?
    };
    for (id, title, artist, show, credits) in songs {
        set_song_folds(c, &id, &title, &artist, &show, &credits)?;
    }
    let terms: Vec<(i64, String)> = {
        let mut st = c.prepare("SELECT id, term FROM terms")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    for (id, term) in terms {
        c.execute(
            "UPDATE terms SET fold=? WHERE id=?",
            params![fold(&term), id],
        )?;
    }
    c.execute(
        "UPDATE hits SET last_seen_at=found_at WHERE last_seen_at IS NULL",
        [],
    )?;
    Ok(())
}

/// Atualiza as colunas normalizadas e a tabela `song_credits` de uma música.
pub fn set_song_folds(
    c: &Connection,
    catalog_id: &str,
    title: &str,
    artist: &str,
    show: &str,
    credits: &str,
) -> rusqlite::Result<()> {
    let search = fold(&format!("{title} {artist} {show} {credits}"));
    c.execute(
        "UPDATE songs SET title_fold=?, artist_fold=?, search_fold=? WHERE catalog_id=?",
        params![fold(title), fold(artist), search, catalog_id],
    )?;
    c.execute("DELETE FROM song_credits WHERE catalog_id=?", [catalog_id])?;
    for name in split_credits(credits) {
        c.execute(
            "INSERT OR IGNORE INTO song_credits(catalog_id, name, name_fold) VALUES (?,?,?)",
            params![catalog_id, name, fold(&name)],
        )?;
    }
    Ok(())
}

// ---------- configurações ----------

pub fn get_setting(c: &Connection, key: &str) -> Option<String> {
    c.query_row("SELECT value FROM settings WHERE key=?", [key], |r| {
        r.get(0)
    })
    .ok()
}

pub fn set_setting(c: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    if value.is_empty() {
        c.execute("DELETE FROM settings WHERE key=?", [key])?;
    } else {
        c.execute(
            "INSERT INTO settings(key, value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
    }
    Ok(())
}

pub fn setting_i64(c: &Connection, key: &str, default: i64) -> i64 {
    get_setting(c, key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ---------- compositores ----------

pub struct NewTerm {
    pub term: String,
    pub visual: bool,
}

#[derive(Debug, PartialEq)]
pub enum AddError {
    Empty,
    Duplicate,
    Db(String),
}

/// Cria um compositor. `name` é o nome em destaque; se ele não estiver entre `terms`,
/// entra como nome só visual (quando há outro nome pesquisável) ou como pesquisável (quando não há).
pub fn add_composer(c: &mut Connection, name: &str, terms: &[NewTerm]) -> Result<i64, AddError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AddError::Empty);
    }
    let mut list: Vec<(String, bool)> = Vec::new(); // (termo, pesquisável)
    for t in terms {
        let t_name = t.term.trim();
        if !t_name.is_empty() && !list.iter().any(|(x, _)| normalize(x) == normalize(t_name)) {
            list.push((t_name.to_string(), !t.visual));
        }
    }
    let featured_listed = list.iter().any(|(x, _)| normalize(x) == normalize(name));
    if !featured_listed {
        let any_searchable = list.iter().any(|(_, s)| *s);
        list.insert(0, (name.to_string(), !any_searchable));
    }
    if !list.iter().any(|(_, s)| *s) {
        // todos marcados como só visuais: o destaque precisa ser pesquisável
        if let Some(first) = list
            .iter_mut()
            .find(|(x, _)| normalize(x) == normalize(name))
        {
            first.1 = true;
        }
    }
    let db_err = |e: rusqlite::Error| AddError::Db(e.to_string());
    let tx = c.transaction().map_err(db_err)?;
    tx.execute(
        "INSERT INTO composers(name, created_at) VALUES (?,?)",
        params![name, crate::error::now()],
    )
    .map_err(db_err)?;
    let id = tx.last_insert_rowid();
    for (term, searchable) in &list {
        let primary = normalize(term) == normalize(name);
        tx.execute(
            "INSERT INTO terms(composer_id, term, norm, fold, is_primary, searchable) VALUES (?,?,?,?,?,?)",
            params![id, term, normalize(term), fold(term), primary, searchable],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation => {
                AddError::Duplicate
            }
            other => AddError::Db(other.to_string()),
        })?;
    }
    tx.commit().map_err(db_err)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migracao_cria_esquema_e_e_idempotente() {
        let c = open_memory().unwrap();
        let v: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, LATEST);
        migrate(&c, None).unwrap();
    }

    #[test]
    fn migra_banco_antigo_preservando_dados() {
        let c = Connection::open_in_memory().unwrap();
        v1(&c).unwrap();
        c.execute(
            "INSERT INTO composers(id,name,created_at) VALUES (1,'Dua Lipa',0)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO terms(composer_id,term,norm,is_primary,baselined,resume_page) VALUES (1,'Dua Lipa','dua lipa',1,1,3)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO songs(catalog_id,title,artist,credits) VALUES ('S1','Pixie Dust','','Dua Lipa, Ali Tamposi')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO hits(composer_id,catalog_id,found_at,is_new) VALUES (1,'S1',100,1)",
            [],
        )
        .unwrap();
        migrate(&c, None).unwrap();
        let (fold_t, resume): (String, i64) = c
            .query_row("SELECT fold, resume_page FROM terms WHERE id=1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((fold_t.as_str(), resume), ("dua lipa", 3));
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM song_credits WHERE catalog_id='S1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);
        let (seen, notified): (i64, i64) = c
            .query_row("SELECT last_seen_at, notified FROM hits", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((seen, notified), (100, 1));
    }

    #[test]
    fn destaque_fora_da_lista_vira_so_visual() {
        let mut c = open_memory().unwrap();
        let id = add_composer(
            &mut c,
            "Tinashe",
            &[NewTerm {
                term: "Tinashe Kachingwe".into(),
                visual: false,
            }],
        )
        .unwrap();
        let rows: Vec<(String, i64, i64)> = c
            .prepare(
                "SELECT term, is_primary, searchable FROM terms WHERE composer_id=? ORDER BY id",
            )
            .unwrap()
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(rows.contains(&("Tinashe".into(), 1, 0)));
        assert!(rows.contains(&("Tinashe Kachingwe".into(), 0, 1)));
    }

    #[test]
    fn nome_duplicado_e_recusado() {
        let mut c = open_memory().unwrap();
        add_composer(&mut c, "Dua Lipa", &[]).unwrap();
        assert_eq!(
            add_composer(&mut c, "dua  lipa", &[]),
            Err(AddError::Duplicate)
        );
        assert_eq!(add_composer(&mut c, "  ", &[]), Err(AddError::Empty));
    }
}
