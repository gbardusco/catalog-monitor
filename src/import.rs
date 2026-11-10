//! Importação do `artists.toml` do projeto antigo (arrangeme-monitor).
//!
//! Cada `[[artist]]` vira um compositor: `alias` é o nome em destaque e cada item de
//! `writers` é um nome pesquisável. Se o alias não estiver entre os writers, entra como
//! nome só visual (evita buscar homônimos). Sem `writers`, o alias é o único nome.

use crate::{
    db::{self, AddError, NewTerm},
    scraper::normalize,
};
use rusqlite::Connection;
use serde::Deserialize;

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    artist: Vec<Artist>,
}

#[derive(Deserialize)]
struct Artist {
    alias: String,
    #[serde(default)]
    writers: Vec<String>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Summary {
    pub created: Vec<String>,
    pub skipped: Vec<String>,
}

pub fn import_toml_str(conn: &mut Connection, text: &str) -> Result<Summary, String> {
    let file: File = toml::from_str(text).map_err(|e| format!("TOML inválido: {e}"))?;
    let mut sum = Summary::default();
    for a in file.artist {
        let terms: Vec<NewTerm> = a
            .writers
            .iter()
            .map(|w| NewTerm {
                term: w.clone(),
                visual: false,
            })
            .collect();
        // já existe algum dos nomes? então ignora (importação idempotente)
        let exists = std::iter::once(a.alias.as_str())
            .chain(a.writers.iter().map(String::as_str))
            .any(|n| {
                conn.query_row("SELECT 1 FROM terms WHERE norm=?", [normalize(n)], |_| {
                    Ok(())
                })
                .is_ok()
            });
        if exists {
            sum.skipped.push(a.alias);
            continue;
        }
        match db::add_composer(conn, &a.alias, &terms) {
            Ok(_) => sum.created.push(a.alias),
            Err(AddError::Duplicate) => sum.skipped.push(a.alias),
            Err(AddError::Empty) => {}
            Err(AddError::Db(e)) => return Err(e),
        }
    }
    Ok(sum)
}

pub fn import_toml(conn: &mut Connection, path: &str) -> Result<Summary, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("não foi possível ler {path}: {e}"))?;
    import_toml_str(conn, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [[artist]]
        alias = "Ava Max"
        writers = ["Ava Max", "Amanda Koci"]

        [[artist]]
        alias = "Tinashe"
        writers = ["Tinashe Kachingwe"]

        [[artist]]
        alias = "Kygo"
    "#;

    #[test]
    fn importa_e_e_idempotente() {
        let mut c = db::open_memory().unwrap();
        let s1 = import_toml_str(&mut c, SAMPLE).unwrap();
        assert_eq!(s1.created, vec!["Ava Max", "Tinashe", "Kygo"]);
        let s2 = import_toml_str(&mut c, SAMPLE).unwrap();
        assert!(s2.created.is_empty());
        assert_eq!(s2.skipped.len(), 3);
        // Tinashe: destaque só visual + nome real pesquisável
        let rows: Vec<(String, i64, i64)> = c
            .prepare("SELECT t.term, t.is_primary, t.searchable FROM terms t JOIN composers c ON c.id=t.composer_id WHERE c.name='Tinashe' ORDER BY t.id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(rows.contains(&("Tinashe".into(), 1, 0)));
        assert!(rows.contains(&("Tinashe Kachingwe".into(), 0, 1)));
    }
}
