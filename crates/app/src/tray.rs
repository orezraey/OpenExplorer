//! System tray icon and menu for Open Explorer (Windows and Linux).

use openexplorer_i18n::t;

use crate::State;

const TRAY_SIZE: u32 = 32;
const TRAY_RGBA: &[u8] = include_bytes!("../assets/tray-32.rgba");

/// Builds the tray context menu in the active UI language.
pub fn menu() -> Vec<rune::TrayItem> {
    vec![
        rune::TrayItem::action("open", t("Abrir Open Explorer", "Open Open Explorer")),
        rune::TrayItem::Separator,
        rune::TrayItem::action("quit", t("Sair", "Quit")),
    ]
}

/// Starts the system tray icon on Windows and Linux when not running headless. Not on macOS,
/// where a menu bar icon is the exception and Cmd+Q already quits.
pub fn start(main_win: rune::AppHandle<State>) -> Option<rune::Tray> {
    if rune::is_headless() || cfg!(target_os = "macos") {
        return None;
    }
    let spec = rune::TraySpec::new(
        "openexplorer",
        "Open Explorer",
        rune::TrayIcon::rgba(TRAY_SIZE, TRAY_SIZE, TRAY_RGBA.to_vec()),
    )
    .tooltip("Open Explorer")
    .menu(menu());

    let handle = main_win.clone();
    match rune::Tray::new(spec, move |event| match event {
        rune::TrayEvent::Activate { token } => match token {
            Some(tok) => handle.activate(tok),
            None => handle.open(),
        },
        rune::TrayEvent::Menu(id) if id == "open" => handle.open(),
        rune::TrayEvent::Menu(id) if id == "quit" => {
            let win = handle.clone();
            handle.update(move |s, tx| {
                if crate::actions::close_requested(s) == rune::CloseResponse::Close {
                    crate::window_place::remember(s);
                    tx.quit();
                } else {
                    win.open();
                }
            });
        }
        _ => {}
    }) {
        Ok(tray) => Some(tray),
        Err(rune::TrayError::Unsupported) => None,
        Err(e) => {
            openexplorer_gdrive::log(&format!("tray: {e}"));
            None
        }
    }
}

/// Refreshes the tray menu labels (for example after switching the interface language).
pub fn refresh_menu(s: &State) {
    if let Some(tray) = &s.tray {
        tray.set_menu(menu());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_rgba_is_32x32_and_visible() {
        assert_eq!(TRAY_RGBA.len(), (TRAY_SIZE * TRAY_SIZE * 4) as usize);
        let opaque = TRAY_RGBA.as_chunks::<4>().0.iter().filter(|px| px[3] > 128).count();
        assert!(opaque > 100, "expected visible tray icon pixels, got {opaque}");
    }

    #[test]
    fn tray_menu_has_open_and_quit() {
        let items = menu();
        assert!(items.iter().any(|i| matches!(i, rune::TrayItem::Action { id, .. } if id == "open")));
        assert!(items.iter().any(|i| matches!(i, rune::TrayItem::Action { id, .. } if id == "quit")));
    }
}
