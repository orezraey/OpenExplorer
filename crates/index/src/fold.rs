//! Names as a search compares them: lowercase, without accents, so "relatorio" finds
//! "Relatório 2025.pdf" and "acao" finds "AÇÃO".

/// Appends `s` folded to `out`.
pub fn fold_into(s: &str, out: &mut Vec<u8>) {
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c.to_ascii_lowercase() as u8);
            continue;
        }
        for l in c.to_lowercase() {
            let mut buf = [0u8; 4];
            out.extend_from_slice(plain(l).encode_utf8(&mut buf).as_bytes());
        }
    }
}

pub fn fold(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    fold_into(s, &mut out);
    out
}

/// A lowercase Latin letter without its accent.
fn plain(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => 'c',
        'ď' | 'đ' => 'd',
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => 'g',
        'ĥ' | 'ħ' => 'h',
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => 'i',
        'ĵ' => 'j',
        'ķ' => 'k',
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => 'l',
        'ñ' | 'ń' | 'ņ' | 'ň' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => 'o',
        'ŕ' | 'ŗ' | 'ř' => 'r',
        'ś' | 'ŝ' | 'ş' | 'š' => 's',
        'ţ' | 'ť' | 'ŧ' => 't',
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
        'ŵ' => 'w',
        'ý' | 'ÿ' | 'ŷ' => 'y',
        'ź' | 'ż' | 'ž' => 'z',
        _ => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_and_accents_fold_away() {
        assert_eq!(fold("Relatório AÇÃO.PDF"), b"relatorio acao.pdf");
        assert_eq!(fold("Ñandú über"), b"nandu uber");
        // Other scripts keep their letters, lowercased.
        assert_eq!(fold("ΑΒΓ 日本"), "αβγ 日本".as_bytes());
    }
}
