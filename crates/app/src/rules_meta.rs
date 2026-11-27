//! Meta Tags engine for the Rules-based batch rename: catalog of all 21 categories (130+ tags)
//! and native Rust metadata extractors (File, BinarySign, Hash, Image, EXIF, IPTC, TIFF, MP3,
//! FLAC, WMA, Audio, AVI, HTML, Document/PDF, Font, Email, Torrent, VersionInfo, Date, Clipboard).

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local, Utc};
use openexplorer_i18n::t;
use regex::Regex;

use crate::model::{Entry, Location, format_bytes, shown};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TagCategory {
    All,
    Date,
    File,
    BinarySign,
    Mp3,
    Wma,
    Flac,
    Audio,
    Exif,
    Iptc,
    Tiff,
    Image,
    Html,
    Document,
    Hash,
    Font,
    Email,
    Outlook,
    Avi,
    Torrent,
    Android,
    VersionInfo,
}

impl TagCategory {
    pub const ALL: [Self; 22] = [
        Self::All,
        Self::Date,
        Self::File,
        Self::BinarySign,
        Self::Mp3,
        Self::Wma,
        Self::Flac,
        Self::Audio,
        Self::Exif,
        Self::Iptc,
        Self::Tiff,
        Self::Image,
        Self::Html,
        Self::Document,
        Self::Hash,
        Self::Font,
        Self::Email,
        Self::Outlook,
        Self::Avi,
        Self::Torrent,
        Self::Android,
        Self::VersionInfo,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => t("Todas", "All"),
            Self::Date => "Date",
            Self::File => "File",
            Self::BinarySign => "BinarySign",
            Self::Mp3 => "MP3",
            Self::Wma => "WMA",
            Self::Flac => "FLAC",
            Self::Audio => "Audio",
            Self::Exif => "EXIF",
            Self::Iptc => "IPTC",
            Self::Tiff => "TIFF",
            Self::Image => "Image",
            Self::Html => "HTML",
            Self::Document => "Document",
            Self::Hash => "Hash",
            Self::Font => "Font",
            Self::Email => "Email",
            Self::Outlook => "Outlook",
            Self::Avi => "AVI",
            Self::Torrent => "Torrent",
            Self::Android => "Android",
            Self::VersionInfo => "VersionInfo",
        }
    }
}

pub struct MetaTagDef {
    pub name: &'static str,
    pub category: TagCategory,
    pub desc_pt: &'static str,
    pub desc_en: &'static str,
    pub file_only: bool,
}

impl MetaTagDef {
    pub fn description(&self) -> &'static str {
        t(self.desc_pt, self.desc_en)
    }
}

macro_rules! tag {
    ($name:expr, $cat:ident, $pt:expr, $en:expr) => {
        MetaTagDef { name: $name, category: TagCategory::$cat, desc_pt: $pt, desc_en: $en, file_only: true }
    };
    ($name:expr, $cat:ident, $pt:expr, $en:expr, any) => {
        MetaTagDef { name: $name, category: TagCategory::$cat, desc_pt: $pt, desc_en: $en, file_only: false }
    };
}

pub const META_TAGS: &[MetaTagDef] = &[
    // Date / Clipboard
    tag!("Date_Now", Date, "Data e hora atual", "Current date and time", any),
    tag!("Clipboard", Date, "Texto atual da área de transferência", "Current clipboard text", any),
    // File
    tag!("File_Size", File, "Tamanho formatado do arquivo", "Formatted file size"),
    tag!("File_SizeBytes", File, "Tamanho do arquivo em bytes", "File size in bytes"),
    tag!("File_SizeKB", File, "Tamanho do arquivo em KB", "File size in KB"),
    tag!("File_SizeMB", File, "Tamanho do arquivo em MB", "File size in MB"),
    tag!("File_SizeGB", File, "Tamanho do arquivo em GB", "File size in GB"),
    tag!("File_DateCreated", File, "Data de criação do arquivo", "File creation date"),
    tag!("File_DateModified", File, "Data de modificação do arquivo", "File modification date", any),
    tag!("File_FilePath", File, "Caminho completo do arquivo", "Full file path", any),
    tag!("File_FileName", File, "Nome do arquivo com extensão", "File name with extension", any),
    tag!("File_BaseName", File, "Nome base sem extensão", "Base name without extension", any),
    tag!("File_Extension", File, "Extensão do arquivo (sem ponto)", "File extension (without dot)", any),
    tag!("File_FolderName", File, "Nome da pasta pai", "Parent folder name", any),
    tag!("File_FolderPath", File, "Caminho da pasta pai", "Parent folder path", any),
    tag!("File_GrandparentFolderName", File, "Nome da pasta avô", "Grandparent folder name", any),
    tag!("File_GrandparentFolderPath", File, "Caminho da pasta avô", "Grandparent folder path", any),
    tag!("File_DOSName", File, "Nome curto DOS (8.3)", "Short DOS name (8.3)", any),
    // BinarySign
    tag!("BinarySign_Extension", BinarySign, "Extensão detectada pela assinatura binária", "Extension detected by binary signature"),
    tag!("BinarySign_Description", BinarySign, "Descrição do formato pela assinatura binária", "Format description by binary signature"),
    // MP3
    tag!("MP3_Artist", Mp3, "Artista da tag ID3 (MP3)", "ID3 Artist (MP3)"),
    tag!("MP3_Title", Mp3, "Título da faixa (MP3)", "Track title (MP3)"),
    tag!("MP3_Album", Mp3, "Álbum (MP3)", "Album (MP3)"),
    tag!("MP3_AlbumArtist", Mp3, "Artista do álbum (MP3)", "Album artist (MP3)"),
    tag!("MP3_Year", Mp3, "Ano de lançamento (MP3)", "Release year (MP3)"),
    tag!("MP3_Genre", Mp3, "Gênero musical (MP3)", "Music genre (MP3)"),
    tag!("MP3_TrackNo", Mp3, "Número da faixa (MP3)", "Track number (MP3)"),
    tag!("MP3_TrackTotal", Mp3, "Total de faixas (MP3)", "Total tracks (MP3)"),
    tag!("MP3_DiscNo", Mp3, "Número do disco (MP3)", "Disc number (MP3)"),
    tag!("MP3_DiscTotal", Mp3, "Total de discos (MP3)", "Total discs (MP3)"),
    // WMA
    tag!("WMA_Artist", Wma, "Artista (WMA/ASF)", "Artist (WMA/ASF)"),
    tag!("WMA_Title", Wma, "Título (WMA/ASF)", "Title (WMA/ASF)"),
    tag!("WMA_Album", Wma, "Álbum (WMA/ASF)", "Album (WMA/ASF)"),
    tag!("WMA_AlbumArtist", Wma, "Artista do álbum (WMA/ASF)", "Album artist (WMA/ASF)"),
    tag!("WMA_Year", Wma, "Ano (WMA/ASF)", "Year (WMA/ASF)"),
    tag!("WMA_Genre", Wma, "Gênero (WMA/ASF)", "Genre (WMA/ASF)"),
    tag!("WMA_TrackNo", Wma, "Número da faixa (WMA/ASF)", "Track number (WMA/ASF)"),
    tag!("WMA_TrackTotal", Wma, "Total de faixas (WMA/ASF)", "Total tracks (WMA/ASF)"),
    tag!("WMA_DiscNo", Wma, "Número do disco (WMA/ASF)", "Disc number (WMA/ASF)"),
    tag!("WMA_DiscTotal", Wma, "Total de discos (WMA/ASF)", "Total discs (WMA/ASF)"),
    // FLAC
    tag!("FLAC_Artist", Flac, "Artista (FLAC)", "Artist (FLAC)"),
    tag!("FLAC_Title", Flac, "Título (FLAC)", "Title (FLAC)"),
    tag!("FLAC_Album", Flac, "Álbum (FLAC)", "Album (FLAC)"),
    tag!("FLAC_AlbumArtist", Flac, "Artista do álbum (FLAC)", "Album artist (FLAC)"),
    tag!("FLAC_Year", Flac, "Ano (FLAC)", "Year (FLAC)"),
    tag!("FLAC_Genre", Flac, "Gênero (FLAC)", "Genre (FLAC)"),
    tag!("FLAC_TrackNo", Flac, "Número da faixa (FLAC)", "Track number (FLAC)"),
    tag!("FLAC_TrackTotal", Flac, "Total de faixas (FLAC)", "Total tracks (FLAC)"),
    tag!("FLAC_DiscNo", Flac, "Número do disco (FLAC)", "Disc number (FLAC)"),
    tag!("FLAC_DiscTotal", Flac, "Total de discos (FLAC)", "Total discs (FLAC)"),
    // Audio
    tag!("Audio_Duration", Audio, "Duração formatada do áudio (mm:ss)", "Formatted audio duration (mm:ss)"),
    tag!("Audio_DurationMins", Audio, "Duração do áudio em minutos", "Audio duration in minutes"),
    tag!("Audio_DurationSecs", Audio, "Duração do áudio em segundos", "Audio duration in seconds"),
    // EXIF
    tag!("EXIF_Date", Exif, "Data original da foto (EXIF)", "Original photo date (EXIF)"),
    tag!("EXIF_Model", Exif, "Modelo da câmera (EXIF)", "Camera model (EXIF)"),
    tag!("EXIF_Make", Exif, "Fabricante da câmera (EXIF)", "Camera manufacturer (EXIF)"),
    tag!("EXIF_Description", Exif, "Descrição da imagem (EXIF)", "Image description (EXIF)"),
    // IPTC
    tag!("IPTC_Keywords", Iptc, "Palavras-chave (IPTC)", "Keywords (IPTC)"),
    tag!("IPTC_ObjectName", Iptc, "Nome do objeto (IPTC)", "Object name (IPTC)"),
    tag!("IPTC_Caption", Iptc, "Legenda (IPTC)", "Caption (IPTC)"),
    tag!("IPTC_Headline", Iptc, "Título/Manchete (IPTC)", "Headline (IPTC)"),
    tag!("IPTC_Location", Iptc, "Localização (IPTC)", "Location (IPTC)"),
    tag!("IPTC_City", Iptc, "Cidade (IPTC)", "City (IPTC)"),
    // TIFF
    tag!("TIFF_Date", Tiff, "Data da imagem (TIFF)", "Image date (TIFF)"),
    tag!("TIFF_Model", Tiff, "Modelo da câmera (TIFF)", "Camera model (TIFF)"),
    tag!("TIFF_Make", Tiff, "Fabricante (TIFF)", "Manufacturer (TIFF)"),
    tag!("TIFF_Description", Tiff, "Descrição (TIFF)", "Description (TIFF)"),
    // Image
    tag!("Image_AspectRatio", Image, "Proporção da imagem (ex.: 16x9)", "Image aspect ratio (e.g. 16x9)"),
    tag!("Image_WidthAndHeight", Image, "Largura x Altura (ex.: 1920x1080)", "Width x Height (e.g. 1920x1080)"),
    tag!("Image_Width", Image, "Largura da imagem em pixels", "Image width in pixels"),
    tag!("Image_Height", Image, "Altura da imagem em pixels", "Image height in pixels"),
    tag!("Image_Pixels", Image, "Total de pixels da imagem", "Total image pixels"),
    // HTML / Document
    tag!("HTML_Title", Html, "Título <title> de arquivo HTML", "<title> of HTML file"),
    tag!("Document_Title", Document, "Título do documento (PDF)", "Document title (PDF)"),
    tag!("Document_Subject", Document, "Assunto do documento (PDF)", "Document subject (PDF)"),
    tag!("Document_Author", Document, "Autor do documento (PDF)", "Document author (PDF)"),
    tag!("Document_PageCount", Document, "Quantidade de páginas (PDF)", "Page count (PDF)"),
    // Hash
    tag!("Hash_CRC32", Hash, "Checksum CRC32 em hexadecimal", "CRC32 checksum in hex"),
    tag!("Hash_MD5", Hash, "Hash MD5 em hexadecimal", "MD5 hash in hex"),
    tag!("Hash_SHA1", Hash, "Hash SHA-1 em hexadecimal", "SHA-1 hash in hex"),
    tag!("Hash_SHA256", Hash, "Hash SHA-256 em hexadecimal", "SHA-256 hash in hex"),
    tag!("Hash_SHA512", Hash, "Hash SHA-512 em hexadecimal", "SHA-512 hash in hex"),
    // Font
    tag!("Font_Name", Font, "Nome completo da fonte (TTF/OTF)", "Font full name (TTF/OTF)"),
    tag!("Font_FamilyName", Font, "Família da fonte (TTF/OTF)", "Font family name (TTF/OTF)"),
    tag!("Font_PostScriptName", Font, "Nome PostScript da fonte", "Font PostScript name"),
    tag!("Font_Revision", Font, "Revisão/versão da fonte", "Font revision"),
    // Email
    tag!("Email_MessageID", Email, "Message-ID do e-mail (.eml)", "Email Message-ID (.eml)"),
    tag!("Email_DateSent", Email, "Data de envio local (.eml)", "Local sent date (.eml)"),
    tag!("Email_DateSentUTC", Email, "Data de envio UTC (.eml)", "UTC sent date (.eml)"),
    tag!("Email_DateSentOriginal", Email, "Cabeçalho Date original (.eml)", "Original Date header (.eml)"),
    tag!("Email_Subject", Email, "Assunto do e-mail (.eml)", "Email subject (.eml)"),
    tag!("Email_Sender", Email, "Remetente completo (.eml)", "Full sender (.eml)"),
    tag!("Email_SenderName", Email, "Nome do remetente (.eml)", "Sender name (.eml)"),
    tag!("Email_SenderEmail", Email, "Endereço do remetente (.eml)", "Sender email address (.eml)"),
    tag!("Email_Recipients", Email, "Destinatários (.eml)", "Recipients (.eml)"),
    tag!("Email_Recipient", Email, "Primeiro destinatário (.eml)", "First recipient (.eml)"),
    tag!("Email_RecipientName", Email, "Nome do destinatário (.eml)", "Recipient name (.eml)"),
    tag!("Email_RecipientEmail", Email, "E-mail do destinatário (.eml)", "Recipient email (.eml)"),
    // Outlook
    tag!("Outlook_MessageID", Outlook, "Message-ID (.msg/.eml)", "Message-ID (.msg/.eml)"),
    tag!("Outlook_Subject", Outlook, "Assunto (.msg/.eml)", "Subject (.msg/.eml)"),
    tag!("Outlook_Sender", Outlook, "Remetente (.msg/.eml)", "Sender (.msg/.eml)"),
    tag!("Outlook_SenderName", Outlook, "Nome do remetente (.msg/.eml)", "Sender name (.msg/.eml)"),
    tag!("Outlook_SenderEmail", Outlook, "E-mail do remetente (.msg/.eml)", "Sender email (.msg/.eml)"),
    tag!("Outlook_Recipients", Outlook, "Destinatários (.msg/.eml)", "Recipients (.msg/.eml)"),
    tag!("Outlook_Recipient", Outlook, "Primeiro destinatário (.msg/.eml)", "First recipient (.msg/.eml)"),
    tag!("Outlook_RecipientName", Outlook, "Nome do destinatário (.msg/.eml)", "Recipient name (.msg/.eml)"),
    tag!("Outlook_RecipientEmail", Outlook, "E-mail do destinatário (.msg/.eml)", "Recipient email (.msg/.eml)"),
    tag!("Outlook_Body", Outlook, "Corpo da mensagem", "Message body"),
    tag!("Outlook_DateSent", Outlook, "Data de envio local", "Local sent date"),
    tag!("Outlook_DateSentUTC", Outlook, "Data de envio UTC", "UTC sent date"),
    tag!("Outlook_DateHeader", Outlook, "Data do cabeçalho local", "Header date local"),
    tag!("Outlook_DateHeaderUTC", Outlook, "Data do cabeçalho UTC", "Header date UTC"),
    tag!("Outlook_DateHeaderOriginal", Outlook, "Data original do cabeçalho", "Original header date"),
    // AVI
    tag!("AVI_FramesPerSecond", Avi, "Quadros por segundo (AVI)", "Frames per second (AVI)"),
    tag!("AVI_TotalFrames", Avi, "Total de quadros (AVI)", "Total frames (AVI)"),
    tag!("AVI_Duration", Avi, "Duração formatada (AVI)", "Formatted duration (AVI)"),
    tag!("AVI_DurationMins", Avi, "Duração em minutos (AVI)", "Duration in minutes (AVI)"),
    tag!("AVI_DurationSecs", Avi, "Duração em segundos (AVI)", "Duration in seconds (AVI)"),
    tag!("AVI_WidthAndHeight", Avi, "Largura x Altura (AVI)", "Width x Height (AVI)"),
    tag!("AVI_Width", Avi, "Largura do vídeo (AVI)", "Video width (AVI)"),
    tag!("AVI_Height", Avi, "Altura do vídeo (AVI)", "Video height (AVI)"),
    tag!("AVI_VideoCodec", Avi, "FourCC do codec de vídeo (AVI)", "Video codec FourCC (AVI)"),
    // Torrent
    tag!("Torrent_Name", Torrent, "Nome do torrent (.torrent)", "Torrent name (.torrent)"),
    tag!("Torrent_Hash", Torrent, "InfoHash SHA-1 (.torrent)", "SHA-1 InfoHash (.torrent)"),
    tag!("Torrent_Announce", Torrent, "URL do tracker announce", "Tracker announce URL"),
    tag!("Torrent_Comment", Torrent, "Comentário (.torrent)", "Comment (.torrent)"),
    tag!("Torrent_Publisher", Torrent, "Publicador / Criado por", "Publisher / Created by"),
    tag!("Torrent_CreationDateUTC", Torrent, "Data de criação UTC (.torrent)", "Creation date UTC (.torrent)"),
    tag!("Torrent_CreationDateLocal", Torrent, "Data de criação local (.torrent)", "Creation date local (.torrent)"),
    tag!("Torrent_CountFiles", Torrent, "Quantidade de arquivos (.torrent)", "File count (.torrent)"),
    tag!("Torrent_TotalSize", Torrent, "Tamanho total formatado (.torrent)", "Formatted total size (.torrent)"),
    tag!("Torrent_TotalSizeBytes", Torrent, "Tamanho total em bytes (.torrent)", "Total size in bytes (.torrent)"),
    tag!("Torrent_TotalSizeKB", Torrent, "Tamanho total em KB (.torrent)", "Total size in KB (.torrent)"),
    tag!("Torrent_TotalSizeMB", Torrent, "Tamanho total em MB (.torrent)", "Total size in MB (.torrent)"),
    tag!("Torrent_TotalSizeGB", Torrent, "Tamanho total em GB (.torrent)", "Total size in GB (.torrent)"),
    // Android
    tag!("Android_Label", Android, "Rótulo do aplicativo (.apk)", "App label (.apk)"),
    tag!("Android_Package", Android, "Nome do pacote (.apk)", "Package name (.apk)"),
    tag!("Android_Version", Android, "versionName (.apk)", "versionName (.apk)"),
    tag!("Android_VersionCode", Android, "versionCode (.apk)", "versionCode (.apk)"),
    tag!("Android_MinSdkVersion", Android, "minSdkVersion (.apk)", "minSdkVersion (.apk)"),
    tag!("Android_TargetSdkVersion", Android, "targetSdkVersion (.apk)", "targetSdkVersion (.apk)"),
    tag!("Android_CompileSdkVersion", Android, "compileSdkVersion (.apk)", "compileSdkVersion (.apk)"),
    tag!("Android_MinSdkVersionName", Android, "Nome da versão Android mínima", "Min Android version name"),
    tag!("Android_TargetSdkVersionName", Android, "Nome da versão Android alvo", "Target Android version name"),
    tag!("Android_CompileSdkVersionName", Android, "Nome da versão Android de compilação", "Compile Android version name"),
    tag!("Android_Architectures", Android, "Arquiteturas nativas (.apk)", "Native architectures (.apk)"),
    tag!("Android_Permissions", Android, "Permissões (.apk)", "Permissions (.apk)"),
    // VersionInfo
    tag!("VersionInfo_FileVersion", VersionInfo, "Versão do arquivo (.exe/.dll)", "File version (.exe/.dll)"),
    tag!("VersionInfo_ProductVersion", VersionInfo, "Versão do produto (.exe/.dll)", "Product version (.exe/.dll)"),
    tag!("VersionInfo_Product", VersionInfo, "Nome do produto (.exe/.dll)", "Product name (.exe/.dll)"),
    tag!("VersionInfo_Company", VersionInfo, "Empresa (.exe/.dll)", "Company name (.exe/.dll)"),
    tag!("VersionInfo_Copyright", VersionInfo, "Direitos autorais (.exe/.dll)", "Copyright (.exe/.dll)"),
    tag!("VersionInfo_Description", VersionInfo, "Descrição do arquivo (.exe/.dll)", "File description (.exe/.dll)"),
    tag!("VersionInfo_Comments", VersionInfo, "Comentários (.exe/.dll)", "Comments (.exe/.dll)"),
];

// ---------------------------------------------------------------------------------------------
// Tag Expansion & Caching

static TAG_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r":([A-Za-z0-9_]+):").unwrap());
static TAG_CACHE: LazyLock<Mutex<HashMap<(String, String), String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Hashes read whole files, minutes for big videos or a network drive: they run on one
/// background thread, so the window never waits on them. The jobs asked for, by cache key.
static HASH_PENDING: LazyLock<Mutex<std::collections::HashSet<(String, String)>>> = LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));
/// A file tag to compute: the file's path as the cache keys it, the tag, the path, the item.
type HashJob = (String, String, std::path::PathBuf, Entry);
static HASH_QUEUE: LazyLock<Mutex<Option<Sender<HashJob>>>> = LazyLock::new(|| Mutex::new(None));
/// Set when a background hash lands, for the batch window's poll to draw again.
static CHANGED: AtomicBool = AtomicBool::new(false);
static WAKER: LazyLock<Mutex<Option<rune::Waker>>> = LazyLock::new(|| Mutex::new(None));

thread_local! {
    /// Whether the current preview pass met a tag still being computed.
    static WAITED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What a tag still being computed shows meanwhile.
pub const PENDING: &str = "…";

/// The batch window's waker, called when a background hash lands.
pub fn set_waker(waker: rune::Waker) {
    *WAKER.lock().unwrap_or_else(|e| e.into_inner()) = Some(waker);
}

/// Starts a preview pass: [`waited`] then says whether it met a tag still being computed.
pub fn begin_pass() {
    WAITED.with(|w| w.set(false));
}

pub fn waited() -> bool {
    WAITED.with(std::cell::Cell::get)
}

/// Whether a background hash landed since the last call.
pub fn take_changed() -> bool {
    CHANGED.swap(false, Ordering::AcqRel)
}

/// Queues `tag` of the file at `path` for the background thread, once.
fn compute_later(key: (String, String), path: std::path::PathBuf, entry: &Entry) {
    if !HASH_PENDING.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone()) {
        return;
    }
    let mut queue = HASH_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    let tx = queue.get_or_insert_with(|| {
        let (tx, rx) = std::sync::mpsc::channel::<HashJob>();
        std::thread::spawn(move || {
            for (file, tag, path, entry) in rx {
                let val = compute_file_tag(&tag, &path, &entry).unwrap_or_default();
                let key = (file, tag);
                if let Ok(mut g) = TAG_CACHE.lock() {
                    g.insert(key.clone(), val);
                }
                HASH_PENDING.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
                CHANGED.store(true, Ordering::Release);
                if let Some(wake) = WAKER.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    wake();
                }
            }
        });
        tx
    });
    let (file, tag) = key;
    if let Err(job) = tx.send((file, tag, path, entry.clone())) {
        // The thread is gone: forget the job, so the next pass asks again.
        *queue = None;
        let (file, tag, ..) = job.0;
        HASH_PENDING.lock().unwrap_or_else(|e| e.into_inner()).remove(&(file, tag));
    }
}

pub fn clear_cache() {
    if let Ok(mut g) = TAG_CACHE.lock() {
        g.clear();
    }
}

/// Whether `input` holds a `:TagName:` placeholder, so it reads differently for each item.
pub fn has_tags(input: &str) -> bool {
    input.contains(':') && TAG_REGEX.is_match(input)
}

/// Expands all `:TagName:` placeholders inside `input` for `entry` at `loc`.
pub fn expand_tags(input: &str, entry: &Entry, loc: &Location) -> String {
    if !input.contains(':') {
        return input.to_string();
    }
    TAG_REGEX
        .replace_all(input, |caps: &regex::Captures| {
            let tag_name = &caps[1];
            resolve_tag(tag_name, entry, loc).unwrap_or_default()
        })
        .into_owned()
}

pub fn resolve_tag(tag: &str, entry: &Entry, loc: &Location) -> Option<String> {
    // Non-cached live tags:
    if tag == "Date_Now" {
        return Some(Local::now().format("%Y-%m-%d %H-%M-%S").to_string());
    }
    if tag == "Clipboard" {
        return arboard::Clipboard::new().ok().and_then(|mut c| c.get_text().ok()).map(|s| s.lines().next().unwrap_or("").to_string());
    }

    let local_path = loc.local_path(entry);
    let shown_name = shown(&entry.name);
    let (base, ext) = match shown_name.rfind('.') {
        Some(i) if i > 0 && !entry.is_dir => (&shown_name[..i], &shown_name[i + 1..]),
        _ => (shown_name.as_str(), ""),
    };

    // Fast entry-only tags:
    match tag {
        "File_FileName" => return Some(shown_name),
        "File_BaseName" => return Some(base.to_string()),
        "File_Extension" => return Some(ext.to_string()),
        "File_FilePath" => {
            return Some(local_path.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| shown_name));
        }
        "File_FolderName" => {
            return local_path.as_ref().and_then(|p| p.parent()).and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
        }
        "File_FolderPath" => {
            return local_path.as_ref().and_then(|p| p.parent()).map(|p| p.to_string_lossy().into_owned());
        }
        "File_GrandparentFolderName" => {
            return local_path
                .as_ref()
                .and_then(|p| p.parent())
                .and_then(|p| p.parent())
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned());
        }
        "File_GrandparentFolderPath" => {
            return local_path.as_ref().and_then(|p| p.parent()).and_then(|p| p.parent()).map(|p| p.to_string_lossy().into_owned());
        }
        "File_Size" => return entry.size.map(format_bytes),
        "File_SizeBytes" => return entry.size.map(|b| b.to_string()),
        "File_SizeKB" => return entry.size.map(|b| format!("{}", b / 1024)),
        "File_SizeMB" => return entry.size.map(|b| format!("{:.2}", b as f64 / (1024.0 * 1024.0))),
        "File_SizeGB" => return entry.size.map(|b| format!("{:.2}", b as f64 / (1024.0 * 1024.0 * 1024.0))),
        "File_DateModified" => {
            if let Some(t) = entry.modified {
                return Some(fmt_sys_time(t));
            }
        }
        _ => {}
    }

    let path = local_path?;
    let cache_key = (path.to_string_lossy().into_owned(), tag.to_string());
    if let Ok(g) = TAG_CACHE.lock()
        && let Some(cached) = g.get(&cache_key)
    {
        return Some(cached.clone());
    }

    if tag.starts_with("Hash_") {
        WAITED.with(|w| w.set(true));
        compute_later(cache_key, path, entry);
        return Some(PENDING.into());
    }
    // Kept for as long as the batch window shows these items: emptying it when it grew past a
    // size made a batch of more files than that read them all again at every preview.
    let val = compute_file_tag(tag, &path, entry).unwrap_or_default();
    if let Ok(mut g) = TAG_CACHE.lock() {
        g.insert(cache_key, val.clone());
    }
    Some(val)
}

fn fmt_sys_time(st: SystemTime) -> String {
    let dt: DateTime<Local> = st.into();
    dt.format("%Y-%m-%d %H-%M-%S").to_string()
}

fn compute_file_tag(tag: &str, path: &Path, entry: &Entry) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    match tag {
        "File_DateCreated" => return meta.created().ok().map(fmt_sys_time),
        "File_DateModified" => return meta.modified().ok().map(fmt_sys_time),
        "File_DOSName" => return Some(dos_name(path)),
        _ => {}
    }

    if entry.is_dir || meta.is_dir() {
        return None;
    }

    if let Some(rest) = tag.strip_prefix("Hash_") {
        return compute_hash(rest, path);
    }
    if tag == "BinarySign_Extension" || tag == "BinarySign_Description" {
        let (ext, desc) = detect_binary_signature(path)?;
        return Some(if tag == "BinarySign_Extension" { ext.into() } else { desc.into() });
    }
    if tag.starts_with("Image_") {
        return image_tag(tag, path);
    }
    if tag.starts_with("EXIF_") || tag.starts_with("TIFF_") {
        return exif_tiff_tag(tag, path);
    }
    if tag.starts_with("IPTC_") {
        return iptc_tag(tag, path);
    }
    if tag.starts_with("MP3_") || tag == "Audio_Duration" || tag == "Audio_DurationMins" || tag == "Audio_DurationSecs" {
        if let Some(v) = mp3_or_audio_tag(tag, path) {
            return Some(v);
        }
    }
    if tag.starts_with("FLAC_") || tag.starts_with("Audio_") {
        if let Some(v) = flac_tag(tag, path) {
            return Some(v);
        }
    }
    if tag.starts_with("WMA_") || tag.starts_with("Audio_") {
        if let Some(v) = wma_tag(tag, path) {
            return Some(v);
        }
    }
    if tag.starts_with("AVI_") {
        return avi_tag(tag, path);
    }
    if tag == "HTML_Title" {
        return html_title(path);
    }
    if tag.starts_with("Document_") {
        return pdf_tag(tag, path);
    }
    if tag.starts_with("Font_") {
        return font_tag(tag, path);
    }
    if tag.starts_with("Email_") || tag.starts_with("Outlook_") {
        return email_tag(tag, path);
    }
    if tag.starts_with("Torrent_") {
        return torrent_tag(tag, path);
    }
    if tag.starts_with("VersionInfo_") {
        return version_info_tag(tag, path);
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Binary Signature Detection

pub fn detect_binary_signature(path: &Path) -> Option<(&'static str, &'static str)> {
    let mut f = File::open(path).ok()?;
    let mut buf = [0u8; 64];
    let n = f.read(&mut buf).ok()?;
    let b = &buf[..n];
    if b.starts_with(b"%PDF-") {
        Some(("pdf", "PDF Document"))
    } else if b.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some(("png", "PNG Image"))
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(("jpg", "JPEG Image"))
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some(("gif", "GIF Image"))
    } else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some(("webp", "WebP Image"))
    } else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"AVI " {
        Some(("avi", "AVI Video"))
    } else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WAVE" {
        Some(("wav", "WAVE Audio"))
    } else if b.starts_with(b"BM") && b.len() >= 14 {
        Some(("bmp", "Bitmap Image"))
    } else if b.starts_with(b"II*\0") || b.starts_with(b"MM\0*") {
        Some(("tiff", "TIFF Image"))
    } else if b.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        Some(("mkv", "Matroska / WebM Video"))
    } else if b.len() >= 12 && &b[4..8] == b"ftyp" {
        Some(("mp4", "MPEG-4 Video"))
    } else if b.starts_with(b"fLaC") {
        Some(("flac", "FLAC Audio"))
    } else if b.starts_with(b"OggS") {
        Some(("ogg", "Ogg Audio/Video"))
    } else if b.starts_with(b"ID3") || (b.len() >= 2 && b[0] == 0xFF && (b[1] & 0xE0) == 0xE0) {
        Some(("mp3", "MP3 Audio"))
    } else if b.starts_with(&[0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11]) {
        Some(("wma", "Windows Media Audio/Video"))
    } else if b.starts_with(b"PK\x03\x04") || b.starts_with(b"PK\x05\x06") {
        Some(("zip", "ZIP Archive"))
    } else if b.starts_with(b"Rar!\x1A\x07") {
        Some(("rar", "RAR Archive"))
    } else if b.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]) {
        Some(("7z", "7-Zip Archive"))
    } else if b.starts_with(&[0x1F, 0x8B, 0x08]) {
        Some(("gz", "GZIP Archive"))
    } else if b.starts_with(b"MZ") {
        Some(("exe", "Windows Executable / DLL"))
    } else if b.starts_with(b"\x7FELF") {
        Some(("elf", "ELF Binary"))
    } else if b.starts_with(&[0x00, 0x01, 0x00, 0x00]) || b.starts_with(b"OTTO") {
        Some(("ttf", "TrueType / OpenType Font"))
    } else if b.starts_with(b"wOFF") {
        Some(("woff", "Web Open Font Format"))
    } else if b.starts_with(b"wOF2") {
        Some(("woff2", "Web Open Font Format 2"))
    } else if b.starts_with(b"d8:announce") || b.starts_with(b"d13:announce") {
        Some(("torrent", "BitTorrent File"))
    } else if b.starts_with(b"<!DOCTYPE html") || b.starts_with(b"<html") || b.starts_with(b"<HTML") {
        Some(("html", "HTML Document"))
    } else if b.starts_with(b"<?xml") {
        Some(("xml", "XML Document"))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Hashes

fn compute_hash(kind: &str, path: &Path) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let mut buf = [0u8; 65536];
    match kind {
        "CRC32" => {
            let mut h = crc32fast::Hasher::new();
            while let Ok(n) = f.read(&mut buf) {
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            Some(format!("{:08X}", h.finalize()))
        }
        "MD5" => {
            use md5::Digest as _;
            let mut h = md5::Md5::new();
            while let Ok(n) = f.read(&mut buf) {
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            Some(hex_lower(&h.finalize()))
        }
        "SHA1" => {
            use sha1::Digest as _;
            let mut h = sha1::Sha1::new();
            while let Ok(n) = f.read(&mut buf) {
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            Some(hex_lower(&h.finalize()))
        }
        "SHA256" => {
            use sha2::Digest as _;
            let mut h = sha2::Sha256::new();
            while let Ok(n) = f.read(&mut buf) {
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            Some(hex_lower(&h.finalize()))
        }
        "SHA512" => {
            use sha2::Digest as _;
            let mut h = sha2::Sha512::new();
            while let Ok(n) = f.read(&mut buf) {
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            Some(hex_lower(&h.finalize()))
        }
        _ => None,
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xF) as usize] as char);
    }
    out
}

fn dos_name(path: &Path) -> String {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut out = vec![0u16; 260];
        let len = unsafe { windows_sys::Win32::Storage::FileSystem::GetShortPathNameW(wide.as_ptr(), out.as_mut_ptr(), out.len() as u32) };
        if len > 0 && (len as usize) < out.len() {
            let short = std::ffi::OsString::from_wide(&out[..len as usize]);
            if let Some(name) = Path::new(&short).file_name() {
                return name.to_string_lossy().into_owned();
            }
        }
    }
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Image / EXIF / TIFF / IPTC

fn image_tag(tag: &str, path: &Path) -> Option<String> {
    let dim = imagesize::size(path).ok()?;
    let (w, h) = (dim.width as u64, dim.height as u64);
    match tag {
        "Image_Width" => Some(w.to_string()),
        "Image_Height" => Some(h.to_string()),
        "Image_WidthAndHeight" => Some(format!("{w}x{h}")),
        "Image_Pixels" => Some((w * h).to_string()),
        "Image_AspectRatio" => {
            if w == 0 || h == 0 {
                return None;
            }
            let g = gcd(w, h);
            Some(format!("{}x{}", w / g, h / g))
        }
        _ => None,
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a.max(1)
}

fn read_head(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let mut f = File::open(path).ok()?;
    let mut buf = vec![0u8; limit];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

fn exif_tiff_tag(tag: &str, path: &Path) -> Option<String> {
    let data = read_head(path, 256 * 1024)?;
    let tiff = if data.starts_with(&[0xFF, 0xD8]) {
        // Scan JPEG APP1 markers for "Exif\0\0"
        let mut pos = 2usize;
        let mut found = None;
        while pos + 4 < data.len() {
            if data[pos] != 0xFF {
                break;
            }
            let marker = data[pos + 1];
            if marker == 0xDA || marker == 0xD9 {
                break;
            }
            if marker == 0x00 || (0xD0..=0xD8).contains(&marker) {
                pos += 2;
                continue;
            }
            let seg_len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
            if seg_len < 2 || pos + 2 + seg_len > data.len() {
                break;
            }
            if marker == 0xE1 && seg_len >= 8 && &data[pos + 4..pos + 10] == b"Exif\0\0" {
                found = Some(&data[pos + 10..pos + 2 + seg_len]);
                break;
            }
            pos += 2 + seg_len;
        }
        found?
    } else if data.starts_with(b"II*\0") || data.starts_with(b"MM\0*") {
        data.as_slice()
    } else {
        return None;
    };

    if tiff.len() < 8 {
        return None;
    }
    let le = &tiff[0..2] == b"II";
    let u16_at = |o: usize| -> Option<u16> {
        let s = tiff.get(o..o + 2)?;
        Some(if le { u16::from_le_bytes([s[0], s[1]]) } else { u16::from_be_bytes([s[0], s[1]]) })
    };
    let u32_at = |o: usize| -> Option<u32> {
        let s = tiff.get(o..o + 4)?;
        Some(if le { u32::from_le_bytes([s[0], s[1], s[2], s[3]]) } else { u32::from_be_bytes([s[0], s[1], s[2], s[3]]) })
    };
    let read_ascii = |ifd_entry: usize| -> Option<String> {
        let typ = u16_at(ifd_entry + 2)?;
        let cnt = u32_at(ifd_entry + 4)? as usize;
        if typ != 2 || cnt == 0 {
            return None;
        }
        let raw = if cnt <= 4 {
            tiff.get(ifd_entry + 8..ifd_entry + 8 + cnt)?
        } else {
            let off = u32_at(ifd_entry + 8)? as usize;
            tiff.get(off..off + cnt)?
        };
        let s = String::from_utf8_lossy(raw).trim_end_matches('\0').trim().to_string();
        (!s.is_empty()).then_some(s)
    };

    let want_id: u16 = match tag {
        "EXIF_Make" | "TIFF_Make" => 0x010F,
        "EXIF_Model" | "TIFF_Model" => 0x0110,
        "EXIF_Description" | "TIFF_Description" => 0x010E,
        "EXIF_Date" | "TIFF_Date" => 0x0132, // DateTime; also check ExifIFD 0x9003 below
        _ => return None,
    };

    let ifd0 = u32_at(4)? as usize;
    let count = u16_at(ifd0)? as usize;
    let mut exif_ifd_off: Option<usize> = None;
    let mut date_fallback: Option<String> = None;

    for i in 0..count.min(128) {
        let ent = ifd0 + 2 + i * 12;
        let Some(id) = u16_at(ent) else { break };
        if id == 0x8769 {
            exif_ifd_off = u32_at(ent + 8).map(|v| v as usize);
        }
        if id == want_id {
            let val = read_ascii(ent);
            if want_id == 0x0132 {
                date_fallback = val;
            } else if val.is_some() {
                return val;
            }
        }
    }

    if matches!(tag, "EXIF_Date" | "TIFF_Date")
        && let Some(eoff) = exif_ifd_off
        && let Some(ecount) = u16_at(eoff)
    {
        for i in 0..(ecount as usize).min(128) {
            let ent = eoff + 2 + i * 12;
            let Some(id) = u16_at(ent) else { break };
            if id == 0x9003 || id == 0x9004 {
                if let Some(d) = read_ascii(ent) {
                    return Some(d);
                }
            }
        }
    }
    date_fallback
}

fn iptc_tag(tag: &str, path: &Path) -> Option<String> {
    let data = read_head(path, 256 * 1024)?;
    if !data.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let dataset: u8 = match tag {
        "IPTC_ObjectName" => 5,
        "IPTC_Keywords" => 25,
        "IPTC_City" => 90,
        "IPTC_Location" => 92,
        "IPTC_Headline" => 105,
        "IPTC_Caption" => 120,
        _ => return None,
    };
    let mut pos = 2usize;
    while pos + 4 < data.len() {
        if data[pos] != 0xFF {
            break;
        }
        let marker = data[pos + 1];
        if marker == 0xDA || marker == 0xD9 {
            break;
        }
        let seg_len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        if seg_len < 2 || pos + 2 + seg_len > data.len() {
            break;
        }
        if marker == 0xED {
            let seg = &data[pos + 4..pos + 2 + seg_len];
            let mut vals = Vec::new();
            for i in 0..seg.len().saturating_sub(5) {
                if seg[i] == 0x1C && seg[i + 1] == 0x02 && seg[i + 2] == dataset {
                    let len = u16::from_be_bytes([seg[i + 3], seg[i + 4]]) as usize;
                    if let Some(slice) = seg.get(i + 5..i + 5 + len) {
                        let s = String::from_utf8_lossy(slice).trim().to_string();
                        if !s.is_empty() {
                            vals.push(s);
                        }
                    }
                }
            }
            if !vals.is_empty() {
                return Some(vals.join(", "));
            }
        }
        pos += 2 + seg_len;
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Audio: MP3 (ID3v1/v2), FLAC, WMA (ASF)

fn mp3_or_audio_tag(tag: &str, path: &Path) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let file_len = f.metadata().ok()?.len();
    let mut head = [0u8; 10];
    if f.read(&mut head).ok()? < 10 {
        return None;
    }
    let mut map: HashMap<&'static str, String> = HashMap::new();
    let mut audio_offset = 0u64;

    if &head[0..3] == b"ID3" {
        let ver = head[3];
        let tag_size = (((head[6] & 0x7F) as u32) << 21)
            | (((head[7] & 0x7F) as u32) << 14)
            | (((head[8] & 0x7F) as u32) << 7)
            | ((head[9] & 0x7F) as u32);
        audio_offset = 10 + tag_size as u64;
        let to_read = (tag_size as usize).min(512 * 1024);
        let mut body = vec![0u8; to_read];
        if f.read_exact(&mut body).is_ok() && (ver == 3 || ver == 4) {
            let mut p = 0usize;
            while p + 10 <= body.len() {
                let fid = &body[p..p + 4];
                if fid[0] == 0 {
                    break;
                }
                let fsize = if ver == 4 {
                    (((body[p + 4] & 0x7F) as usize) << 21)
                        | (((body[p + 5] & 0x7F) as usize) << 14)
                        | (((body[p + 6] & 0x7F) as usize) << 7)
                        | ((body[p + 7] & 0x7F) as usize)
                } else {
                    u32::from_be_bytes([body[p + 4], body[p + 5], body[p + 6], body[p + 7]]) as usize
                };
                p += 10;
                if fsize == 0 || p + fsize > body.len() {
                    break;
                }
                let txt = decode_id3_text(&body[p..p + fsize]);
                match fid {
                    b"TPE1" => {
                        map.insert("MP3_Artist", txt);
                    }
                    b"TIT2" => {
                        map.insert("MP3_Title", txt);
                    }
                    b"TALB" => {
                        map.insert("MP3_Album", txt);
                    }
                    b"TPE2" => {
                        map.insert("MP3_AlbumArtist", txt);
                    }
                    b"TYER" | b"TDRC" => {
                        map.entry("MP3_Year").or_insert_with(|| txt.chars().take(4).collect());
                    }
                    b"TCON" => {
                        map.insert("MP3_Genre", txt);
                    }
                    b"TRCK" => {
                        let (a, b) = split_pair(&txt);
                        map.insert("MP3_TrackNo", a);
                        if !b.is_empty() {
                            map.insert("MP3_TrackTotal", b);
                        }
                    }
                    b"TPOS" => {
                        let (a, b) = split_pair(&txt);
                        map.insert("MP3_DiscNo", a);
                        if !b.is_empty() {
                            map.insert("MP3_DiscTotal", b);
                        }
                    }
                    _ => {}
                }
                p += fsize;
            }
        }
    }

    if let Some(v) = map.get(tag) {
        return Some(v.clone());
    }

    // Audio duration estimate from first MP3 frame header:
    if tag.starts_with("Audio_Duration") && file_len > audio_offset + 4 {
        let _ = f.seek(SeekFrom::Start(audio_offset));
        let mut scan = [0u8; 4096];
        let n = f.read(&mut scan).ok()?;
        for i in 0..n.saturating_sub(4) {
            if scan[i] == 0xFF && (scan[i + 1] & 0xE0) == 0xE0 {
                let ver_bits = (scan[i + 1] >> 3) & 0x03;
                let layer_bits = (scan[i + 1] >> 1) & 0x03;
                let br_idx = ((scan[i + 2] >> 4) & 0x0F) as usize;
                if ver_bits == 3 && layer_bits == 1 && br_idx > 0 && br_idx < 15 {
                    const KBPS: [u64; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
                    let bitrate_bps = KBPS[br_idx] * 1000;
                    let secs = ((file_len - audio_offset) * 8) / bitrate_bps;
                    return Some(fmt_duration(tag, secs));
                }
            }
        }
    }
    None
}

fn decode_id3_text(raw: &[u8]) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let enc = raw[0];
    let payload = &raw[1..];
    match enc {
        0 => payload.iter().take_while(|&&b| b != 0).map(|&b| b as char).collect::<String>().trim().to_string(),
        1 | 2 => {
            let le = if enc == 1 && payload.starts_with(&[0xFF, 0xFE]) {
                true
            } else {
                enc == 1 && !payload.starts_with(&[0xFE, 0xFF])
            };
            let start = if payload.starts_with(&[0xFF, 0xFE]) || payload.starts_with(&[0xFE, 0xFF]) { 2 } else { 0 };
            let u16s: Vec<u16> = payload[start..]
                .chunks_exact(2)
                .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
                .take_while(|&u| u != 0)
                .collect();
            String::from_utf16_lossy(&u16s).trim().to_string()
        }
        _ => String::from_utf8_lossy(payload).trim_end_matches('\0').trim().to_string(),
    }
}

fn split_pair(s: &str) -> (String, String) {
    match s.split_once('/') {
        Some((a, b)) => (a.trim().to_string(), b.trim().to_string()),
        None => (s.trim().to_string(), String::new()),
    }
}

fn fmt_duration(tag: &str, secs: u64) -> String {
    if tag.ends_with("Secs") {
        secs.to_string()
    } else if tag.ends_with("Mins") {
        (secs / 60).to_string()
    } else {
        format!("{:02}-{:02}", secs / 60, secs % 60)
    }
}

fn flac_tag(tag: &str, path: &Path) -> Option<String> {
    let data = read_head(path, 256 * 1024)?;
    if !data.starts_with(b"fLaC") {
        return None;
    }
    let mut p = 4usize;
    let mut duration_secs: Option<u64> = None;
    let mut map: HashMap<String, String> = HashMap::new();
    while p + 4 <= data.len() {
        let hdr = data[p];
        let is_last = (hdr & 0x80) != 0;
        let btype = hdr & 0x7F;
        let len = (((data[p + 1] as usize) << 16) | ((data[p + 2] as usize) << 8) | (data[p + 3] as usize)) as usize;
        p += 4;
        if p + len > data.len() {
            break;
        }
        let block = &data[p..p + len];
        if btype == 0 && block.len() >= 18 {
            let sr = ((block[10] as u32) << 12) | ((block[11] as u32) << 4) | ((block[12] as u32) >> 4);
            let samples = (((block[13] & 0x0F) as u64) << 32)
                | ((block[14] as u64) << 24)
                | ((block[15] as u64) << 16)
                | ((block[16] as u64) << 8)
                | (block[17] as u64);
            if sr > 0 {
                duration_secs = Some(samples / sr as u64);
            }
        } else if btype == 4 && block.len() >= 8 {
            let vlen = u32::from_le_bytes([block[0], block[1], block[2], block[3]]) as usize;
            let mut q = 4 + vlen;
            if q + 4 <= block.len() {
                let count = u32::from_le_bytes([block[q], block[q + 1], block[q + 2], block[q + 3]]) as usize;
                q += 4;
                for _ in 0..count.min(256) {
                    if q + 4 > block.len() {
                        break;
                    }
                    let clen = u32::from_le_bytes([block[q], block[q + 1], block[q + 2], block[q + 3]]) as usize;
                    q += 4;
                    if q + clen > block.len() {
                        break;
                    }
                    let comment = String::from_utf8_lossy(&block[q..q + clen]);
                    if let Some((k, v)) = comment.split_once('=') {
                        map.insert(k.to_ascii_uppercase(), v.to_string());
                    }
                    q += clen;
                }
            }
        }
        p += len;
        if is_last {
            break;
        }
    }
    if tag.starts_with("Audio_Duration") {
        return duration_secs.map(|s| fmt_duration(tag, s));
    }
    match tag {
        "FLAC_Artist" => map.get("ARTIST").cloned(),
        "FLAC_Title" => map.get("TITLE").cloned(),
        "FLAC_Album" => map.get("ALBUM").cloned(),
        "FLAC_AlbumArtist" => map.get("ALBUMARTIST").cloned(),
        "FLAC_Year" => map.get("DATE").or_else(|| map.get("YEAR")).map(|s| s.chars().take(4).collect()),
        "FLAC_Genre" => map.get("GENRE").cloned(),
        "FLAC_TrackNo" => map.get("TRACKNUMBER").map(|s| split_pair(s).0),
        "FLAC_TrackTotal" => map.get("TRACKTOTAL").cloned().or_else(|| map.get("TRACKNUMBER").map(|s| split_pair(s).1).filter(|x| !x.is_empty())),
        "FLAC_DiscNo" => map.get("DISCNUMBER").map(|s| split_pair(s).0),
        "FLAC_DiscTotal" => map.get("DISCTOTAL").cloned().or_else(|| map.get("DISCNUMBER").map(|s| split_pair(s).1).filter(|x| !x.is_empty())),
        _ => None,
    }
}

fn wma_tag(tag: &str, path: &Path) -> Option<String> {
    // ASF Header Object GUID: 30 26 B2 75 8E 66 CF 11 A6 D9 00 AA 00 62 CE 6C
    let data = read_head(path, 128 * 1024)?;
    if !data.starts_with(&[0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11]) || data.len() < 30 {
        return None;
    }
    const CONTENT_DESC: [u8; 16] = [0x33, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C];
    const FILE_PROPS: [u8; 16] = [0xA1, 0xDC, 0xAB, 0x8C, 0x47, 0xA9, 0xCF, 0x11, 0x8E, 0xE4, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65];
    let mut p = 30usize;
    while p + 24 <= data.len() {
        let guid = &data[p..p + 16];
        let size = u64::from_le_bytes(data[p + 16..p + 24].try_into().ok()?) as usize;
        // `size` comes from the file: a damaged one could overflow the addition.
        let Some(end) = p.checked_add(size).filter(|&e| size >= 24 && e <= data.len()) else { break };
        let body = &data[p + 24..end];
        if guid == FILE_PROPS && tag.starts_with("Audio_Duration") && body.len() >= 48 {
            let play_dur = u64::from_le_bytes(body[40..48].try_into().ok()?); // 100-ns units
            return Some(fmt_duration(tag, play_dur / 10_000_000));
        }
        if guid == CONTENT_DESC && body.len() >= 10 {
            let title_len = u16::from_le_bytes([body[0], body[1]]) as usize;
            let author_len = u16::from_le_bytes([body[2], body[3]]) as usize;
            let off = 10usize;
            if tag == "WMA_Title" && off + title_len <= body.len() {
                return Some(utf16le(&body[off..off + title_len]));
            }
            if tag == "WMA_Artist" && off + title_len + author_len <= body.len() {
                return Some(utf16le(&body[off + title_len..off + title_len + author_len]));
            }
        }
        p += size;
    }
    None
}

fn utf16le(b: &[u8]) -> String {
    let u: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&x| x != 0).collect();
    String::from_utf16_lossy(&u).trim().to_string()
}

// ---------------------------------------------------------------------------------------------
// AVI, HTML, PDF, Font, Email, Torrent, VersionInfo

fn avi_tag(tag: &str, path: &Path) -> Option<String> {
    let data = read_head(path, 8192)?;
    if data.len() < 64 || &data[0..4] != b"RIFF" || &data[8..12] != b"AVI " {
        return None;
    }
    let pos = data.windows(4).position(|w| w == b"avih")?;
    // The 40 bytes read below start 8 bytes after the chunk id.
    if pos + 48 > data.len() {
        return None;
    }
    let avih = &data[pos + 8..];
    let us_per_frame = u32::from_le_bytes(avih[0..4].try_into().ok()?);
    let total_frames = u32::from_le_bytes(avih[16..20].try_into().ok()?);
    let width = u32::from_le_bytes(avih[32..36].try_into().ok()?);
    let height = u32::from_le_bytes(avih[36..40].try_into().ok()?);
    let fps = if us_per_frame > 0 { 1_000_000.0 / us_per_frame as f64 } else { 0.0 };
    let secs = if fps > 0.0 { (total_frames as f64 / fps) as u64 } else { 0 };
    match tag {
        "AVI_FramesPerSecond" => Some(format!("{fps:.2}")),
        "AVI_TotalFrames" => Some(total_frames.to_string()),
        "AVI_Width" => Some(width.to_string()),
        "AVI_Height" => Some(height.to_string()),
        "AVI_WidthAndHeight" => Some(format!("{width}x{height}")),
        "AVI_Duration" | "AVI_DurationMins" | "AVI_DurationSecs" => Some(fmt_duration(tag, secs)),
        "AVI_VideoCodec" => {
            let s = data.windows(4).position(|w| w == b"strh")?;
            data.get(s + 12..s + 16).map(|c| String::from_utf8_lossy(c).trim().to_string())
        }
        _ => None,
    }
}

fn html_title(path: &Path) -> Option<String> {
    let data = read_head(path, 64 * 1024)?;
    let s = String::from_utf8_lossy(&data);
    let re = Regex::new(r"(?is)<title[^>]*>(.*?)</title>").ok()?;
    let cap = re.captures(&s)?;
    let title = cap[1].split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then_some(title)
}

fn pdf_tag(tag: &str, path: &Path) -> Option<String> {
    let data = read_head(path, 256 * 1024)?;
    if !data.starts_with(b"%PDF-") {
        return None;
    }
    let text = String::from_utf8_lossy(&data);
    match tag {
        "Document_PageCount" => {
            let re = Regex::new(r"/Type\s*/Pages\b[^>]*?/Count\s+(\d+)").ok()?;
            re.captures(&text).map(|c| c[1].to_string())
        }
        "Document_Title" => pdf_info_field(&text, "Title"),
        "Document_Subject" => pdf_info_field(&text, "Subject"),
        "Document_Author" => pdf_info_field(&text, "Author"),
        _ => None,
    }
}

fn pdf_info_field(text: &str, key: &str) -> Option<String> {
    let pat = format!(r"/{key}\s*\(([^)]*)\)");
    let re = Regex::new(&pat).ok()?;
    let val = re.captures(text)?[1].trim().to_string();
    (!val.is_empty()).then_some(val)
}

fn font_tag(tag: &str, path: &Path) -> Option<String> {
    let data = read_head(path, 128 * 1024)?;
    if data.len() < 12 {
        return None;
    }
    let num_tables = u16::from_be_bytes([data[4], data[5]]) as usize;
    let mut name_off = None;
    for i in 0..num_tables.min(64) {
        let t = 12 + i * 16;
        if t + 16 > data.len() {
            break;
        }
        if &data[t..t + 4] == b"name" {
            name_off = Some(u32::from_be_bytes([data[t + 8], data[t + 9], data[t + 10], data[t + 11]]) as usize);
            break;
        }
    }
    let off = name_off?;
    if off + 6 > data.len() {
        return None;
    }
    let count = u16::from_be_bytes([data[off + 2], data[off + 3]]) as usize;
    let str_storage = off + u16::from_be_bytes([data[off + 4], data[off + 5]]) as usize;
    let want_id: u16 = match tag {
        "Font_FamilyName" => 1,
        "Font_Name" => 4,
        "Font_Revision" => 5,
        "Font_PostScriptName" => 6,
        _ => return None,
    };
    for i in 0..count.min(256) {
        let rec = off + 6 + i * 12;
        if rec + 12 > data.len() {
            break;
        }
        let pid = u16::from_be_bytes([data[rec], data[rec + 1]]);
        let nid = u16::from_be_bytes([data[rec + 6], data[rec + 7]]);
        if nid != want_id {
            continue;
        }
        let len = u16::from_be_bytes([data[rec + 8], data[rec + 9]]) as usize;
        let soff = str_storage + u16::from_be_bytes([data[rec + 10], data[rec + 11]]) as usize;
        let Some(slice) = data.get(soff..soff + len) else { continue };
        let val = if pid == 3 || pid == 0 {
            let u: Vec<u16> = slice.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&u)
        } else {
            String::from_utf8_lossy(slice).into_owned()
        };
        let clean = val.trim().to_string();
        if !clean.is_empty() {
            return Some(clean);
        }
    }
    None
}

fn email_tag(tag: &str, path: &Path) -> Option<String> {
    let data = read_head(path, 64 * 1024)?;
    let raw = String::from_utf8_lossy(&data);
    let mut headers: HashMap<String, String> = HashMap::new();
    let mut last_key = String::new();
    let mut body_start = raw.len();
    for (idx, line) in raw.lines().enumerate() {
        if line.is_empty() {
            body_start = idx;
            break;
        }
        if (line.starts_with(' ') || line.starts_with('\t')) && !last_key.is_empty() {
            if let Some(v) = headers.get_mut(&last_key) {
                v.push(' ');
                v.push_str(line.trim());
            }
        } else if let Some((k, v)) = line.split_once(':') {
            last_key = k.trim().to_ascii_lowercase();
            headers.insert(last_key.clone(), v.trim().to_string());
        }
    }
    let key_suffix = tag.split_once('_').map(|x| x.1).unwrap_or(tag);
    match key_suffix {
        "MessageID" => headers.get("message-id").cloned(),
        "Subject" => headers.get("subject").cloned(),
        "Sender" => headers.get("from").cloned(),
        "SenderName" => headers.get("from").map(|s| s.split('<').next().unwrap_or(s).trim_matches(|c| c == '"' || c == ' ').to_string()),
        "SenderEmail" => headers.get("from").and_then(|s| extract_email_addr(s)),
        "Recipients" => headers.get("to").cloned(),
        "Recipient" => headers.get("to").map(|s| s.split(',').next().unwrap_or(s).trim().to_string()),
        "RecipientName" => headers.get("to").map(|s| s.split(',').next().unwrap_or(s).split('<').next().unwrap_or("").trim().to_string()),
        "RecipientEmail" => headers.get("to").and_then(|s| extract_email_addr(s)),
        "DateSent" | "DateSentUTC" | "DateSentOriginal" | "DateHeader" | "DateHeaderUTC" | "DateHeaderOriginal" => headers.get("date").cloned(),
        "Body" => {
            let body: String = raw.lines().skip(body_start + 1).take(3).collect::<Vec<_>>().join(" ");
            Some(body.trim().to_string())
        }
        _ => None,
    }
}

fn extract_email_addr(s: &str) -> Option<String> {
    if let (Some(a), Some(b)) = (s.find('<'), s.find('>'))
        && b > a
    {
        return Some(s[a + 1..b].trim().to_string());
    }
    s.split_whitespace().find(|w| w.contains('@')).map(|w| w.trim_matches(',').to_string())
}

fn torrent_tag(tag: &str, path: &Path) -> Option<String> {
    let f = File::open(path).ok()?;
    let mut data = Vec::new();
    f.take(2 * 1024 * 1024).read_to_end(&mut data).ok()?;
    let (val, _, info_slice) = bencode_parse(&data, 0)?;
    let BVal::Dict(root) = val else { return None };

    let info = root.iter().find(|(k, _)| k == b"info").map(|(_, v)| v);
    let dict_str = |d: &[(Vec<u8>, BVal)], key: &[u8]| -> Option<String> {
        d.iter().find(|(k, _)| k == key).and_then(|(_, v)| match v {
            BVal::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        })
    };
    let dict_int = |d: &[(Vec<u8>, BVal)], key: &[u8]| -> Option<i64> {
        d.iter().find(|(k, _)| k == key).and_then(|(_, v)| match v {
            BVal::Int(i) => Some(*i),
            _ => None,
        })
    };

    match tag {
        "Torrent_Announce" => dict_str(&root, b"announce"),
        "Torrent_Comment" => dict_str(&root, b"comment"),
        "Torrent_Publisher" => dict_str(&root, b"created by"),
        "Torrent_CreationDateUTC" => dict_int(&root, b"creation date").and_then(|ts| DateTime::<Utc>::from_timestamp(ts, 0).map(|d| d.format("%Y-%m-%d %H-%M-%S").to_string())),
        "Torrent_CreationDateLocal" => dict_int(&root, b"creation date").and_then(|ts| {
            DateTime::<Utc>::from_timestamp(ts, 0).map(|d| d.with_timezone(&Local).format("%Y-%m-%d %H-%M-%S").to_string())
        }),
        "Torrent_Hash" => {
            use sha1::Digest as _;
            info_slice.map(|s| hex_lower(&sha1::Sha1::digest(s)))
        }
        "Torrent_Name" => info.and_then(|i| match i {
            BVal::Dict(d) => dict_str(d, b"name"),
            _ => None,
        }),
        "Torrent_CountFiles" | "Torrent_TotalSize" | "Torrent_TotalSizeBytes" | "Torrent_TotalSizeKB" | "Torrent_TotalSizeMB" | "Torrent_TotalSizeGB" => {
            let BVal::Dict(d) = info? else { return None };
            let (count, total_bytes) = if let Some((_, BVal::List(files))) = d.iter().find(|(k, _)| k == b"files") {
                let sum: u64 = files
                    .iter()
                    .filter_map(|f| match f {
                        BVal::Dict(fd) => dict_int(fd, b"length").map(|l| l.max(0) as u64),
                        _ => None,
                    })
                    .sum();
                (files.len(), sum)
            } else if let Some(len) = dict_int(d, b"length") {
                (1, len.max(0) as u64)
            } else {
                return None;
            };
            match tag {
                "Torrent_CountFiles" => Some(count.to_string()),
                "Torrent_TotalSize" => Some(format_bytes(total_bytes)),
                "Torrent_TotalSizeBytes" => Some(total_bytes.to_string()),
                "Torrent_TotalSizeKB" => Some((total_bytes / 1024).to_string()),
                "Torrent_TotalSizeMB" => Some(format!("{:.2}", total_bytes as f64 / (1024.0 * 1024.0))),
                "Torrent_TotalSizeGB" => Some(format!("{:.2}", total_bytes as f64 / (1024.0 * 1024.0 * 1024.0))),
                _ => None,
            }
        }
        _ => None,
    }
}

enum BVal {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<BVal>),
    Dict(Vec<(Vec<u8>, BVal)>),
}

fn bencode_parse<'a>(data: &'a [u8], pos: usize) -> Option<(BVal, usize, Option<&'a [u8]>)> {
    let b = *data.get(pos)?;
    match b {
        b'i' => {
            let end = data[pos + 1..].iter().position(|&c| c == b'e')? + pos + 1;
            let n = std::str::from_utf8(&data[pos + 1..end]).ok()?.parse::<i64>().ok()?;
            Some((BVal::Int(n), end + 1, None))
        }
        b'l' => {
            let mut p = pos + 1;
            let mut list = Vec::new();
            while *data.get(p)? != b'e' {
                let (v, np, _) = bencode_parse(data, p)?;
                list.push(v);
                p = np;
            }
            Some((BVal::List(list), p + 1, None))
        }
        b'd' => {
            let mut p = pos + 1;
            let mut dict = Vec::new();
            let mut info_slice = None;
            while *data.get(p)? != b'e' {
                let (BVal::Bytes(k), np, _) = bencode_parse(data, p)? else { return None };
                let val_start = np;
                let (v, vp, sub_info) = bencode_parse(data, np)?;
                if k == b"info" {
                    info_slice = Some(&data[val_start..vp]);
                } else if info_slice.is_none() {
                    info_slice = sub_info;
                }
                dict.push((k, v));
                p = vp;
            }
            Some((BVal::Dict(dict), p + 1, info_slice))
        }
        b'0'..=b'9' => {
            let colon = data[pos..].iter().position(|&c| c == b':')? + pos;
            let len = std::str::from_utf8(&data[pos..colon]).ok()?.parse::<usize>().ok()?;
            let start = colon + 1;
            let end = start + len;
            let slice = data.get(start..end)?;
            Some((BVal::Bytes(slice.to_vec()), end, None))
        }
        _ => None,
    }
}

fn version_info_tag(tag: &str, path: &Path) -> Option<String> {
    // Searches UTF-16LE StringFileInfo key pairs inside PE files (.exe/.dll).
    let f = File::open(path).ok()?;
    let mut data = Vec::new();
    f.take(1024 * 1024).read_to_end(&mut data).ok()?;
    if !data.starts_with(b"MZ") {
        return None;
    }
    let field = match tag {
        "VersionInfo_FileVersion" => "FileVersion",
        "VersionInfo_ProductVersion" => "ProductVersion",
        "VersionInfo_Product" => "ProductName",
        "VersionInfo_Company" => "CompanyName",
        "VersionInfo_Copyright" => "LegalCopyright",
        "VersionInfo_Description" => "FileDescription",
        "VersionInfo_Comments" => "Comments",
        _ => return None,
    };
    let needle: Vec<u8> = field.encode_utf16().chain(Some(0)).flat_map(|u| u.to_le_bytes()).collect();
    let pos = data.windows(needle.len()).position(|w| w == needle.as_slice())?;
    // Align to 4 bytes after key + null terminator.
    let mut val_start = pos + needle.len();
    while val_start % 4 != 0 && val_start < data.len() {
        val_start += 1;
    }
    let slice = data.get(val_start..val_start.saturating_add(256).min(data.len()))?;
    let val = utf16le(slice);
    (!val.is_empty()).then_some(val)
}

#[allow(dead_code)]
fn _epoch_unused() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
