//! The interface's language: Portuguese or English, chosen once at start and again whenever
//! the user changes it in Options. Every crate that writes text for people (messages, labels,
//! errors) picks its words through here, with both versions side by side at the place they're
//! used:
//!
//! ```
//! use openexplorer_i18n::{t, tr};
//! let n = 3;
//! let title = t("Opções", "Options");
//! let done = tr!("Copiado: {n} itens.", "Copied: {n} items.");
//! ```
//!
//! Portuguese is what a process starts with, so tests read the Portuguese words.

use std::sync::atomic::{AtomicBool, Ordering};

static ENGLISH: AtomicBool = AtomicBool::new(false);

/// Shows the interface in English (`true`) or Portuguese from now on.
pub fn set_english(on: bool) {
    ENGLISH.store(on, Ordering::Relaxed);
}

/// Whether the interface is in English.
pub fn english() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

/// The Portuguese or the English words, as the interface is now.
pub fn t(pt: &'static str, en: &'static str) -> &'static str {
    if english() { en } else { pt }
}

/// [`t`] for words with values in them: both strings are `format!` strings over the same
/// arguments, and the result is a `String`.
#[macro_export]
macro_rules! tr {
    ($pt:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::english() { format!($en $(, $arg)*) } else { format!($pt $(, $arg)*) }
    };
}

/// Whether the system's own interface is in a language other than Portuguese, for the
/// Automatic choice: English then, Portuguese for Portuguese.
pub fn system_english() -> bool {
    !system_portuguese()
}

#[cfg(windows)]
fn system_portuguese() -> bool {
    // SAFETY: no arguments; returns the user's interface language id.
    let lang = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
    // The primary language is the low ten bits; LANG_PORTUGUESE is 0x16.
    lang & 0x3ff == 0x16
}

#[cfg(not(windows))]
fn system_portuguese() -> bool {
    ["LC_ALL", "LC_MESSAGES", "LANG"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty())).is_some_and(|v| v.to_ascii_lowercase().starts_with("pt"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_follow_the_language() {
        let n = 2;
        assert_eq!((t("Sim", "Yes"), tr!("{n} itens", "{n} items")), ("Sim", "2 itens".to_string()));
        set_english(true);
        assert_eq!((t("Sim", "Yes"), tr!("{n} itens", "{n} items")), ("Yes", "2 items".to_string()));
        set_english(false);
    }
}
