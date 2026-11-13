//! Open Explorer's icons: SVG bodies on a 16-point grid. File-type and place icons carry their
//! own colours; the toolbar ones draw in `currentColor`, so they follow the text colour.

use rune::Icon;

macro_rules! icon {
    ($id:ident, $name:literal, $body:expr) => {
        pub static $id: Icon = Icon { name: concat!("oe-", $name), view: 16.0, body: $body };
    };
}

// Places and folders.
icon!(FOLDER, "folder", r##"<path d="M1 3.5C1 2.95 1.45 2.5 2 2.5H6L7.5 4H14C14.55 4 15 4.45 15 5V12.5C15 13.05 14.55 13.5 14 13.5H2C1.45 13.5 1 13.05 1 12.5Z" fill="#E8A93A"/><path d="M1 5.5H15V12.5C15 13.05 14.55 13.5 14 13.5H2C1.45 13.5 1 13.05 1 12.5Z" fill="#F6C451"/>"##);
icon!(FOLDER_LINK, "folder-link", r##"<path d="M1 3.5C1 2.95 1.45 2.5 2 2.5H6L7.5 4H14C14.55 4 15 4.45 15 5V12.5C15 13.05 14.55 13.5 14 13.5H2C1.45 13.5 1 13.05 1 12.5Z" fill="#E8A93A"/><path d="M1 5.5H15V12.5C15 13.05 14.55 13.5 14 13.5H2C1.45 13.5 1 13.05 1 12.5Z" fill="#F6C451"/><rect x="2" y="8" width="5.5" height="5" rx="1" fill="#FFFFFF"/><path d="M3.3 11.8L5.8 9.3M4.3 9.2H5.9V10.8" stroke="#2F6FDE" stroke-width="1" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"##);
icon!(HOME, "home", r##"<path d="M2 7.5L8 2.5L14 7.5V13.5H2Z" fill="#5B8DEF"/><rect x="6.5" y="9" width="3" height="4.5" fill="#FFFFFF"/>"##);
icon!(DISK, "disk", r##"<rect x="1.5" y="5" width="13" height="6.5" rx="1.5" fill="#8A94A6"/><rect x="1.5" y="9" width="13" height="2.5" rx="1" fill="#6B7487"/><circle cx="12.3" cy="10.25" r="0.75" fill="#7CE38B"/>"##);
icon!(CLOUD, "cloud", r##"<path d="M4.5 13C2.6 13 1 11.6 1 9.8C1 8.2 2.2 6.9 3.8 6.7C4.3 4.6 6.1 3 8.3 3C10.6 3 12.5 4.7 12.8 6.9C14.1 7.2 15 8.3 15 9.7C15 11.5 13.6 13 11.8 13Z" fill="#4C8BF5"/>"##);
icon!(SHARED, "shared", r##"<path d="M4.5 13C2.6 13 1 11.6 1 9.8C1 8.2 2.2 6.9 3.8 6.7C4.3 4.6 6.1 3 8.3 3C10.6 3 12.5 4.7 12.8 6.9C14.1 7.2 15 8.3 15 9.7C15 11.5 13.6 13 11.8 13Z" fill="#3BA676"/><circle cx="6.3" cy="8" r="1.3" fill="#FFFFFF"/><circle cx="9.7" cy="8" r="1.3" fill="#FFFFFF"/><path d="M4 11.6C4.3 10.4 5.2 9.8 6.3 9.8C7.4 9.8 8.3 10.4 8.6 11.6ZM7.4 11.6C7.7 10.4 8.6 9.8 9.7 9.8C10.8 9.8 11.7 10.4 12 11.6Z" fill="#FFFFFF"/>"##);
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
icon!(CHEVRON, "chevron", r#"<path d="M6 4L10 8L6 12" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(ADD_ACCOUNT, "add-account", r##"<path d="M3.5 12.5C2 12.5 0.8 11.4 0.8 10C0.8 8.8 1.7 7.8 2.9 7.6C3.3 6 4.7 4.8 6.4 4.8C8.2 4.8 9.6 6.1 9.9 7.8C10.9 8 11.6 8.9 11.6 9.9C11.6 11.3 10.5 12.5 9.1 12.5Z" fill="#4C8BF5"/><circle cx="12.3" cy="4.2" r="3.2" fill="#1E8E50"/><path d="M12.3 2.6V5.8M10.7 4.2H13.9" stroke="#FFFFFF" stroke-width="1.1" stroke-linecap="round"/>"##);
icon!(SIGN_OUT, "sign-out", r#"<path d="M6.5 2.5H3.5C2.95 2.5 2.5 2.95 2.5 3.5V12.5C2.5 13.05 2.95 13.5 3.5 13.5H6.5M10 5L13 8L10 11M13 8H6" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);

// Context menu, in the text colour.
icon!(M_OPEN, "m-open", r#"<path d="M1.5 13.5V3.5H6L7.5 5H13.5V7.5" stroke="currentColor" stroke-width="1.1" stroke-linejoin="round" fill="none"/><path d="M1.5 13.5L3.5 7.5H15L13 13.5H1.5Z" stroke="currentColor" stroke-width="1.1" stroke-linejoin="round" fill="none"/>"#);
icon!(M_CUT, "m-cut", r#"<circle cx="4.5" cy="12" r="2" stroke="currentColor" stroke-width="1.1" fill="none"/><circle cx="11.5" cy="12" r="2" stroke="currentColor" stroke-width="1.1" fill="none"/><path d="M6 10.6L12 2.5M10 10.6L4 2.5" stroke="currentColor" stroke-width="1.1" stroke-linecap="round"/>"#);
icon!(M_COPY, "m-copy", r#"<rect x="5.5" y="5.5" width="8" height="9" rx="1.2" stroke="currentColor" stroke-width="1.1" fill="none"/><path d="M10.5 3.5V2.7C10.5 2.04 9.96 1.5 9.3 1.5H3.7C3.04 1.5 2.5 2.04 2.5 2.7V10.3C2.5 10.96 3.04 11.5 3.7 11.5H5.5" stroke="currentColor" stroke-width="1.1" fill="none"/>"#);
icon!(M_PASTE, "m-paste", r#"<rect x="3" y="3" width="10" height="11.5" rx="1.2" stroke="currentColor" stroke-width="1.1" fill="none"/><rect x="5.5" y="1.5" width="5" height="3" rx="0.8" stroke="currentColor" stroke-width="1.1" fill="none"/><path d="M5.5 8H10.5M5.5 10.5H9" stroke="currentColor" stroke-width="1.1" stroke-linecap="round"/>"#);
icon!(M_DELETE, "m-delete", r#"<path d="M2.5 4H13.5M6 4V2.5H10V4M3.8 4L4.6 14H11.4L12.2 4" stroke="currentColor" stroke-width="1.1" stroke-linecap="round" stroke-linejoin="round" fill="none"/><path d="M6.6 6.5V11.5M9.4 6.5V11.5" stroke="currentColor" stroke-width="1.1" stroke-linecap="round"/>"#);
icon!(M_RENAME, "m-rename", r#"<path d="M10.5 2.5L13.5 5.5L5.5 13.5H2.5V10.5Z" stroke="currentColor" stroke-width="1.1" stroke-linejoin="round" fill="none"/><path d="M9 4L12 7" stroke="currentColor" stroke-width="1.1"/>"#);
icon!(M_NEW_FOLDER, "m-new-folder", r#"<path d="M1.5 13V3.5H6L7.5 5H14.5V8" stroke="currentColor" stroke-width="1.1" stroke-linejoin="round" fill="none"/><path d="M1.5 13H8.5" stroke="currentColor" stroke-width="1.1"/><path d="M12 9.5V14.5M9.5 12H14.5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/>"#);
icon!(M_PROPERTIES, "m-properties", r#"<rect x="2" y="2" width="12" height="12" rx="1.5" stroke="currentColor" stroke-width="1.1" fill="none"/><path d="M5 5.5H11M5 8H11M5 10.5H8.5" stroke="currentColor" stroke-width="1.1" stroke-linecap="round"/>"#);
