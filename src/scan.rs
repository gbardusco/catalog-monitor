//! Varredura de compositores: carga inicial, verificação incremental, retomada e agendador.

use crate::error::now;
use crate::{
    app::{App, Progress},
    db, notify,
    scraper::{self, Song},
};
use rusqlite::params;
use std::{sync::Arc, time::Duration};

const RETRY_AFTER_ERROR_SECS: i64 = 3600;

struct TermRow {
    id: i64,
    term: String,
    baselined: bool,
    resume_page: usize,
    total_expected: usize,
}

fn stamp() -> String {
    chrono::Local::now().format("%F %T").to_string()
}

/// `full` = recarregamento geral. `trigger`: "auto" | "manual" | "resume".
pub async fn scan_composer(
    app: &Arc<App>,
    id: i64,
    full: bool,
    trigger: &str,
) -> Result<String, String> {
    {
        let mut sc = app.scanning.lock().unwrap();
        if sc.contains_key(&id) {
            return Err("err.scan_running".into());
        }
        sc.insert(id, Progress::default());
    }
    app.emit();
    let res = scan_inner(app, id, full, trigger).await;
    app.scanning.lock().unwrap().remove(&id);
    app.emit();
    res
}

/// Sem `full`, uma verificação "limpa" para na primeira página sem nenhum registro novo;
/// uma carga interrompida (resume_page > 0) continua de onde parou.
async fn scan_inner(app: &Arc<App>, id: i64, full: bool, trigger: &str) -> Result<String, String> {
    let terms: Vec<TermRow> = {
        let db = app.db.lock().unwrap();
        let mut st = db
            .prepare("SELECT id, term, baselined, resume_page, total_expected FROM terms WHERE composer_id=? AND searchable=1 ORDER BY id")
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
    let started = now();
    let delay = app.delay();
    let run_id = {
        let db = app.db.lock().unwrap();
        db.execute(
            "UPDATE composers SET last_attempt_at=? WHERE id=?",
            params![started, id],
        )
        .ok();
        db.execute(
            "INSERT INTO runs(composer_id, started_at, trigger, full) VALUES (?,?,?,?)",
            params![id, started, trigger, full],
        )
        .map_err(|e| e.to_string())?;
        db.last_insert_rowid()
    };

    let (mut total_new, mut total_fetched) = (0usize, 0usize);
    let (mut errors, mut detail): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    let steps = terms.len();
    for (i, tr) in terms.into_iter().enumerate() {
        let TermRow {
            id: term_id,
            term,
            baselined,
            resume_page,
            total_expected,
        } = tr;
        let clean = baselined && resume_page == 0 && !full;
        // recua 1 página ao retomar (o site pode ter deslocado itens); duplicatas são ignoradas
        let start = if full || resume_page <= 1 {
            1
        } else {
            resume_page - 1
        };
        {
            let db = app.db.lock().unwrap();
            db.execute(
                "UPDATE terms SET resume_page=MAX(resume_page,1) WHERE id=?",
                [term_id],
            )
            .ok();
        }
        let progress = |done: usize, total: usize| {
            app.scanning.lock().unwrap().insert(
                id,
                Progress {
                    term: term.clone(),
                    done,
                    total,
                    step: i + 1,
                    steps,
                },
            );
            app.emit();
        };
        progress((start - 1) * scraper::PAGE_SIZE, total_expected);

        let (mut new_here, mut fetched) = (0usize, 0usize);
        let res = {
            let mut on_page = |page: usize, total: usize, songs: &[Song]| -> Result<bool, String> {
                let page_new = save_page(app, id, term_id, baselined, page, total, songs)?;
                new_here += page_new;
                fetched += songs.len();
                progress(
                    ((page - 1) * scraper::PAGE_SIZE + songs.len()).min(total.max(1)),
                    total,
                );
                Ok(!(clean && page_new == 0))
            };
            scraper::search_pages(&app.fetch, &term, start, delay, &mut on_page).await
        };
        total_fetched += fetched;
        match res {
            Err(e) => errors.push(e), // checkpoint fica salvo: dá para continuar depois
            Ok(()) => {
                let db = app.db.lock().unwrap();
                db.execute(
                    "UPDATE terms SET baselined=1, resume_page=0 WHERE id=?",
                    [term_id],
                )
                .map_err(|e| e.to_string())?;
                if baselined {
                    total_new += new_here;
                    detail.push(format!("scan.incremental|{term}|{fetched}|{new_here}"));
                } else {
                    detail.push(format!("scan.initial|{term}|{fetched}"));
                }
            }
        }
    }

    // Recarregamento geral concluído sem erros: o que não apareceu mais foi removido do site.
    if full && errors.is_empty() {
        let db = app.db.lock().unwrap();
        let n = db
            .execute(
                "UPDATE hits SET removed_at=? WHERE composer_id=? AND removed_at IS NULL
                 AND COALESCE(last_seen_at,0) < ?",
                params![now(), id, started],
            )
            .unwrap_or(0);
        if n > 0 {
            detail.push(format!("scan.removed|{n}"));
        }
    }

    notify_new(app, id, &mut detail).await;

    let result = {
        let db = app.db.lock().unwrap();
        let ok = errors.is_empty();
        if ok {
            db.execute(
                "UPDATE composers SET last_ok_at=?, last_error=NULL WHERE id=?",
                params![now(), id],
            )
            .ok();
        } else {
            let msg = errors.join("; ");
            db.execute(
                "UPDATE composers SET last_error=? WHERE id=?",
                params![msg, id],
            )
            .ok();
            detail.push(format!("scan.error|{msg}"));
        }
        db.execute(
            "UPDATE runs SET ended_at=?, fetched=?, new_count=?, ok=?, detail=? WHERE id=?",
            params![
                now(),
                total_fetched as i64,
                total_new as i64,
                ok,
                detail.join("\n"),
                run_id
            ],
        )
        .ok();
        // poda: mantém só os 1000 registros de auditoria mais recentes
        db.execute(
            "DELETE FROM runs WHERE id <= (SELECT MAX(id) FROM runs) - 1000",
            [],
        )
        .ok();
        if ok {
            Ok(format!("{total_new} new"))
        } else {
            Err(errors.join("; "))
        }
    };
    println!("[{}] composer {id}: {}", stamp(), detail.join(" | "));
    result
}

/// Grava uma página (músicas, créditos, vínculos) num checkpoint. Devolve quantos vínculos são novos.
fn save_page(
    app: &Arc<App>,
    composer_id: i64,
    term_id: i64,
    baselined: bool,
    page: usize,
    total: usize,
    songs: &[Song],
) -> Result<usize, String> {
    let mut conn = app.db.lock().unwrap();
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let mut page_new = 0usize;
    let t = now();
    for s in songs {
        tx.execute(
            "INSERT INTO songs(catalog_id, title, show_name, artist, credits, date_added, medley, uploads, copyright, url)
             VALUES (?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(catalog_id) DO UPDATE SET title=excluded.title, show_name=excluded.show_name,
               artist=excluded.artist, credits=excluded.credits, date_added=excluded.date_added,
               medley=excluded.medley, uploads=excluded.uploads, copyright=excluded.copyright, url=excluded.url",
            params![s.catalog_id, s.title, s.show, s.artist, s.credits, s.date_added, s.medley, s.uploads, s.copyright, s.url],
        )
        .map_err(|e| e.to_string())?;
        db::set_song_folds(&tx, &s.catalog_id, &s.title, &s.artist, &s.show, &s.credits)
            .map_err(|e| e.to_string())?;
        // já existe? só atualiza "visto por último" (e desfaz "removido")
        let seen = tx
            .execute(
                "UPDATE hits SET last_seen_at=?, removed_at=NULL WHERE composer_id=? AND catalog_id=?",
                params![t, composer_id, s.catalog_id],
            )
            .map_err(|e| e.to_string())?;
        if seen == 0 {
            // Na carga inicial de um termo (baseline) nada é "novo".
            tx.execute(
                "INSERT INTO hits(composer_id, catalog_id, found_at, is_new, last_seen_at, notified) VALUES (?,?,?,?,?,?)",
                params![composer_id, s.catalog_id, t, baselined, t, !baselined],
            )
            .map_err(|e| e.to_string())?;
            page_new += 1;
        }
    }
    // checkpoint: a próxima retomada começa depois desta página
    tx.execute(
        "UPDATE terms SET resume_page=?, total_expected=? WHERE id=?",
        params![page + 1, total, term_id],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(if baselined { page_new } else { 0 })
}

/// Avisa (desktop, Discord, Telegram, ntfy, webhook) os registros novos ainda não avisados.
async fn notify_new(app: &Arc<App>, id: i64, detail: &mut Vec<String>) {
    let (name, records, channels): (String, Vec<(String, notify::Record)>, notify::Channels) = {
        let db = app.db.lock().unwrap();
        let name: String = db
            .query_row("SELECT name FROM composers WHERE id=?", [id], |r| r.get(0))
            .unwrap_or_default();
        let mut st = match db.prepare(
            "SELECT h.catalog_id, COALESCE(s.title,''), COALESCE(s.url,''), COALESCE(s.credits,''), COALESCE(s.date_added,'')
             FROM hits h JOIN songs s USING(catalog_id)
             WHERE h.composer_id=? AND h.is_new=1 AND h.notified=0 AND h.removed_at IS NULL
             ORDER BY s.date_added DESC",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        let rows: Vec<(String, notify::Record)> = st
            .query_map([id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    notify::Record {
                        title: r.get(1)?,
                        url: r.get(2)?,
                        credits: r.get(3)?,
                        date_added: r.get(4)?,
                    },
                ))
            })
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default();
        (name, rows, notify::Channels::load(&db))
    };
    if records.is_empty() {
        return;
    }
    let list: Vec<notify::Record> = records.iter().map(|(_, r)| r.clone()).collect();
    let outcome = notify::dispatch(&app.out, &channels, &name, &list).await;
    for (ch, err) in &outcome.failed {
        detail.push(format!("notify.failed|{ch}|{err}"));
    }
    if outcome.delivered() {
        let db = app.db.lock().unwrap();
        for (catalog_id, _) in &records {
            db.execute(
                "UPDATE hits SET notified=1 WHERE composer_id=? AND catalog_id=?",
                params![id, catalog_id],
            )
            .ok();
        }
    }
}

/// Verifica a cada ciclo quem está devendo (24h por padrão, ajustável) e retoma cargas incompletas.
pub async fn scheduler(app: Arc<App>) {
    let mut first = true;
    loop {
        let due: Vec<(i64, bool)> = {
            let interval = app.interval_secs();
            let db = app.db.lock().unwrap();
            let t = now();
            // Na partida, retoma cargas interrompidas na hora; depois respeita a janela de retentativa.
            let retry_before = if first {
                i64::MAX
            } else {
                t - RETRY_AFTER_ERROR_SECS
            };
            let mut st = db
                .prepare(
                    "SELECT id, EXISTS(SELECT 1 FROM terms x WHERE x.composer_id=c.id AND x.searchable=1
                            AND (x.baselined=0 OR x.resume_page>0)) FROM composers c WHERE
                       (last_ok_at IS NULL OR last_ok_at < ?1 OR EXISTS(
                          SELECT 1 FROM terms x WHERE x.composer_id=c.id AND x.searchable=1
                            AND (x.baselined=0 OR x.resume_page>0))) AND
                       (last_attempt_at IS NULL OR last_attempt_at < ?2)",
                )
                .unwrap();
            let rows = st
                .query_map(params![t - interval, retry_before], |r| {
                    Ok((r.get(0)?, r.get::<_, i64>(1)? != 0))
                })
                .unwrap()
                .filter_map(Result::ok)
                .collect();
            rows
        };
        first = false;
        for (id, resuming) in due {
            let _ = scan_composer(&app, id, false, if resuming { "resume" } else { "auto" }).await;
        }
        tokio::time::sleep(Duration::from_secs(600)).await;
    }
}
