//! Normalização e casamento de nomes.
//!
//! `fold` deixa só letras/números minúsculos e sem acento, separados por um espaço:
//! "Billie Eilish O'Connell" -> "billie eilish o connell". Créditos são comparados por
//! **nome inteiro** (igualdade de `fold`), então "Amy Lee" não casa com "Amy Lee Roberts".

use unicode_normalization::UnicodeNormalization;

pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for c in s.nfd() {
        if ('\u{0300}'..='\u{036f}').contains(&c) {
            continue;
        }
        if c.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.extend(c.to_lowercase());
        } else {
            pending_space = true;
        }
    }
    out
}

/// "A, B, C" -> ["A", "B", "C"]
pub fn split_credits(credits: &str) -> Vec<String> {
    credits
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_basico() {
        assert_eq!(
            fold("  Billie  Eilish O'Connell "),
            "billie eilish o connell"
        );
        assert_eq!(fold("Beyoncé Knowles-Carter"), "beyonce knowles carter");
        assert_eq!(fold("!!!"), "");
    }

    #[test]
    fn nome_inteiro_nao_casa_com_prefixo() {
        assert_ne!(fold("Amy Lee"), fold("Amy Lee Roberts"));
        assert_eq!(fold("AMY LEE"), fold("amy lee"));
    }

    #[test]
    fn separa_creditos() {
        assert_eq!(
            split_credits("Dua Lipa, Ali Tamposi,  ,Billy Walsh"),
            vec!["Dua Lipa", "Ali Tamposi", "Billy Walsh"]
        );
        assert!(split_credits("").is_empty());
    }
}
