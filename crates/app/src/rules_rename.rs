//! ReNamer-style multi-rule batch rename engine: 18 composable rules, rule stack execution,
//! presets persistence (`rename-presets.json`), and step-by-step analyzer.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::LazyLock;

use chrono::{Datelike, Duration as ChronoDuration, NaiveDate, NaiveDateTime, Timelike};
use openexplorer_i18n::{t, tr};
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use regex::{NoExpand, Regex, RegexBuilder};
use rune::TextEdit;
use serde::{Deserialize, Serialize};

use crate::model::{Entry, Location, shown};
use crate::{rules_meta, rules_script};

// ---------------------------------------------------------------------------------------------
// Rule Types & Configurations

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleKind {
    Insert,
    Delete,
    Remove,
    Replace,
    Rearrange,
    Extension,
    Strip,
    Case,
    Serialize,
    Random,
    Padding,
    CleanUp,
    Translit,
    ReformatDate,
    Regex,
    PascalScript,
    UserInput,
    Mapping,
}

impl RuleKind {
    pub const ALL: [Self; 18] = [
        Self::Insert,
        Self::Delete,
        Self::Remove,
        Self::Replace,
        Self::Rearrange,
        Self::Extension,
        Self::Strip,
        Self::Case,
        Self::Serialize,
        Self::Random,
        Self::Padding,
        Self::CleanUp,
        Self::Translit,
        Self::ReformatDate,
        Self::Regex,
        Self::PascalScript,
        Self::UserInput,
        Self::Mapping,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Insert => t("Inserir", "Insert"),
            Self::Delete => t("Excluir", "Delete"),
            Self::Remove => t("Remover", "Remove"),
            Self::Replace => t("Substituir", "Replace"),
            Self::Rearrange => t("Reorganizar", "Rearrange"),
            Self::Extension => t("Extensão", "Extension"),
            Self::Strip => t("Retirar", "Strip"),
            Self::Case => t("Maiúsculo/Minúsculo", "Case"),
            Self::Serialize => t("Serializar", "Serialize"),
            Self::Random => t("Aleatório", "Random"),
            Self::Padding => t("Preenchimento", "Padding"),
            Self::CleanUp => t("Limpar", "Clean Up"),
            Self::Translit => "Translit",
            Self::ReformatDate => t("Reformatar Data", "Reformat Date"),
            Self::Regex => t("Expressões Regulares", "Regular Expressions"),
            Self::PascalScript => "Script Pascal",
            Self::UserInput => t("Entrada do Usuário", "User Input"),
            Self::Mapping => t("Mapeamento", "Mapping"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum InsertWhere {
    #[default]
    Prefix,
    Suffix,
    Position,
    AfterText,
    BeforeText,
    ReplaceCurrent,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeleteFrom {
    #[default]
    Position,
    Delimiter,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeleteUntil {
    #[default]
    Count,
    Delimiter,
    UntilEnd,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Occurrences {
    #[default]
    All,
    First,
    Last,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RearrangeSplit {
    #[default]
    Delimiters,
    Positions,
    ExactPattern,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum StripWhere {
    #[default]
    Everywhere,
    Main,
    Right,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaseMode {
    #[default]
    TitleCase,
    Lower,
    Upper,
    Invert,
    FirstLetter,
    Sentence,
    None,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NumSystem {
    #[default]
    Decimal,
    Roman,
    AlphaLower,
    AlphaUpper,
    Hex,
}

impl NumSystem {
    pub const ALL: [Self; 5] = [Self::Decimal, Self::Roman, Self::AlphaLower, Self::AlphaUpper, Self::Hex];

    pub fn label(self) -> &'static str {
        match self {
            Self::Decimal => t("Dígitos decimais (0..9)", "Decimal digits (0..9)"),
            Self::Roman => t("Algarismos romanos (I, II...)", "Roman numerals (I, II...)"),
            Self::AlphaLower => t("Letras minúsculas (a..z)", "Lowercase letters (a..z)"),
            Self::AlphaUpper => t("Letras maiúsculas (A..Z)", "Uppercase letters (A..Z)"),
            Self::Hex => t("Hexadecimal (0..F)", "Hexadecimal (0..F)"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    #[default]
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TranslitDir {
    #[default]
    Forward,
    Backward,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeUnit {
    Seconds,
    Minutes,
    #[default]
    Hours,
    Days,
    Months,
    Years,
}

impl TimeUnit {
    pub const ALL: [Self; 6] = [Self::Seconds, Self::Minutes, Self::Hours, Self::Days, Self::Months, Self::Years];

    pub fn label(self) -> &'static str {
        match self {
            Self::Seconds => t("Segundos", "Seconds"),
            Self::Minutes => t("Minutos", "Minutes"),
            Self::Hours => t("Horas", "Hours"),
            Self::Days => t("Dias", "Days"),
            Self::Months => t("Meses", "Months"),
            Self::Years => t("Anos", "Years"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserInputMode {
    Before,
    After,
    #[default]
    Replace,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleConfig {
    Insert {
        text: String,
        where_mode: InsertWhere,
        position: usize,
        rtl: bool,
        after_text: String,
        before_text: String,
        skip_ext: bool,
    },
    Delete {
        from_mode: DeleteFrom,
        from_pos: usize,
        from_delim: String,
        until_mode: DeleteUntil,
        until_count: usize,
        until_delim: String,
        delete_current: bool,
        skip_ext: bool,
        rtl: bool,
        keep_delimiters: bool,
    },
    Remove {
        text: String,
        occurrences: Occurrences,
        case_sensitive: bool,
        whole_words: bool,
        skip_ext: bool,
        wildcards: bool,
    },
    Replace {
        find: String,
        replace: String,
        occurrences: Occurrences,
        case_sensitive: bool,
        whole_words: bool,
        skip_ext: bool,
        wildcards: bool,
    },
    Rearrange {
        split_mode: RearrangeSplit,
        delimiters: String,
        new_pattern: String,
        skip_ext: bool,
        rtl: bool,
    },
    Extension {
        set_new: bool,
        new_ext: String,
        append_original: bool,
        detect_binary: bool,
        remove_duplicates: bool,
        dup_case_sensitive: bool,
    },
    Strip {
        english: bool,
        english_chars: String,
        digits: bool,
        digits_chars: String,
        symbols: bool,
        symbols_chars: String,
        brackets: bool,
        brackets_chars: String,
        custom: bool,
        custom_chars: String,
        unicode_range: bool,
        unicode_range_str: String,
        where_mode: StripWhere,
        invert: bool,
        case_sensitive: bool,
        skip_ext: bool,
    },
    Case {
        mode: CaseMode,
        preserve_case: bool,
        skip_ext: bool,
        force_fragments: bool,
        fragments: String,
        ext_lower: bool,
        ext_upper: bool,
    },
    Serialize {
        start: i64,
        repeat: u32,
        step: i64,
        reset_every_on: bool,
        reset_every: u32,
        reset_folder_change: bool,
        reset_name_change: bool,
        pad_zeros: bool,
        pad_length: usize,
        num_system: NumSystem,
        where_mode: InsertWhere,
        position: usize,
        skip_ext: bool,
    },
    Random {
        length: usize,
        unique: bool,
        digits: bool,
        letters: bool,
        custom: bool,
        custom_chars: String,
        where_mode: InsertWhere,
        position: usize,
        skip_ext: bool,
        seed: u64,
    },
    Padding {
        pad_numbers: bool,
        pad_numbers_len: usize,
        strip_zeros: bool,
        pad_text: bool,
        pad_text_len: usize,
        pad_text_chars: String,
        pad_text_side: Side,
        skip_ext: bool,
    },
    CleanUp {
        strip_parens: bool,
        strip_brackets: bool,
        strip_braces: bool,
        replace_dot: bool,
        replace_comma: bool,
        replace_underscore: bool,
        replace_plus: bool,
        replace_dash: bool,
        replace_pct20: bool,
        skip_versions: bool,
        fix_spaces: bool,
        normalize_unicode_spaces: bool,
        strip_emoji: bool,
        strip_accents: bool,
        space_before_camel: bool,
        prepare_sharepoint: bool,
        skip_ext: bool,
    },
    Translit {
        alphabet: String,
        direction: TranslitDir,
        auto_case: bool,
        skip_ext: bool,
    },
    ReformatDate {
        find_fmt: String,
        replace_fmt: String,
        whole_words: bool,
        skip_ext: bool,
        custom_short_months: bool,
        short_months: String,
        custom_long_months: bool,
        long_months: String,
        adjust_on: bool,
        adjust_amount: i64,
        adjust_unit: TimeUnit,
    },
    Regex {
        pattern: String,
        replace: String,
        case_sensitive: bool,
        skip_ext: bool,
    },
    PascalScript {
        code: String,
    },
    UserInput {
        lines: String,
        mode: UserInputMode,
        skip_ext: bool,
        trim_lines: bool,
        skip_empty: bool,
    },
    Mapping {
        entries: Vec<(String, String)>,
        allow_reuse: bool,
        partial_match: bool,
        inverse: bool,
        case_sensitive: bool,
        skip_ext: bool,
    },
}

impl RuleConfig {
    pub fn default_for(kind: RuleKind) -> Self {
        match kind {
            RuleKind::Insert => Self::Insert {
                text: String::new(),
                where_mode: InsertWhere::Prefix,
                position: 1,
                rtl: false,
                after_text: String::new(),
                before_text: String::new(),
                skip_ext: true,
            },
            RuleKind::Delete => Self::Delete {
                from_mode: DeleteFrom::Position,
                from_pos: 1,
                from_delim: String::new(),
                until_mode: DeleteUntil::Count,
                until_count: 1,
                until_delim: String::new(),
                delete_current: false,
                skip_ext: true,
                rtl: false,
                keep_delimiters: false,
            },
            RuleKind::Remove => Self::Remove {
                text: String::new(),
                occurrences: Occurrences::All,
                case_sensitive: false,
                whole_words: false,
                skip_ext: true,
                wildcards: false,
            },
            RuleKind::Replace => Self::Replace {
                find: String::new(),
                replace: String::new(),
                occurrences: Occurrences::All,
                case_sensitive: false,
                whole_words: false,
                skip_ext: true,
                wildcards: false,
            },
            RuleKind::Rearrange => Self::Rearrange {
                split_mode: RearrangeSplit::Delimiters,
                delimiters: " - ".into(),
                new_pattern: "$1".into(),
                skip_ext: true,
                rtl: false,
            },
            RuleKind::Extension => Self::Extension {
                set_new: true,
                new_ext: String::new(),
                append_original: false,
                detect_binary: false,
                remove_duplicates: false,
                dup_case_sensitive: false,
            },
            RuleKind::Strip => Self::Strip {
                english: false,
                english_chars: "abcdefghijklmnopqrstuvwxyz".into(),
                digits: false,
                digits_chars: "1234567890".into(),
                symbols: false,
                symbols_chars: r##"!"#$%&'*+,-./:;=?@\^_`|~"##.into(),
                brackets: false,
                brackets_chars: "(){}[]<>".into(),
                custom: false,
                custom_chars: String::new(),
                unicode_range: false,
                unicode_range_str: "10000-10FFFF".into(),
                where_mode: StripWhere::Everywhere,
                invert: false,
                case_sensitive: false,
                skip_ext: true,
            },
            RuleKind::Case => Self::Case {
                mode: CaseMode::TitleCase,
                preserve_case: false,
                skip_ext: true,
                force_fragments: false,
                fragments: "CD, DVD, DJ".into(),
                ext_lower: false,
                ext_upper: false,
            },
            RuleKind::Serialize => Self::Serialize {
                start: 1,
                repeat: 1,
                step: 1,
                reset_every_on: false,
                reset_every: 1,
                reset_folder_change: false,
                reset_name_change: false,
                pad_zeros: false,
                pad_length: 2,
                num_system: NumSystem::Decimal,
                where_mode: InsertWhere::Prefix,
                position: 1,
                skip_ext: true,
            },
            RuleKind::Random => Self::Random {
                length: 6,
                unique: true,
                digits: true,
                letters: true,
                custom: false,
                custom_chars: String::new(),
                where_mode: InsertWhere::Prefix,
                position: 1,
                skip_ext: true,
                seed: 0x5EED_CAFE_u64,
            },
            RuleKind::Padding => Self::Padding {
                pad_numbers: false,
                pad_numbers_len: 2,
                strip_zeros: false,
                pad_text: false,
                pad_text_len: 1,
                pad_text_chars: "0".into(),
                pad_text_side: Side::Left,
                skip_ext: true,
            },
            RuleKind::CleanUp => Self::CleanUp {
                strip_parens: false,
                strip_brackets: false,
                strip_braces: false,
                replace_dot: false,
                replace_comma: false,
                replace_underscore: false,
                replace_plus: false,
                replace_dash: false,
                replace_pct20: false,
                skip_versions: false,
                fix_spaces: true,
                normalize_unicode_spaces: true,
                strip_emoji: false,
                strip_accents: false,
                space_before_camel: false,
                prepare_sharepoint: false,
                skip_ext: true,
            },
            RuleKind::Translit => Self::Translit {
                alphabet: DEFAULT_LATIN_TRANSLIT.into(),
                direction: TranslitDir::Forward,
                auto_case: true,
                skip_ext: true,
            },
            RuleKind::ReformatDate => Self::ReformatDate {
                find_fmt: "yyyy-mm-dd".into(),
                replace_fmt: "dd-mm-yyyy".into(),
                whole_words: true,
                skip_ext: true,
                custom_short_months: false,
                short_months: "Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec".into(),
                custom_long_months: false,
                long_months: "January,February,March,April,May,June,July,August,September,October,November,December".into(),
                adjust_on: false,
                adjust_amount: 1,
                adjust_unit: TimeUnit::Hours,
            },
            RuleKind::Regex => Self::Regex {
                pattern: String::new(),
                replace: String::new(),
                case_sensitive: false,
                skip_ext: true,
            },
            RuleKind::PascalScript => Self::PascalScript {
                code: "begin\n  // Adicione seu código aqui\nend.".into(),
            },
            RuleKind::UserInput => Self::UserInput {
                lines: String::new(),
                mode: UserInputMode::Replace,
                skip_ext: true,
                trim_lines: true,
                skip_empty: false,
            },
            RuleKind::Mapping => Self::Mapping {
                entries: vec![(String::new(), String::new())],
                allow_reuse: false,
                partial_match: false,
                inverse: false,
                case_sensitive: false,
                skip_ext: true,
            },
        }
    }

    pub fn kind(&self) -> RuleKind {
        match self {
            Self::Insert { .. } => RuleKind::Insert,
            Self::Delete { .. } => RuleKind::Delete,
            Self::Remove { .. } => RuleKind::Remove,
            Self::Replace { .. } => RuleKind::Replace,
            Self::Rearrange { .. } => RuleKind::Rearrange,
            Self::Extension { .. } => RuleKind::Extension,
            Self::Strip { .. } => RuleKind::Strip,
            Self::Case { .. } => RuleKind::Case,
            Self::Serialize { .. } => RuleKind::Serialize,
            Self::Random { .. } => RuleKind::Random,
            Self::Padding { .. } => RuleKind::Padding,
            Self::CleanUp { .. } => RuleKind::CleanUp,
            Self::Translit { .. } => RuleKind::Translit,
            Self::ReformatDate { .. } => RuleKind::ReformatDate,
            Self::Regex { .. } => RuleKind::Regex,
            Self::PascalScript { .. } => RuleKind::PascalScript,
            Self::UserInput { .. } => RuleKind::UserInput,
            Self::Mapping { .. } => RuleKind::Mapping,
        }
    }
}

pub const DEFAULT_LATIN_TRANSLIT: &str = "á=a, à=a, ã=a, â=a, ä=a, å=a\né=e, è=e, ê=e, ë=e\ní=i, ì=i, î=i, ï=i\nó=o, ò=o, õ=o, ô=o, ö=o, ø=o\nú=u, ù=u, û=u, ü=u\nç=c, ñ=n, ý=y, ÿ=y, ß=ss";
pub const CYRILLIC_TRANSLIT: &str = "а=a, б=b, в=v, г=g, д=d, е=e, ё=yo, ж=zh, з=z, и=i, й=y, к=k, л=l, м=m, н=n, о=o, п=p, р=r, с=s, т=t, у=u, ф=f, х=kh, ц=ts, ч=ch, ш=sh, щ=shch, ъ=, ы=y, ь=, э=e, ю=yu, я=ya";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub enabled: bool,
    #[serde(default)]
    pub comment: String,
    pub config: RuleConfig,
}

impl Rule {
    pub fn new(config: RuleConfig) -> Self {
        Self { enabled: true, comment: String::new(), config }
    }

    pub fn summary(&self) -> String {
        let flags_suffix = |flags: &[(&'static str, &'static str, bool)]| -> String {
            let active: Vec<&str> = flags.iter().filter(|(_, _, on)| *on).map(|(pt, en, _)| t(*pt, *en)).collect();
            if active.is_empty() {
                String::new()
            } else {
                format!(" ({})", active.join(", "))
            }
        };
        let occ_label = |occ: Occurrences| match occ {
            Occurrences::All => t("todos", "all"),
            Occurrences::First => t("primeiro", "first"),
            Occurrences::Last => t("último", "last"),
        };
        let where_desc = |wm: InsertWhere, pos: usize, rtl: bool, after: &str, before: &str| -> String {
            match wm {
                InsertWhere::Prefix => t("como Prefixo", "as Prefix").into(),
                InsertWhere::Suffix => t("como Sufixo", "as Suffix").into(),
                InsertWhere::Position => {
                    if rtl {
                        tr!("na posição {pos} (dir→esq)", "at position {pos} (RTL)")
                    } else {
                        tr!("na posição {pos}", "at position {pos}")
                    }
                }
                InsertWhere::AfterText => tr!("após texto \"{after}\"", "after text \"{after}\""),
                InsertWhere::BeforeText => tr!("antes do texto \"{before}\"", "before text \"{before}\""),
                InsertWhere::ReplaceCurrent => t("substituindo nome atual", "replacing current name").into(),
            }
        };

        match &self.config {
            RuleConfig::Insert { text, where_mode, position, rtl, after_text, before_text, skip_ext } => {
                let pos_str = where_desc(*where_mode, *position, *rtl, after_text, before_text);
                let tail = flags_suffix(&[("ignorar extensão", "skip extension", *skip_ext)]);
                tr!("Inserir \"{text}\" {pos_str}{tail}", "Insert \"{text}\" {pos_str}{tail}")
            }
            RuleConfig::Delete { from_mode, from_pos, from_delim, until_mode, until_count, until_delim, delete_current, skip_ext, rtl, keep_delimiters } => {
                let tail = flags_suffix(&[
                    ("não remover delimitadores", "do not remove delimiters", *keep_delimiters),
                    ("direita-para-esquerda", "right-to-left", *rtl),
                    ("ignorar extensão", "skip extension", *skip_ext),
                ]);
                if *delete_current {
                    format!("{}{tail}", t("Excluir nome atual", "Delete current name"))
                } else {
                    let f = match from_mode {
                        DeleteFrom::Position => tr!("posição {from_pos}", "position {from_pos}"),
                        DeleteFrom::Delimiter => tr!("delimitador \"{from_delim}\"", "delimiter \"{from_delim}\""),
                    };
                    let u = match until_mode {
                        DeleteUntil::Count => tr!("contagem {until_count}", "count {until_count}"),
                        DeleteUntil::Delimiter => tr!("delimitador \"{until_delim}\"", "delimiter \"{until_delim}\""),
                        DeleteUntil::UntilEnd => t("o final", "the end").into(),
                    };
                    tr!("Excluir de {f} até {u}{tail}", "Delete from {f} until {u}{tail}")
                }
            }
            RuleConfig::Remove { text, occurrences, case_sensitive, whole_words, skip_ext, wildcards } => {
                let occ = occ_label(*occurrences);
                let tail = flags_suffix(&[
                    ("diferenciar maiúsculas", "case sensitive", *case_sensitive),
                    ("palavras inteiras", "whole words", *whole_words),
                    ("curingas", "wildcards", *wildcards),
                    ("ignorar extensão", "skip extension", *skip_ext),
                ]);
                tr!("Remover {occ} \"{text}\"{tail}", "Remove {occ} \"{text}\"{tail}")
            }
            RuleConfig::Replace { find, replace, occurrences, case_sensitive, whole_words, skip_ext, wildcards } => {
                let occ = occ_label(*occurrences);
                let tail = flags_suffix(&[
                    ("diferenciar maiúsculas", "case sensitive", *case_sensitive),
                    ("palavras inteiras", "whole words", *whole_words),
                    ("curingas", "wildcards", *wildcards),
                    ("ignorar extensão", "skip extension", *skip_ext),
                ]);
                tr!("Substituir {occ} \"{find}\" com \"{replace}\"{tail}", "Replace {occ} \"{find}\" with \"{replace}\"{tail}")
            }
            RuleConfig::Rearrange { split_mode, delimiters, new_pattern, skip_ext, rtl } => {
                let how = match split_mode {
                    RearrangeSplit::Delimiters => tr!("Dividir por delimitadores \"{delimiters}\"", "Split by delimiters \"{delimiters}\""),
                    RearrangeSplit::Positions => tr!("Dividir nas posições \"{delimiters}\"", "Split at positions \"{delimiters}\""),
                    RearrangeSplit::ExactPattern => tr!("Dividir por padrão exato \"{delimiters}\"", "Split by exact pattern \"{delimiters}\""),
                };
                let tail = flags_suffix(&[
                    ("direita-para-esquerda", "right-to-left", *rtl),
                    ("ignorar extensão", "skip extension", *skip_ext),
                ]);
                tr!("{how}, Novo padrão \"{new_pattern}\"{tail}", "{how}, New pattern \"{new_pattern}\"{tail}")
            }
            RuleConfig::Extension { set_new, new_ext, append_original, detect_binary, remove_duplicates, dup_case_sensitive } => {
                let mut parts: Vec<String> = Vec::new();
                if *detect_binary {
                    parts.push(t("Detectar assinatura binária", "Detect binary signature").into());
                } else if *set_new {
                    parts.push(tr!("Alterar extensão para \".{new_ext}\"", "Set extension to \".{new_ext}\""));
                }
                if *append_original {
                    parts.push(t("anexar ao nome original", "append to original").into());
                }
                if *remove_duplicates {
                    parts.push(t("remover extensões duplicadas", "remove duplicate extensions").into());
                }
                if parts.is_empty() {
                    parts.push(t("Sem alteração de extensão", "No extension changes").into());
                }
                let tail = flags_suffix(&[("diferenciar maiúsculas", "case sensitive", *dup_case_sensitive)]);
                format!("{}{tail}", parts.join(", "))
            }
            RuleConfig::Strip { english, digits, symbols, brackets, custom, custom_chars, unicode_range, where_mode, invert, case_sensitive, skip_ext, .. } => {
                let mut sets: Vec<String> = Vec::new();
                if *english { sets.push(t("inglês", "English").into()); }
                if *digits { sets.push(t("dígitos", "digits").into()); }
                if *symbols { sets.push(t("símbolos", "symbols").into()); }
                if *brackets { sets.push(t("colchetes", "brackets").into()); }
                if *custom { sets.push(tr!("\"{custom_chars}\"", "\"{custom_chars}\"")); }
                if *unicode_range { sets.push(t("unicode", "unicode").into()); }
                let what = if sets.is_empty() { t("nenhum conjunto", "none").into() } else { sets.join(", ") };
                let loc = match where_mode {
                    StripWhere::Everywhere => t("em todo lugar", "everywhere"),
                    StripWhere::Main => t("no início", "leading"),
                    StripWhere::Right => t("à direita", "trailing"),
                };
                let tail = flags_suffix(&[
                    ("inverter seleção", "invert selection", *invert),
                    ("diferenciar maiúsculas", "case sensitive", *case_sensitive),
                    ("ignorar extensão", "skip extension", *skip_ext),
                ]);
                tr!("Retirar ({what}) {loc}{tail}", "Strip ({what}) {loc}{tail}")
            }
            RuleConfig::Case { mode, preserve_case, skip_ext, force_fragments, fragments, ext_lower, ext_upper } => {
                let base = match mode {
                    CaseMode::TitleCase => t("Primeira Letra Da Palavra Em Maiúscula", "Capitalize Every Word"),
                    CaseMode::Lower => t("tudo em minúsculo", "all lower case"),
                    CaseMode::Upper => t("TUDO EM MAIÚSCULO", "ALL UPPER CASE"),
                    CaseMode::Invert => t("iNVERTER mAIÚSCULAS/mINÚSCULAS", "iNVERT cASE"),
                    CaseMode::FirstLetter => t("Primeira letra maiúscula", "First letter capital"),
                    CaseMode::Sentence => t("Iniciar sentença em maiúsculas", "Sentence case"),
                    CaseMode::None => t("(nenhum)", "(none)"),
                };
                let mut extra: Vec<String> = Vec::new();
                if *preserve_case { extra.push(t("preservar existentes", "preserve existing").into()); }
                if *force_fragments && !fragments.trim().is_empty() {
                    extra.push(tr!("fragmentos \"{fragments}\"", "fragments \"{fragments}\""));
                }
                if *ext_lower { extra.push(t("ext. minúscula", "ext. lower").into()); }
                if *ext_upper { extra.push(t("ext. maiúscula", "ext. UPPER").into()); }
                if *skip_ext { extra.push(t("ignorar extensão", "skip extension").into()); }
                if extra.is_empty() {
                    base.into()
                } else {
                    format!("{base} ({})", extra.join(", "))
                }
            }
            RuleConfig::Serialize { start, step, pad_zeros, pad_length, where_mode, position, skip_ext, .. } => {
                let pos_str = where_desc(*where_mode, *position, false, "", "");
                let tail = flags_suffix(&[
                    ("ignorar extensão", "skip extension", *skip_ext),
                ]);
                let pad_note = if *pad_zeros {
                    tr!(", preencher zeros={pad_length}", ", pad zeros={pad_length}")
                } else {
                    String::new()
                };
                tr!(
                    "Serializar incremental de {start} (passo {step}{pad_note}) {pos_str}{tail}",
                    "Serialize incremental from {start} (step {step}{pad_note}) {pos_str}{tail}"
                )
            }
            RuleConfig::Random { length, digits, letters, custom, where_mode, position, skip_ext, .. } => {
                let pos_str = where_desc(*where_mode, *position, false, "", "");
                let mut pool = Vec::new();
                if *digits { pool.push(t("dígitos", "digits")); }
                if *letters { pool.push(t("letras", "letters")); }
                if *custom { pool.push(t("usuário", "custom")); }
                let pool_s = if pool.is_empty() { t("padrão", "default").into() } else { pool.join("+") };
                let tail = flags_suffix(&[("ignorar extensão", "skip extension", *skip_ext)]);
                tr!(
                    "Aleatório de {length} car. ({pool_s}) {pos_str}{tail}",
                    "Random {length} chars ({pool_s}) {pos_str}{tail}"
                )
            }
            RuleConfig::Padding { pad_numbers, pad_numbers_len, strip_zeros, pad_text, pad_text_len, pad_text_chars, skip_ext, .. } => {
                let mut ops: Vec<String> = Vec::new();
                if *pad_numbers { ops.push(tr!("zeros até compr. {pad_numbers_len}", "zeros to len {pad_numbers_len}")); }
                if *strip_zeros { ops.push(t("remover zeros à esquerda", "strip leading zeros").into()); }
                if *pad_text { ops.push(tr!("texto com \"{pad_text_chars}\" até {pad_text_len}", "text \"{pad_text_chars}\" to {pad_text_len}")); }
                let what = if ops.is_empty() { t("sem preenchimento", "no padding").into() } else { ops.join(", ") };
                let tail = flags_suffix(&[("ignorar extensão", "skip extension", *skip_ext)]);
                tr!("Preencher ({what}){tail}", "Padding ({what}){tail}")
            }
            RuleConfig::CleanUp { strip_parens, strip_brackets, strip_braces, fix_spaces, strip_accents, skip_ext, .. } => {
                let mut acts: Vec<&str> = Vec::new();
                if *strip_parens || *strip_brackets || *strip_braces {
                    acts.push(t("retirar colchetes", "strip brackets"));
                }
                if *fix_spaces {
                    acts.push(t("corrigir espaços", "fix spaces"));
                }
                if *strip_accents {
                    acts.push(t("remover acentos", "strip accents"));
                }
                let head = if acts.is_empty() {
                    t("Limpar caracteres e espaços", "Clean up characters and spaces").into()
                } else {
                    acts.join(", ")
                };
                let tail = flags_suffix(&[("ignorar extensão", "skip extension", *skip_ext)]);
                format!("{head}{tail}")
            }
            RuleConfig::Translit { direction, auto_case, skip_ext, .. } => {
                let d = match direction {
                    TranslitDir::Forward => t("Avançar", "Forward"),
                    TranslitDir::Backward => t("Retroceder", "Backward"),
                };
                let tail = flags_suffix(&[
                    ("ajuste automático", "auto case", *auto_case),
                    ("ignorar extensão", "skip extension", *skip_ext),
                ]);
                tr!("Transliterar ({d}){tail}", "Transliterate ({d}){tail}")
            }
            RuleConfig::ReformatDate { find_fmt, replace_fmt, whole_words, skip_ext, adjust_on, adjust_amount, adjust_unit, .. } => {
                let adj = if *adjust_on && *adjust_amount != 0 {
                    let u = adjust_unit.label().to_lowercase();
                    format!(" ({:+} {u})", adjust_amount)
                } else {
                    String::new()
                };
                let tail = flags_suffix(&[
                    ("palavras inteiras", "whole words", *whole_words),
                    ("ignorar extensão", "skip extension", *skip_ext),
                ]);
                tr!(
                    "Converter formato \"{find_fmt}\" para \"{replace_fmt}\"{adj}{tail}",
                    "Reformat \"{find_fmt}\" to \"{replace_fmt}\"{adj}{tail}"
                )
            }
            RuleConfig::Regex { pattern, replace, case_sensitive, skip_ext } => {
                let tail = flags_suffix(&[
                    ("diferenciar maiúsculas", "case sensitive", *case_sensitive),
                    ("ignorar extensão", "skip extension", *skip_ext),
                ]);
                tr!(
                    "Substituir expressão \"{pattern}\" com \"{replace}\"{tail}",
                    "Replace regex \"{pattern}\" with \"{replace}\"{tail}"
                )
            }
            RuleConfig::PascalScript { code } => {
                let flat: Vec<&str> = code.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
                if flat.is_empty() {
                    t("(Script Pascal vazio)", "(Empty Pascal Script)").into()
                } else {
                    flat.join(" ")
                }
            }
            RuleConfig::UserInput { lines, mode, skip_ext, .. } => {
                let n = lines.lines().count();
                let m = match mode {
                    UserInputMode::Before => t("na frente", "before"),
                    UserInputMode::After => t("após", "after"),
                    UserInputMode::Replace => t("substituir", "replace"),
                };
                let tail = flags_suffix(&[("ignorar extensão", "skip extension", *skip_ext)]);
                tr!("Entrada ({m}, {n} linha(s)){tail}", "User input ({m}, {n} line(s)){tail}")
            }
            RuleConfig::Mapping { entries, partial_match, inverse, case_sensitive, skip_ext, .. } => {
                let tail = flags_suffix(&[
                    ("corresp. parcial", "partial match", *partial_match),
                    ("inverso", "inverse", *inverse),
                    ("diferenciar maiúsculas", "case sensitive", *case_sensitive),
                    ("ignorar extensão", "skip extension", *skip_ext),
                ]);
                tr!("Mapear {} entrada(s){tail}", "Map {} entry(ies){tail}", entries.len())
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Presets Persistence

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulePreset {
    pub name: String,
    pub rules: Vec<Rule>,
}

pub fn load_presets() -> Vec<RulePreset> {
    let path = openexplorer_gdrive::config_dir().join("rename-presets.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_presets(presets: &[RulePreset]) {
    let Ok(bytes) = serde_json::to_vec_pretty(presets) else { return };
    let _ = std::fs::create_dir_all(openexplorer_gdrive::config_dir());
    let _ = std::fs::write(openexplorer_gdrive::config_dir().join("rename-presets.json"), bytes);
}

pub fn builtin_presets() -> Vec<RulePreset> {
    vec![
        RulePreset {
            name: t("Limpar nome para web / mídia", "Clean up name for web / media").into(),
            rules: vec![
                Rule::new(RuleConfig::CleanUp {
                    strip_parens: false,
                    strip_brackets: true,
                    strip_braces: true,
                    replace_dot: true,
                    replace_comma: false,
                    replace_underscore: true,
                    replace_plus: false,
                    replace_dash: false,
                    replace_pct20: true,
                    skip_versions: true,
                    fix_spaces: true,
                    normalize_unicode_spaces: true,
                    strip_emoji: false,
                    strip_accents: false,
                    space_before_camel: false,
                    prepare_sharepoint: false,
                    skip_ext: true,
                }),
                Rule::new(RuleConfig::Case {
                    mode: CaseMode::TitleCase,
                    preserve_case: false,
                    skip_ext: true,
                    force_fragments: false,
                    fragments: "CD, DVD, DJ".into(),
                    ext_lower: true,
                    ext_upper: false,
                }),
            ],
        },
        RulePreset {
            name: t("Numerar com zeros (01, 02...)", "Zero-padded numbering (01, 02...)").into(),
            rules: vec![Rule::new(RuleConfig::Serialize {
                start: 1,
                repeat: 1,
                step: 1,
                reset_every_on: false,
                reset_every: 1,
                reset_folder_change: false,
                reset_name_change: false,
                pad_zeros: true,
                pad_length: 2,
                num_system: NumSystem::Decimal,
                where_mode: InsertWhere::Prefix,
                position: 1,
                skip_ext: true,
            })],
        },
        RulePreset {
            name: t("Remover acentos e normalizar espaços", "Strip accents and normalize spaces").into(),
            rules: vec![Rule::new(RuleConfig::CleanUp {
                strip_parens: false,
                strip_brackets: false,
                strip_braces: false,
                replace_dot: false,
                replace_comma: false,
                replace_underscore: true,
                replace_plus: false,
                replace_dash: false,
                replace_pct20: true,
                skip_versions: true,
                fix_spaces: true,
                normalize_unicode_spaces: true,
                strip_emoji: true,
                strip_accents: true,
                space_before_camel: false,
                prepare_sharepoint: false,
                skip_ext: true,
            })],
        },
    ]
}

// ---------------------------------------------------------------------------------------------
// UI State for the Rules Tab & Popups

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RulesPopup {
    /// Presets manager modal (load, save current stack, delete).
    Presets,
    /// Step-by-step rule analyzer modal.
    Analyze,
    /// Edit comment for rule at index.
    Comment(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetaTagTarget {
    InsertText,
    RemoveText,
    ReplaceWith,
    RearrangePattern,
    RegexPattern,
    PascalScript,
}

pub struct RuleEditorState {
    pub kind: RuleKind,
    pub config: RuleConfig,
    /// Index in `RulesState::rules` when editing an existing rule, or `None` when adding.
    pub edit_index: Option<usize>,
    /// The edited rule as it was when the editor opened. The batch window stays usable meanwhile,
    /// so deleting or moving rules there can shift `edit_index`: saving looks the rule up again.
    pub edit_original: Option<RuleConfig>,
    /// Optional insertion slot (`Adicionar regra (acima)` / `(abaixo)`) when adding a new rule.
    pub insert_at: Option<usize>,
    /// Active Meta Tags sub-picker target inside the independent Rule Editor window.
    pub meta_target: Option<MetaTagTarget>,
    // Shared text edits synced with `config` for smooth Rune TextInput editing:
    pub edit_a: TextEdit,
    pub edit_b: TextEdit,
    pub edit_c: TextEdit,
    pub edit_d: TextEdit,
    pub edit_e: TextEdit,
    pub edit_f: TextEdit,
    pub nums: [TextEdit; 6],
    pub script_status: Option<Result<String, String>>,
    pub show_scripts_menu: bool,
    pub show_help: bool,
    pub show_date_find_dd: bool,
    pub show_date_rep_dd: bool,
    pub show_num_sys_dd: bool,
    pub show_time_unit_dd: bool,
    /// Table editor inputs for `Mapping`:
    pub map_key: TextEdit,
    pub map_val: TextEdit,
}

impl RuleEditorState {
    pub fn from_config(config: RuleConfig) -> Self {
        let kind = config.kind();
        let mut s = Self {
            kind,
            config,
            edit_index: None,
            edit_original: None,
            insert_at: None,
            meta_target: None,
            edit_a: TextEdit::new(""),
            edit_b: TextEdit::new(""),
            edit_c: TextEdit::new(""),
            edit_d: TextEdit::new(""),
            edit_e: TextEdit::new(""),
            edit_f: TextEdit::new(""),
            nums: std::array::from_fn(|_| TextEdit::new("")),
            script_status: None,
            show_scripts_menu: false,
            show_help: false,
            show_date_find_dd: false,
            show_date_rep_dd: false,
            show_num_sys_dd: false,
            show_time_unit_dd: false,
            map_key: TextEdit::new(""),
            map_val: TextEdit::new(""),
        };
        s.load_edits_from_config();
        s
    }

    pub fn switch_kind(&mut self, kind: RuleKind) {
        if self.kind == kind {
            return;
        }
        self.kind = kind;
        self.config = RuleConfig::default_for(kind);
        self.meta_target = None;
        self.script_status = None;
        self.show_scripts_menu = false;
        self.show_help = false;
        self.show_date_find_dd = false;
        self.show_date_rep_dd = false;
        self.show_num_sys_dd = false;
        self.show_time_unit_dd = false;
        self.load_edits_from_config();
    }

    pub fn load_edits_from_config(&mut self) {
        self.nums = std::array::from_fn(|_| TextEdit::new(""));
        match &self.config {
            RuleConfig::Insert { text, after_text, before_text, position, .. } => {
                self.edit_a = TextEdit::new(text.clone());
                self.edit_b = TextEdit::new(after_text.clone());
                self.edit_c = TextEdit::new(before_text.clone());
                self.nums[0] = TextEdit::new(position.to_string());
            }
            RuleConfig::Delete { from_delim, until_delim, from_pos, until_count, .. } => {
                self.edit_a = TextEdit::new(from_delim.clone());
                self.edit_b = TextEdit::new(until_delim.clone());
                self.nums[0] = TextEdit::new(from_pos.to_string());
                self.nums[1] = TextEdit::new(until_count.to_string());
            }
            RuleConfig::Remove { text, .. } => {
                self.edit_a = TextEdit::new(text.clone());
            }
            RuleConfig::Replace { find, replace, .. } => {
                self.edit_a = TextEdit::new(find.clone());
                self.edit_b = TextEdit::new(replace.clone());
            }
            RuleConfig::Rearrange { delimiters, new_pattern, .. } => {
                self.edit_a = TextEdit::new(delimiters.clone());
                self.edit_b = TextEdit::new(new_pattern.clone());
            }
            RuleConfig::Extension { new_ext, .. } => {
                self.edit_a = TextEdit::new(new_ext.clone());
            }
            RuleConfig::Strip { english_chars, digits_chars, symbols_chars, brackets_chars, custom_chars, unicode_range_str, .. } => {
                self.edit_a = TextEdit::new(english_chars.clone());
                self.edit_b = TextEdit::new(digits_chars.clone());
                self.edit_c = TextEdit::new(symbols_chars.clone());
                self.edit_d = TextEdit::new(brackets_chars.clone());
                self.edit_e = TextEdit::new(custom_chars.clone());
                self.edit_f = TextEdit::new(unicode_range_str.clone());
            }
            RuleConfig::Case { fragments, .. } => {
                self.edit_a = TextEdit::new(fragments.clone());
            }
            RuleConfig::Serialize { start, repeat, step, reset_every, pad_length, position, .. } => {
                self.nums[0] = TextEdit::new(start.to_string());
                self.nums[1] = TextEdit::new(repeat.to_string());
                self.nums[2] = TextEdit::new(step.to_string());
                self.nums[3] = TextEdit::new(reset_every.to_string());
                self.nums[4] = TextEdit::new(pad_length.to_string());
                self.nums[5] = TextEdit::new(position.to_string());
            }
            RuleConfig::Random { custom_chars, length, position, .. } => {
                self.edit_a = TextEdit::new(custom_chars.clone());
                self.nums[0] = TextEdit::new(length.to_string());
                self.nums[1] = TextEdit::new(position.to_string());
            }
            RuleConfig::Padding { pad_text_chars, pad_numbers_len, pad_text_len, .. } => {
                self.edit_a = TextEdit::new(pad_text_chars.clone());
                self.nums[0] = TextEdit::new(pad_numbers_len.to_string());
                self.nums[1] = TextEdit::new(pad_text_len.to_string());
            }
            RuleConfig::Translit { alphabet, .. } => {
                self.edit_a = TextEdit::new(alphabet.clone());
            }
            RuleConfig::ReformatDate { find_fmt, replace_fmt, short_months, long_months, adjust_amount, .. } => {
                self.edit_a = TextEdit::new(find_fmt.clone());
                self.edit_b = TextEdit::new(replace_fmt.clone());
                self.edit_c = TextEdit::new(short_months.clone());
                self.edit_d = TextEdit::new(long_months.clone());
                self.nums[0] = TextEdit::new(adjust_amount.to_string());
            }
            RuleConfig::Regex { pattern, replace, .. } => {
                self.edit_a = TextEdit::new(pattern.clone());
                self.edit_b = TextEdit::new(replace.clone());
            }
            RuleConfig::PascalScript { code } => {
                self.edit_a = TextEdit::new(code.clone());
            }
            RuleConfig::UserInput { lines, .. } => {
                self.edit_a = TextEdit::new(lines.clone());
            }
            RuleConfig::CleanUp { .. } | RuleConfig::Mapping { .. } => {}
        }
    }

    pub fn sync_edits_to_config(&mut self) {
        let parse_i64 = |idx: usize, fallback: i64, min: i64, max: i64| -> i64 {
            self.nums[idx].text.trim().parse::<i64>().unwrap_or(fallback).clamp(min, max)
        };
        match &mut self.config {
            RuleConfig::Insert { text, after_text, before_text, position, .. } => {
                *text = self.edit_a.text.clone();
                *after_text = self.edit_b.text.clone();
                *before_text = self.edit_c.text.clone();
                *position = parse_i64(0, *position as i64, 1, 999_999) as usize;
            }
            RuleConfig::Delete { from_delim, until_delim, from_pos, until_count, .. } => {
                *from_delim = self.edit_a.text.clone();
                *until_delim = self.edit_b.text.clone();
                *from_pos = parse_i64(0, *from_pos as i64, 1, 999_999) as usize;
                *until_count = parse_i64(1, *until_count as i64, 1, 999_999) as usize;
            }
            RuleConfig::Remove { text, .. } => *text = self.edit_a.text.clone(),
            RuleConfig::Replace { find, replace, .. } => {
                *find = self.edit_a.text.clone();
                *replace = self.edit_b.text.clone();
            }
            RuleConfig::Rearrange { delimiters, new_pattern, .. } => {
                *delimiters = self.edit_a.text.clone();
                *new_pattern = self.edit_b.text.clone();
            }
            RuleConfig::Extension { new_ext, .. } => *new_ext = self.edit_a.text.trim_start_matches('.').to_string(),
            RuleConfig::Strip { english_chars, digits_chars, symbols_chars, brackets_chars, custom_chars, unicode_range_str, .. } => {
                *english_chars = self.edit_a.text.clone();
                *digits_chars = self.edit_b.text.clone();
                *symbols_chars = self.edit_c.text.clone();
                *brackets_chars = self.edit_d.text.clone();
                *custom_chars = self.edit_e.text.clone();
                *unicode_range_str = self.edit_f.text.clone();
            }
            RuleConfig::Case { fragments, .. } => *fragments = self.edit_a.text.clone(),
            RuleConfig::Serialize { start, repeat, step, reset_every, pad_length, position, .. } => {
                *start = parse_i64(0, *start, -999_999, 999_999);
                *repeat = parse_i64(1, *repeat as i64, 1, 999_999) as u32;
                *step = parse_i64(2, *step, -999_999, 999_999);
                *reset_every = parse_i64(3, *reset_every as i64, 1, 999_999) as u32;
                *pad_length = parse_i64(4, *pad_length as i64, 1, 64) as usize;
                *position = parse_i64(5, *position as i64, 1, 999_999) as usize;
            }
            RuleConfig::Random { custom_chars, length, position, .. } => {
                *custom_chars = self.edit_a.text.clone();
                *length = parse_i64(0, *length as i64, 1, 64) as usize;
                *position = parse_i64(1, *position as i64, 1, 999_999) as usize;
            }
            RuleConfig::Padding { pad_text_chars, pad_numbers_len, pad_text_len, .. } => {
                *pad_text_chars = self.edit_a.text.clone();
                *pad_numbers_len = parse_i64(0, *pad_numbers_len as i64, 1, 64) as usize;
                *pad_text_len = parse_i64(1, *pad_text_len as i64, 1, 64) as usize;
            }
            RuleConfig::Translit { alphabet, .. } => *alphabet = self.edit_a.text.clone(),
            RuleConfig::ReformatDate { find_fmt, replace_fmt, short_months, long_months, adjust_amount, .. } => {
                *find_fmt = self.edit_a.text.clone();
                *replace_fmt = self.edit_b.text.clone();
                *short_months = self.edit_c.text.clone();
                *long_months = self.edit_d.text.clone();
                *adjust_amount = parse_i64(0, *adjust_amount, -999_999, 999_999);
            }
            RuleConfig::Regex { pattern, replace, .. } => {
                *pattern = self.edit_a.text.clone();
                *replace = self.edit_b.text.clone();
            }
            RuleConfig::PascalScript { code } => *code = self.edit_a.text.clone(),
            RuleConfig::UserInput { lines, .. } => *lines = self.edit_a.text.clone(),
            RuleConfig::CleanUp { .. } | RuleConfig::Mapping { .. } => {}
        }
    }

    pub fn insert_meta_tag(&mut self, target: MetaTagTarget, tag_name: &str) {
        let token = format!(":{tag_name}:");
        let edit = match target {
            MetaTagTarget::InsertText
            | MetaTagTarget::RemoveText
            | MetaTagTarget::RegexPattern
            | MetaTagTarget::PascalScript => &mut self.edit_a,
            MetaTagTarget::ReplaceWith | MetaTagTarget::RearrangePattern => &mut self.edit_b,
        };
        if target == MetaTagTarget::PascalScript {
            edit.text.push_str(&format!("MetaTag('{token}')"));
        } else {
            edit.text.push_str(&token);
        }
        self.sync_edits_to_config();
    }
}

pub const DEFAULT_RULE_COLS: [f32; 3] = [56.0, 150.0, 480.0];
pub const DEFAULT_PREVIEW_COLS: [f32; 4] = [78.0, 290.0, 290.0, 120.0];

pub fn clamp_rule_col(i: usize, w: f32) -> f32 {
    match i {
        0 => w.clamp(42.0, 140.0),
        1 => w.clamp(80.0, 380.0),
        _ => w.clamp(140.0, 1400.0),
    }
}

pub fn clamp_preview_col(i: usize, w: f32) -> f32 {
    match i {
        0 => w.clamp(56.0, 160.0),
        1 | 2 => w.clamp(100.0, 900.0),
        _ => w.clamp(64.0, 360.0),
    }
}

pub struct RulesState {
    pub rules: Vec<Rule>,
    pub selected: Option<usize>,
    pub all_selected: bool,
    pub col_widths: [f32; 3],
    pub preview_cols: [f32; 4],
    pub stack_menu: Option<(Option<usize>, rune::PointerPress)>,
    pub comment_edit: TextEdit,
    pub popup: Option<RulesPopup>,
    pub editor: RuleEditorState,
    // Meta Tags picker state
    pub tag_cat: rules_meta::TagCategory,
    pub tag_cat_open: bool,
    pub tag_search: TextEdit,
    pub tag_selected: usize,
    // Presets state
    pub presets: Vec<RulePreset>,
    pub preset_name: TextEdit,
    // Analyze modal state
    pub analyze_input: TextEdit,
}

impl Default for RulesState {
    fn default() -> Self {
        Self::new()
    }
}

impl RulesState {
    pub fn new() -> Self {
        Self {
            rules: Vec::new(),
            selected: None,
            all_selected: false,
            col_widths: DEFAULT_RULE_COLS,
            preview_cols: DEFAULT_PREVIEW_COLS,
            stack_menu: None,
            comment_edit: TextEdit::new(""),
            popup: None,
            editor: RuleEditorState::from_config(RuleConfig::default_for(RuleKind::Insert)),
            tag_cat: rules_meta::TagCategory::All,
            tag_cat_open: false,
            tag_search: TextEdit::new(""),
            tag_selected: 0,
            presets: load_presets(),
            preset_name: TextEdit::new(""),
            analyze_input: TextEdit::new(""),
        }
    }

    pub fn open_add_at(&mut self, insert_at: Option<usize>) {
        self.editor = RuleEditorState::from_config(RuleConfig::default_for(RuleKind::Insert));
        self.editor.insert_at = insert_at;
        self.popup = None;
        self.stack_menu = None;
    }

    pub fn open_edit(&mut self, idx: usize) {
        let Some(rule) = self.rules.get(idx) else { return };
        self.selected = Some(idx);
        self.all_selected = false;
        self.editor = RuleEditorState::from_config(rule.config.clone());
        self.editor.edit_index = Some(idx);
        self.editor.edit_original = Some(rule.config.clone());
        self.popup = None;
        self.stack_menu = None;
    }

    pub fn commit_editor(&mut self) {
        self.editor.sync_edits_to_config();
        let config = self.editor.config.clone();
        // The rule edited, found again if the list changed while the editor was open; gone, the
        // edits become a new rule rather than overwrite another.
        let edited = self.editor.edit_index.and_then(|idx| match &self.editor.edit_original {
            Some(original) if self.rules.get(idx).is_some_and(|r| &r.config == original) => Some(idx),
            Some(original) => self.rules.iter().position(|r| &r.config == original),
            None => (idx < self.rules.len()).then_some(idx),
        });
        match (edited, self.editor.insert_at) {
            (Some(idx), _) => {
                self.rules[idx].config = config.clone();
                self.selected = Some(idx);
                self.editor.edit_index = Some(idx);
                self.editor.edit_original = Some(config);
            }
            (None, Some(pos)) if pos <= self.rules.len() => {
                self.rules.insert(pos, Rule::new(config));
                self.selected = Some(pos);
            }
            _ => {
                self.rules.push(Rule::new(config));
                self.selected = Some(self.rules.len() - 1);
            }
        }
        self.all_selected = false;
        self.editor.insert_at = None;
        self.editor.meta_target = None;
    }

    pub fn remove_selected(&mut self) {
        if self.all_selected {
            self.clear_all();
            return;
        }
        let Some(i) = self.selected else { return };
        if i < self.rules.len() {
            self.rules.remove(i);
            self.selected = if self.rules.is_empty() { None } else { Some(i.min(self.rules.len() - 1)) };
        }
    }

    pub fn clear_all(&mut self) {
        self.rules.clear();
        self.selected = None;
        self.all_selected = false;
    }

    pub fn duplicate_selected(&mut self) {
        let Some(i) = self.selected else { return };
        if let Some(cloned) = self.rules.get(i).cloned() {
            let at = i + 1;
            self.rules.insert(at, cloned);
            self.selected = Some(at);
            self.all_selected = false;
        }
    }

    pub fn select_all(&mut self) {
        if !self.rules.is_empty() {
            self.all_selected = true;
            if self.selected.is_none() {
                self.selected = Some(0);
            }
        }
    }

    pub fn set_all_enabled(&mut self, enabled: bool) {
        for r in &mut self.rules {
            r.enabled = enabled;
        }
    }

    pub fn open_comment(&mut self, idx: usize) {
        if let Some(r) = self.rules.get(idx) {
            self.selected = Some(idx);
            self.all_selected = false;
            self.comment_edit = TextEdit::new(r.comment.clone());
            self.popup = Some(RulesPopup::Comment(idx));
            self.stack_menu = None;
        }
    }

    pub fn export_rules_text(&self) -> String {
        let mut lines = Vec::with_capacity(self.rules.len() + 1);
        lines.push(format!("#\t{}\t{}\t{}", t("Ativo", "Active"), t("Regra", "Rule"), t("Instrução", "Instruction")));
        for (i, r) in self.rules.iter().enumerate() {
            let mark = if r.enabled { "[x]" } else { "[ ]" };
            let cmt = if r.comment.trim().is_empty() {
                String::new()
            } else {
                format!(" // {}", r.comment.trim())
            };
            lines.push(format!("{}\t{mark}\t{}\t{}{cmt}", i + 1, r.config.kind().label(), r.summary()));
        }
        lines.join("\n")
    }

    pub fn move_selected(&mut self, delta: isize) {
        let Some(i) = self.selected else { return };
        let j = (i as isize + delta).clamp(0, self.rules.len().saturating_sub(1) as isize) as usize;
        if i != j && j < self.rules.len() {
            self.rules.swap(i, j);
            self.selected = Some(j);
            self.all_selected = false;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Rule Execution Engine

/// Evaluates all enabled rules in `rules` across `items`, returning `(new_name, error)` per item.
pub fn evaluate_rules(rules: &[Rule], items: &[Entry], checked: &[bool], loc: &Location) -> Vec<(String, Option<String>)> {
    let mut names: Vec<String> = items.iter().map(|e| shown(&e.name)).collect();
    let mut errors: Vec<Option<String>> = vec![None; items.len()];

    for rule in rules {
        if !rule.enabled {
            continue;
        }
        apply_rule_batch(&rule.config, items, checked, loc, &mut names, &mut errors);
    }

    names.into_iter().zip(errors).collect()
}

/// Evaluates rules step-by-step on a single sample name (used in the Analyze modal).
pub fn analyze_sample(rules: &[Rule], sample: &str, entry: &Entry, loc: &Location) -> Vec<(String, String, Option<String>)> {
    let mut cur = sample.to_string();
    let mut steps = Vec::with_capacity(rules.len());
    let single_entry = Entry { name: sample.to_string(), ..entry.clone() };
    let items = [single_entry];
    let checked = [true];
    for rule in rules {
        if !rule.enabled {
            steps.push((rule.config.kind().label().to_string(), cur.clone(), None));
            continue;
        }
        let mut names = vec![cur];
        let mut errs = vec![None];
        apply_rule_batch(&rule.config, &items, &checked, loc, &mut names, &mut errs);
        cur = names.pop().unwrap();
        let err = errs.pop().unwrap();
        steps.push((rule.config.kind().label().to_string(), cur.clone(), err));
    }
    steps
}

fn split_stem_ext(name: &str, is_dir: bool, skip_ext: bool) -> (String, String) {
    if !skip_ext || is_dir {
        return (name.to_string(), String::new());
    }
    match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_string(), name[i..].to_string()),
        _ => (name.to_string(), String::new()),
    }
}

fn apply_rule_batch(
    cfg: &RuleConfig,
    items: &[Entry],
    checked: &[bool],
    loc: &Location,
    names: &mut [String],
    errors: &mut [Option<String>],
) {
    match cfg {
        RuleConfig::Serialize {
            start,
            repeat,
            step,
            reset_every_on,
            reset_every,
            reset_folder_change,
            reset_name_change,
            pad_zeros,
            pad_length,
            num_system,
            where_mode,
            position,
            skip_ext,
        } => {
            let mut active_idx = 0u64;
            let mut prev_folder: Option<PathBuf> = None;
            let mut prev_name: Option<String> = None;
            let rep = (*repeat).max(1) as u64;
            let reset_n = (*reset_every).max(1) as u64;

            for (i, item) in items.iter().enumerate() {
                if !checked.get(i).copied().unwrap_or(true) {
                    continue;
                }
                let cur_folder = loc.local_path(item).and_then(|p| p.parent().map(|d| d.to_path_buf()));
                let (stem, ext) = split_stem_ext(&names[i], item.is_dir, *skip_ext);

                if *reset_folder_change && prev_folder.as_ref().is_some_and(|pf| Some(pf) != cur_folder.as_ref()) {
                    active_idx = 0;
                }
                if *reset_name_change && prev_name.as_ref().is_some_and(|pn| pn != &stem) {
                    active_idx = 0;
                }
                prev_folder = cur_folder;
                prev_name = Some(stem.clone());

                let seq_step = if *reset_every_on { (active_idx / rep) % reset_n } else { active_idx / rep };
                let num_val = start + (seq_step as i64) * step;
                let mut formatted = format_num_system(num_val, *num_system);
                if *pad_zeros && formatted.len() < *pad_length {
                    let sign = if formatted.starts_with('-') { "-" } else { "" };
                    let body = formatted.trim_start_matches('-');
                    let zeros = "0".repeat(pad_length.saturating_sub(body.len()));
                    formatted = format!("{sign}{zeros}{body}");
                }
                let next_base = insert_into(&stem, &formatted, *where_mode, *position, false, "", "");
                names[i] = format!("{next_base}{ext}");
                active_idx += 1;
            }
        }
        RuleConfig::Random { length, unique, digits, letters, custom, custom_chars, where_mode, position, skip_ext, seed } => {
            let mut pool: Vec<char> = Vec::new();
            if *digits {
                pool.extend('0'..='9');
            }
            if *letters {
                pool.extend('a'..='z');
            }
            if *custom {
                pool.extend(custom_chars.chars());
            }
            if pool.is_empty() {
                pool.extend('0'..='9');
            }
            let len = (*length).max(1).min(64);
            let mut rng = SmallRng::seed_from_u64(*seed);
            let mut used: HashSet<String> = HashSet::new();

            for (i, item) in items.iter().enumerate() {
                if !checked.get(i).copied().unwrap_or(true) {
                    continue;
                }
                let mut token = String::with_capacity(len);
                for _attempt in 0..64 {
                    token.clear();
                    for _ in 0..len {
                        let idx = rng.random_range(0..pool.len());
                        token.push(pool[idx]);
                    }
                    if !*unique || used.insert(token.clone()) {
                        break;
                    }
                }
                let (stem, ext) = split_stem_ext(&names[i], item.is_dir, *skip_ext);
                let next_base = insert_into(&stem, &token, *where_mode, *position, false, "", "");
                names[i] = format!("{next_base}{ext}");
            }
        }
        RuleConfig::UserInput { lines, mode, skip_ext, trim_lines, skip_empty } => {
            let mut list: Vec<String> = lines
                .lines()
                .map(|l| if *trim_lines { l.trim().to_string() } else { l.to_string() })
                .filter(|l| !*skip_empty || !l.is_empty())
                .collect();
            let mut line_idx = 0usize;
            for (i, item) in items.iter().enumerate() {
                if !checked.get(i).copied().unwrap_or(true) {
                    continue;
                }
                let Some(val) = list.get(line_idx).cloned() else { break };
                line_idx += 1;
                if val.is_empty() && *mode == UserInputMode::Replace {
                    continue;
                }
                let (stem, ext) = split_stem_ext(&names[i], item.is_dir, *skip_ext);
                let next_base = match mode {
                    UserInputMode::Before => format!("{val}{stem}"),
                    UserInputMode::After => format!("{stem}{val}"),
                    UserInputMode::Replace => val,
                };
                names[i] = format!("{next_base}{ext}");
            }
            let _ = &mut list;
        }
        RuleConfig::Mapping { entries, allow_reuse, partial_match, inverse, case_sensitive, skip_ext } => {
            // Each row as (what to find, what to put, the regex of a partial match ignoring case).
            let rows: Vec<(&str, &str, Option<Regex>)> = entries
                .iter()
                .map(|(a, b)| {
                    let (src, dst) = if *inverse { (b.as_str(), a.as_str()) } else { (a.as_str(), b.as_str()) };
                    let any_case = *partial_match && !*case_sensitive && !src.is_empty();
                    (src, dst, any_case.then(|| RegexBuilder::new(&regex::escape(src)).case_insensitive(true).build().ok()).flatten())
                })
                .collect();
            let mut used: HashSet<usize> = HashSet::new();
            for (i, item) in items.iter().enumerate() {
                if !checked.get(i).copied().unwrap_or(true) {
                    continue;
                }
                let (stem, ext) = split_stem_ext(&names[i], item.is_dir, *skip_ext);
                for (row_idx, (src, dst, any_case)) in rows.iter().enumerate() {
                    let (src, dst) = (*src, *dst);
                    if src.is_empty() || (!*allow_reuse && used.contains(&row_idx)) {
                        continue;
                    }
                    let matched = if *partial_match {
                        if *case_sensitive {
                            if stem.contains(src) {
                                Some(stem.replace(src, dst))
                            } else {
                                None
                            }
                        } else {
                            any_case.as_ref().filter(|r| r.is_match(&stem)).map(|r| r.replace_all(&stem, NoExpand(dst)).into_owned())
                        }
                    } else if *case_sensitive {
                        (stem == src).then(|| dst.to_string())
                    } else {
                        stem.eq_ignore_ascii_case(src).then(|| dst.to_string())
                    };
                    if let Some(next_base) = matched {
                        used.insert(row_idx);
                        names[i] = format!("{next_base}{ext}");
                        break;
                    }
                }
            }
        }
        RuleConfig::PascalScript { code } => match rules_script::compile(code) {
            Ok(compiled) => {
                for (i, item) in items.iter().enumerate() {
                    if !checked.get(i).copied().unwrap_or(true) {
                        continue;
                    }
                    match rules_script::run(&compiled, &names[i], i, item, loc) {
                        Ok(next) => names[i] = next,
                        Err(e) => {
                            if errors[i].is_none() {
                                errors[i] = Some(e);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                for (i, _) in items.iter().enumerate() {
                    if checked.get(i).copied().unwrap_or(true) && errors[i].is_none() {
                        errors[i] = Some(e.clone());
                    }
                }
            }
        },
        // Per-item rules:
        other => {
            let prepared = Prepared::of(other);
            for (i, item) in items.iter().enumerate() {
                if !checked.get(i).copied().unwrap_or(true) {
                    continue;
                }
                match apply_single_rule(other, &prepared, &names[i], item, loc) {
                    Ok(next) => names[i] = next,
                    Err(e) => {
                        if errors[i].is_none() {
                            errors[i] = Some(e);
                        }
                    }
                }
            }
        }
    }
}

/// What a rule works out once for the whole batch rather than again for each item: compiling a
/// regex costs far more than running it on a name.
enum Prepared<'a> {
    Nothing,
    /// Remove and Replace: the text they look for.
    Find(Result<Regex, String>),
    /// Case: each fragment kept as typed, with the regex that finds it.
    Fragments(Vec<(Regex, &'a str)>),
    /// Rearrange by delimiters: `None` with no delimiter to split at.
    Delimiters(Option<Regex>),
    /// Reformat Date: `None` with a format left empty.
    Date(Result<Option<DatePlan<'a>>, String>),
    /// Regular Expressions: `None` when the pattern holds a meta tag, which each item fills in.
    Pattern(Option<Result<Regex, String>>),
}

impl<'a> Prepared<'a> {
    fn of(cfg: &'a RuleConfig) -> Self {
        match cfg {
            RuleConfig::Remove { text: find, case_sensitive, whole_words, wildcards, .. } | RuleConfig::Replace { find, case_sensitive, whole_words, wildcards, .. }
                if !find.is_empty() =>
            {
                Self::Find(find_regex(find, *case_sensitive, *whole_words, *wildcards))
            }
            RuleConfig::Case { force_fragments: true, fragments, .. } => Self::Fragments(
                fragments
                    .split(',')
                    .map(str::trim)
                    .filter(|f| !f.is_empty())
                    .filter_map(|f| Regex::new(&format!(r"(?i)\b{}\b", regex::escape(f))).ok().map(|re| (re, f)))
                    .collect(),
            ),
            RuleConfig::Rearrange { split_mode: RearrangeSplit::Delimiters, delimiters, .. } => Self::Delimiters(delimiters_regex(delimiters)),
            RuleConfig::ReformatDate { find_fmt, replace_fmt, custom_short_months, short_months, custom_long_months, long_months, .. } => {
                let short = month_names(*custom_short_months, short_months, EN_SHORT_MONTHS);
                let long = month_names(*custom_long_months, long_months, EN_LONG_MONTHS);
                Self::Date(DatePlan::new(find_fmt, replace_fmt, short, long))
            }
            RuleConfig::Regex { pattern, case_sensitive, .. } if !pattern.is_empty() => Self::Pattern((!rules_meta::has_tags(pattern)).then(|| pattern_regex(pattern, *case_sensitive))),
            _ => Self::Nothing,
        }
    }
}

fn pattern_regex(pattern: &str, case_sensitive: bool) -> Result<Regex, String> {
    RegexBuilder::new(pattern).case_insensitive(!case_sensitive).build().map_err(|e| tr!("Regex inválida: {e}", "Invalid regex: {e}"))
}

fn apply_single_rule(cfg: &RuleConfig, prepared: &Prepared, current: &str, item: &Entry, loc: &Location) -> Result<String, String> {
    match cfg {
        RuleConfig::Insert { text, where_mode, position, rtl, after_text, before_text, skip_ext } => {
            let expanded = rules_meta::expand_tags(text, item, loc);
            let (stem, ext) = split_stem_ext(current, item.is_dir, *skip_ext);
            let next_base = insert_into(&stem, &expanded, *where_mode, *position, *rtl, after_text, before_text);
            Ok(format!("{next_base}{ext}"))
        }
        RuleConfig::Delete {
            from_mode,
            from_pos,
            from_delim,
            until_mode,
            until_count,
            until_delim,
            delete_current,
            skip_ext,
            rtl,
            keep_delimiters,
        } => {
            let (stem, ext) = split_stem_ext(current, item.is_dir, *skip_ext);
            if *delete_current {
                return Ok(ext);
            }
            let chars: Vec<char> = stem.chars().collect();
            let len = chars.len();
            if len == 0 {
                return Ok(current.to_string());
            }

            let start_char = match from_mode {
                DeleteFrom::Position => {
                    let p = (*from_pos).max(1) - 1;
                    if *rtl { len.saturating_sub(p) } else { p.min(len) }
                }
                DeleteFrom::Delimiter => {
                    if from_delim.is_empty() {
                        return Ok(current.to_string());
                    }
                    let byte_idx = if *rtl { stem.rfind(from_delim) } else { stem.find(from_delim) };
                    let Some(bi) = byte_idx else { return Ok(current.to_string()) };
                    let c_idx = stem[..bi].chars().count();
                    if *keep_delimiters {
                        c_idx + from_delim.chars().count()
                    } else {
                        c_idx
                    }
                }
            };

            let end_char = match until_mode {
                DeleteUntil::UntilEnd => {
                    if *rtl && *from_mode == DeleteFrom::Position {
                        0
                    } else {
                        len
                    }
                }
                DeleteUntil::Count => {
                    if *rtl && *from_mode == DeleteFrom::Position {
                        start_char.saturating_sub(*until_count)
                    } else {
                        (start_char + *until_count).min(len)
                    }
                }
                DeleteUntil::Delimiter => {
                    if until_delim.is_empty() {
                        return Ok(current.to_string());
                    }
                    let search_from_char = if *rtl { 0 } else { start_char };
                    let byte_off: usize = chars[..search_from_char].iter().map(|c| c.len_utf8()).sum();
                    let found_byte = if *rtl {
                        // `start_char` counts characters; slice at its byte offset.
                        let start_byte: usize = chars[..start_char.min(len)].iter().map(|c| c.len_utf8()).sum();
                        stem[..start_byte].rfind(until_delim)
                    } else {
                        stem[byte_off..].find(until_delim).map(|rel| byte_off + rel)
                    };
                    let Some(bi) = found_byte else { return Ok(current.to_string()) };
                    let c_idx = stem[..bi].chars().count();
                    if *keep_delimiters {
                        c_idx
                    } else {
                        c_idx + until_delim.chars().count()
                    }
                }
            };

            let (lo, hi) = if start_char <= end_char { (start_char, end_char) } else { (end_char, start_char) };
            let next_base: String = chars[..lo].iter().chain(&chars[hi..]).collect();
            Ok(format!("{next_base}{ext}"))
        }
        RuleConfig::Remove { occurrences, skip_ext, wildcards, .. } => {
            // Nothing prepared: there's no text to remove.
            let Prepared::Find(re) = prepared else { return Ok(current.to_string()) };
            let (stem, ext) = split_stem_ext(current, item.is_dir, *skip_ext);
            let next_base = replace_occurrences(&stem, re.as_ref().map_err(String::clone)?, "", *occurrences, *wildcards);
            Ok(format!("{next_base}{ext}"))
        }
        RuleConfig::Replace { replace, occurrences, skip_ext, wildcards, .. } => {
            let Prepared::Find(re) = prepared else { return Ok(current.to_string()) };
            let rep = rules_meta::expand_tags(replace, item, loc);
            let (stem, ext) = split_stem_ext(current, item.is_dir, *skip_ext);
            let next_base = replace_occurrences(&stem, re.as_ref().map_err(String::clone)?, &rep, *occurrences, *wildcards);
            Ok(format!("{next_base}{ext}"))
        }
        RuleConfig::Rearrange { split_mode, delimiters, new_pattern, skip_ext, rtl } => {
            let (stem, ext) = split_stem_ext(current, item.is_dir, *skip_ext);
            let pat = rules_meta::expand_tags(new_pattern, item, loc);
            let by_delimiters = match prepared {
                Prepared::Delimiters(re) => re.as_ref(),
                _ => None,
            };
            let parts = split_for_rearrange(&stem, *split_mode, delimiters, by_delimiters, *rtl);
            let next_base = apply_rearrange_pattern(&stem, &parts, &pat);
            Ok(format!("{next_base}{ext}"))
        }
        RuleConfig::Extension { set_new, new_ext, append_original, detect_binary, remove_duplicates, dup_case_sensitive } => {
            let mut out = current.to_string();
            if *remove_duplicates {
                out = strip_duplicate_extensions(&out, *dup_case_sensitive);
            }
            // Through the tags, which read each file once: the preview runs again at every change.
            if *detect_binary
                && let Some(detected_ext) = rules_meta::resolve_tag("BinarySign_Extension", item, loc).filter(|ext| !ext.is_empty())
            {
                let stem = match out.rfind('.') {
                    Some(i) if i > 0 => &out[..i],
                    _ => out.as_str(),
                };
                out = format!("{stem}.{detected_ext}");
            } else if *set_new {
                let ext_clean = new_ext.trim().trim_start_matches('.');
                if *append_original {
                    out = if ext_clean.is_empty() { out } else { format!("{out}.{ext_clean}") };
                } else {
                    let stem = match out.rfind('.') {
                        Some(i) if i > 0 => &out[..i],
                        _ => out.as_str(),
                    };
                    out = if ext_clean.is_empty() { stem.to_string() } else { format!("{stem}.{ext_clean}") };
                }
            }
            Ok(out)
        }
        RuleConfig::Strip {
            english,
            english_chars,
            digits,
            digits_chars,
            symbols,
            symbols_chars,
            brackets,
            brackets_chars,
            custom,
            custom_chars,
            unicode_range,
            unicode_range_str,
            where_mode,
            invert,
            case_sensitive,
            skip_ext,
        } => {
            let (stem, ext) = split_stem_ext(current, item.is_dir, *skip_ext);
            let mut chars_set: HashSet<char> = HashSet::new();
            let add_chars = |set: &mut HashSet<char>, s: &str| {
                for ch in s.chars() {
                    if *case_sensitive {
                        set.insert(ch);
                    } else {
                        for lc in ch.to_lowercase() {
                            set.insert(lc);
                        }
                    }
                }
            };
            if *english {
                add_chars(&mut chars_set, english_chars);
            }
            if *digits {
                add_chars(&mut chars_set, digits_chars);
            }
            if *symbols {
                add_chars(&mut chars_set, symbols_chars);
            }
            if *brackets {
                add_chars(&mut chars_set, brackets_chars);
            }
            if *custom {
                add_chars(&mut chars_set, custom_chars);
            }
            let range_bounds = if *unicode_range { parse_hex_range(unicode_range_str) } else { None };

            let is_selected = |ch: char| -> bool {
                let in_set = if *case_sensitive {
                    chars_set.contains(&ch)
                } else {
                    ch.to_lowercase().any(|lc| chars_set.contains(&lc))
                };
                let in_range = range_bounds.is_some_and(|(lo, hi)| (ch as u32) >= lo && (ch as u32) <= hi);
                let hit = in_set || in_range;
                if *invert { !hit } else { hit }
            };

            let next_base: String = match where_mode {
                StripWhere::Everywhere => stem.chars().filter(|&c| !is_selected(c)).collect(),
                StripWhere::Main => stem.chars().skip_while(|&c| is_selected(c)).collect(),
                StripWhere::Right => {
                    let mut v: Vec<char> = stem.chars().collect();
                    while v.last().is_some_and(|&c| is_selected(c)) {
                        v.pop();
                    }
                    v.into_iter().collect()
                }
            };
            Ok(format!("{next_base}{ext}"))
        }
        RuleConfig::Case { mode, preserve_case, skip_ext, ext_lower, ext_upper, .. } => {
            let (stem, mut ext) = split_stem_ext(current, item.is_dir, *skip_ext);
            let mut next_base = apply_case(&stem, *mode, *preserve_case);
            if let Prepared::Fragments(kept) = prepared {
                for (re, frag) in kept {
                    next_base = re.replace_all(&next_base, NoExpand(frag)).into_owned();
                }
            }
            if *ext_lower {
                ext = ext.to_lowercase();
            } else if *ext_upper {
                ext = ext.to_uppercase();
            }
            Ok(format!("{next_base}{ext}"))
        }
        RuleConfig::Padding { pad_numbers, pad_numbers_len, strip_zeros, pad_text, pad_text_len, pad_text_chars, pad_text_side, skip_ext } => {
            let (stem, ext) = split_stem_ext(current, item.is_dir, *skip_ext);
            let mut out = stem;
            if *strip_zeros {
                out = RE_LEADING_ZEROS.replace_all(&out, "$1").into_owned();
            }
            if *pad_numbers && *pad_numbers_len > 0 {
                let w = *pad_numbers_len;
                out = RE_DIGITS
                    .replace_all(&out, |caps: &regex::Captures| {
                        let d = &caps[0];
                        if d.len() < w {
                            format!("{}{d}", "0".repeat(w - d.len()))
                        } else {
                            d.to_string()
                        }
                    })
                    .into_owned();
            }
            if *pad_text && *pad_text_len > out.chars().count() {
                let pad_unit = if pad_text_chars.is_empty() { " " } else { pad_text_chars.as_str() };
                let need = pad_text_len - out.chars().count();
                let mut pad_str = String::new();
                while pad_str.chars().count() < need {
                    pad_str.push_str(pad_unit);
                }
                let pad_exact: String = pad_str.chars().take(need).collect();
                out = match pad_text_side {
                    Side::Left => format!("{pad_exact}{out}"),
                    Side::Right => format!("{out}{pad_exact}"),
                };
            }
            Ok(format!("{out}{ext}"))
        }
        RuleConfig::CleanUp {
            strip_parens,
            strip_brackets,
            strip_braces,
            replace_dot,
            replace_comma,
            replace_underscore,
            replace_plus,
            replace_dash,
            replace_pct20,
            skip_versions,
            fix_spaces,
            normalize_unicode_spaces,
            strip_emoji,
            strip_accents,
            space_before_camel,
            prepare_sharepoint,
            skip_ext,
        } => {
            let effective_skip = *skip_ext && !*prepare_sharepoint;
            let (mut s, ext) = split_stem_ext(current, item.is_dir, effective_skip);
            if *strip_parens {
                s = RE_IN_PARENS.replace_all(&s, " ").into_owned();
            }
            if *strip_brackets {
                s = RE_IN_BRACKETS.replace_all(&s, " ").into_owned();
            }
            if *strip_braces {
                s = RE_IN_BRACES.replace_all(&s, " ").into_owned();
            }
            if *replace_pct20 {
                // `%20` has no letters: there's no case to ignore.
                s = s.replace("%20", " ");
            }
            if *replace_dot {
                if *skip_versions {
                    // A dot between two digits stays: it's a version (`1.2.3`).
                    let mut out = String::with_capacity(s.len());
                    let chars: Vec<char> = s.chars().collect();
                    for (idx, &ch) in chars.iter().enumerate() {
                        if ch == '.' && idx > 0 && idx + 1 < chars.len() && chars[idx - 1].is_ascii_digit() && chars[idx + 1].is_ascii_digit() {
                            out.push('.');
                        } else if ch == '.' {
                            out.push(' ');
                        } else {
                            out.push(ch);
                        }
                    }
                    s = out;
                } else {
                    s = s.replace('.', " ");
                }
            }
            if *replace_comma {
                s = s.replace(',', " ");
            }
            if *replace_underscore {
                s = s.replace('_', " ");
            }
            if *replace_plus {
                s = s.replace('+', " ");
            }
            if *replace_dash {
                s = s.replace('-', " ");
            }
            if *normalize_unicode_spaces {
                s = s
                    .chars()
                    .map(|c| if c.is_whitespace() { ' ' } else { c })
                    .collect();
            }
            if *strip_emoji {
                s = s
                    .chars()
                    .filter(|&c| {
                        let u = c as u32;
                        !((0x1F300..=0x1FAFF).contains(&u) || (0x2600..=0x27BF).contains(&u) || u == 0xFE0F)
                    })
                    .collect();
            }
            if *strip_accents {
                s = strip_diacritics(&s);
            }
            if *space_before_camel {
                let mut out = String::with_capacity(s.len() + 8);
                let mut prev: Option<char> = None;
                for ch in s.chars() {
                    if let Some(p) = prev
                        && p.is_lowercase()
                        && ch.is_uppercase()
                    {
                        out.push(' ');
                    }
                    out.push(ch);
                    prev = Some(ch);
                }
                s = out;
            }
            if *fix_spaces {
                s = s.split_whitespace().collect::<Vec<_>>().join(" ");
            }
            if *prepare_sharepoint {
                s = s
                    .chars()
                    .map(|c| if matches!(c, '~' | '"' | '#' | '%' | '&' | '*' | ':' | '<' | '>' | '?' | '/' | '\\' | '{' | '|' | '}') { '_' } else { c })
                    .collect::<String>()
                    .trim_matches(['.', ' '])
                    .to_string();
            }
            Ok(format!("{s}{ext}"))
        }
        RuleConfig::Translit { alphabet, direction, auto_case, skip_ext } => {
            let (stem, ext) = split_stem_ext(current, item.is_dir, *skip_ext);
            let pairs = parse_translit_alphabet(alphabet, *direction);
            let mut out = stem;
            for (src, dst) in &pairs {
                if src.is_empty() {
                    continue;
                }
                if *auto_case {
                    out = out.replace(src, dst);
                    let src_up = src.to_uppercase();
                    let dst_up = dst.to_uppercase();
                    if src_up != *src {
                        out = out.replace(&src_up, &dst_up);
                    }
                } else {
                    out = out.replace(src, dst);
                }
            }
            Ok(format!("{out}{ext}"))
        }
        RuleConfig::ReformatDate { whole_words, skip_ext, adjust_on, adjust_amount, adjust_unit, .. } => {
            let plan = match prepared {
                Prepared::Date(Ok(Some(plan))) => plan,
                Prepared::Date(Err(e)) => return Err(e.clone()),
                _ => return Ok(current.to_string()),
            };
            let (stem, ext) = split_stem_ext(current, item.is_dir, *skip_ext);
            let adjust = adjust_on.then_some((*adjust_amount, *adjust_unit));
            let next_base = reformat_date_in_str(&stem, plan, *whole_words, adjust);
            Ok(format!("{next_base}{ext}"))
        }
        RuleConfig::Regex { pattern, replace, case_sensitive, skip_ext } => {
            let per_item;
            let re = match prepared {
                Prepared::Pattern(Some(re)) => re.as_ref().map_err(String::clone)?,
                Prepared::Pattern(None) => {
                    per_item = pattern_regex(&rules_meta::expand_tags(pattern, item, loc), *case_sensitive)?;
                    &per_item
                }
                // Nothing prepared: the pattern is empty.
                _ => return Ok(current.to_string()),
            };
            let expanded_rep = rules_meta::expand_tags(replace, item, loc);
            let (stem, ext) = split_stem_ext(current, item.is_dir, *skip_ext);
            let next_base = re
                .replace_all(&stem, |caps: &regex::Captures| expand_regex_replace(caps, &expanded_rep))
                .into_owned();
            Ok(format!("{next_base}{ext}"))
        }
        _ => Ok(current.to_string()),
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers

// The regexes that never change, compiled once for the program.
static RE_LEADING_ZEROS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b0+(\d+)\b").unwrap());
static RE_DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());
static RE_IN_PARENS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\([^)]*\)").unwrap());
static RE_IN_BRACKETS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[[^\]]*\]").unwrap());
static RE_IN_BRACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{[^}]*\}").unwrap());
/// `$1`, `$-1`, `$0` in a Rearrange pattern.
static RE_PART_REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$(-?\d+)").unwrap());

/// Expands regex replacement backreferences (`$0`, `$1`, `${name}`, `$$`, `$&`) without treating
/// a trailing underscore (as in `$1_$2`) as part of the capture group name.
fn expand_regex_replace(caps: &regex::Captures, tpl: &str) -> String {
    let mut out = String::with_capacity(tpl.len());
    let mut i = 0;
    let bytes = tpl.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if next == b'$' {
                out.push('$');
                i += 2;
                continue;
            }
            if next == b'&' {
                out.push_str(&caps[0]);
                i += 2;
                continue;
            }
            if next == b'{'
                && let Some(end) = tpl[i + 2..].find('}')
            {
                let key = &tpl[i + 2..i + 2 + end];
                if let Ok(idx) = key.parse::<usize>() {
                    if let Some(m) = caps.get(idx) {
                        out.push_str(m.as_str());
                    }
                } else if let Some(m) = caps.name(key) {
                    out.push_str(m.as_str());
                }
                i += 3 + end;
                continue;
            }
            if next.is_ascii_digit() {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() && j - (i + 1) < 2 {
                    j += 1;
                }
                // Prefer 2-digit index only if that capture group exists; otherwise 1 digit.
                let full: usize = tpl[i + 1..j].parse().unwrap_or(0);
                if j - (i + 1) == 2 && full >= caps.len() {
                    let one = (bytes[i + 1] - b'0') as usize;
                    if let Some(m) = caps.get(one) {
                        out.push_str(m.as_str());
                    }
                    i += 2;
                } else {
                    if let Some(m) = caps.get(full) {
                        out.push_str(m.as_str());
                    }
                    i = j;
                }
                continue;
            }
        }
        let ch = tpl[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn insert_into(stem: &str, text: &str, mode: InsertWhere, pos: usize, rtl: bool, after: &str, before: &str) -> String {
    if text.is_empty() && mode != InsertWhere::ReplaceCurrent {
        return stem.to_string();
    }
    match mode {
        InsertWhere::Prefix => format!("{text}{stem}"),
        InsertWhere::Suffix => format!("{stem}{text}"),
        InsertWhere::ReplaceCurrent => text.to_string(),
        InsertWhere::Position => {
            let chars: Vec<char> = stem.chars().collect();
            let p = pos.max(1) - 1;
            let idx = if rtl { chars.len().saturating_sub(p) } else { p.min(chars.len()) };
            let head: String = chars[..idx].iter().collect();
            let tail: String = chars[idx..].iter().collect();
            format!("{head}{text}{tail}")
        }
        InsertWhere::AfterText => {
            if after.is_empty() {
                return stem.to_string();
            }
            let hit = if rtl { stem.rfind(after) } else { stem.find(after) };
            match hit {
                Some(i) => format!("{}{text}{}", &stem[..i + after.len()], &stem[i + after.len()..]),
                None => stem.to_string(),
            }
        }
        InsertWhere::BeforeText => {
            if before.is_empty() {
                return stem.to_string();
            }
            let hit = if rtl { stem.rfind(before) } else { stem.find(before) };
            match hit {
                Some(i) => format!("{}{text}{}", &stem[..i], &stem[i..]),
                None => stem.to_string(),
            }
        }
    }
}

/// The regex that finds what Remove and Replace look for.
fn find_regex(find: &str, case_sensitive: bool, whole_words: bool, wildcards: bool) -> Result<Regex, String> {
    let mut pat = if wildcards { wildcard_to_regex(find) } else { regex::escape(find) };
    if whole_words {
        pat = format!(r"\b(?:{pat})\b");
    }
    RegexBuilder::new(&pat).case_insensitive(!case_sensitive).build().map_err(|e| e.to_string())
}

fn replace_occurrences(hay: &str, re: &Regex, replace: &str, occ: Occurrences, wildcards: bool) -> String {
    let expand = |caps: &regex::Captures| -> String {
        if wildcards {
            let mut out = replace.to_string();
            for g in (1..caps.len()).rev() {
                let val = caps.get(g).map(|m| m.as_str()).unwrap_or("");
                out = out.replace(&format!("${g}"), val);
            }
            out
        } else {
            replace.to_string()
        }
    };

    match occ {
        Occurrences::All => re.replace_all(hay, |c: &regex::Captures| expand(c)).into_owned(),
        Occurrences::First => re.replace(hay, |c: &regex::Captures| expand(c)).into_owned(),
        Occurrences::Last => {
            let all: Vec<_> = re.captures_iter(hay).collect();
            let Some(last) = all.last() else { return hay.to_string() };
            let m = last.get(0).unwrap();
            let rep = expand(last);
            format!("{}{rep}{}", &hay[..m.start()], &hay[m.end()..])
        }
    }
}

fn wildcard_to_regex(pat: &str) -> String {
    let mut out = String::with_capacity(pat.len() * 2);
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '*' => out.push_str("(.*?)"),
            '?' => out.push_str("(.)"),
            '[' => {
                if let Some(rel) = chars[i + 1..].iter().position(|&c| c == ']') {
                    let cls: String = chars[i..=i + 1 + rel].iter().collect();
                    out.push('(');
                    out.push_str(&cls);
                    out.push(')');
                    i += rel + 1;
                } else {
                    out.push_str(r"\[");
                }
            }
            c => out.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    out
}

/// The regex that splits a name at any of `delimiters` (`a|b|c`); `None` when there are none.
fn delimiters_regex(delimiters: &str) -> Option<Regex> {
    let seps: Vec<String> = delimiters.split('|').filter(|s| !s.is_empty()).map(regex::escape).collect();
    if seps.is_empty() { None } else { Regex::new(&seps.join("|")).ok() }
}

/// `by_delimiters` is [`delimiters_regex`] of `delimiters`, used when `mode` splits by them.
fn split_for_rearrange(stem: &str, mode: RearrangeSplit, delimiters: &str, by_delimiters: Option<&Regex>, rtl: bool) -> Vec<String> {
    let mut parts: Vec<String> = match mode {
        RearrangeSplit::ExactPattern => {
            if delimiters.is_empty() {
                vec![stem.to_string()]
            } else {
                stem.split(delimiters).map(str::to_string).collect()
            }
        }
        RearrangeSplit::Delimiters => match by_delimiters {
            Some(re) => re.split(stem).map(str::to_string).collect(),
            None => vec![stem.to_string()],
        },
        RearrangeSplit::Positions => {
            let mut cuts: Vec<usize> = delimiters
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|s| s.parse::<usize>().ok())
                .filter(|&p| p > 0)
                .collect();
            cuts.sort_unstable();
            cuts.dedup();
            let chars: Vec<char> = stem.chars().collect();
            let mut prev = 0usize;
            let mut out = Vec::new();
            for cut in cuts {
                let c = cut.min(chars.len());
                if c > prev {
                    out.push(chars[prev..c].iter().collect());
                    prev = c;
                }
            }
            if prev < chars.len() {
                out.push(chars[prev..].iter().collect());
            }
            out
        }
    };
    if rtl {
        parts.reverse();
    }
    parts
}

fn apply_rearrange_pattern(orig: &str, parts: &[String], pattern: &str) -> String {
    RE_PART_REF.replace_all(pattern, |caps: &regex::Captures| {
        let idx: isize = caps[1].parse().unwrap_or(0);
        if idx == 0 {
            orig.to_string()
        } else if idx > 0 {
            parts.get((idx - 1) as usize).cloned().unwrap_or_default()
        } else {
            let from_end = (-idx) as usize;
            if from_end <= parts.len() {
                parts[parts.len() - from_end].clone()
            } else {
                String::new()
            }
        }
    })
    .into_owned()
}

fn strip_duplicate_extensions(name: &str, case_sensitive: bool) -> String {
    let Some((base, ext)) = name.rsplit_once('.') else { return name.to_string() };
    let mut cur = base.to_string();
    while let Some((b2, e2)) = cur.rsplit_once('.') {
        let same = if case_sensitive { e2 == ext } else { e2.eq_ignore_ascii_case(ext) };
        if same {
            cur = b2.to_string();
        } else {
            break;
        }
    }
    format!("{cur}.{ext}")
}

fn parse_hex_range(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.split_once('-')?;
    let lo = u32::from_str_radix(a.trim(), 16).ok()?;
    let hi = u32::from_str_radix(b.trim(), 16).ok()?;
    Some((lo.min(hi), lo.max(hi)))
}

fn apply_case(s: &str, mode: CaseMode, preserve_case: bool) -> String {
    match mode {
        CaseMode::None => s.to_string(),
        CaseMode::Lower => s.to_lowercase(),
        CaseMode::Upper => s.to_uppercase(),
        CaseMode::Invert => s
            .chars()
            .flat_map(|c| {
                if c.is_uppercase() {
                    c.to_lowercase().collect::<Vec<_>>()
                } else {
                    c.to_uppercase().collect::<Vec<_>>()
                }
            })
            .collect(),
        CaseMode::TitleCase => {
            let mut out = String::with_capacity(s.len());
            let mut start = true;
            for ch in s.chars() {
                if !ch.is_alphanumeric() {
                    start = true;
                    out.push(ch);
                } else if start {
                    for uc in ch.to_uppercase() {
                        out.push(uc);
                    }
                    start = false;
                } else if preserve_case {
                    out.push(ch);
                } else {
                    for lc in ch.to_lowercase() {
                        out.push(lc);
                    }
                }
            }
            out
        }
        CaseMode::FirstLetter => {
            let mut out = String::with_capacity(s.len());
            let mut done = false;
            for ch in s.chars() {
                if !done && ch.is_alphabetic() {
                    for uc in ch.to_uppercase() {
                        out.push(uc);
                    }
                    done = true;
                } else if preserve_case {
                    out.push(ch);
                } else {
                    for lc in ch.to_lowercase() {
                        out.push(lc);
                    }
                }
            }
            out
        }
        CaseMode::Sentence => {
            let mut out = String::with_capacity(s.len());
            let mut cap_next = true;
            for ch in s.chars() {
                if matches!(ch, '.' | '!' | '?') {
                    cap_next = true;
                    out.push(ch);
                } else if cap_next && ch.is_alphabetic() {
                    for uc in ch.to_uppercase() {
                        out.push(uc);
                    }
                    cap_next = false;
                } else if preserve_case {
                    out.push(ch);
                } else {
                    for lc in ch.to_lowercase() {
                        out.push(lc);
                    }
                }
            }
            out
        }
    }
}

fn format_num_system(val: i64, sys: NumSystem) -> String {
    match sys {
        NumSystem::Decimal => val.to_string(),
        NumSystem::Hex => format!("{val:X}"),
        NumSystem::Roman => to_roman(val.max(1) as u32),
        NumSystem::AlphaLower => to_alpha(val.max(1) as u32, false),
        NumSystem::AlphaUpper => to_alpha(val.max(1) as u32, true),
    }
}

fn to_roman(mut n: u32) -> String {
    const MAP: &[(u32, &str)] = &[
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut out = String::new();
    for &(v, sym) in MAP {
        while n >= v {
            out.push_str(sym);
            n -= v;
        }
    }
    out
}

fn to_alpha(mut n: u32, upper: bool) -> String {
    let mut chars = Vec::new();
    while n > 0 {
        n -= 1;
        let base = if upper { b'A' } else { b'a' };
        chars.push((base + (n % 26) as u8) as char);
        n /= 26;
    }
    chars.into_iter().rev().collect()
}

fn strip_diacritics(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'á' | 'à' | 'ã' | 'â' | 'ä' | 'å' => 'a',
            'Á' | 'À' | 'Ã' | 'Â' | 'Ä' | 'Å' => 'A',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'É' | 'È' | 'Ê' | 'Ë' => 'E',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'Í' | 'Ì' | 'Î' | 'Ï' => 'I',
            'ó' | 'ò' | 'õ' | 'ô' | 'ö' | 'ø' => 'o',
            'Ó' | 'Ò' | 'Õ' | 'Ô' | 'Ö' | 'Ø' => 'O',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'Ú' | 'Ù' | 'Û' | 'Ü' => 'U',
            'ç' => 'c',
            'Ç' => 'C',
            'ñ' => 'n',
            'Ñ' => 'N',
            'ý' | 'ÿ' => 'y',
            'Ý' => 'Y',
            _ => c,
        })
        .collect()
}

fn parse_translit_alphabet(text: &str, dir: TranslitDir) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for token in text.split([',', '\n', ';']) {
        let t = token.trim();
        if let Some((a, b)) = t.split_once('=') {
            let (l, r) = (a.trim().to_string(), b.trim().to_string());
            if dir == TranslitDir::Forward {
                out.push((l, r));
            } else {
                out.push((r, l));
            }
        }
    }
    out.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    out
}

pub const DATE_FORMATS: &[&str] = &[
    "yyyy-mm-dd",
    "dd-mm-yyyy",
    "mm-dd-yyyy",
    "yyyy.mm.dd",
    "dd.mm.yyyy",
    "yyyymmdd",
    "yyyy-mm-dd hh-nn-ss",
    "dd-mm-yyyy hh-nn",
];

const EN_SHORT_MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const EN_LONG_MONTHS: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

/// A piece of a ReNamer date format: a field or literal text.
#[derive(Clone, Copy, PartialEq, Debug)]
enum DateTok<'a> {
    Lit(&'a str),
    Year4,
    Year2,
    MonthLong,
    MonthShort,
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

/// Splits a ReNamer date format (`yyyy`, `yy`, `mmmm`, `mmm`, `mm`, `dd`, `hh`, `nn`, `ss`) into
/// its fields, longest token first, everything else as literal text.
fn date_tokens(fmt: &str) -> Vec<DateTok<'_>> {
    const FIELDS: [(&str, DateTok<'static>); 9] = [
        ("yyyy", DateTok::Year4),
        ("yy", DateTok::Year2),
        ("mmmm", DateTok::MonthLong),
        ("mmm", DateTok::MonthShort),
        ("mm", DateTok::Month),
        ("dd", DateTok::Day),
        ("hh", DateTok::Hour),
        ("nn", DateTok::Minute),
        ("ss", DateTok::Second),
    ];
    let mut out = Vec::new();
    let mut rest = fmt;
    while let Some(ch) = rest.chars().next() {
        if let Some((tok, field)) = FIELDS.iter().find(|(tok, _)| rest.starts_with(tok)) {
            out.push(*field);
            rest = &rest[tok.len()..];
        } else {
            out.push(DateTok::Lit(&rest[..ch.len_utf8()]));
            rest = &rest[ch.len_utf8()..];
        }
    }
    out
}

/// The 12 month names a rule reads and writes: the user's list when its box is on and the list
/// has exactly 12 names, English otherwise.
fn month_names(custom: bool, list: &str, english: [&str; 12]) -> [String; 12] {
    if custom {
        let names: Vec<String> = list.split(',').map(|n| n.trim().to_string()).collect();
        if names.len() == 12 && names.iter().all(|n| !n.is_empty()) {
            return names.try_into().unwrap_or_else(|_| english.map(String::from));
        }
    }
    english.map(String::from)
}

/// One capture group that matches any of `names`, ignoring case, longest first so "Junho" is
/// not cut at "Jun".
fn names_group(names: &[String; 12]) -> String {
    let mut sorted: Vec<&String> = names.iter().collect();
    sorted.sort_by_key(|n| std::cmp::Reverse(n.chars().count()));
    let alts: Vec<String> = sorted.iter().map(|n| regex::escape(n)).collect();
    format!("((?i:{}))", alts.join("|"))
}

fn month_of(names: &[String; 12], text: &str) -> Option<u32> {
    let text = text.to_lowercase();
    names.iter().position(|n| n.to_lowercase() == text).map(|i| i as u32 + 1)
}

/// A Reformat Date rule ready to run on each name: the regex its find format makes, the fields
/// that regex captures in order, the pieces of its replace format and the month names it uses.
struct DatePlan<'a> {
    re: Regex,
    fields: Vec<DateTok<'a>>,
    replace: Vec<DateTok<'a>>,
    short: [String; 12],
    long: [String; 12],
}

impl<'a> DatePlan<'a> {
    /// `None` when a format is empty: the rule then changes nothing.
    fn new(find_fmt: &'a str, replace_fmt: &'a str, short: [String; 12], long: [String; 12]) -> Result<Option<Self>, String> {
        if find_fmt.trim().is_empty() || replace_fmt.trim().is_empty() {
            return Ok(None);
        }
        let find = date_tokens(find_fmt);
        let fields: Vec<DateTok> = find.iter().copied().filter(|t| !matches!(t, DateTok::Lit(_))).collect();
        let pat: String = find
            .iter()
            .map(|t| match t {
                DateTok::Lit(s) => regex::escape(s),
                DateTok::Year4 => r"(\d{4})".into(),
                DateTok::Year2 => r"(\d{2})".into(),
                DateTok::MonthLong => names_group(&long),
                DateTok::MonthShort => names_group(&short),
                DateTok::Month | DateTok::Day | DateTok::Hour | DateTok::Minute | DateTok::Second => r"(\d{1,2})".into(),
            })
            .collect();
        let re = Regex::new(&pat).map_err(|e| e.to_string())?;
        Ok(Some(Self { re, fields, replace: date_tokens(replace_fmt), short, long }))
    }
}

fn reformat_date_in_str(input: &str, plan: &DatePlan, whole_words: bool, adjust: Option<(i64, TimeUnit)>) -> String {
    let DatePlan { re, fields, replace, short, long } = plan;

    let mut out = String::with_capacity(input.len());
    let mut last = 0;
    for caps in re.captures_iter(input) {
        let m = caps.get(0).expect("group 0 always matches");
        if whole_words {
            let prev_alnum = input[..m.start()].chars().next_back().is_some_and(|c| c.is_alphanumeric());
            let next_alnum = input[m.end()..].chars().next().is_some_and(|c| c.is_alphanumeric());
            if prev_alnum || next_alnum {
                continue;
            }
        }
        let Some(mut dt) = read_date(&caps, fields, short, long) else { continue };
        out.push_str(&input[last..m.start()]);
        if let Some((amount, unit)) = adjust {
            dt = match unit {
                TimeUnit::Seconds => dt + ChronoDuration::seconds(amount),
                TimeUnit::Minutes => dt + ChronoDuration::minutes(amount),
                TimeUnit::Hours => dt + ChronoDuration::hours(amount),
                TimeUnit::Days => dt + ChronoDuration::days(amount),
                TimeUnit::Months => add_months(dt, amount),
                TimeUnit::Years => add_months(dt, amount * 12),
            };
        }
        for t in replace {
            match t {
                DateTok::Lit(s) => out.push_str(s),
                DateTok::Year4 => out.push_str(&format!("{:04}", dt.year())),
                DateTok::Year2 => out.push_str(&format!("{:02}", dt.year().rem_euclid(100))),
                DateTok::MonthLong => out.push_str(&long[dt.month0() as usize]),
                DateTok::MonthShort => out.push_str(&short[dt.month0() as usize]),
                DateTok::Month => out.push_str(&format!("{:02}", dt.month())),
                DateTok::Day => out.push_str(&format!("{:02}", dt.day())),
                DateTok::Hour => out.push_str(&format!("{:02}", dt.hour())),
                DateTok::Minute => out.push_str(&format!("{:02}", dt.minute())),
                DateTok::Second => out.push_str(&format!("{:02}", dt.second())),
            }
        }
        last = m.end();
    }
    out.push_str(&input[last..]);
    out
}

/// Builds the date a match names. It needs a year, a month and a day; a time field left out is
/// 0. A two-digit year is 1969 to 2068, as chrono reads `%y`.
fn read_date(caps: &regex::Captures, fields: &[DateTok], short: &[String; 12], long: &[String; 12]) -> Option<NaiveDateTime> {
    let (mut y, mut mo, mut d, mut h, mut mi, mut s) = (None, None, None, 0, 0, 0);
    for (i, field) in fields.iter().enumerate() {
        let text = caps.get(i + 1)?.as_str();
        let num = || text.parse::<u32>().ok();
        match field {
            DateTok::Year4 => y = Some(text.parse::<i32>().ok()?),
            DateTok::Year2 => {
                let v = text.parse::<i32>().ok()?;
                y = Some(if v < 69 { 2000 + v } else { 1900 + v });
            }
            DateTok::MonthLong => mo = Some(month_of(long, text)?),
            DateTok::MonthShort => mo = Some(month_of(short, text)?),
            DateTok::Month => mo = Some(num()?),
            DateTok::Day => d = Some(num()?),
            DateTok::Hour => h = num()?,
            DateTok::Minute => mi = num()?,
            DateTok::Second => s = num()?,
            DateTok::Lit(_) => {}
        }
    }
    NaiveDate::from_ymd_opt(y?, mo?, d?)?.and_hms_opt(h, mi, s)
}

fn add_months(dt: NaiveDateTime, delta_months: i64) -> NaiveDateTime {
    let total_m = dt.year() as i64 * 12 + (dt.month0() as i64) + delta_months;
    let y = total_m.div_euclid(12) as i32;
    let m = (total_m.rem_euclid(12) + 1) as u32;
    let d = dt.day().min(days_in_month(y, m));
    NaiveDate::from_ymd_opt(y, m, d)
        .unwrap_or_else(|| dt.date())
        .and_hms_opt(dt.hour(), dt.minute(), dt.second())
        .unwrap_or(dt)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (ny, nm) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    NaiveDate::from_ymd_opt(ny, nm, 1)
        .and_then(|d| d.pred_opt())
        .map(|d| d.day())
        .unwrap_or(28)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> Entry {
        Entry {
            id: name.into(),
            name: name.into(),
            is_dir: false,
            size: Some(2048),
            modified: None,
            mime: None,
            target: None,
            starred: false,
            md5: None,
            drive_origin: None,
        }
    }

    #[cfg(test)]
    fn rule(config: RuleConfig) -> Rule {
        Rule::new(config)
    }

    #[test]
    fn insert_delete_remove_and_replace_rules() {
        let items = vec![file("Relatorio_Final_2025.pdf")];
        let loc = Location::Local(PathBuf::from("C:/docs"));
        let rules = vec![
            rule(RuleConfig::Replace {
                find: "_".into(),
                replace: " ".into(),
                occurrences: Occurrences::All,
                case_sensitive: false,
                whole_words: false,
                skip_ext: true,
                wildcards: false,
            }),
            rule(RuleConfig::Insert {
                text: "[OK] ".into(),
                where_mode: InsertWhere::Prefix,
                position: 1,
                rtl: false,
                after_text: String::new(),
                before_text: String::new(),
                skip_ext: true,
            }),
            rule(RuleConfig::Delete {
                from_mode: DeleteFrom::Delimiter,
                from_pos: 1,
                from_delim: " 2025".into(),
                until_mode: DeleteUntil::UntilEnd,
                until_count: 1,
                until_delim: String::new(),
                delete_current: false,
                skip_ext: true,
                rtl: false,
                keep_delimiters: false,
            }),
        ];
        let res = evaluate_rules(&rules, &items, &[true], &loc);
        assert_eq!(res[0].0, "[OK] Relatorio Final.pdf");
    }

    #[test]
    fn rearrange_case_serialize_and_cleanup_rules() {
        let items = vec![file("artist - track_one [1080p].MP3"), file("artist - track_two (demo).MP3")];
        let loc = Location::Local(PathBuf::from("C:/music"));
        let rules = vec![
            rule(RuleConfig::CleanUp {
                strip_parens: true,
                strip_brackets: true,
                strip_braces: false,
                replace_dot: false,
                replace_comma: false,
                replace_underscore: true,
                replace_plus: false,
                replace_dash: false,
                replace_pct20: false,
                skip_versions: true,
                fix_spaces: true,
                normalize_unicode_spaces: true,
                strip_emoji: false,
                strip_accents: false,
                space_before_camel: false,
                prepare_sharepoint: false,
                skip_ext: true,
            }),
            rule(RuleConfig::Rearrange {
                split_mode: RearrangeSplit::Delimiters,
                delimiters: " - ".into(),
                new_pattern: "$2 - $1".into(),
                skip_ext: true,
                rtl: false,
            }),
            rule(RuleConfig::Case {
                mode: CaseMode::TitleCase,
                preserve_case: false,
                skip_ext: true,
                force_fragments: false,
                fragments: String::new(),
                ext_lower: true,
                ext_upper: false,
            }),
            rule(RuleConfig::Serialize {
                start: 1,
                repeat: 1,
                step: 1,
                reset_every_on: false,
                reset_every: 1,
                reset_folder_change: false,
                reset_name_change: false,
                pad_zeros: true,
                pad_length: 2,
                num_system: NumSystem::Decimal,
                where_mode: InsertWhere::Prefix,
                position: 1,
                skip_ext: true,
            }),
        ];
        // Insert " - " separator after serialize using a quick rule:
        let res = evaluate_rules(&rules, &items, &[true, true], &loc);
        assert_eq!(res[0].0, "01Track One - Artist.mp3");
        assert_eq!(res[1].0, "02Track Two - Artist.mp3");
    }

    #[test]
    fn reformat_date_reads_and_writes_custom_month_names() {
        let date = |find: &str, replace: &str, custom: bool, name: &str| {
            let rules = vec![rule(RuleConfig::ReformatDate {
                find_fmt: find.into(),
                replace_fmt: replace.into(),
                whole_words: false,
                skip_ext: true,
                custom_short_months: custom,
                short_months: "jan,fev,mar,abr,mai,jun,jul,ago,set,out,nov,dez".into(),
                custom_long_months: custom,
                long_months: "janeiro,fevereiro,março,abril,maio,junho,julho,agosto,setembro,outubro,novembro,dezembro".into(),
                adjust_on: false,
                adjust_amount: 0,
                adjust_unit: TimeUnit::Days,
            })];
            evaluate_rules(&rules, &[file(name)], &[true], &Location::Local(PathBuf::from("C:/tmp"))).remove(0).0
        };
        // Custom long and short names are read, ignoring case, and written.
        assert_eq!(date("dd mmmm yyyy", "yyyy-mm-dd", true, "Foto 15 Março 2024.jpg"), "Foto 2024-03-15.jpg");
        assert_eq!(date("yyyy-mm-dd", "dd mmm yyyy", true, "2024-02-05.txt"), "05 fev 2024.txt");
        assert_eq!(date("yyyy-mm-dd", "mmmm", true, "x 2024-06-30.txt"), "x junho.txt");
        // With the boxes off, the names stay English.
        assert_eq!(date("dd mmm yyyy", "yyyy-mm-dd", false, "15 Mar 2024.txt"), "2024-03-15.txt");
        assert_eq!(date("dd mmm yyyy", "yyyy-mm-dd", false, "15 mar 2024 a.txt"), "2024-03-15 a.txt");
        assert_eq!(date("yyyy-mm-dd", "dd mmmm yy", false, "2024-12-01.txt"), "01 December 24.txt");
        // A list without 12 names falls back to English; an impossible date is left alone.
        let short_list = |n: &str| {
            let rules = vec![rule(RuleConfig::ReformatDate {
                find_fmt: "dd mmm yyyy".into(),
                replace_fmt: "yyyy-mm-dd".into(),
                whole_words: false,
                skip_ext: true,
                custom_short_months: true,
                short_months: "a,b,c".into(),
                custom_long_months: false,
                long_months: String::new(),
                adjust_on: false,
                adjust_amount: 0,
                adjust_unit: TimeUnit::Days,
            })];
            evaluate_rules(&rules, &[file(n)], &[true], &Location::Local(PathBuf::from("C:/tmp"))).remove(0).0
        };
        assert_eq!(short_list("15 Mar 2024.txt"), "2024-03-15.txt");
        assert_eq!(date("yyyy-mm-dd", "dd.mm.yyyy", false, "2024-02-30.txt"), "2024-02-30.txt");
        assert_eq!(date("yyyymmdd hh-nn", "dd/mm/yy hh:nn", false, "20240105 07-30.txt"), "05/01/24 07:30.txt");
    }

    #[test]
    fn reformat_date_pascal_script_translit_and_mapping() {
        let items = vec![file("Ação_2026-11-26.txt")];
        let loc = Location::Local(PathBuf::from("C:/tmp"));
        let rules = vec![
            rule(RuleConfig::Translit {
                alphabet: DEFAULT_LATIN_TRANSLIT.into(),
                direction: TranslitDir::Forward,
                auto_case: true,
                skip_ext: true,
            }),
            rule(RuleConfig::ReformatDate {
                find_fmt: "yyyy-mm-dd".into(),
                replace_fmt: "dd-mm-yyyy".into(),
                whole_words: true,
                skip_ext: true,
                custom_short_months: false,
                short_months: String::new(),
                custom_long_months: false,
                long_months: String::new(),
                adjust_on: true,
                adjust_amount: 1,
                adjust_unit: TimeUnit::Days,
            }),
            rule(RuleConfig::PascalScript {
                code: "begin\n  FileName := WideUpperCase(WideExtractBaseName(FileName)) + WideExtractFileExt(FileName);\nend.".into(),
            }),
        ];
        let res = evaluate_rules(&rules, &items, &[true], &loc);
        assert_eq!(res[0].0, "ACAO_27-11-2026.txt");
        assert!(res[0].1.is_none());
    }

    #[test]
    fn strip_extension_padding_regex_user_input_and_mapping() {
        let items = vec![file("  7 - Demo File.TMP  ")];
        let loc = Location::Local(PathBuf::from("C:/tmp"));
        let rules = vec![
            rule(RuleConfig::Strip {
                english: false,
                english_chars: String::new(),
                digits: false,
                digits_chars: String::new(),
                symbols: false,
                symbols_chars: String::new(),
                brackets: false,
                brackets_chars: String::new(),
                custom: true,
                custom_chars: " ".into(),
                unicode_range: false,
                unicode_range_str: String::new(),
                where_mode: StripWhere::Right,
                invert: false,
                case_sensitive: false,
                skip_ext: false,
            }),
            rule(RuleConfig::Extension {
                set_new: true,
                new_ext: "log".into(),
                append_original: false,
                detect_binary: false,
                remove_duplicates: false,
                dup_case_sensitive: false,
            }),
            rule(RuleConfig::Regex {
                pattern: r"^\s*(\d+)\s*-\s*(.+)$".into(),
                replace: "$1_$2".into(),
                case_sensitive: false,
                skip_ext: true,
            }),
            rule(RuleConfig::Padding {
                pad_numbers: true,
                pad_numbers_len: 3,
                strip_zeros: false,
                pad_text: false,
                pad_text_len: 0,
                pad_text_chars: String::new(),
                pad_text_side: Side::Left,
                skip_ext: true,
            }),
            rule(RuleConfig::Mapping {
                entries: vec![("Demo File".into(), "Sample".into())],
                allow_reuse: false,
                partial_match: true,
                inverse: false,
                case_sensitive: true,
                skip_ext: true,
            }),
        ];
        let res = evaluate_rules(&rules, &items, &[true], &loc);
        assert_eq!(res[0].0, "007_Sample.log");
        assert!(res[0].1.is_none());

        let ui_rules = vec![rule(RuleConfig::UserInput {
            lines: "custom_name".into(),
            mode: UserInputMode::Replace,
            skip_ext: true,
            trim_lines: true,
            skip_empty: true,
        })];
        let ui_res = evaluate_rules(&ui_rules, &[file("orig.txt")], &[true], &loc);
        assert_eq!(ui_res[0].0, "custom_name.txt");
    }

    #[test]
    fn accents_never_break_the_delete_rule_or_scripts() {
        // Delete from position 3, counted from the right, up to "-": the slice is by characters.
        let cfg = RuleConfig::Delete {
            from_mode: DeleteFrom::Position,
            from_pos: 3,
            from_delim: String::new(),
            until_mode: DeleteUntil::Delimiter,
            until_count: 1,
            until_delim: "-".into(),
            delete_current: false,
            skip_ext: true,
            rtl: true,
            keep_delimiters: false,
        };
        let loc = Location::Local(PathBuf::from("C:/tmp"));
        // It used to slice inside "ã" and panic.
        let out = evaluate_rules(&[rule(cfg.clone())], &[file("ããã-xy.txt")], &[true], &loc);
        assert_eq!(out[0].0, "ããã-xy.txt");
        // From the last character, back to "-": "xy" goes.
        let mut cfg = cfg;
        if let RuleConfig::Delete { from_pos, .. } = &mut cfg {
            *from_pos = 1;
        }
        let out = evaluate_rules(&[rule(cfg)], &[file("ããã-xy.txt")], &[true], &loc);
        assert_eq!(out[0].0, "ããã-.txt");
        // A (* comment *) with accents compiles.
        assert!(rules_script::compile("(* ação *) begin FileName := 'x' + FileName; end.").is_ok());
    }

    #[test]
    fn saving_the_editor_finds_the_rule_again_after_the_list_changed() {
        let mut rs = RulesState::new();
        for text in ["a", "b", "c"] {
            let mut cfg = RuleConfig::default_for(RuleKind::Insert);
            if let RuleConfig::Insert { text: t, .. } = &mut cfg {
                *t = text.into();
            }
            rs.rules.push(Rule::new(cfg));
        }
        rs.open_edit(1);
        // Meanwhile, in the batch window, the first rule goes.
        rs.selected = Some(0);
        rs.remove_selected();
        rs.editor.edit_a = TextEdit::new("B");
        rs.commit_editor();
        let texts: Vec<String> = rs.rules.iter().map(|r| match &r.config {
            RuleConfig::Insert { text, .. } => text.clone(),
            _ => String::new(),
        }).collect();
        assert_eq!(texts, ["B", "c"]);
    }

    #[test]
    fn a_short_avi_file_reads_no_tag_and_does_not_panic() {
        let dir = std::env::temp_dir().join(format!("oe-rules-avi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // "avih" 42 bytes from the end: past the old check, short of the 48 bytes read.
        let mut data = b"RIFF\0\0\0\0AVI ".to_vec();
        data.resize(64, 0);
        data.extend_from_slice(b"avih");
        data.resize(64 + 42, 0);
        std::fs::write(dir.join("v.avi"), &data).unwrap();
        let entry = Entry { name: "v.avi".into(), ..file("v.avi") };
        let got = rules_meta::resolve_tag("AVI_Width", &entry, &Location::Local(dir.clone()));
        assert_eq!(got.as_deref(), Some(""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rule_stack_context_actions_and_export() {
        let mut rs = RulesState::new();
        rs.rules.push(rule(RuleConfig::Extension {
            set_new: true,
            new_ext: "mp4".into(),
            append_original: false,
            detect_binary: false,
            remove_duplicates: false,
            dup_case_sensitive: false,
        }));
        rs.selected = Some(0);
        rs.duplicate_selected();
        assert_eq!(rs.rules.len(), 2);

        rs.open_add_at(Some(1));
        rs.editor.edit_a = TextEdit::new("pre_");
        rs.commit_editor();
        assert_eq!(rs.rules.len(), 3);
        assert_eq!(rs.rules[1].config.kind(), RuleKind::Insert);

        rs.rules[1].comment = "prefix step".into();
        rs.set_all_enabled(false);
        assert!(rs.rules.iter().all(|r| !r.enabled));
        rs.set_all_enabled(true);
        assert!(rs.rules.iter().all(|r| r.enabled));

        let exported = rs.export_rules_text();
        assert!(exported.contains("// prefix step"));

        rs.select_all();
        assert!(rs.all_selected);
        rs.remove_selected();
        assert!(rs.rules.is_empty());
        assert!(!rs.all_selected);
    }
}
