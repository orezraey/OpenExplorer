//! Built-in self-updater reading GitHub Releases, inspired by Spotifast / fastframe-update.
//!
//! Flow:
//! - Checks `https://api.github.com/repos/orezraey/OpenExplorer/releases/latest` in the background
//!   (at most once every 24 h automatically, or on demand from Options).
//! - Verifies `checksums.txt` (and its Ed25519 signature `checksums.txt.sig` when
//!   [`PUBLISHER_KEY`] is configured).
//! - When **Update automatically** is enabled, downloads newer releases on the same major version
//!   in the background (unless the user intentionally downgraded), verifies the SHA-256, and stages
//!   them under `config_dir()/updates/`.
//! - The user can click **Restart to update** right away, or simply keep working: on the next
//!   start, [`install_kept`] applies the verified staged update before the window opens.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use openexplorer_i18n::{t, tr};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Current build version from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// GitHub repository hosting releases.
pub const REPO: &str = "orezraey/OpenExplorer";

/// Minimum interval between automatic background checks (24 hours).
pub const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Ed25519 public key (64 hex characters) used to require a signed `checksums.txt.sig`
/// on every release. Matches the private key in GitHub Secret `UPDATE_SIGNING_KEY`.
pub const PUBLISHER_KEY: Option<&str> = Some("f03cdd4d372b791c4b7f47a2ad320b813f9ca391a5fce04c19254ef4ee165e74");

/// Pending update to hand off right after `on_quit` saves caches.
static PENDING_HANDOFF: Mutex<Option<Staged>> = Mutex::new(None);

/// What [`install_kept`] found kept from an earlier run (`Some(None)`: nothing valid), for
/// [`UpdateState::new`] to take instead of hashing the file again.
static KEPT: Mutex<Option<Option<Staged>>> = Mutex::new(None);

/// How a release should be offered to an install that can update itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offer {
    /// Download in the background and install at the next start (or on "Restart to update").
    Automatic,
    /// Notify the user; only download and install when they ask.
    Manual,
}

/// How this running copy of Open Explorer was installed, which decides which release asset to
/// fetch and how to apply it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstallKind {
    /// Installed via Inno Setup (has `unins000.exe` beside `openexplorer.exe`).
    WindowsInstalled { install_dir: PathBuf },
    /// Standalone portable `.exe` on Windows.
    WindowsPortable { exe_path: PathBuf },
    /// Standalone Linux binary in a writable directory.
    LinuxBinary { exe_path: PathBuf },
    /// macOS `.app` bundle in a writable directory.
    MacAppBundle { app_dir: PathBuf },
}

/// A GitHub Release newer than [`VERSION`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub notes: String,
    pub asset_name: String,
    pub download_url: String,
    pub size: u64,
    pub sha256: Option<String>,
    /// The release's `checksums.txt` and its Ed25519 signature (hex), kept with the download so
    /// the staged file is checked against them again before it runs.
    pub signed: Option<Signed>,
}

/// A release's `checksums.txt` and the hex of its Ed25519 signature.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signed {
    pub checksums: String,
    pub signature: String,
}

/// A downloaded and SHA-256-verified update staged on disk, ready to install.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Staged {
    pub version: String,
    pub file_path: PathBuf,
    pub sha256: String,
    pub install_kind: InstallKind,
    pub release_url: String,
    /// The signed checksums it was downloaded against. `staged.json` sits in the user's own
    /// folder, so what it names is only run once this signature and the file's line in it check
    /// out again (see [`verify_staged`]).
    #[serde(default)]
    pub signed: Signed,
}

/// Download/install progress shown in the update modal and Options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadState {
    Idle,
    Downloading { received: u64, total: u64 },
    Ready(Staged),
    Installing,
    Failed(String),
}

/// Messages sent from background update threads back to `State`.
pub enum Answer {
    Checked { manual: bool, result: Result<Option<Release>, String> },
    Progress { received: u64, total: u64 },
    Downloaded(Result<Staged, String>),
}

/// Persisted updater bookkeeping stored separately from `settings.json` so checking for updates
/// never marks Options as "modified" against `Settings::default()`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct StoredState {
    newest_run: Option<String>,
    last_checked: Option<u64>,
}

/// Live updater state kept on `crate::State`.
pub struct UpdateState {
    pub checking: bool,
    pub checked_once: bool,
    pub check_error: Option<String>,
    pub release: Option<Release>,
    pub support: Result<InstallKind, String>,
    pub download: DownloadState,
    pub newest_run: Option<String>,
    pub last_checked: Option<u64>,
}

impl UpdateState {
    /// Nothing known and nothing read: a placeholder state's, and the tests'.
    pub fn idle() -> Self {
        Self {
            checking: false,
            checked_once: false,
            check_error: None,
            release: None,
            support: Err(String::new()),
            download: DownloadState::Idle,
            newest_run: None,
            last_checked: None,
        }
    }

    /// The main window's: how this copy was installed, and the update kept from an earlier run,
    /// as [`install_kept`] already checked it (its file is not hashed twice).
    pub fn new() -> Self {
        if cfg!(test) {
            return Self::idle();
        }
        let stored = load_stored_state();
        let support = detect_install();
        let mut download = DownloadState::Idle;
        let mut release = None;
        let kept = KEPT.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_else(|| load_valid_staged().ok().flatten());
        if let Some(staged) = kept
            && is_newer(VERSION, &staged.version)
        {
            release = Some(Release {
                version: staged.version.clone(),
                url: staged.release_url.clone(),
                notes: String::new(),
                asset_name: staged.file_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                download_url: String::new(),
                size: fs::metadata(&staged.file_path).map(|m| m.len()).unwrap_or(0),
                sha256: Some(staged.sha256.clone()),
                signed: Some(staged.signed.clone()),
            });
            download = DownloadState::Ready(staged);
        }
        Self {
            checking: false,
            checked_once: false,
            check_error: None,
            release,
            support,
            download,
            newest_run: stored.newest_run,
            last_checked: stored.last_checked,
        }
    }

    pub fn offer(&self, automatic: bool) -> Option<Offer> {
        let rel = self.release.as_ref()?;
        Some(offer(VERSION, &rel.version, automatic, self.newest_run.as_deref()))
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.download, DownloadState::Ready(_))
    }

    pub fn should_auto_check(&self, now_unix: u64) -> bool {
        if self.checking || self.checked_once {
            return false;
        }
        match self.last_checked {
            Some(last) => now_unix.saturating_sub(last) >= CHECK_INTERVAL_SECS,
            None => true,
        }
    }
}

/// Result of inspecting CLI flags at startup before Rune parses them.
pub struct StartupCli {
    /// Clean arguments to forward to Rune.
    pub args: Vec<String>,
    /// Receipt or error reported by the update helper on relaunch.
    pub notice: Option<Result<String, String>>,
}

/// Handles `--version` immediately (printing `openexplorer <ver>` and exiting) and strips
/// `--update-receipt` / `--update-error` flags before Rune sees them.
pub fn intercept_cli(raw_args: Vec<String>) -> StartupCli {
    if raw_args.first().is_some_and(|a| a == "--version" || a == "-V") {
        println!("openexplorer {VERSION}");
        std::process::exit(0);
    }
    let mut clean = Vec::with_capacity(raw_args.len());
    let mut notice = None;
    let mut i = 0;
    while i < raw_args.len() {
        match raw_args[i].as_str() {
            "--update-receipt" => {
                if let Some(ver) = raw_args.get(i + 1) {
                    notice = Some(Ok(ver.clone()));
                    i += 2;
                    continue;
                }
            }
            "--update-error" => {
                if let Some(err) = raw_args.get(i + 1) {
                    notice = Some(Err(err.clone()));
                    i += 2;
                    continue;
                }
            }
            _ => {}
        }
        clean.push(raw_args[i].clone());
        i += 1;
    }
    StartupCli { args: clean, notice }
}

/// Runs at startup, before the window opens:
/// 1. Records `VERSION` in `newest_run` if it is newer than previously seen.
/// 2. Cleans up leftover `.old` portable binaries and obsolete staged downloads.
/// 3. If a verified staged update is waiting and policy allows automatic installation at start,
///    hands off to the installer/helper and returns `true` (caller must exit immediately).
pub fn install_kept(automatic: bool) -> bool {
    cleanup_leftovers();
    let mut stored = load_stored_state();
    let bump = stored.newest_run.as_deref().is_none_or(|prev| is_newer(prev, VERSION));
    if bump {
        stored.newest_run = Some(VERSION.to_string());
        save_stored_state(&stored);
    }

    let staged = match load_valid_staged() {
        Ok(Some(s)) => s,
        Ok(None) => {
            *KEPT.lock().unwrap_or_else(|e| e.into_inner()) = Some(None);
            return false;
        }
        Err(e) => {
            openexplorer_gdrive::log(&format!("atualização: pacote salvo inválido: {e}"));
            clear_staged();
            *KEPT.lock().unwrap_or_else(|e| e.into_inner()) = Some(None);
            return false;
        }
    };

    if !is_newer(VERSION, &staged.version) {
        clear_staged();
        *KEPT.lock().unwrap_or_else(|e| e.into_inner()) = Some(None);
        return false;
    }
    *KEPT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Some(staged.clone()));

    if !installs_at_start(VERSION, &staged.version, automatic, stored.newest_run.as_deref()) {
        return false;
    }

    openexplorer_gdrive::log(&format!("atualização: instalando v{} baixada anteriormente", staged.version));
    match handoff(&staged) {
        Ok(()) => true,
        Err(e) => {
            openexplorer_gdrive::log(&format!("atualização: falha ao iniciar instalação de v{}: {e}", staged.version));
            false
        }
    }
}

/// Queues `staged` to be handed off right after `on_quit` saves caches.
pub fn schedule_handoff(staged: Staged) {
    *PENDING_HANDOFF.lock().unwrap_or_else(|e| e.into_inner()) = Some(staged);
}

/// Clears any scheduled handoff (e.g. if the user cancels closing while transfers run), and gives
/// back the update it held, ready again.
pub fn cancel_handoff() -> Option<Staged> {
    PENDING_HANDOFF.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Runs any scheduled handoff during `on_quit`. Returns `true` if a handoff was launched
/// (so plain `relaunch()` should be skipped).
pub fn run_pending_handoff() -> bool {
    let Some(staged) = PENDING_HANDOFF.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return false;
    };
    if let Err(e) = handoff(&staged) {
        openexplorer_gdrive::log(&format!("atualização: falha ao iniciar atualização para v{}: {e}", staged.version));
        return false;
    }
    true
}

// ---------------------------------------------------------------------------------------------
// Policy (SemVer & downgrade protection)

/// Returns `true` when `candidate` is a stable SemVer strictly greater than `current`.
pub fn is_newer(current: &str, candidate: &str) -> bool {
    let Ok(cur) = Version::parse(current.trim_start_matches('v')) else { return false };
    let Ok(cand) = Version::parse(candidate.trim_start_matches('v')) else { return false };
    if !cand.pre.is_empty() {
        return false;
    }
    cand > cur
}

/// Decides whether `candidate` should download/install automatically or wait for the user.
pub fn offer(current: &str, candidate: &str, automatic: bool, newest_run: Option<&str>) -> Offer {
    if !automatic {
        return Offer::Manual;
    }
    let Ok(cur) = Version::parse(current.trim_start_matches('v')) else { return Offer::Manual };
    let Ok(cand) = Version::parse(candidate.trim_start_matches('v')) else { return Offer::Manual };
    if cur.major != cand.major {
        return Offer::Manual;
    }
    if let Some(newest) = newest_run
        && let Ok(n) = Version::parse(newest.trim_start_matches('v'))
        && n > cur
    {
        // User intentionally downgraded from a newer version: never auto-upgrade back.
        return Offer::Manual;
    }
    Offer::Automatic
}

/// Whether a staged update should be installed automatically at startup before opening the window.
pub fn installs_at_start(current: &str, staged_version: &str, automatic: bool, newest_run: Option<&str>) -> bool {
    is_newer(current, staged_version) && offer(current, staged_version, automatic, newest_run) == Offer::Automatic
}

// ---------------------------------------------------------------------------------------------
// Installation detection & asset naming

/// Expected GitHub Release asset name for `version` and `kind`.
pub fn asset_name_for(version: &str, kind: &InstallKind) -> String {
    let v = version.trim_start_matches('v');
    match kind {
        InstallKind::WindowsInstalled { .. } => format!("OpenExplorer-{v}-windows-x64-setup.exe"),
        InstallKind::WindowsPortable { .. } => format!("OpenExplorer-{v}-windows-x64-portable.exe"),
        InstallKind::LinuxBinary { .. } => format!("OpenExplorer-{v}-linux-x64.tar.gz"),
        InstallKind::MacAppBundle { .. } => format!("OpenExplorer-{v}-macos-universal.zip"),
    }
}

/// Detects how the currently running executable was installed.
// Each system's block ends the function where it is the only one compiled.
#[allow(clippy::needless_return)]
pub fn detect_install() -> Result<InstallKind, String> {
    let exe = std::env::current_exe().map_err(|e| tr!("Não foi possível localizar o executável: {}", "Could not locate executable: {}", e))?;
    let exe = exe.canonicalize().unwrap_or(exe);

    // Development builds under Cargo's `target/` directory should never overwrite themselves.
    if is_dev_build(&exe) {
        return Err(t(
            "Esta cópia está rodando a partir de uma compilação de desenvolvimento (target/). Baixe uma release oficial no GitHub para testar a atualização automática.",
            "This copy is running from a development build (target/). Download an official release from GitHub to use self-updates.",
        )
        .into());
    }

    #[cfg(windows)]
    {
        let dir = exe.parent().ok_or_else(|| t("Pasta do executável indisponível.", "Executable folder unavailable.").to_string())?;
        if dir.join("unins000.exe").is_file() {
            return Ok(InstallKind::WindowsInstalled { install_dir: dir.to_path_buf() });
        }
        if is_dir_writable(dir) {
            return Ok(InstallKind::WindowsPortable { exe_path: exe });
        }
        return Err(t(
            "Esta cópia está em uma pasta sem permissão de escrita e sem o instalador do Open Explorer.",
            "This copy is in a read-only folder without the Open Explorer installer.",
        )
        .into());
    }

    #[cfg(target_os = "macos")]
    {
        // Expect `.../Open Explorer.app/Contents/MacOS/openexplorer`
        if let Some(macos_dir) = exe.parent()
            && macos_dir.file_name().is_some_and(|n| n == "MacOS")
            && let Some(contents) = macos_dir.parent()
            && contents.file_name().is_some_and(|n| n == "Contents")
            && let Some(app_dir) = contents.parent()
            && app_dir.extension().is_some_and(|e| e == "app")
        {
            if let Some(parent) = app_dir.parent()
                && is_dir_writable(parent)
            {
                return Ok(InstallKind::MacAppBundle { app_dir: app_dir.to_path_buf() });
            }
            return Err(t(
                "Mova o Open Explorer.app para uma pasta com permissão de escrita (como /Applications) para atualizar automaticamente.",
                "Move Open Explorer.app to a writable folder (such as /Applications) to update automatically.",
            )
            .into());
        }
        return Err(t(
            "Esta cópia não está rodando dentro de um pacote Open Explorer.app.",
            "This copy is not running inside an Open Explorer.app bundle.",
        )
        .into());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = exe.parent().ok_or_else(|| t("Pasta do executável indisponível.", "Executable folder unavailable.").to_string())?;
        if is_dir_writable(dir) {
            return Ok(InstallKind::LinuxBinary { exe_path: exe });
        }
        return Err(t(
            "O executável está em uma pasta gerenciada pelo sistema sem permissão de escrita. Atualize pelo gerenciador de pacotes ou baixe a nova versão no GitHub.",
            "The executable is in a system-managed read-only folder. Update it via your package manager or download the new release from GitHub.",
        )
        .into());
    }
}

fn is_dev_build(exe: &Path) -> bool {
    let mut comps = exe.components().peekable();
    while let Some(c) = comps.next() {
        if c.as_os_str() == "target"
            && let Some(next) = comps.peek()
        {
            let s = next.as_os_str();
            if s == "debug" || s == "release" {
                return true;
            }
        }
    }
    false
}

fn is_dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".oe-write-test-{}", std::process::id()));
    match File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------------------------
// GitHub Release check & download

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

fn http_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .build();
    config.into()
}

fn download_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_body(Some(Duration::from_secs(300)))
        .build();
    config.into()
}

/// Checks GitHub Releases for a newer version than [`VERSION`].
pub fn check_latest(kind: Option<&InstallKind>) -> Result<Option<Release>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let agent = http_agent();
    let mut resp = agent
        .get(&url)
        .header("User-Agent", &format!("OpenExplorer/{VERSION}"))
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| tr!("Não foi possível verificar atualizações: {}", "Could not check for updates: {}", e))?;

    let gh: GhRelease = resp
        .body_mut()
        .read_json()
        .map_err(|e| tr!("Resposta inválida do GitHub: {}", "Invalid response from GitHub: {}", e))?;

    if gh.draft || gh.prerelease {
        return Ok(None);
    }

    let version = gh.tag_name.trim_start_matches('v').to_string();
    if !is_newer(VERSION, &version) {
        return Ok(None);
    }

    // Fetch and optionally verify checksums.txt.
    let (checksums, signed) = fetch_verified_checksums(&agent, &gh.assets)?;

    let preferred_name = kind.map(|k| asset_name_for(&version, k)).unwrap_or_else(|| default_platform_asset(&version));
    let asset = gh
        .assets
        .iter()
        .find(|a| a.name == preferred_name)
        .or_else(|| fallback_platform_asset(&gh.assets, &version, &preferred_name));

    let (asset_name, download_url, size) = match asset {
        Some(a) => (a.name.clone(), a.browser_download_url.clone(), a.size),
        None => (preferred_name, String::new(), 0),
    };
    let sha256 = checksums.get(&asset_name).cloned();

    // If a publisher signing key is compiled in, refuse any release whose signed checksums.txt
    // does not list our target asset.
    if PUBLISHER_KEY.is_some() && !download_url.is_empty() && sha256.is_none() {
        return Err(t(
            "A atualização foi recusada porque o arquivo não consta no checksums.txt assinado.",
            "Update refused because the file is missing from the signed checksums.txt.",
        )
        .into());
    }

    Ok(Some(Release {
        version,
        url: gh.html_url,
        notes: gh.body.trim().to_string(),
        asset_name,
        download_url,
        size,
        sha256,
        signed,
    }))
}

fn default_platform_asset(version: &str) -> String {
    if cfg!(windows) {
        format!("OpenExplorer-{version}-windows-x64-setup.exe")
    } else if cfg!(target_os = "macos") {
        format!("OpenExplorer-{version}-macos-universal.zip")
    } else {
        format!("OpenExplorer-{version}-linux-x64.tar.gz")
    }
}

/// An asset of the same kind as `preferred` (its part after the version: `-windows-x64-portable.exe`,
/// `-windows-x64-setup.exe`…) whose name carries `version` another way. Never one of another
/// kind: a portable copy replaced by the installer would run the installer at every start.
fn fallback_platform_asset<'a>(assets: &'a [GhAsset], version: &str, preferred: &str) -> Option<&'a GhAsset> {
    let suffix = preferred.split_once(version).map(|(_, rest)| rest)?;
    assets.iter().find(|a| a.name.contains(version) && a.name.ends_with(suffix))
}

fn fetch_verified_checksums(agent: &ureq::Agent, assets: &[GhAsset]) -> Result<(HashMap<String, String>, Option<Signed>), String> {
    let Some(sums_asset) = assets.iter().find(|a| a.name == "checksums.txt") else {
        if PUBLISHER_KEY.is_some() {
            return Err(t(
                "Esta release não contém o arquivo checksums.txt assinado.",
                "This release does not include a signed checksums.txt file.",
            )
            .into());
        }
        return Ok((HashMap::new(), None));
    };

    let body = agent
        .get(&sums_asset.browser_download_url)
        .header("User-Agent", &format!("OpenExplorer/{VERSION}"))
        .call()
        .map_err(|e| tr!("Falha ao baixar checksums.txt: {}", "Failed to download checksums.txt: {}", e))?
        .body_mut()
        .read_to_vec()
        .map_err(|e| tr!("Falha ao ler checksums.txt: {}", "Failed to read checksums.txt: {}", e))?;

    if let Some(pubkey_hex) = PUBLISHER_KEY {
        let sig_asset = assets
            .iter()
            .find(|a| a.name == "checksums.txt.sig")
            .ok_or_else(|| t("Esta release não contém a assinatura checksums.txt.sig.", "This release is missing checksums.txt.sig.").to_string())?;
        let sig = agent
            .get(&sig_asset.browser_download_url)
            .header("User-Agent", &format!("OpenExplorer/{VERSION}"))
            .call()
            .map_err(|e| tr!("Falha ao baixar assinatura: {}", "Failed to download signature: {}", e))?
            .body_mut()
            .read_to_vec()
            .map_err(|e| tr!("Falha ao ler assinatura: {}", "Failed to read signature: {}", e))?;
        verify_signature(pubkey_hex, &body, &sig)?;
        let text = String::from_utf8(body).map_err(|_| t("checksums.txt inválido.", "Invalid checksums.txt.").to_string())?;
        let signature = if sig.len() == 64 { hex::encode(&sig) } else { String::from_utf8_lossy(&sig).trim().to_string() };
        return Ok((parse_checksums(&text), Some(Signed { checksums: text, signature })));
    }

    let text = String::from_utf8_lossy(&body);
    Ok((parse_checksums(&text), None))
}

/// Whether `staged` may run: with a publisher key, its signed `checksums.txt` must still verify,
/// name the file it points at as this release's asset for its install kind, and give that file's
/// SHA-256. `staged.json` is in the user's folder, so this, not the file, is what trusts it.
pub fn verify_staged(staged: &Staged) -> Result<(), String> {
    let Some(key) = PUBLISHER_KEY else { return Ok(()) };
    let refused = || t("A atualização salva não confere com a assinatura da release.", "The saved update doesn't match the release's signature.").to_string();
    verify_signature(key, staged.signed.checksums.as_bytes(), staged.signed.signature.as_bytes()).map_err(|_| refused())?;
    let name = staged.file_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if name != asset_name_for(&staged.version, &staged.install_kind) {
        return Err(refused());
    }
    match parse_checksums(&staged.signed.checksums).get(&name) {
        Some(sum) if sum.eq_ignore_ascii_case(&staged.sha256) => Ok(()),
        _ => Err(refused()),
    }
}

/// Parses standard `sha256sum` output (`<64-hex>  <filename>` or `<64-hex> *<filename>`).
pub fn parse_checksums(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let Some(hash) = parts.next() else { continue };
        let Some(rest) = parts.next() else { continue };
        if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            let name = rest.trim().trim_start_matches('*');
            let file_name = Path::new(name).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| name.to_string());
            if !file_name.is_empty() {
                map.insert(file_name, hash.to_ascii_lowercase());
            }
        }
    }
    map
}

/// Verifies an Ed25519 signature over `message` against `pubkey_hex` (64 hex chars).
/// Accepts either raw 64-byte signatures or ASCII hex-encoded 128-char signatures.
pub fn verify_signature(pubkey_hex: &str, message: &[u8], sig_bytes: &[u8]) -> Result<(), String> {
    let pubkey = hex::decode(pubkey_hex.trim()).map_err(|_| "Chave pública inválida.".to_string())?;
    if pubkey.len() != 32 {
        return Err("Chave pública Ed25519 deve ter 32 bytes.".into());
    }
    let raw_sig: Vec<u8> = if sig_bytes.len() == 64 {
        sig_bytes.to_vec()
    } else {
        let trimmed = std::str::from_utf8(sig_bytes).map_err(|_| "Assinatura inválida.".to_string())?.trim();
        hex::decode(trimmed).map_err(|_| "Assinatura inválida.".to_string())?
    };
    if raw_sig.len() != 64 {
        return Err("Assinatura Ed25519 deve ter 64 bytes.".into());
    }
    let key = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &pubkey);
    key.verify(message, &raw_sig).map_err(|_| {
        t(
            "A assinatura criptográfica da atualização é inválida.",
            "The cryptographic signature of the update is invalid.",
        )
        .to_string()
    })
}

/// Downloads `release` into `updates_dir()`, verifying SHA-256 and writing `staged.json`.
pub fn download_release(release: &Release, kind: &InstallKind, mut on_progress: impl FnMut(u64, u64)) -> Result<Staged, String> {
    if release.download_url.is_empty() {
        return Err(t(
            "Nenhum arquivo compatível foi encontrado nesta release.",
            "No compatible file was found in this release.",
        )
        .into());
    }
    let dir = updates_dir();
    fs::create_dir_all(&dir).map_err(|e| tr!("Não foi possível criar pasta de atualizações: {}", "Could not create updates folder: {}", e))?;

    let final_path = dir.join(&release.asset_name);
    let part_path = dir.join(format!("{}.part", release.asset_name));

    let agent = download_agent();
    let mut resp = agent
        .get(&release.download_url)
        .header("User-Agent", &format!("OpenExplorer/{VERSION}"))
        .call()
        .map_err(|e| tr!("Falha ao baixar atualização: {}", "Failed to download update: {}", e))?;

    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(release.size);

    let mut file = File::create(&part_path).map_err(|e| tr!("Não foi possível salvar arquivo: {}", "Could not save file: {}", e))?;
    let mut reader = resp.body_mut().as_reader();
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut received: u64 = 0;
    let mut last_reported: u64 = 0;

    on_progress(0, total);
    loop {
        let n = reader.read(&mut buf).map_err(|e| {
            let _ = fs::remove_file(&part_path);
            tr!("Erro durante o download: {}", "Error during download: {}", e)
        })?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| {
            let _ = fs::remove_file(&part_path);
            tr!("Erro ao gravar arquivo: {}", "Error writing file: {}", e)
        })?;
        hasher.update(&buf[..n]);
        received += n as u64;
        if received.saturating_sub(last_reported) >= 128 * 1024 || (total > 0 && received >= total) {
            last_reported = received;
            on_progress(received, total);
        }
    }
    file.flush().map_err(|e| e.to_string())?;
    drop(file);
    on_progress(received, total.max(received));

    let actual_sha = hex::encode(hasher.finalize());
    if let Some(expected) = &release.sha256
        && !actual_sha.eq_ignore_ascii_case(expected)
    {
        let _ = fs::remove_file(&part_path);
        return Err(t(
            "O arquivo baixado falhou na verificação de integridade (SHA-256).",
            "The downloaded file failed the integrity check (SHA-256).",
        )
        .into());
    }

    if final_path.exists() {
        let _ = fs::remove_file(&final_path);
    }
    fs::rename(&part_path, &final_path).map_err(|e| tr!("Não foi possível finalizar o download: {}", "Could not finalize download: {}", e))?;

    let staged = Staged {
        version: release.version.clone(),
        file_path: final_path,
        sha256: actual_sha,
        install_kind: kind.clone(),
        release_url: release.url.clone(),
        signed: release.signed.clone().unwrap_or_default(),
    };
    save_staged(&staged)?;
    Ok(staged)
}

/// Computes the lowercase hex SHA-256 of a file on disk.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f = File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

// ---------------------------------------------------------------------------------------------
// Handoff (applying the staged update per platform)

/// Spawns the platform-specific installer/swapper for `staged`. The caller must exit promptly so
/// the running executable is unlocked.
pub fn handoff(staged: &Staged) -> Result<(), String> {
    if !staged.file_path.is_file() {
        return Err(t("O arquivo da atualização não está mais disponível.", "The update file is no longer available.").into());
    }
    let actual = sha256_file(&staged.file_path)?;
    if !actual.eq_ignore_ascii_case(&staged.sha256) {
        clear_staged();
        return Err(t("O arquivo salvo da atualização está corrompido.", "The saved update file is corrupted.").into());
    }
    if let Err(e) = verify_staged(staged) {
        clear_staged();
        return Err(e);
    }

    match &staged.install_kind {
        InstallKind::WindowsInstalled { install_dir } => handoff_windows_installed(staged, install_dir),
        InstallKind::WindowsPortable { exe_path } => handoff_windows_portable(staged, exe_path),
        InstallKind::LinuxBinary { exe_path } => handoff_linux(staged, exe_path),
        InstallKind::MacAppBundle { app_dir } => handoff_macos(staged, app_dir),
    }
}

#[cfg(windows)]
fn handoff_windows_installed(staged: &Staged, install_dir: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let pid = std::process::id();
    let setup = staged.file_path.to_string_lossy();
    let exe = install_dir.join("openexplorer.exe");
    let exe_str = exe.to_string_lossy();
    let ver = &staged.version;

    // A small temporary batch script waits for our PID to exit, runs Inno Setup silently
    // (which handles UAC, stops/updates the OpenExplorerIndex service and warms the GPU cache),
    // and relaunches Open Explorer with --update-receipt on success.
    let script_path = std::env::temp_dir().join(format!("openexplorer-update-{pid}.cmd"));
    let script = format!(
        "@echo off\r\n\
         setlocal\r\n\
         :wait\r\n\
         tasklist /FI \"PID eq {pid}\" 2>NUL | find /I \"{pid}\" >NUL\r\n\
         if not errorlevel 1 (\r\n\
           timeout /t 1 /nobreak >NUL\r\n\
           goto wait\r\n\
         )\r\n\
         \"{setup}\" /SILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS\r\n\
         if errorlevel 1 (\r\n\
           start \"\" \"{exe_str}\" --update-error install-failed\r\n\
         ) else (\r\n\
           start \"\" \"{exe_str}\" --update-receipt \"{ver}\"\r\n\
         )\r\n\
         del \"%~f0\" >NUL 2>&1\r\n"
    );
    fs::write(&script_path, script).map_err(|e| e.to_string())?;
    std::process::Command::new("cmd.exe")
        .args(["/C", &script_path.to_string_lossy()])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(not(windows))]
fn handoff_windows_installed(_staged: &Staged, _install_dir: &Path) -> Result<(), String> {
    Err("Windows installer handoff is only supported on Windows.".into())
}

#[cfg(windows)]
fn handoff_windows_portable(staged: &Staged, exe_path: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let pid = std::process::id();
    let src = staged.file_path.to_string_lossy();
    let dst = exe_path.to_string_lossy();
    let old = format!("{dst}.old");
    let ver = &staged.version;

    let script_path = std::env::temp_dir().join(format!("openexplorer-update-{pid}.cmd"));
    let script = format!(
        "@echo off\r\n\
         setlocal\r\n\
         :wait\r\n\
         tasklist /FI \"PID eq {pid}\" 2>NUL | find /I \"{pid}\" >NUL\r\n\
         if not errorlevel 1 (\r\n\
           timeout /t 1 /nobreak >NUL\r\n\
           goto wait\r\n\
         )\r\n\
         del /F /Q \"{old}\" >NUL 2>&1\r\n\
         move /Y \"{dst}\" \"{old}\" >NUL\r\n\
         if errorlevel 1 (\r\n\
           start \"\" \"{dst}\" --update-error replace-failed\r\n\
           del \"%~f0\" >NUL 2>&1\r\n\
           exit /b 1\r\n\
         )\r\n\
         copy /Y \"{src}\" \"{dst}\" >NUL\r\n\
         if errorlevel 1 (\r\n\
           move /Y \"{old}\" \"{dst}\" >NUL\r\n\
           start \"\" \"{dst}\" --update-error copy-failed\r\n\
           del \"%~f0\" >NUL 2>&1\r\n\
           exit /b 1\r\n\
         )\r\n\
         del /F /Q \"{old}\" >NUL 2>&1\r\n\
         start \"\" \"{dst}\" --update-receipt \"{ver}\"\r\n\
         del \"%~f0\" >NUL 2>&1\r\n"
    );
    fs::write(&script_path, script).map_err(|e| e.to_string())?;
    std::process::Command::new("cmd.exe")
        .args(["/C", &script_path.to_string_lossy()])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(not(windows))]
fn handoff_windows_portable(_staged: &Staged, _exe_path: &Path) -> Result<(), String> {
    Err("Windows portable handoff is only supported on Windows.".into())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn handoff_linux(staged: &Staged, exe_path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let tmp_dir = updates_dir().join(format!("extract-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).map_err(|e| e.to_string())?;

    let status = std::process::Command::new("tar")
        .args(["xzf"])
        .arg(&staged.file_path)
        .arg("-C")
        .arg(&tmp_dir)
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        let _ = fs::remove_dir_all(&tmp_dir);
        return Err("Falha ao extrair o pacote .tar.gz.".into());
    }

    let extracted = tmp_dir.join("openexplorer");
    if !extracted.is_file() {
        let _ = fs::remove_dir_all(&tmp_dir);
        return Err("Executável openexplorer não encontrado no pacote.".into());
    }
    fs::set_permissions(&extracted, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;

    // Copy beside target first so rename is atomic on the same filesystem.
    let sibling = exe_path.with_extension("new");
    fs::copy(&extracted, &sibling).map_err(|e| e.to_string())?;
    fs::set_permissions(&sibling, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    fs::rename(&sibling, exe_path).map_err(|e| {
        let _ = fs::remove_file(&sibling);
        e.to_string()
    })?;
    let _ = fs::remove_dir_all(&tmp_dir);

    std::process::Command::new(exe_path)
        .args(["--update-receipt", &staged.version])
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn handoff_linux(_staged: &Staged, _exe_path: &Path) -> Result<(), String> {
    Err("Linux handoff is only supported on Linux.".into())
}

#[cfg(target_os = "macos")]
fn handoff_macos(staged: &Staged, app_dir: &Path) -> Result<(), String> {
    let pid = std::process::id();
    let tmp_dir = updates_dir().join(format!("extract-{pid}"));
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).map_err(|e| e.to_string())?;

    let status = std::process::Command::new("ditto")
        .args(["-x", "-k"])
        .arg(&staged.file_path)
        .arg(&tmp_dir)
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        let _ = fs::remove_dir_all(&tmp_dir);
        return Err("Falha ao extrair o pacote .zip.".into());
    }

    let new_app = tmp_dir.join("Open Explorer.app");
    if !new_app.is_dir() {
        let _ = fs::remove_dir_all(&tmp_dir);
        return Err("Open Explorer.app não encontrado no pacote.".into());
    }

    let script = r#"
pid="$1"
new_app="$2"
target_app="$3"
tmp_dir="$4"
ver="$5"
while kill -0 "$pid" 2>/dev/null; do sleep 0.2; done
backup="${target_app}.old"
rm -rf "$backup"
if mv "$target_app" "$backup"; then
  if mv "$new_app" "$target_app"; then
    rm -rf "$backup" "$tmp_dir"
    open -a "$target_app" --args --update-receipt "$ver"
    exit 0
  else
    mv "$backup" "$target_app"
  fi
fi
rm -rf "$tmp_dir"
open -a "$target_app" --args --update-error replace-failed
"#;
    std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .arg("openexplorer-updater")
        .arg(pid.to_string())
        .arg(&new_app)
        .arg(app_dir)
        .arg(&tmp_dir)
        .arg(&staged.version)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn handoff_macos(_staged: &Staged, _app_dir: &Path) -> Result<(), String> {
    Err("macOS handoff is only supported on macOS.".into())
}

/// The message for an error code the update helpers pass back with `--update-error` (ASCII, as
/// the Windows script runs in the console's code page), in the app's language.
pub fn error_text(code: &str) -> String {
    match code {
        "install-failed" => t("A instalação da atualização foi cancelada ou falhou.", "The update's installation was canceled or failed.").into(),
        "replace-failed" => t("Não foi possível substituir o Open Explorer pela nova versão.", "Open Explorer couldn't be replaced with the new version.").into(),
        "copy-failed" => t("Não foi possível copiar a nova versão do Open Explorer.", "The new version of Open Explorer couldn't be copied.").into(),
        other => tr!("A atualização falhou: {other}", "The update failed: {other}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Persistence helpers

pub fn updates_dir() -> PathBuf {
    openexplorer_gdrive::config_dir().join("updates")
}

fn state_file() -> PathBuf {
    updates_dir().join("state.json")
}

fn staged_file() -> PathBuf {
    updates_dir().join("staged.json")
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn load_stored_state() -> StoredState {
    fs::read(state_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_stored_state(state: &StoredState) {
    let Ok(bytes) = serde_json::to_vec_pretty(state) else { return };
    let _ = fs::create_dir_all(updates_dir());
    let _ = fs::write(state_file(), bytes);
}

pub fn record_check(now: u64) {
    let mut stored = load_stored_state();
    stored.last_checked = Some(now);
    save_stored_state(&stored);
}

fn save_staged(staged: &Staged) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(staged).map_err(|e| e.to_string())?;
    fs::create_dir_all(updates_dir()).map_err(|e| e.to_string())?;
    fs::write(staged_file(), bytes).map_err(|e| e.to_string())
}

fn load_valid_staged() -> Result<Option<Staged>, String> {
    let Ok(bytes) = fs::read(staged_file()) else { return Ok(None) };
    let staged: Staged = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if !staged.file_path.is_file() {
        return Ok(None);
    }
    let actual = sha256_file(&staged.file_path)?;
    if !actual.eq_ignore_ascii_case(&staged.sha256) {
        return Err("SHA-256 mismatch on staged update".into());
    }
    verify_staged(&staged)?;
    Ok(Some(staged))
}

pub fn clear_staged() {
    if let Ok(bytes) = fs::read(staged_file())
        && let Ok(staged) = serde_json::from_slice::<Staged>(&bytes)
    {
        let _ = fs::remove_file(&staged.file_path);
    }
    let _ = fs::remove_file(staged_file());
}

fn cleanup_leftovers() {
    if let Ok(exe) = std::env::current_exe() {
        let old = PathBuf::from(format!("{}.old", exe.to_string_lossy()));
        if old.is_file() {
            let _ = fs::remove_file(old);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_comparison_and_policy_match_spotifast() {
        assert!(is_newer("0.4.0", "0.4.1"));
        assert!(is_newer("0.4.0", "v0.5.0"));
        assert!(!is_newer("0.4.0", "0.4.0"));
        assert!(!is_newer("0.4.0", "0.3.9"));
        assert!(!is_newer("0.4.0", "0.5.0-rc.1"));

        // Same major, automatic on, no downgrade -> Automatic.
        assert_eq!(offer("0.4.0", "0.5.0", true, Some("0.4.0")), Offer::Automatic);
        assert!(installs_at_start("0.4.0", "0.5.0", true, Some("0.4.0")));

        // Automatic off -> Manual.
        assert_eq!(offer("0.4.0", "0.5.0", false, Some("0.4.0")), Offer::Manual);
        assert!(!installs_at_start("0.4.0", "0.5.0", false, Some("0.4.0")));

        // Major bump -> Manual.
        assert_eq!(offer("0.4.0", "1.0.0", true, Some("0.4.0")), Offer::Manual);

        // User intentionally downgraded from 0.5.0 -> never auto-upgrade back.
        assert_eq!(offer("0.4.0", "0.5.0", true, Some("0.5.0")), Offer::Manual);
        assert!(!installs_at_start("0.4.0", "0.5.0", true, Some("0.5.0")));
    }

    #[test]
    fn checksums_parser_handles_standard_and_binary_lines() {
        let h1 = "a".repeat(64);
        let h2 = "B".repeat(64);
        let text = format!(
            "# comment\n\
             {h1}  OpenExplorer-0.5.0-windows-x64-setup.exe\n\
             {h2} *dist/OpenExplorer-0.5.0-linux-x64.tar.gz\n\
             badline\n"
        );
        let map = parse_checksums(&text);
        assert_eq!(map.get("OpenExplorer-0.5.0-windows-x64-setup.exe").map(String::as_str), Some(h1.as_str()));
        assert_eq!(map.get("OpenExplorer-0.5.0-linux-x64.tar.gz").map(String::as_str), Some(h2.to_ascii_lowercase().as_str()));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn ed25519_signature_verification_works_for_raw_and_hex() {
        use ring::rand::SystemRandom;
        use ring::signature::{Ed25519KeyPair, KeyPair};

        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let pub_hex = hex::encode(pair.public_key().as_ref());

        let msg = b"deadbeef  OpenExplorer-0.5.0-windows-x64-setup.exe\n";
        let sig = pair.sign(msg);
        let sig_hex = format!("{}\n", hex::encode(sig.as_ref()));

        assert!(verify_signature(&pub_hex, msg, sig.as_ref()).is_ok());
        assert!(verify_signature(&pub_hex, msg, sig_hex.as_bytes()).is_ok());
        assert!(verify_signature(&pub_hex, b"tampered", sig.as_ref()).is_err());
    }

    /// The release workflow signs with `openssl pkeyutl -sign -rawin` and the Ed25519 key in
    /// UPDATE_SIGNING_KEY: a signature made that way with that key, over a sample checksums.txt,
    /// verifies against the key the app carries.
    #[test]
    fn the_release_workflows_signature_verifies() {
        let sums = b"deadbeef  OpenExplorer-test.exe
";
        let sig = hex::decode("079916e5b2df0309e44bd6eb92a7107b9aae3d3fb88f1d6bbd365bb9210f25567d48f49dadc6cb0dfcf2e9a875a8045ee4633b872f3f913bb8fe6ea29bf60009").unwrap();
        let key = PUBLISHER_KEY.unwrap();
        assert!(verify_signature(key, sums, &sig).is_ok());
        assert!(verify_signature(key, b"deadbeef  OpenExplorer-other.exe
", &sig).is_err());
    }

    #[test]
    fn publisher_key_is_a_valid_ed25519_public_key() {
        let key = PUBLISHER_KEY.expect("release signing public key must be configured");
        let bytes = hex::decode(key).expect("publisher key must be valid hex");
        assert_eq!(bytes.len(), 32);
    }

    #[test]
    fn cli_interception_strips_update_flags_and_keeps_others() {
        let cli = intercept_cli(vec![
            "--update-receipt".into(),
            "0.5.0".into(),
            "--snap".into(),
            "out.png".into(),
        ]);
        assert_eq!(cli.notice, Some(Ok("0.5.0".into())));
        assert_eq!(cli.args, vec!["--snap", "out.png"]);

        let err = intercept_cli(vec!["--update-error".into(), "falhou".into()]);
        assert_eq!(err.notice, Some(Err("falhou".into())));
        assert!(err.args.is_empty());
    }

    /// A staged update whose signed checksums don't verify, or name another file or kind, is
    /// refused: `staged.json` alone can't choose what runs.
    #[test]
    fn a_staged_update_needs_the_release_signature() {
        let sum = "a".repeat(64);
        let staged = Staged {
            version: "9.0.0".into(),
            file_path: PathBuf::from("OpenExplorer-9.0.0-windows-x64-setup.exe"),
            sha256: sum.clone(),
            install_kind: InstallKind::WindowsInstalled { install_dir: PathBuf::new() },
            release_url: String::new(),
            signed: Signed { checksums: format!("{sum}  OpenExplorer-9.0.0-windows-x64-setup.exe
"), signature: "00".repeat(64) },
        };
        assert!(verify_staged(&staged).is_err());
        assert!(verify_staged(&Staged { signed: Signed::default(), ..staged }).is_err());
    }

    /// The fallback asset is of the same kind: never the installer for a portable copy.
    #[test]
    fn the_fallback_asset_keeps_the_install_kind() {
        let asset = |name: &str| GhAsset { name: name.into(), browser_download_url: String::new(), size: 0 };
        let assets = [asset("OpenExplorer-0.5.0-windows-x64-setup.exe"), asset("OpenExplorer-0.5.0-build1-windows-x64-portable.exe")];
        let portable = asset_name_for("0.5.0", &InstallKind::WindowsPortable { exe_path: PathBuf::new() });
        assert_eq!(fallback_platform_asset(&assets, "0.5.0", &portable).map(|a| a.name.as_str()), Some("OpenExplorer-0.5.0-build1-windows-x64-portable.exe"));
        assert!(fallback_platform_asset(&assets[..1], "0.5.0", &portable).is_none());
    }

    #[test]
    fn helper_error_codes_read_in_the_apps_language() {
        assert_eq!(error_text("install-failed"), "A instalação da atualização foi cancelada ou falhou.");
        assert_eq!(error_text("xyz"), "A atualização falhou: xyz");
    }

    #[test]
    fn asset_names_match_release_workflow() {
        assert_eq!(
            asset_name_for("0.5.0", &InstallKind::WindowsInstalled { install_dir: PathBuf::new() }),
            "OpenExplorer-0.5.0-windows-x64-setup.exe"
        );
        assert_eq!(
            asset_name_for("v0.5.0", &InstallKind::WindowsPortable { exe_path: PathBuf::new() }),
            "OpenExplorer-0.5.0-windows-x64-portable.exe"
        );
        assert_eq!(
            asset_name_for("0.5.0", &InstallKind::LinuxBinary { exe_path: PathBuf::new() }),
            "OpenExplorer-0.5.0-linux-x64.tar.gz"
        );
        assert_eq!(
            asset_name_for("0.5.0", &InstallKind::MacAppBundle { app_dir: PathBuf::new() }),
            "OpenExplorer-0.5.0-macos-universal.zip"
        );
    }
}
