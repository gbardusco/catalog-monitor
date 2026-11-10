//! API HTTP usada pela página.

use crate::{
    app::App,
    db::{self, AddError, NewTerm},
    error::{bad, internal, not_found, now, ApiError, ApiResult},
    matching::{fold, split_credits},
    notify, scan,
    scraper::{self, normalize},
};
use axum::{
    extract::{Path, Query, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Response,
    },
    routing::{delete, get, post},
    Json, Router,
};
use rusqlite::{params, params_from_iter, types::Value as SqlValue, Connection};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, convert::Infallible, sync::Arc};
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

type S = State<Arc<App>>;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/feed.xml", get(feed))
        .route("/api/events", get(events))
        .route("/api/composers", get(list_composers).post(add_composer))
        .route("/api/composers/:id", delete(del_composer))
        .route("/api/composers/:id/aliases", post(add_alias))
        .route("/api/composers/:id/featured", post(set_featured))
        .route("/api/composers/:id/merge", post(merge))
        .route("/api/composers/:id/scan", post(scan_one))
        .route("/api/composers/:id/songs", get(songs))
        .route("/api/composers/:id/connections", get(connections))
        .route("/api/composers/:id/export", get(export_one))
        .route("/api/aliases/:id", delete(del_alias).patch(patch_alias))
        .route("/api/scan", post(scan_all))
        .route("/api/new", get(list_new))
        .route("/api/runs", get(runs))
        .route("/api/export", get(export_all))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/notify/test", post(notify_test))
        .layer(middleware::from_fn(guard))
        .with_state(app)
}

// ---------- segurança local ----------

/// Só aceita requisições cujo `Host`/`Origin` sejam locais (bloqueia CSRF e DNS rebinding).
async fn guard(req: Request, next: Next) -> Response {
    fn host_of(v: &str) -> &str {
        let v = v.split("://").last().unwrap_or(v);
        let v = v.split('/').next().unwrap_or(v);
        match v.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => h,
            _ => v,
        }
    }
    let extra = std::env::var("CATALOG_ALLOWED_HOSTS").unwrap_or_default();
    let allowed = |h: &str| {
        let h = host_of(h);
        matches!(h, "localhost" | "127.0.0.1" | "[::1]")
            || extra
                .split(',')
                .any(|x| !x.trim().is_empty() && x.trim() == h)
    };
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(allowed);
    let origin_ok = req
        .headers()
        .get(header::ORIGIN)
        .is_none_or(|v| v.to_str().map(allowed).unwrap_or(false));
    if !host_ok || !origin_ok {
        return (StatusCode::FORBIDDEN, "err.forbidden").into_response();
    }
    next.run(req).await
}

async fn index() -> impl IntoResponse {
    Html(include_str!("../static/index.html"))
}

async fn events(State(app): S) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(app.events.subscribe()).filter_map(|m| {
        m.ok()
            .map(|v| Ok::<_, Infallible>(Event::default().data(v)))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ---------- consultas compartilhadas ----------

/// Tipo de casamento de um registro com o compositor: credit | artist | title | none.
/// Espera os aliases `h` (hits) e `s` (songs). Só nomes pesquisáveis contam.
const KIND_SQL: &str = "CASE
  WHEN EXISTS(SELECT 1 FROM song_credits sc JOIN terms t ON t.fold=sc.name_fold
              WHERE sc.catalog_id=h.catalog_id AND t.composer_id=h.composer_id AND t.searchable=1) THEN 'credit'
  WHEN EXISTS(SELECT 1 FROM terms t WHERE t.composer_id=h.composer_id AND t.searchable=1 AND t.fold<>''
              AND instr(' '||s.artist_fold||' ', ' '||t.fold||' ')>0) THEN 'artist'
  WHEN EXISTS(SELECT 1 FROM terms t WHERE t.composer_id=h.composer_id AND t.searchable=1 AND t.fold<>''
              AND instr(' '||s.title_fold||' ', ' '||t.fold||' ')>0) THEN 'title'
  ELSE 'none' END";

fn all_terms_by_fold(db: &Connection) -> HashMap<String, (i64, String)> {
    let mut map = HashMap::new();
    if let Ok(mut st) = db.prepare("SELECT t.fold, c.id, c.name FROM terms t JOIN composers c ON c.id=t.composer_id WHERE t.fold<>''") {
        if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))) {
            for (f, id, name) in rows.flatten() {
                map.insert(f, (id, name));
            }
        }
    }
    map
}

fn credit_links(credits: &str, by_fold: &HashMap<String, (i64, String)>) -> Value {
    json!(split_credits(credits)
        .into_iter()
        .map(|n| {
            let owner = by_fold.get(&fold(&n)).map(|(id, _)| *id);
            json!({"name": n, "composer_id": owner})
        })
        .collect::<Vec<_>>())
}

fn query_all<P: rusqlite::Params, T>(
    db: &Connection,
    sql: &str,
    p: P,
    f: impl FnMut(&rusqlite::Row) -> rusqlite::Result<T>,
) -> Result<Vec<T>, ApiError> {
    let mut st = db.prepare(sql).map_err(internal)?;
    let rows = st.query_map(p, f).map_err(internal)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(internal)
}

// ---------- compositores ----------

async fn list_composers(State(app): S) -> ApiResult {
    let db = app.db.lock().unwrap();
    let scanning = app.scanning.lock().unwrap().clone();
    let comps = query_all(
        &db,
        "SELECT c.id, c.name, c.last_ok_at, c.last_error,
           (SELECT COUNT(*) FROM hits h WHERE h.composer_id=c.id AND h.removed_at IS NULL),
           (SELECT COUNT(*) FROM hits h WHERE h.composer_id=c.id AND h.is_new=1)
         FROM composers c ORDER BY c.name COLLATE NOCASE",
        [],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        },
    )?;
    let mut out = Vec::new();
    for (id, name, last_ok, last_error, total, new) in comps {
        let mut incomplete: Vec<Value> = Vec::new();
        let terms = query_all(
            &db,
            "SELECT id, term, is_primary, searchable, baselined, resume_page, total_expected FROM terms WHERE composer_id=? ORDER BY is_primary DESC, id",
            [id],
            |r| {
                let (term, searchable) = (r.get::<_, String>(1)?, r.get::<_, i64>(3)? != 0);
                let (baselined, resume, tot) = (r.get::<_, i64>(4)? != 0, r.get::<_, i64>(5)?, r.get::<_, i64>(6)?);
                let pending = searchable && (!baselined || resume > 0);
                let loaded = ((resume - 1).max(0) * scraper::PAGE_SIZE as i64).min(tot);
                Ok((
                    json!({"id": r.get::<_, i64>(0)?, "term": term.clone(), "primary": r.get::<_, i64>(2)? != 0, "searchable": searchable}),
                    pending.then(|| json!({"term": term, "initial": !baselined, "loaded": loaded, "total": tot})),
                ))
            },
        )?;
        let terms: Vec<Value> = terms
            .into_iter()
            .map(|(t, inc)| {
                if let Some(i) = inc {
                    incomplete.push(i);
                }
                t
            })
            .collect();
        out.push(json!({
            "id": id, "name": name, "terms": terms, "last_ok_at": last_ok, "last_error": last_error,
            "total": total, "new": new, "incomplete": incomplete,
            "scanning": scanning.get(&id).map(|p| json!({"term": p.term, "done": p.done, "total": p.total, "step": p.step, "steps": p.steps})),
        }));
    }
    Ok(Json(json!(out)))
}

#[derive(Deserialize)]
struct TermBody {
    term: String,
    #[serde(default)]
    visual: bool,
}

#[derive(Deserialize)]
struct NewComposer {
    name: String,
    #[serde(default)]
    terms: Vec<TermBody>,
}

fn add_err(e: AddError) -> ApiError {
    match e {
        AddError::Empty => bad("err.empty_name"),
        AddError::Duplicate => bad("err.duplicate"),
        AddError::Db(m) => internal(m),
    }
}

async fn add_composer(State(app): S, Json(b): Json<NewComposer>) -> ApiResult {
    let terms: Vec<NewTerm> = b
        .terms
        .into_iter()
        .map(|t| NewTerm {
            term: t.term,
            visual: t.visual,
        })
        .collect();
    let id = {
        let mut db = app.db.lock().unwrap();
        db::add_composer(&mut db, &b.name, &terms).map_err(add_err)?
    };
    spawn_scan(&app, id, false, "manual");
    app.emit();
    Ok(Json(json!({"id": id})))
}

fn spawn_scan(app: &Arc<App>, id: i64, full: bool, trigger: &'static str) {
    let app = app.clone();
    tokio::spawn(async move {
        let _ = scan::scan_composer(&app, id, full, trigger).await;
    });
}

#[derive(Deserialize)]
struct ScanQuery {
    /// "1" = recarregamento geral (todas as páginas)
    full: Option<String>,
}

async fn del_composer(State(app): S, Path(id): Path<i64>) -> ApiResult {
    {
        let db = app.db.lock().unwrap();
        db.execute("DELETE FROM composers WHERE id=?", [id])
            .map_err(internal)?;
        db.execute(
            "DELETE FROM songs WHERE catalog_id NOT IN (SELECT catalog_id FROM hits)",
            [],
        )
        .map_err(internal)?;
    }
    app.emit();
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct AliasBody {
    alias: String,
    /// true = só exibição: não busca no site nem entra no casamento de créditos
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
        let exists: bool = db
            .query_row("SELECT 1 FROM composers WHERE id=?", [id], |_| Ok(()))
            .is_ok();
        if !exists {
            return Err(not_found("err.not_found"));
        }
        db.execute(
            "INSERT INTO terms(composer_id, term, norm, fold, searchable) VALUES (?,?,?,?,?)",
            params![id, alias, normalize(alias), fold(alias), !b.visual],
        )
        .map_err(|_| bad("err.duplicate"))?;
    }
    if !b.visual {
        spawn_scan(&app, id, false, "manual");
    }
    app.emit();
    Ok(Json(json!({"ok": true})))
}

fn searchable_count(db: &Connection, composer_id: i64) -> i64 {
    db.query_row(
        "SELECT COUNT(*) FROM terms WHERE composer_id=? AND searchable=1",
        [composer_id],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

async fn del_alias(State(app): S, Path(id): Path<i64>) -> ApiResult {
    {
        let db = app.db.lock().unwrap();
        let (cid, primary, searchable): (i64, bool, bool) = db
            .query_row(
                "SELECT composer_id, is_primary, searchable FROM terms WHERE id=?",
                [id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get::<_, i64>(1)? != 0,
                        r.get::<_, i64>(2)? != 0,
                    ))
                },
            )
            .map_err(|_| not_found("err.not_found"))?;
        if primary {
            return Err(bad("err.primary_alias"));
        }
        if searchable && searchable_count(&db, cid) <= 1 {
            return Err(bad("err.last_searchable"));
        }
        db.execute("DELETE FROM terms WHERE id=?", [id])
            .map_err(internal)?;
    }
    app.emit();
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct PatchAlias {
    visual: bool,
}

/// Alterna um nome entre "pesquisável" e "só visual".
async fn patch_alias(State(app): S, Path(id): Path<i64>, Json(b): Json<PatchAlias>) -> ApiResult {
    let (cid, now_searchable) = {
        let db = app.db.lock().unwrap();
        let (cid, searchable): (i64, bool) = db
            .query_row(
                "SELECT composer_id, searchable FROM terms WHERE id=?",
                [id],
                |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0)),
            )
            .map_err(|_| not_found("err.not_found"))?;
        if b.visual && searchable && searchable_count(&db, cid) <= 1 {
            return Err(bad("err.last_searchable"));
        }
        // volta a ser pesquisável: precisa de carga inicial própria
        db.execute(
            "UPDATE terms SET baselined=CASE WHEN ?1=1 AND searchable=0 THEN 0 ELSE baselined END, searchable=?1 WHERE id=?2",
            params![!b.visual, id],
        )
        .map_err(internal)?;
        (cid, !b.visual)
    };
    if now_searchable {
        spawn_scan(&app, cid, false, "manual");
    }
    app.emit();
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct FeaturedBody {
    term_id: i64,
}

/// Escolhe qual nome fica em destaque (nome exibido do compositor).
async fn set_featured(
    State(app): S,
    Path(id): Path<i64>,
    Json(b): Json<FeaturedBody>,
) -> ApiResult {
    {
        let mut db = app.db.lock().unwrap();
        let tx = db.transaction().map_err(internal)?;
        let term: String = tx
            .query_row(
                "SELECT term FROM terms WHERE id=? AND composer_id=?",
                params![b.term_id, id],
                |r| r.get(0),
            )
            .map_err(|_| not_found("err.not_found"))?;
        tx.execute(
            "UPDATE terms SET is_primary=(id=?) WHERE composer_id=?",
            params![b.term_id, id],
        )
        .map_err(internal)?;
        tx.execute("UPDATE composers SET name=? WHERE id=?", params![term, id])
            .map_err(internal)?;
        tx.commit().map_err(internal)?;
    }
    app.emit();
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct MergeBody {
    into: i64,
}

/// Junta o compositor `id` em `into`: nomes viram aliases e os registros são unificados.
async fn merge(State(app): S, Path(id): Path<i64>, Json(b): Json<MergeBody>) -> ApiResult {
    if id == b.into {
        return Err(bad("err.same_composer"));
    }
    {
        let mut db = app.db.lock().unwrap();
        let tx = db.transaction().map_err(internal)?;
        tx.execute(
            "UPDATE terms SET composer_id=?, is_primary=0 WHERE composer_id=?",
            params![b.into, id],
        )
        .map_err(internal)?;
        tx.execute(
            "INSERT OR IGNORE INTO hits(composer_id, catalog_id, found_at, is_new, last_seen_at, removed_at, notified)
             SELECT ?, catalog_id, found_at, is_new, last_seen_at, removed_at, notified FROM hits WHERE composer_id=?",
            params![b.into, id],
        )
        .map_err(internal)?;
        tx.execute("DELETE FROM composers WHERE id=?", [id])
            .map_err(internal)?;
        tx.commit().map_err(internal)?;
    }
    app.emit();
    Ok(Json(json!({"ok": true})))
}

async fn scan_one(State(app): S, Path(id): Path<i64>, Query(q): Query<ScanQuery>) -> ApiResult {
    if app.scanning.lock().unwrap().contains_key(&id) {
        return Err(bad("err.scan_running"));
    }
    spawn_scan(&app, id, q.full.as_deref() == Some("1"), "manual");
    Ok(Json(json!({"started": true})))
}

async fn scan_all(State(app): S, Query(q): Query<ScanQuery>) -> ApiResult {
    let full = q.full.as_deref() == Some("1");
    let ids: Vec<i64> = {
        let db = app.db.lock().unwrap();
        query_all(&db, "SELECT id FROM composers", [], |r| r.get(0))?
    };
    tokio::spawn(async move {
        for id in ids {
            let _ = scan::scan_composer(&app, id, full, "manual").await;
        }
    });
    Ok(Json(json!({"started": true})))
}

// ---------- registros ----------

#[derive(Deserialize, Default)]
struct SongQuery {
    q: Option<String>,
    /// all | credit | artist | title | none
    kind: Option<String>,
    /// "1" = só novos
    new: Option<String>,
    /// "1" = esconde removidos do site
    hide_removed: Option<String>,
    /// id de outro compositor: só músicas em comum
    with: Option<i64>,
    sort: Option<String>,
    dir: Option<String>,
    page: Option<usize>,
    limit: Option<usize>,
}

async fn songs(State(app): S, Path(id): Path<i64>, Query(q): Query<SongQuery>) -> ApiResult {
    let db = app.db.lock().unwrap();
    let mut inner = format!(
        "SELECT s.catalog_id, s.title, s.show_name, s.artist, s.credits, s.date_added, s.medley, s.uploads,
                s.copyright, s.url, h.is_new, h.found_at, h.removed_at, {KIND_SQL} AS kind
         FROM hits h JOIN songs s ON s.catalog_id=h.catalog_id WHERE h.composer_id=?"
    );
    let mut args: Vec<SqlValue> = vec![id.into()];
    let needle = q.q.as_deref().map(fold).unwrap_or_default();
    if !needle.is_empty() {
        inner.push_str(" AND s.search_fold LIKE ?");
        args.push(format!("%{needle}%").into());
    }
    if q.new.as_deref() == Some("1") {
        inner.push_str(" AND h.is_new=1");
    }
    if q.hide_removed.as_deref() == Some("1") {
        inner.push_str(" AND h.removed_at IS NULL");
    }
    if let Some(other) = q.with {
        inner.push_str(" AND EXISTS(SELECT 1 FROM hits h2 WHERE h2.catalog_id=h.catalog_id AND h2.composer_id=?)");
        args.push(other.into());
    }
    let mut outer_where = String::new();
    if let Some(k) = q
        .kind
        .as_deref()
        .filter(|k| ["credit", "artist", "title", "none"].contains(k))
    {
        outer_where = " WHERE kind=?".into();
        args.push(k.to_string().into());
    }
    let dir = if q.dir.as_deref() == Some("asc") {
        "ASC"
    } else {
        "DESC"
    };
    let order = match q.sort.as_deref() {
        Some("title") => format!("title COLLATE NOCASE {dir}"),
        Some("found") => format!("found_at {dir}"),
        Some("uploads") => format!("uploads {dir}, date_added DESC"),
        Some("kind") => format!("kind {dir}, date_added DESC"),
        _ => format!("date_added {dir}, catalog_id {dir}"),
    };
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let page = q.page.unwrap_or(1).max(1);

    let total: i64 = db
        .query_row(
            &format!("SELECT COUNT(*) FROM ({inner}){outer_where}"),
            params_from_iter(args.iter()),
            |r| r.get(0),
        )
        .map_err(internal)?;
    let by_fold = all_terms_by_fold(&db);
    let mut page_args = args.clone();
    page_args.push((limit as i64).into());
    page_args.push((((page - 1) * limit) as i64).into());
    let rows = query_all(
        &db,
        &format!("SELECT * FROM ({inner}){outer_where} ORDER BY {order} LIMIT ? OFFSET ?"),
        params_from_iter(page_args.iter()),
        |r| {
            let credits: String = r.get::<_, Option<String>>(4)?.unwrap_or_default();
            Ok(json!({
                "catalog_id": r.get::<_, String>(0)?, "title": r.get::<_, Option<String>>(1)?,
                "show": r.get::<_, Option<String>>(2)?, "artist": r.get::<_, Option<String>>(3)?,
                "credit_links": credit_links(&credits, &by_fold), "credits": credits,
                "date_added": r.get::<_, Option<String>>(5)?, "medley": r.get::<_, i64>(6)? != 0,
                "uploads": r.get::<_, i64>(7)?, "copyright": r.get::<_, Option<String>>(8)?,
                "url": r.get::<_, Option<String>>(9)?, "is_new": r.get::<_, i64>(10)? != 0,
                "found_at": r.get::<_, i64>(11)?, "removed": r.get::<_, Option<i64>>(12)?.is_some(),
                "kind": r.get::<_, String>(13)?,
            }))
        },
    )?;

    // outros compositores monitorados que também têm cada música
    let ids: Vec<String> = rows
        .iter()
        .filter_map(|v| v["catalog_id"].as_str().map(String::from))
        .collect();
    let mut also: HashMap<String, Vec<Value>> = HashMap::new();
    if !ids.is_empty() {
        let marks = vec!["?"; ids.len()].join(",");
        let mut a: Vec<SqlValue> = vec![id.into()];
        a.extend(ids.iter().map(|s| SqlValue::from(s.clone())));
        let pairs = query_all(
            &db,
            &format!("SELECT h.catalog_id, c.id, c.name FROM hits h JOIN composers c ON c.id=h.composer_id WHERE h.composer_id<>? AND h.catalog_id IN ({marks})"),
            params_from_iter(a.iter()),
            |r| Ok((r.get::<_, String>(0)?, json!({"id": r.get::<_, i64>(1)?, "name": r.get::<_, String>(2)?}))),
        )?;
        for (cid, v) in pairs {
            also.entry(cid).or_default().push(v);
        }
    }
    let items: Vec<Value> = rows
        .into_iter()
        .map(|mut v| {
            let cid = v["catalog_id"].as_str().unwrap_or("").to_string();
            v["also"] = json!(also.get(&cid).cloned().unwrap_or_default());
            v
        })
        .collect();
    Ok(Json(
        json!({"total": total, "page": page, "limit": limit, "items": items}),
    ))
}

async fn list_new(State(app): S) -> ApiResult {
    let db = app.db.lock().unwrap();
    let by_fold = all_terms_by_fold(&db);
    let rows = query_all(
        &db,
        &format!(
            "SELECT h.composer_id, c.name, s.catalog_id, s.title, s.credits, s.artist, s.date_added, s.url, h.found_at, {KIND_SQL}
             FROM hits h JOIN songs s ON s.catalog_id=h.catalog_id JOIN composers c ON c.id=h.composer_id
             WHERE h.is_new=1 ORDER BY h.found_at DESC, s.date_added DESC, s.catalog_id DESC"
        ),
        [],
        |r| {
            let credits: String = r.get::<_, Option<String>>(4)?.unwrap_or_default();
            Ok(json!({
                "composer_id": r.get::<_, i64>(0)?, "composer": r.get::<_, String>(1)?, "catalog_id": r.get::<_, String>(2)?,
                "title": r.get::<_, Option<String>>(3)?, "credit_links": credit_links(&credits, &by_fold), "credits": credits,
                "artist": r.get::<_, Option<String>>(5)?, "date_added": r.get::<_, Option<String>>(6)?,
                "url": r.get::<_, Option<String>>(7)?, "found_at": r.get::<_, i64>(8)?, "kind": r.get::<_, String>(9)?,
            }))
        },
    )?;
    Ok(Json(json!(rows)))
}

/// Painel de conexões: músicas em comum com outros compositores e co-autores frequentes.
async fn connections(State(app): S, Path(id): Path<i64>) -> ApiResult {
    let db = app.db.lock().unwrap();
    let shared = query_all(
        &db,
        "SELECT c2.id, c2.name, COUNT(*) FROM hits h1
         JOIN hits h2 ON h2.catalog_id=h1.catalog_id AND h2.composer_id<>h1.composer_id
         JOIN composers c2 ON c2.id=h2.composer_id
         WHERE h1.composer_id=? AND h1.removed_at IS NULL AND h2.removed_at IS NULL
         GROUP BY c2.id ORDER BY 3 DESC, c2.name COLLATE NOCASE",
        [id],
        |r| {
            Ok(
                json!({"composer_id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?, "count": r.get::<_, i64>(2)?}),
            )
        },
    )?;
    let cowriters = query_all(
        &db,
        "SELECT sc.name, COUNT(*) n FROM hits h JOIN song_credits sc ON sc.catalog_id=h.catalog_id
         WHERE h.composer_id=?1 AND h.removed_at IS NULL
           AND EXISTS(SELECT 1 FROM song_credits sc2 JOIN terms t ON t.fold=sc2.name_fold
                      WHERE sc2.catalog_id=h.catalog_id AND t.composer_id=?1 AND t.searchable=1)
           AND sc.name_fold NOT IN (SELECT fold FROM terms WHERE fold<>'')
         GROUP BY sc.name_fold ORDER BY n DESC, sc.name LIMIT 15",
        [id],
        |r| Ok(json!({"name": r.get::<_, String>(0)?, "count": r.get::<_, i64>(1)?})),
    )?;
    Ok(Json(json!({"shared": shared, "cowriters": cowriters})))
}

// ---------- auditoria ----------

async fn runs(State(app): S) -> ApiResult {
    let db = app.db.lock().unwrap();
    let rows = query_all(
        &db,
        "SELECT r.id, COALESCE(c.name,'-'), r.started_at, r.ended_at, r.trigger, r.full, r.fetched, r.new_count, r.ok, r.detail
         FROM runs r LEFT JOIN composers c ON c.id=r.composer_id ORDER BY r.id DESC LIMIT 100",
        [],
        |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?, "composer": r.get::<_, String>(1)?, "started_at": r.get::<_, i64>(2)?,
                "ended_at": r.get::<_, Option<i64>>(3)?, "trigger": r.get::<_, String>(4)?, "full": r.get::<_, i64>(5)? != 0,
                "fetched": r.get::<_, i64>(6)?, "new_count": r.get::<_, i64>(7)?, "ok": r.get::<_, Option<i64>>(8)?.map(|v| v != 0),
                "detail": r.get::<_, String>(9)?,
            }))
        },
    )?;
    Ok(Json(json!(rows)))
}

// ---------- exportação e feed ----------

#[derive(Deserialize)]
struct ExportQuery {
    format: Option<String>,
}

struct ExportRow {
    composer: String,
    title: String,
    credits: String,
    artist: String,
    show: String,
    date_added: String,
    found_at: i64,
    is_new: bool,
    kind: String,
    removed: bool,
    url: String,
}

fn export_rows(db: &Connection, composer: Option<i64>) -> Result<Vec<ExportRow>, ApiError> {
    query_all(
        db,
        &format!(
            "SELECT c.name, COALESCE(s.title,''), COALESCE(s.credits,''), COALESCE(s.artist,''), COALESCE(s.show_name,''),
                    COALESCE(s.date_added,''), h.found_at, h.is_new, h.removed_at IS NOT NULL, COALESCE(s.url,''), {KIND_SQL}
             FROM hits h JOIN songs s ON s.catalog_id=h.catalog_id JOIN composers c ON c.id=h.composer_id
             WHERE (?1 IS NULL OR h.composer_id=?1)
             ORDER BY c.name COLLATE NOCASE, s.date_added DESC, s.catalog_id DESC"
        ),
        [composer],
        |r| {
            Ok(ExportRow {
                composer: r.get(0)?,
                title: r.get(1)?,
                credits: r.get(2)?,
                artist: r.get(3)?,
                show: r.get(4)?,
                date_added: r.get(5)?,
                found_at: r.get(6)?,
                is_new: r.get::<_, i64>(7)? != 0,
                removed: r.get::<_, i64>(8)? != 0,
                url: r.get(9)?,
                kind: r.get(10)?,
            })
        },
    )
}

fn csv_field(s: &str) -> String {
    // também neutraliza injeção de fórmula em planilhas (=, +, -, @ no início)
    let s = if s.starts_with(['=', '+', '-', '@']) {
        format!("'{s}")
    } else {
        s.to_string()
    };
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

fn export_response(rows: Vec<ExportRow>, format: Option<&str>, name: &str) -> Response {
    if format == Some("json") {
        let v: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({"composer": r.composer, "title": r.title, "credits": r.credits, "artist": r.artist, "show": r.show,
                       "date_added": r.date_added, "found_at": r.found_at, "new": r.is_new, "match": r.kind,
                       "removed": r.removed, "url": r.url})
            })
            .collect();
        return (
            [
                (header::CONTENT_TYPE, "application/json".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{name}.json\""),
                ),
            ],
            serde_json::to_string_pretty(&v).unwrap_or_default(),
        )
            .into_response();
    }
    let mut out = String::from(
        "\u{feff}composer,title,credits,artist,show,date_added,found_at,new,match,removed,url\n",
    );
    for r in rows {
        let found = chrono::DateTime::from_timestamp(r.found_at, 0)
            .map(|d| d.to_rfc3339())
            .unwrap_or_default();
        let cols = [
            csv_field(&r.composer),
            csv_field(&r.title),
            csv_field(&r.credits),
            csv_field(&r.artist),
            csv_field(&r.show),
            csv_field(&r.date_added),
            found,
            r.is_new.to_string(),
            r.kind.clone(),
            r.removed.to_string(),
            csv_field(&r.url),
        ];
        out.push_str(&cols.join(","));
        out.push('\n');
    }
    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}.csv\""),
            ),
        ],
        out,
    )
        .into_response()
}

async fn export_one(
    State(app): S,
    Path(id): Path<i64>,
    Query(q): Query<ExportQuery>,
) -> Result<Response, ApiError> {
    let db = app.db.lock().unwrap();
    let rows = export_rows(&db, Some(id))?;
    Ok(export_response(
        rows,
        q.format.as_deref(),
        "catalog-monitor-composer",
    ))
}

async fn export_all(State(app): S, Query(q): Query<ExportQuery>) -> Result<Response, ApiError> {
    let db = app.db.lock().unwrap();
    let rows = export_rows(&db, None)?;
    Ok(export_response(
        rows,
        q.format.as_deref(),
        "catalog-monitor",
    ))
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Feed Atom com os registros novos mais recentes (qualquer leitor RSS lê).
async fn feed(State(app): S) -> Result<Response, ApiError> {
    let db = app.db.lock().unwrap();
    let rows = query_all(
        &db,
        "SELECT h.composer_id, c.name, s.catalog_id, COALESCE(s.title,''), COALESCE(s.credits,''), COALESCE(s.url,''), h.found_at
         FROM hits h JOIN songs s ON s.catalog_id=h.catalog_id JOIN composers c ON c.id=h.composer_id
         WHERE h.is_new=1 AND h.removed_at IS NULL ORDER BY h.found_at DESC, s.catalog_id DESC LIMIT 100",
        [],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, i64>(6)?)),
    )?;
    let rfc = |t: i64| {
        chrono::DateTime::from_timestamp(t, 0)
            .map(|d| d.to_rfc3339())
            .unwrap_or_default()
    };
    let updated = rows.first().map(|r| rfc(r.6)).unwrap_or_else(|| rfc(now()));
    let mut out = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<feed xmlns=\"http://www.w3.org/2005/Atom\">\n<title>Catalog Monitor</title>\n<id>tag:catalog-monitor,2026:feed</id>\n<updated>{updated}</updated>\n"
    );
    for (cid, name, sid, title, credits, url, found) in rows {
        out.push_str(&format!(
            "<entry><title>{}</title><id>tag:catalog-monitor,2026:{cid}:{}</id><updated>{}</updated><link href=\"{}\"/><summary>{}</summary></entry>\n",
            xml(&format!("{name}: {title}")), xml(&sid), rfc(found), xml(&url), xml(&credits)
        ));
    }
    out.push_str("</feed>\n");
    Ok((
        [(header::CONTENT_TYPE, "application/atom+xml; charset=utf-8")],
        out,
    )
        .into_response())
}

// ---------- configurações e avisos ----------

fn settings_json(db: &Connection) -> Value {
    let ch = notify::Channels::load(db);
    json!({
        "interval_hours": db::setting_i64(db, "interval_hours", 24),
        "delay_ms": db::setting_i64(db, "delay_ms", 1200),
        "desktop": ch.desktop,
        // segredos nunca saem do servidor: só "configurado: sim/não"
        "discord": ch.discord.is_some(), "telegram": ch.telegram.is_some(),
        "ntfy": ch.ntfy.is_some(), "webhook": ch.webhook.is_some(),
    })
}

async fn get_settings(State(app): S) -> ApiResult {
    let db = app.db.lock().unwrap();
    Ok(Json(settings_json(&db)))
}

/// Corpo: mapa chave -> valor. `null`/"" apaga a chave; chaves ausentes ficam como estão.
async fn put_settings(State(app): S, Json(b): Json<HashMap<String, Value>>) -> ApiResult {
    let db = app.db.lock().unwrap();
    for (k, v) in &b {
        let (key, val): (&str, String) = match (k.as_str(), v) {
            ("interval_hours", Value::Number(n)) => (
                "interval_hours",
                n.as_i64().unwrap_or(24).clamp(1, 720).to_string(),
            ),
            ("delay_ms", Value::Number(n)) => (
                "delay_ms",
                n.as_i64().unwrap_or(1200).clamp(200, 60_000).to_string(),
            ),
            ("desktop", Value::Bool(on)) => {
                ("notify_desktop", if *on { "1".into() } else { "0".into() })
            }
            ("discord" | "ntfy" | "webhook", Value::String(s)) => {
                let s = s.trim();
                if !s.is_empty() && !(s.starts_with("https://") || s.starts_with("http://")) {
                    return Err(bad("err.invalid_url"));
                }
                (
                    if k == "discord" {
                        "notify_discord"
                    } else if k == "ntfy" {
                        "notify_ntfy"
                    } else {
                        "notify_webhook"
                    },
                    s.to_string(),
                )
            }
            ("discord" | "ntfy" | "webhook" | "telegram_token" | "telegram_chat", Value::Null) => {
                let key = match k.as_str() {
                    "discord" => "notify_discord",
                    "ntfy" => "notify_ntfy",
                    "webhook" => "notify_webhook",
                    "telegram_token" => "notify_telegram_token",
                    _ => "notify_telegram_chat",
                };
                (key, String::new())
            }
            ("telegram_token", Value::String(s)) => ("notify_telegram_token", s.trim().to_string()),
            ("telegram_chat", Value::String(s)) => ("notify_telegram_chat", s.trim().to_string()),
            _ => return Err(bad("err.invalid_setting")),
        };
        db::set_setting(&db, key, &val).map_err(internal)?;
    }
    Ok(Json(settings_json(&db)))
}

async fn notify_test(State(app): S) -> ApiResult {
    let channels = {
        let db = app.db.lock().unwrap();
        notify::Channels::load(&db)
    };
    let sample = notify::Record {
        title: "Catalog Monitor — test".into(),
        url: "https://www.arrangeme.com".into(),
        credits: "Test".into(),
        date_added: String::new(),
    };
    let out = notify::dispatch(&app.out, &channels, "Test", &[sample]).await;
    Ok(Json(
        json!({"sent": out.sent, "failed": out.failed.iter().map(|(c, e)| json!({"channel": c, "error": e})).collect::<Vec<_>>()}),
    ))
}
