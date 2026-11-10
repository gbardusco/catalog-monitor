mod scraper;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{delete, get, post},
    Json, Router,
};
use rusqlite::{params, Connection};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

const CHECK_INTERVAL_SECS: i64 = 24 * 3600;
const RETRY_AFTER_ERROR_SECS: i64 = 3600;

struct App {
    db: Mutex<Connection>,
    http: reqwest::Client,
    /// compositor -> (termo atual, baixados, total esperado, termo n, de m)
    scanning: Mutex<HashMap<i64, Progress>>,
}
#[derive(Clone, Default)]
struct Progress {
    term: String,
    done: usize,
    total: usize,
    step: usize,
    steps: usize,
}
type S = State<Arc<App>>;
type ApiResult = Result<Json<Value>, (StatusCode, String)>;

fn err500<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
fn bad<T: Into<String>>(m: T) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, m.into())
}
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn init_db(c: &Connection) -> rusqlite::Result<()> {
    init_schema(c)?;
    // migração: nomes "só visuais" (não entram na busca nem na checagem de créditos)
    let _ = c.execute("ALTER TABLE terms ADD COLUMN searchable INTEGER NOT NULL DEFAULT 1", []);
    // retomada: resume_page = próxima página a buscar (0 = nada pendente)
    let _ = c.execute("ALTER TABLE terms ADD COLUMN resume_page INTEGER NOT NULL DEFAULT 0", []);
    let _ = c.execute("ALTER TABLE terms ADD COLUMN total_expected INTEGER NOT NULL DEFAULT 0", []);
    Ok(())
}

fn init_schema(c: &Connection) -> rusqlite::Result<()> {
    c.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
        CREATE TABLE IF NOT EXISTS composers(
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
            PRIMARY KEY(composer_id, catalog_id));
        CREATE TABLE IF NOT EXISTS log(
            id INTEGER PRIMARY KEY, at INTEGER NOT NULL, composer_id INTEGER, msg TEXT NOT NULL);",
    )
}

fn log(db: &Connection, composer_id: i64, msg: &str) {
    let _ = db.execute(
        "INSERT INTO log(at, composer_id, msg) VALUES (?,?,?)",
        params![now(), composer_id, msg],
    );
    println!("[{}] composer {composer_id}: {msg}", chrono::Local::now().format("%F %T"));
}

// ---------- varredura ----------

/// Notificação nativa do desktop (Linux, via `notify-send`); falha em silêncio se não existir.
fn desktop_notify(composer: &str, titles: &[String]) {
    let lang = std::env::var("CATALOG_LANG")
        .or_else(|_| std::env::var("LANG"))
        .unwrap_or_default()
        .to_lowercase();
    let (many, more) = if lang.starts_with("pt") {
        ("novo(s) registro(s)", "e mais")
    } else if lang.starts_with("es") {
        ("nuevo(s) registro(s)", "y")
    } else {
        ("new record(s)", "and")
    };
    let head = format!("{composer}: {} {many}", titles.len());
    let mut body = titles.iter().take(5).cloned().collect::<Vec<_>>().join("\n");
    if titles.len() > 5 {
        body.push_str(&format!("\n… {more} {}", titles.len() - 5));
    }
    let _ = std::process::Command::new("notify-send")
        .args(["-a", "Catalog Monitor", "-i", "audio-x-generic", &head, &body])
        .spawn();
}

async fn scan_composer(app: &Arc<App>, id: i64, full: bool) -> Result<String, String> {
    {
        let mut sc = app.scanning.lock().unwrap();
        if sc.contains_key(&id) {
            return Err("err.scan_running".into());
        }
        sc.insert(id, Progress::default());
    }
    let res = scan_inner(app, id, full).await;
    app.scanning.lock().unwrap().remove(&id);
    res
}

struct TermRow {
    id: i64,
    term: String,
    baselined: bool,
    resume_page: usize,
    total_expected: usize,
}

/// `full` = recarregamento geral: percorre todas as páginas, atualizando todos os campos.
/// Sem `full`, uma verificação "limpa" para na primeira página sem nenhum registro novo;
/// uma carga interrompida (resume_page > 0) continua de onde parou.
async fn scan_inner(app: &Arc<App>, id: i64, full: bool) -> Result<String, String> {
    let terms: Vec<TermRow> = {
        let db = app.db.lock().unwrap();
        let mut st = db
            .prepare("SELECT id, term, baselined, resume_page, total_expected FROM terms WHERE composer_id=? AND searchable=1 ORDER BY is_primary DESC, id")
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map([id], |r| {
                Ok(TermRow {
                    id: r.get(0)?,
                    term: r.get(1)?,
                    baselined: r.get::<_, i64>(2)? != 0,
                    resume_page: r.get::<_, i64>(3)?.max(0) as usize,
                    total_expected: r.get::<_, i64>(4)?.max(0) as usize,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        rows
    };
    if terms.is_empty() {
        return Err("err.not_found".into());
    }
    {
        let db = app.db.lock().unwrap();
        db.execute("UPDATE composers SET last_attempt_at=? WHERE id=?", params![now(), id]).ok();
    }

    let (mut total_new, mut errors) = (0usize, Vec::new());
    let mut new_titles: Vec<String> = Vec::new();
    let steps = terms.len();
    for (i, tr) in terms.into_iter().enumerate() {
        let TermRow { id: term_id, term, baselined, resume_page, total_expected } = tr;
        let clean = baselined && resume_page == 0 && !full;
        // recua 1 página ao retomar (o site pode ter deslocado itens); duplicatas são ignoradas
        let start = if full || resume_page <= 1 { 1 } else { resume_page - 1 };
        {
            let db = app.db.lock().unwrap();
            db.execute("UPDATE terms SET resume_page=MAX(resume_page,1) WHERE id=?", [term_id]).ok();
        }
        let progress = |done: usize, total: usize| {
            app.scanning
                .lock()
                .unwrap()
                .insert(id, Progress { term: term.clone(), done, total, step: i + 1, steps });
        };
        progress((start - 1) * scraper::PAGE_SIZE, total_expected);

        let (mut new_here, mut fetched) = (0usize, 0usize);
        let res = {
            let mut on_page = |page: usize, total: usize, songs: &[scraper::Song]| -> Result<bool, String> {
                let mut db = app.db.lock().unwrap();
                let tx = db.transaction().map_err(|e| e.to_string())?;
                let mut page_new = 0usize;
                for s in songs {
                    tx.execute(
                        "INSERT INTO songs VALUES (?,?,?,?,?,?,?,?,?,?)
                         ON CONFLICT(catalog_id) DO UPDATE SET title=excluded.title, show_name=excluded.show_name,
                           artist=excluded.artist, credits=excluded.credits, date_added=excluded.date_added,
                           medley=excluded.medley, uploads=excluded.uploads, copyright=excluded.copyright",
                        params![s.catalog_id, s.title, s.show, s.artist, s.credits, s.date_added,
                                s.medley, s.uploads, s.copyright, s.url],
                    )
                    .map_err(|e| e.to_string())?;
                    // Na carga inicial de um termo (baseline) nada é "novo".
                    let n = tx
                        .execute(
                            "INSERT OR IGNORE INTO hits(composer_id, catalog_id, found_at, is_new) VALUES (?,?,?,?)",
                            params![id, s.catalog_id, now(), baselined],
                        )
                        .map_err(|e| e.to_string())?;
                    if n > 0 {
                        page_new += 1;
                        if baselined {
                            new_titles.push(s.title.clone());
                        }
                    }
                }
                // checkpoint: a próxima retomada começa depois desta página
                tx.execute(
                    "UPDATE terms SET resume_page=?, total_expected=? WHERE id=?",
                    params![page + 1, total, term_id],
                )
                .map_err(|e| e.to_string())?;
                tx.commit().map_err(|e| e.to_string())?;
                drop(db);
                if baselined {
                    new_here += page_new;
                }
                fetched += songs.len();
                progress(((page - 1) * scraper::PAGE_SIZE + songs.len()).min(total.max(1)), total);
                Ok(!(clean && page_new == 0))
            };
            scraper::search_pages(&app.http, &term, start, &mut on_page).await
        };
        match res {
            Err(e) => errors.push(e), // checkpoint fica salvo: dá para continuar depois
            Ok(()) => {
                let db = app.db.lock().unwrap();
                db.execute("UPDATE terms SET baselined=1, resume_page=0 WHERE id=?", [term_id]).map_err(|e| e.to_string())?;
                total_new += new_here;
                // mensagens do log são códigos "chave|arg|arg", traduzidas na página
                log(&db, id, &if baselined {
                    format!("scan.incremental|{term}|{fetched}|{new_here}")
                } else {
                    format!("scan.initial|{term}|{fetched}")
                });
            }
        }
    }
    let db = app.db.lock().unwrap();
    if !new_titles.is_empty() {
        let name: String = db
            .query_row("SELECT name FROM composers WHERE id=?", [id], |r| r.get(0))
            .unwrap_or_default();
        desktop_notify(&name, &new_titles);
    }
    if errors.is_empty() {
        db.execute("UPDATE composers SET last_ok_at=?, last_error=NULL WHERE id=?", params![now(), id]).ok();
        Ok(format!("{total_new} new"))
    } else {
        let msg = errors.join("; ");
        db.execute("UPDATE composers SET last_error=? WHERE id=?", params![msg, id]).ok();
        log(&db, id, &format!("scan.error|{msg}"));
        Err(msg)
    }
}

async fn scheduler(app: Arc<App>) {
    let mut first = true;
    loop {
        let due: Vec<i64> = {
            let db = app.db.lock().unwrap();
            let t = now();
            // Na partida, retoma cargas interrompidas na hora; depois respeita a janela de retentativa.
            let retry_before = if first { i64::MAX } else { t - RETRY_AFTER_ERROR_SECS };
            let mut st = db
                .prepare(
                    "SELECT id FROM composers c WHERE
                       (last_ok_at IS NULL OR last_ok_at < ?1 OR EXISTS(
                          SELECT 1 FROM terms x WHERE x.composer_id=c.id AND x.searchable=1
                            AND (x.baselined=0 OR x.resume_page>0))) AND
                       (last_attempt_at IS NULL OR last_attempt_at < ?2)",
                )
                .unwrap();
            let rows = st
                .query_map(params![t - CHECK_INTERVAL_SECS, retry_before], |r| r.get(0))
                .unwrap()
                .filter_map(Result::ok)
                .collect();
            rows
        };
        first = false;
        for id in due {
            let _ = scan_composer(&app, id, false).await;
        }
        tokio::time::sleep(Duration::from_secs(600)).await;
    }
}

// ---------- API ----------

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

async fn list_composers(State(app): S) -> ApiResult {
    let db = app.db.lock().unwrap();
    let scanning = app.scanning.lock().unwrap().clone();
    let mut st = db.prepare(
        "SELECT c.id, c.name, c.last_ok_at, c.last_error,
           (SELECT COUNT(*) FROM hits h WHERE h.composer_id=c.id),
           (SELECT COUNT(*) FROM hits h WHERE h.composer_id=c.id AND h.is_new=1)
         FROM composers c ORDER BY c.name COLLATE NOCASE",
    ).map_err(err500)?;
    let mut out = Vec::new();
    let rows = st
        .query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<String>>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?))
        })
        .map_err(err500)?;
    for row in rows {
        let (id, name, last_ok, last_error, total, new) = row.map_err(err500)?;
        let mut ts = db
            .prepare("SELECT id, term, is_primary, searchable, baselined, resume_page, total_expected FROM terms WHERE composer_id=? ORDER BY is_primary DESC, id")
            .map_err(err500)?;
        let mut incomplete: Vec<Value> = Vec::new();
        let aliases: Vec<Value> = ts
            .query_map([id], |r| {
                let (term, searchable) = (r.get::<_, String>(1)?, r.get::<_, i64>(3)? != 0);
                let (baselined, resume, total) = (r.get::<_, i64>(4)? != 0, r.get::<_, i64>(5)?, r.get::<_, i64>(6)?);
                // pendente: carga inicial não concluída ou verificação interrompida no meio
                let pending = searchable && (!baselined || resume > 0);
                let loaded = ((resume - 1).max(0) * scraper::PAGE_SIZE as i64).min(total);
                Ok((
                    json!({"id": r.get::<_,i64>(0)?, "term": term.clone(), "primary": r.get::<_,i64>(2)? != 0, "searchable": searchable}),
                    pending.then(|| json!({"term": term, "initial": !baselined, "loaded": loaded, "total": total})),
                ))
            })
            .map_err(err500)?
            .filter_map(Result::ok)
            .map(|(a, inc)| {
                if let Some(i) = inc {
                    incomplete.push(i);
                }
                a
            })
            .collect();
        out.push(json!({"id": id, "name": name, "terms": aliases, "last_ok_at": last_ok,
            "last_error": last_error, "total": total, "new": new, "incomplete": incomplete,
            "scanning": scanning.get(&id).map(|p| json!({"term": p.term, "done": p.done, "total": p.total, "step": p.step, "steps": p.steps}))}));
    }
    Ok(Json(json!(out)))
}

async fn add_composer(State(app): S, Json(b): Json<NameBody>) -> ApiResult {
    let name = b.name.trim().to_string();
    if name.is_empty() {
        return Err(bad("err.empty_name"));
    }
    let id = {
        let mut db = app.db.lock().unwrap();
        let tx = db.transaction().map_err(err500)?;
        tx.execute("INSERT INTO composers(name, created_at) VALUES (?,?)", params![name, now()]).map_err(err500)?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO terms(composer_id, term, norm, is_primary) VALUES (?,?,?,1)",
            params![id, name, scraper::normalize(&name)],
        )
        .map_err(|_| bad("err.duplicate"))?;
        tx.commit().map_err(err500)?;
        id
    };
    spawn_scan(app, id);
    Ok(Json(json!({"id": id})))
}

fn spawn_scan(app: Arc<App>, id: i64) {
    spawn_scan_mode(app, id, false);
}

fn spawn_scan_mode(app: Arc<App>, id: i64, full: bool) {
    tokio::spawn(async move {
        let _ = scan_composer(&app, id, full).await;
    });
}

#[derive(Deserialize)]
struct ScanQuery {
    /// "1" = recarregamento geral (todas as páginas)
    full: Option<String>,
}

async fn del_composer(State(app): S, Path(id): Path<i64>) -> ApiResult {
    let db = app.db.lock().unwrap();
    db.execute("DELETE FROM composers WHERE id=?", [id]).map_err(err500)?;
    db.execute("DELETE FROM songs WHERE catalog_id NOT IN (SELECT catalog_id FROM hits)", []).map_err(err500)?;
    Ok(Json(json!({"ok": true})))
}

async fn rename_composer(State(app): S, Path(id): Path<i64>, Json(b): Json<NameBody>) -> ApiResult {
    let db = app.db.lock().unwrap();
    db.execute("UPDATE composers SET name=? WHERE id=?", params![b.name.trim(), id]).map_err(err500)?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct AliasBody {
    alias: String,
    /// true = só exibição: não busca no site nem entra na checagem de créditos
    #[serde(default)]
    visual: bool,
}

async fn add_alias(State(app): S, Path(id): Path<i64>, Json(b): Json<AliasBody>) -> ApiResult {
    let alias = b.alias.trim();
    if alias.is_empty() {
        return Err(bad("err.empty_alias"));
    }
    {
        let db = app.db.lock().unwrap();
        db.execute(
            "INSERT INTO terms(composer_id, term, norm, searchable) VALUES (?,?,?,?)",
            params![id, alias, scraper::normalize(alias), !b.visual],
        )
        .map_err(|_| bad("err.duplicate"))?;
    }
    if !b.visual {
        spawn_scan(app, id);
    }
    Ok(Json(json!({"ok": true})))
}

async fn del_alias(State(app): S, Path(id): Path<i64>) -> ApiResult {
    let db = app.db.lock().unwrap();
    let n = db
        .execute("DELETE FROM terms WHERE id=? AND is_primary=0", [id])
        .map_err(err500)?;
    if n == 0 {
        return Err(bad("err.primary_alias"));
    }
    Ok(Json(json!({"ok": true})))
}

/// Junta o compositor `id` em `into`: nomes viram aliases e os registros são unificados.
#[derive(Deserialize)]
struct MergeBody {
    into: i64,
}
async fn merge(State(app): S, Path(id): Path<i64>, Json(b): Json<MergeBody>) -> ApiResult {
    if id == b.into {
        return Err(bad("err.same_composer"));
    }
    let mut db = app.db.lock().unwrap();
    let tx = db.transaction().map_err(err500)?;
    tx.execute("UPDATE terms SET composer_id=?, is_primary=0 WHERE composer_id=?", params![b.into, id]).map_err(err500)?;
    tx.execute(
        "INSERT OR IGNORE INTO hits SELECT ?, catalog_id, found_at, is_new FROM hits WHERE composer_id=?",
        params![b.into, id],
    ).map_err(err500)?;
    tx.execute("DELETE FROM composers WHERE id=?", [id]).map_err(err500)?;
    tx.commit().map_err(err500)?;
    Ok(Json(json!({"ok": true})))
}

async fn scan_one(State(app): S, Path(id): Path<i64>, Query(q): Query<ScanQuery>) -> ApiResult {
    if app.scanning.lock().unwrap().contains_key(&id) {
        return Err(bad("err.scan_running"));
    }
    spawn_scan_mode(app, id, q.full.as_deref() == Some("1"));
    Ok(Json(json!({"started": true})))
}

async fn scan_all(State(app): S, Query(q): Query<ScanQuery>) -> ApiResult {
    let full = q.full.as_deref() == Some("1");
    let ids: Vec<i64> = {
        let db = app.db.lock().unwrap();
        let mut st = db.prepare("SELECT id FROM composers").map_err(err500)?;
        let r = st.query_map([], |r| r.get(0)).map_err(err500)?.filter_map(Result::ok).collect();
        r
    };
    tokio::spawn(async move {
        for id in ids {
            let _ = scan_composer(&app, id, full).await;
        }
    });
    Ok(Json(json!({"started": true})))
}

#[derive(Deserialize)]
struct SongQuery {
    /// "1" = só músicas em que o compositor consta nos créditos
    credits_only: Option<String>,
    new_only: Option<String>,
}

async fn songs(State(app): S, Path(id): Path<i64>, Query(q): Query<SongQuery>) -> ApiResult {
    let db = app.db.lock().unwrap();
    let terms: Vec<String> = {
        let mut st = db.prepare("SELECT norm FROM terms WHERE composer_id=? AND searchable=1").map_err(err500)?;
        let r = st.query_map([id], |r| r.get(0)).map_err(err500)?.filter_map(Result::ok).collect();
        r
    };
    let mut st = db
        .prepare(
            "SELECT s.catalog_id, s.title, s.show_name, s.artist, s.credits, s.date_added, s.medley,
                    s.uploads, s.copyright, s.url, h.is_new, h.found_at
             FROM hits h JOIN songs s USING(catalog_id) WHERE h.composer_id=?
             ORDER BY s.date_added DESC, s.catalog_id DESC",
        )
        .map_err(err500)?;
    let credits_only = q.credits_only.as_deref() == Some("1");
    let new_only = q.new_only.as_deref() == Some("1");
    let mut out = Vec::new();
    let rows = st
        .query_map([id], |r| {
            let credits: String = r.get::<_, Option<String>>(4)?.unwrap_or_default();
            Ok((credits, json!({
                "catalog_id": r.get::<_,String>(0)?, "title": r.get::<_,Option<String>>(1)?,
                "show": r.get::<_,Option<String>>(2)?, "artist": r.get::<_,Option<String>>(3)?,
                "date_added": r.get::<_,Option<String>>(5)?, "medley": r.get::<_,i64>(6)? != 0,
                "uploads": r.get::<_,i64>(7)?, "copyright": r.get::<_,Option<String>>(8)?,
                "url": r.get::<_,Option<String>>(9)?, "is_new": r.get::<_,i64>(10)? != 0,
                "found_at": r.get::<_,i64>(11)?,
            })))
        })
        .map_err(err500)?;
    for row in rows {
        let (credits, mut v) = row.map_err(err500)?;
        let nc = scraper::normalize(&credits);
        let credit_match = terms.iter().any(|t| nc.contains(t.as_str()));
        if (credits_only && !credit_match) || (new_only && v["is_new"] != true) {
            continue;
        }
        v["credits"] = json!(credits);
        v["credit_match"] = json!(credit_match);
        out.push(v);
    }
    Ok(Json(json!(out)))
}

async fn list_new(State(app): S) -> ApiResult {
    let db = app.db.lock().unwrap();
    let mut terms_by: HashMap<i64, Vec<String>> = HashMap::new();
    {
        let mut st = db.prepare("SELECT composer_id, norm FROM terms WHERE searchable=1").map_err(err500)?;
        for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).map_err(err500)? {
            let (c, n) = r.map_err(err500)?;
            terms_by.entry(c).or_default().push(n);
        }
    }
    let mut st = db
        .prepare(
            "SELECT h.composer_id, c.name, s.catalog_id, s.title, s.credits, s.artist, s.date_added, s.url, h.found_at
             FROM hits h JOIN songs s USING(catalog_id) JOIN composers c ON c.id=h.composer_id
             WHERE h.is_new=1 ORDER BY h.found_at DESC, s.date_added DESC, s.catalog_id DESC",
        )
        .map_err(err500)?;
    let rows = st
        .query_map([], |r| {
            let cid: i64 = r.get(0)?;
            let credits: String = r.get::<_, Option<String>>(4)?.unwrap_or_default();
            let nc = scraper::normalize(&credits);
            let m = terms_by.get(&cid).map_or(false, |t| t.iter().any(|x| nc.contains(x.as_str())));
            Ok(json!({"composer_id": cid, "composer": r.get::<_,String>(1)?, "catalog_id": r.get::<_,String>(2)?,
                "title": r.get::<_,Option<String>>(3)?, "credits": credits, "artist": r.get::<_,Option<String>>(5)?,
                "date_added": r.get::<_,Option<String>>(6)?, "url": r.get::<_,Option<String>>(7)?,
                "found_at": r.get::<_,i64>(8)?, "credit_match": m}))
        })
        .map_err(err500)?;
    let out: Vec<Value> = rows.filter_map(Result::ok).collect();
    Ok(Json(json!(out)))
}

async fn get_log(State(app): S) -> ApiResult {
    let db = app.db.lock().unwrap();
    let mut st = db
        .prepare("SELECT l.at, COALESCE(c.name,'-'), l.msg FROM log l LEFT JOIN composers c ON c.id=l.composer_id ORDER BY l.id DESC LIMIT 100")
        .map_err(err500)?;
    let r: Vec<Value> = st
        .query_map([], |r| Ok(json!({"at": r.get::<_,i64>(0)?, "composer": r.get::<_,String>(1)?, "msg": r.get::<_,String>(2)?})))
        .map_err(err500)?
        .filter_map(Result::ok)
        .collect();
    Ok(Json(json!(r)))
}

async fn index() -> impl IntoResponse {
    Html(include_str!("../static/index.html"))
}

#[tokio::main]
async fn main() {
    let db_path = std::env::var("CATALOG_DB").unwrap_or_else(|_| "catalog.db".into());
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8787);
    let conn = Connection::open(&db_path).expect("abrir banco");
    init_db(&conn).expect("criar tabelas");
    let app = Arc::new(App { db: Mutex::new(conn), http: scraper::client(), scanning: Mutex::new(HashMap::new()) });

    tokio::spawn(scheduler(app.clone()));

    let router = Router::new()
        .route("/", get(index))
        .route("/api/composers", get(list_composers).post(add_composer))
        .route("/api/composers/:id", delete(del_composer).patch(rename_composer))
        .route("/api/composers/:id/aliases", post(add_alias))
        .route("/api/composers/:id/merge", post(merge))
        .route("/api/composers/:id/scan", post(scan_one))
        .route("/api/composers/:id/songs", get(songs))
        .route("/api/aliases/:id", delete(del_alias))
        .route("/api/scan", post(scan_all))
        .route("/api/log", get(get_log))
        .route("/api/new", get(list_new))
        .with_state(app);

    let addr = format!("127.0.0.1:{port}");
    println!("Catalog Monitor rodando em http://{addr}  (banco: {db_path})");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    axum::serve(listener, router).await.unwrap();
}
