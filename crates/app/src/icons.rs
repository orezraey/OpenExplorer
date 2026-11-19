//! Open Explorer's icons: SVG bodies on a 16-point grid. File-type and place icons carry their
//! own colours; the toolbar ones draw in `currentColor`, so they follow the text colour.

use rune::Icon;

macro_rules! icon {
    ($id:ident, $name:literal, $body:expr) => {
        pub static $id: Icon = Icon { name: concat!("oe-", $name), view: 16.0, body: $body };
    };
}

macro_rules! icon32 {
    ($id:ident, $name:literal, $body:expr) => {
        pub static $id: Icon = Icon { name: concat!("oe-", $name), view: 32.0, body: $body };
    };
}

// The copy arrows between the panes, in `currentColor`.
icon!(COPY_RIGHT, "copy-right", r##"<path d="M4.5 1.5L11.5 8L4.5 14.5Z" fill="currentColor"/>"##);
icon!(COPY_LEFT, "copy-left", r##"<path d="M11.5 1.5L4.5 8L11.5 14.5Z" fill="currentColor"/>"##);

// The search field's magnifier, in `currentColor`.
icon!(SEARCH, "search", r##"<circle cx="7" cy="7" r="4.5" fill="none" stroke="currentColor" stroke-width="1.4"/><path d="M10.4 10.4L14 14" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>"##);

// Opções, in the toolbar and sidebar: gear, transfers, logs and theme icons, in `currentColor`.
icon!(SETTINGS, "settings", r##"<circle cx="8" cy="8" r="2.2" fill="none" stroke="currentColor" stroke-width="1.3"/><path d="M8 1.5V3.2M8 12.8V14.5M1.5 8H3.2M12.8 8H14.5M3.4 3.4L4.6 4.6M11.4 11.4L12.6 12.6M3.4 12.6L4.6 11.4M11.4 4.6L12.6 3.4" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/><circle cx="8" cy="8" r="4.6" fill="none" stroke="currentColor" stroke-width="1.3"/>"##);
icon!(OPT_TRANSFERS, "opt-transfers", r##"<path d="M5.2 13V3M2.8 5.4L5.2 3L7.6 5.4M10.8 3V13M8.4 10.6L10.8 13L13.2 10.6" fill="none" stroke="currentColor" stroke-width="1.35" stroke-linecap="round" stroke-linejoin="round"/>"##);
icon!(OPT_CACHE, "opt-cache", r##"<ellipse cx="8" cy="3.9" rx="5.3" ry="1.9" fill="none" stroke="currentColor" stroke-width="1.3"/><path d="M2.7 3.9V12.1C2.7 13.15 5.07 14 8 14C10.93 14 13.3 13.15 13.3 12.1V3.9M2.7 8C2.7 9.05 5.07 9.9 8 9.9C10.93 9.9 13.3 9.05 13.3 8" fill="none" stroke="currentColor" stroke-width="1.3"/>"##);
icon!(OPT_LOGS, "opt-logs", r##"<rect x="2.5" y="1.8" width="11" height="12.4" rx="1.6" fill="none" stroke="currentColor" stroke-width="1.3"/><path d="M5.2 5.2H10.8M5.2 8H10.8M5.2 10.8H8.4" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/>"##);
icon!(OPT_EXPERIMENTAL, "opt-experimental", r##"<path d="M6 2.2H10M6.8 2.2V6L3.1 12.2C2.7 12.9 3.2 13.8 4 13.8H12C12.8 13.8 13.3 12.9 12.9 12.2L9.2 6V2.2" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/><path d="M4.6 10.2H11.4" stroke="currentColor" stroke-width="1.1" stroke-linecap="round"/>"##);
icon!(THEME_AUTO, "theme-auto", r##"<rect x="1.8" y="2.8" width="12.4" height="8.4" rx="1.4" fill="none" stroke="currentColor" stroke-width="1.3"/><path d="M8 2.8V11.2" stroke="currentColor" stroke-width="1.1"/><path d="M5.2 13.4H10.8M8 11.2V13.4" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/>"##);
icon!(THEME_LIGHT, "theme-light", r##"<circle cx="8" cy="8" r="3" fill="none" stroke="currentColor" stroke-width="1.3"/><path d="M8 1.8V3.2M8 12.8V14.2M1.8 8H3.2M12.8 8H14.2M3.6 3.6L4.6 4.6M11.4 11.4L12.4 12.4M3.6 12.4L4.6 11.4M11.4 4.6L12.4 3.6" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/>"##);
icon!(THEME_DARK, "theme-dark", r##"<path d="M12.8 9.9A5.4 5.4 0 0 1 6.1 3.2A5.5 5.5 0 1 0 12.8 9.9Z" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>"##);

// An upload, in the transfer queue: an arrow up out of a tray.
icon!(UPLOAD, "upload", r##"<path d="M8 10.5V2.5M4.75 5.5L8 2.25L11.25 5.5M2.5 10V13.5H13.5V10" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/>"##);

// A download, in the transfer queue: an arrow down into a tray.
icon!(DOWNLOAD, "download", r##"<path d="M8 2.5V10.5M4.75 7.5L8 10.75L11.25 7.5M2.5 10V13.5H13.5V10" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/>"##);

// Places and folders.
icon!(FOLDER, "folder", r##"<path d="M1 3.5C1 2.95 1.45 2.5 2 2.5H6L7.5 4H14C14.55 4 15 4.45 15 5V12.5C15 13.05 14.55 13.5 14 13.5H2C1.45 13.5 1 13.05 1 12.5Z" fill="#E8A93A"/><path d="M1 5.5H15V12.5C15 13.05 14.55 13.5 14 13.5H2C1.45 13.5 1 13.05 1 12.5Z" fill="#F6C451"/>"##);
icon!(FOLDER_LINK, "folder-link", r##"<path d="M1 3.5C1 2.95 1.45 2.5 2 2.5H6L7.5 4H14C14.55 4 15 4.45 15 5V12.5C15 13.05 14.55 13.5 14 13.5H2C1.45 13.5 1 13.05 1 12.5Z" fill="#E8A93A"/><path d="M1 5.5H15V12.5C15 13.05 14.55 13.5 14 13.5H2C1.45 13.5 1 13.05 1 12.5Z" fill="#F6C451"/><rect x="2" y="8" width="5.5" height="5" rx="1" fill="#FFFFFF"/><path d="M3.3 11.8L5.8 9.3M4.3 9.2H5.9V10.8" stroke="#2F6FDE" stroke-width="1" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"##);
icon!(DESKTOP, "desktop", r##"<rect x="1.5" y="2.5" width="13" height="8.5" rx="1" fill="#4C8BF5"/><rect x="6.5" y="11" width="3" height="1.5" fill="#8A94A6"/><rect x="4.5" y="12.5" width="7" height="1.2" rx="0.6" fill="#8A94A6"/>"##);
icon!(COMPUTER, "computer", r##"<rect x="1.5" y="2.5" width="13" height="8.5" rx="1" fill="#3B6FD8"/><rect x="2.5" y="3.5" width="11" height="6.5" fill="#8EC5FF"/><rect x="6.5" y="11" width="3" height="1.5" fill="#8A94A6"/><rect x="4.5" y="12.5" width="7" height="1.2" rx="0.6" fill="#8A94A6"/>"##);
icon!(NETWORK, "network", r##"<rect x="5.5" y="1.5" width="5" height="4" rx="0.8" fill="#3B6FD8"/><rect x="1" y="10.5" width="5" height="4" rx="0.8" fill="#3B6FD8"/><rect x="10" y="10.5" width="5" height="4" rx="0.8" fill="#3B6FD8"/><path d="M8 5.5V8M3.5 10.5V8H12.5V10.5" stroke="#8A94A6" stroke-width="1.2" fill="none" stroke-linejoin="round"/>"##);
icon!(LINUX, "linux", r##"<path d="M8 1.5C5.9 1.5 4.6 3.2 4.6 5.6C4.6 7 4 8 3.2 9.3C2.5 10.5 2.6 12 3.6 12.9C4.6 13.8 6.2 14.5 8 14.5C9.8 14.5 11.4 13.8 12.4 12.9C13.4 12 13.5 10.5 12.8 9.3C12 8 11.4 7 11.4 5.6C11.4 3.2 10.1 1.5 8 1.5Z" fill="#2B2F36"/><path d="M8 6.8C6.6 6.8 5.6 8.4 5.6 10.4C5.6 12.2 6.6 13.6 8 13.6C9.4 13.6 10.4 12.2 10.4 10.4C10.4 8.4 9.4 6.8 8 6.8Z" fill="#FFFFFF"/><circle cx="6.9" cy="4.6" r="0.7" fill="#FFFFFF"/><circle cx="9.1" cy="4.6" r="0.7" fill="#FFFFFF"/><path d="M6.9 5.9L8 6.7L9.1 5.9Z" fill="#F2B233"/><path d="M3.2 13.2L5.4 12.6L5.8 14.4L3.6 14.6ZM12.8 13.2L10.6 12.6L10.2 14.4L12.4 14.6Z" fill="#F2B233"/>"##);
icon!(DISK, "disk", r##"<rect x="1.5" y="5" width="13" height="6.5" rx="1.5" fill="#8A94A6"/><rect x="1.5" y="9" width="13" height="2.5" rx="1" fill="#6B7487"/><circle cx="12.3" cy="10.25" r="0.75" fill="#7CE38B"/>"##);
icon!(CLOUD, "cloud", r##"<path d="M4.5 13C2.6 13 1 11.6 1 9.8C1 8.2 2.2 6.9 3.8 6.7C4.3 4.6 6.1 3 8.3 3C10.6 3 12.5 4.7 12.8 6.9C14.1 7.2 15 8.3 15 9.7C15 11.5 13.6 13 11.8 13Z" fill="#4C8BF5"/>"##);
icon!(SHARED, "shared", r##"<path d="M4.5 13C2.6 13 1 11.6 1 9.8C1 8.2 2.2 6.9 3.8 6.7C4.3 4.6 6.1 3 8.3 3C10.6 3 12.5 4.7 12.8 6.9C14.1 7.2 15 8.3 15 9.7C15 11.5 13.6 13 11.8 13Z" fill="#3BA676"/><circle cx="6.3" cy="8" r="1.3" fill="#FFFFFF"/><circle cx="9.7" cy="8" r="1.3" fill="#FFFFFF"/><path d="M4 11.6C4.3 10.4 5.2 9.8 6.3 9.8C7.4 9.8 8.3 10.4 8.6 11.6ZM7.4 11.6C7.7 10.4 8.6 9.8 9.7 9.8C10.8 9.8 11.7 10.4 12 11.6Z" fill="#FFFFFF"/>"##);
// Google Drive's views in the tree.
icon!(SHARED_WITH_ME, "shared-with-me", r##"<circle cx="8" cy="5" r="2.8" fill="#5B8DEF"/><path d="M2.5 14C2.8 10.9 5 9.2 8 9.2C11 9.2 13.2 10.9 13.5 14Z" fill="#5B8DEF"/>"##);
icon!(STAR, "star", r##"<path d="M8 1.5L9.9 5.6L14.3 6.1L11 9.1L11.9 13.5L8 11.3L4.1 13.5L5 9.1L1.7 6.1L6.1 5.6Z" fill="#F4B400" stroke="#D89C00" stroke-width="0.6" stroke-linejoin="round"/>"##);
icon!(RECENT, "recent", r##"<circle cx="8" cy="8" r="6.5" fill="#7D8796"/><path d="M8 4.2V8L10.6 9.6" stroke="#FFFFFF" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"##);
icon!(TRASH, "trash", r##"<path d="M3.2 4.5H12.8L12 14C11.95 14.55 11.5 15 10.95 15H5.05C4.5 15 4.05 14.55 4 14Z" fill="#8A94A6"/><path d="M2 3.2H14V4.6H2ZM6 1.5H10V3.2H6Z" fill="#6B7487"/><path d="M6.5 6.8V12.6M9.5 6.8V12.6" stroke="#FFFFFF" stroke-width="1" stroke-linecap="round"/>"##);
icon!(LAB, "lab", r##"<path d="M6 1.5H10M6.8 1.5V6L2.8 12.6C2.4 13.3 2.9 14.2 3.7 14.2H12.3C13.1 14.2 13.6 13.3 13.2 12.6L9.2 6V1.5" fill="#B9A4F2" stroke="#7B5CD6" stroke-linejoin="round"/>"##);

// Files: a page outline with a coloured band saying what it is.
macro_rules! page {
    ($id:ident, $name:literal, $color:literal, $mark:literal) => {
        icon!(
            $id,
            $name,
            concat!(
                r##"<path d="M3.5 1H9.8L13 4.2V14.2C13 14.65 12.65 15 12.2 15H3.8C3.35 15 3 14.65 3 14.2V1.5C3 1.25 3.25 1 3.5 1Z" fill="#FFFFFF" stroke="#9AA3B2"/><path d="M9.8 1V4.2H13" fill="#E9ECF1" stroke="#9AA3B2" stroke-linejoin="round"/>"##,
                r##"<rect x="3" y="8" width="10" height="5" rx="0.8" fill=""##,
                $color,
                r##""/>"##,
                $mark
            )
        );
    };
}
page!(FILE, "file", "#B4BCC9", "");
page!(IMAGE, "image", "#9A5BD9", r##"<circle cx="6" cy="10" r="0.9" fill="#FFFFFF"/><path d="M4.5 12.4L7 10.6L8.5 11.6L10.5 9.8L11.8 12.4Z" fill="#FFFFFF"/>"##);
page!(VIDEO, "video", "#E0564B", r##"<path d="M7 9.2V11.8L9.4 10.5Z" fill="#FFFFFF"/>"##);
page!(AUDIO, "audio", "#EE8A2E", r##"<path d="M7.2 11.6V9.3L9.6 8.8V11.1" stroke="#FFFFFF" stroke-width="0.9" fill="none"/><circle cx="6.7" cy="11.7" r="0.7" fill="#FFFFFF"/><circle cx="9.1" cy="11.2" r="0.7" fill="#FFFFFF"/>"##);
page!(PDF, "pdf", "#D93A2E", r##"<path d="M5 12.2V9.2H6.3C6.9 9.2 7.2 9.6 7.2 10.1C7.2 10.6 6.9 11 6.3 11H5M8.2 12.2V9.2H9.1C9.9 9.2 10.4 9.8 10.4 10.7C10.4 11.6 9.9 12.2 9.1 12.2Z" stroke="#FFFFFF" stroke-width="0.8" fill="none"/>"##);
page!(DOC, "doc", "#2F6FDE", r##"<path d="M5 9.5H11M5 11.5H9.5" stroke="#FFFFFF" stroke-width="0.9"/>"##);
page!(SHEET, "sheet", "#1E8E50", r##"<path d="M5 9.2H11V12.2H5ZM8 9.2V12.2M5 10.7H11" stroke="#FFFFFF" stroke-width="0.7" fill="none"/>"##);
page!(SLIDES, "slides", "#E9A21B", r##"<rect x="5.2" y="9.2" width="5.6" height="3" rx="0.3" stroke="#FFFFFF" stroke-width="0.8" fill="none"/>"##);
page!(ARCHIVE, "archive", "#8D6E4A", r##"<path d="M7.6 8V13M8.4 8.6V9.4M7.6 10H8.4M7.6 11.2H8.4" stroke="#FFFFFF" stroke-width="0.8"/>"##);
page!(CODE, "code", "#4A5568", r##"<path d="M6.6 9.3L5.2 10.5L6.6 11.7M9.4 9.3L10.8 10.5L9.4 11.7" stroke="#FFFFFF" stroke-width="0.8" fill="none" stroke-linecap="round" stroke-linejoin="round"/>"##);
page!(TEXT, "text", "#7D8796", r##"<path d="M5 9.5H11M5 11.5H10" stroke="#FFFFFF" stroke-width="0.8"/>"##);
page!(APP, "app", "#3E5BA9", r##"<rect x="5.6" y="9" width="4.8" height="3.4" rx="0.5" stroke="#FFFFFF" stroke-width="0.8" fill="none"/><path d="M5.6 10H10.4" stroke="#FFFFFF" stroke-width="0.8"/>"##);
page!(TORRENT, "torrent", "#3E8E7E", r##"<path d="M8 8.8V11.6M6.7 10.5L8 11.8L9.3 10.5" stroke="#FFFFFF" stroke-width="0.9" fill="none" stroke-linecap="round" stroke-linejoin="round"/>"##);

// Toolbar, in the text colour.
icon!(BACK, "back", r#"<path d="M13 8H3.5M7.5 4L3.5 8L7.5 12" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(FORWARD, "forward", r#"<path d="M3 8H12.5M8.5 4L12.5 8L8.5 12" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(UP, "up", r#"<path d="M8 13V3.5M4 7.5L8 3.5L12 7.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(REFRESH, "refresh", r#"<path d="M13 8A5 5 0 1 1 11.5 4.4" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" fill="none"/><path d="M11.8 1.8V4.8H8.8" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(CHEVRON_DOWN, "chevron-down", r#"<path d="M4 6L8 10L12 6" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(CHEVRON, "chevron", r#"<path d="M6 4L10 8L6 12" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(ADD_ACCOUNT, "add-account", r##"<path d="M3.5 12.5C2 12.5 0.8 11.4 0.8 10C0.8 8.8 1.7 7.8 2.9 7.6C3.3 6 4.7 4.8 6.4 4.8C8.2 4.8 9.6 6.1 9.9 7.8C10.9 8 11.6 8.9 11.6 9.9C11.6 11.3 10.5 12.5 9.1 12.5Z" fill="#4C8BF5"/><circle cx="12.3" cy="4.2" r="3.2" fill="#1E8E50"/><path d="M12.3 2.6V5.8M10.7 4.2H13.9" stroke="#FFFFFF" stroke-width="1.1" stroke-linecap="round"/>"##);
icon!(SIGN_OUT, "sign-out", r#"<path d="M6.5 2.5H3.5C2.95 2.5 2.5 2.95 2.5 3.5V12.5C2.5 13.05 2.95 13.5 3.5 13.5H6.5M10 5L13 8L10 11M13 8H6" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(EXTERNAL_LINK, "external-link", r#"<path d="M9.5 2.5H13.5V6.5M13.2 2.8L7.5 8.5M6.5 4H3.5C2.95 4 2.5 4.45 2.5 5V12.5C2.5 13.05 2.95 13.5 3.5 13.5H11C11.55 13.5 12 13.05 12 12.5V9.5" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);

// Category tabs in the unified "Adicionar conta" picker, in `currentColor`.
icon!(CAT_ALL, "cat-all", r#"<rect x="2" y="2" width="5" height="5" rx="1.2" stroke="currentColor" stroke-width="1.3" fill="none"/><rect x="9" y="2" width="5" height="5" rx="1.2" stroke="currentColor" stroke-width="1.3" fill="none"/><rect x="2" y="9" width="5" height="5" rx="1.2" stroke="currentColor" stroke-width="1.3" fill="none"/><rect x="9" y="9" width="5" height="5" rx="1.2" stroke="currentColor" stroke-width="1.3" fill="none"/>"#);
icon!(CAT_CLOUD, "cat-cloud", r#"<path d="M4.5 12.8C2.7 12.8 1.3 11.5 1.3 9.8C1.3 8.3 2.4 7 3.9 6.8C4.4 4.8 6.1 3.3 8.3 3.3C10.5 3.3 12.4 4.9 12.7 7C13.9 7.3 14.7 8.4 14.7 9.7C14.7 11.4 13.4 12.8 11.7 12.8Z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round" fill="none"/>"#);
icon!(CAT_NAS, "cat-nas", r#"<rect x="2" y="2" width="5.2" height="12" rx="1" stroke="currentColor" stroke-width="1.3" fill="none"/><rect x="8.8" y="2" width="5.2" height="12" rx="1" stroke="currentColor" stroke-width="1.3" fill="none"/><path d="M3.6 4.8H5.6M3.6 7.2H5.6M10.4 4.8H12.4M10.4 7.2H12.4" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/><circle cx="4.6" cy="11.4" r="0.8" fill="currentColor"/><circle cx="11.4" cy="11.4" r="0.8" fill="currentColor"/>"#);
icon!(CAT_PROTO, "cat-proto", r#"<rect x="2" y="2.5" width="12" height="4.5" rx="1" stroke="currentColor" stroke-width="1.3" fill="none"/><rect x="2" y="9" width="12" height="4.5" rx="1" stroke="currentColor" stroke-width="1.3" fill="none"/><circle cx="4.5" cy="4.75" r="0.8" fill="currentColor"/><circle cx="4.5" cy="11.25" r="0.8" fill="currentColor"/><path d="M7.2 4.75H11.5M7.2 11.25H11.5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/>"#);

// Providers in the unified "Adicionar conta" picker and tree (32-point canvas).
icon32!(
    PROVIDER_GDRIVE,
    "provider-gdrive",
    r##"<path d="M11.6 4.5L16 12.1L7.2 12.1Z" fill="#00832D"/><path d="M7.2 12.1L16 12.1L11.6 19.7L2.8 19.7Z" fill="#00AC47"/><path d="M2.8 19.7L11.6 19.7L7.2 27.3Z" fill="#0066DA"/><path d="M11.6 19.7L20.4 19.7L24.8 27.3L7.2 27.3Z" fill="#2684FC"/><path d="M20.4 19.7L29.2 19.7L24.8 27.3Z" fill="#EA4335"/><path d="M11.6 4.5L20.4 4.5L29.2 19.7L20.4 19.7Z" fill="#FFBA00"/>"##
);
icon32!(
    PROVIDER_DROPBOX,
    "provider-dropbox",
    r##"<path d="M10 4.5L16 8.5L10 12.5L4 8.5Z M22 4.5L28 8.5L22 12.5L16 8.5Z M10 12.5L16 16.5L10 20.5L4 16.5Z M22 12.5L28 16.5L22 20.5L16 16.5Z M16 18.2L22 22.2L16 26.2L10 22.2Z" fill="#0061FF"/>"##
);
icon32!(
    PROVIDER_MEGA,
    "provider-mega",
    r##"<circle cx="16" cy="16" r="14" fill="#D9272E"/><path d="M9.2 21.5V10.8L16 17.6L22.8 10.8V21.5" fill="none" stroke="#FFFFFF" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round"/>"##
);
icon32!(
    PROVIDER_B2,
    "provider-b2",
    r##"<rect x="2" y="2" width="28" height="28" rx="7" fill="#E21E29"/><path d="M16.6 5.5c.5 3.6-1.9 5.6-3.6 7.6-1.4 1.6-2.6 3.3-2.6 5.7a5.6 5.6 0 0 0 11.2.2c0-2-.8-3.4-1.8-4.7.2 1.6-.4 2.9-1.6 3.4.6-3.3-.8-6.4-1.6-12.2Z" fill="#FFFFFF"/><path d="M13.3 22.6h5.4" stroke="#FFFFFF" stroke-width="1.6" stroke-linecap="round"/>"##
);
icon32!(
    PROVIDER_NEXTCLOUD,
    "provider-nextcloud",
    r##"<rect x="2" y="2" width="28" height="28" rx="7" fill="#0082C9"/><circle cx="8.2" cy="16" r="2.9" fill="none" stroke="#FFFFFF" stroke-width="2.2"/><circle cx="23.8" cy="16" r="2.9" fill="none" stroke="#FFFFFF" stroke-width="2.2"/><circle cx="16" cy="16" r="4.5" fill="none" stroke="#FFFFFF" stroke-width="2.4"/>"##
);
icon32!(
    PROVIDER_OWNCLOUD,
    "provider-owncloud",
    r##"<rect x="2" y="2" width="28" height="28" rx="7" fill="#1D2D44" stroke="#3D5A80" stroke-width="1.2"/><circle cx="11.8" cy="14.5" r="3.4" fill="#FFFFFF" stroke="#1D2D44" stroke-width="1"/><circle cx="16.8" cy="12.6" r="4.2" fill="#FFFFFF" stroke="#1D2D44" stroke-width="1"/><circle cx="21.4" cy="15.4" r="3.1" fill="#FFFFFF" stroke="#1D2D44" stroke-width="1"/><circle cx="9.2" cy="18.2" r="2.9" fill="#FFFFFF" stroke="#1D2D44" stroke-width="1"/><circle cx="23.2" cy="18.5" r="2.7" fill="#FFFFFF" stroke="#1D2D44" stroke-width="1"/><circle cx="19.2" cy="18.6" r="3.4" fill="#FFFFFF" stroke="#1D2D44" stroke-width="1"/><circle cx="14.2" cy="18" r="4" fill="#FFFFFF" stroke="#1D2D44" stroke-width="1"/>"##
);
icon32!(
    PROVIDER_SFTP,
    "provider-sftp",
    r##"<rect x="2" y="2" width="28" height="28" rx="7" fill="#4F46E5"/><path d="M6.5 10.5L11.2 14.8L6.5 19.1" fill="none" stroke="#FFFFFF" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/><path d="M12.2 19.1H16.2" stroke="#A5B4FC" stroke-width="2.2" stroke-linecap="round"/><rect x="18.5" y="16.5" width="7.2" height="6.3" rx="1.6" fill="#6EE7B7"/><path d="M20.2 16.5V14.7C20.2 13.6 21.1 12.7 22.1 12.7C23.1 12.7 24 13.6 24 14.7V16.5" fill="none" stroke="#6EE7B7" stroke-width="1.8" stroke-linecap="round"/><circle cx="22.1" cy="19.6" r="1" fill="#1E1B4B"/>"##
);
icon32!(
    PROVIDER_FTP,
    "provider-ftp",
    r##"<rect x="2" y="2" width="28" height="28" rx="7" fill="#0284C7"/><path d="M6.5 9.5C6.5 8.4 7.4 7.5 8.5 7.5H13L15 9.8H23.5C24.6 9.8 25.5 10.7 25.5 11.8V22.5C25.5 23.6 24.6 24.5 23.5 24.5H8.5C7.4 24.5 6.5 23.6 6.5 22.5Z" fill="#BAE6FD" fill-opacity="0.22"/><path d="M9.5 14H22M18.8 11L22 14L18.8 17" fill="none" stroke="#FFFFFF" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/><path d="M22.5 19.5H10M13.2 16.5L10 19.5L13.2 22.5" fill="none" stroke="#7DD3FC" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/>"##
);
icon32!(
    PROVIDER_WEBDAV,
    "provider-webdav",
    r##"<rect x="2" y="2" width="28" height="28" rx="7" fill="#0D9488"/><circle cx="16" cy="16" r="8.5" fill="none" stroke="#99F6E4" stroke-width="1.8"/><ellipse cx="16" cy="16" rx="4.2" ry="8.5" fill="none" stroke="#99F6E4" stroke-width="1.6"/><path d="M8 13.2H24M8 18.8H24" stroke="#99F6E4" stroke-width="1.5" stroke-linecap="round"/><path d="M11.5 23.8C9.6 23.8 8.2 22.5 8.2 20.8C8.2 19.3 9.3 18.1 10.7 17.9C11.2 16.1 12.8 14.8 14.8 14.8C16.9 14.8 18.6 16.2 19 18.1C20.2 18.4 21 19.4 21 20.7C21 22.4 19.7 23.8 18 23.8Z" fill="#FFFFFF"/>"##
);

// Context menu, in the text colour.
icon!(M_OPEN, "m-open", r#"<path d="M1.5 13.5V3.5H6L7.5 5H13.5V7.5" stroke="currentColor" stroke-width="1.1" stroke-linejoin="round" fill="none"/><path d="M1.5 13.5L3.5 7.5H15L13 13.5H1.5Z" stroke="currentColor" stroke-width="1.1" stroke-linejoin="round" fill="none"/>"#);
icon!(M_SHOW_IN_FOLDER, "m-show-in-folder", r#"<path d="M1.5 13.5V3.5H6L7.5 5H14.5V13.5H1.5Z" stroke="currentColor" stroke-width="1.1" stroke-linejoin="round" fill="none"/><path d="M4.5 9.3H10.5M8.5 7.3L10.5 9.3L8.5 11.3" stroke="currentColor" stroke-width="1.1" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(M_CUT, "m-cut", r#"<circle cx="4.5" cy="12" r="2" stroke="currentColor" stroke-width="1.1" fill="none"/><circle cx="11.5" cy="12" r="2" stroke="currentColor" stroke-width="1.1" fill="none"/><path d="M6 10.6L12 2.5M10 10.6L4 2.5" stroke="currentColor" stroke-width="1.1" stroke-linecap="round"/>"#);
icon!(M_COPY, "m-copy", r#"<rect x="5.5" y="5.5" width="8" height="9" rx="1.2" stroke="currentColor" stroke-width="1.1" fill="none"/><path d="M10.5 3.5V2.7C10.5 2.04 9.96 1.5 9.3 1.5H3.7C3.04 1.5 2.5 2.04 2.5 2.7V10.3C2.5 10.96 3.04 11.5 3.7 11.5H5.5" stroke="currentColor" stroke-width="1.1" fill="none"/>"#);
icon!(M_PASTE, "m-paste", r#"<rect x="3" y="3" width="10" height="11.5" rx="1.2" stroke="currentColor" stroke-width="1.1" fill="none"/><rect x="5.5" y="1.5" width="5" height="3" rx="0.8" stroke="currentColor" stroke-width="1.1" fill="none"/><path d="M5.5 8H10.5M5.5 10.5H9" stroke="currentColor" stroke-width="1.1" stroke-linecap="round"/>"#);
icon!(M_DELETE, "m-delete", r#"<path d="M2.5 4H13.5M6 4V2.5H10V4M3.8 4L4.6 14H11.4L12.2 4" stroke="currentColor" stroke-width="1.1" stroke-linecap="round" stroke-linejoin="round" fill="none"/><path d="M6.6 6.5V11.5M9.4 6.5V11.5" stroke="currentColor" stroke-width="1.1" stroke-linecap="round"/>"#);
icon!(M_RENAME, "m-rename", r#"<path d="M10.5 2.5L13.5 5.5L5.5 13.5H2.5V10.5Z" stroke="currentColor" stroke-width="1.1" stroke-linejoin="round" fill="none"/><path d="M9 4L12 7" stroke="currentColor" stroke-width="1.1"/>"#);
icon!(M_NEW_FOLDER, "m-new-folder", r#"<path d="M1.5 13V3.5H6L7.5 5H14.5V8" stroke="currentColor" stroke-width="1.1" stroke-linejoin="round" fill="none"/><path d="M1.5 13H8.5" stroke="currentColor" stroke-width="1.1"/><path d="M12 9.5V14.5M9.5 12H14.5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/>"#);
icon!(M_STAR, "m-star", r#"<path d="M8 1.8L9.8 5.7L14 6.2L10.9 9L11.7 13.2L8 11.1L4.3 13.2L5.1 9L2 6.2L6.2 5.7Z" stroke="currentColor" stroke-width="1.1" stroke-linejoin="round" fill="none"/>"#);
icon!(M_RESTORE, "m-restore", r#"<path d="M2.5 8A5.5 5.5 0 1 0 4.1 4.1" stroke="currentColor" stroke-width="1.1" stroke-linecap="round" fill="none"/><path d="M2.3 1.8V4.6H5.1" stroke="currentColor" stroke-width="1.1" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(M_EMPTY_TRASH, "m-empty-trash", r#"<path d="M2.5 4H13.5M6 4V2.5H10V4M3.8 4L4.6 14H11.4L12.2 4" stroke="currentColor" stroke-width="1.1" stroke-linecap="round" stroke-linejoin="round" fill="none"/><path d="M6.3 7L9.7 11M9.7 7L6.3 11" stroke="currentColor" stroke-width="1.1" stroke-linecap="round"/>"#);
icon!(M_PROPERTIES, "m-properties", r#"<rect x="2" y="2" width="12" height="12" rx="1.5" stroke="currentColor" stroke-width="1.1" fill="none"/><path d="M5 5.5H11M5 8H11M5 10.5H8.5" stroke="currentColor" stroke-width="1.1" stroke-linecap="round"/>"#);
icon!(M_LINK, "m-link", r#"<path d="M6.8 9.2L9.2 6.8" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/><path d="M7.4 4.6L8.6 3.4C9.8 2.2 11.6 2.2 12.7 3.3C13.8 4.4 13.8 6.2 12.6 7.4L11.4 8.6M8.6 11.4L7.4 12.6C6.2 13.8 4.4 13.8 3.3 12.7C2.2 11.6 2.2 9.8 3.4 8.6L4.6 7.4" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" fill="none"/>"#);
icon!(M_SHARE, "m-share", r#"<circle cx="6" cy="5.5" r="2.5" stroke="currentColor" stroke-width="1.1" fill="none"/><path d="M1.5 13.5C1.8 11.2 3.6 9.6 6 9.6C7.3 9.6 8.4 10 9.2 10.8" stroke="currentColor" stroke-width="1.1" stroke-linecap="round" fill="none"/><path d="M12.5 9V14M10 11.5H15" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/>"#);
