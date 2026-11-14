//! An NTFS volume's files: its MFT read whole into a [`Tree`], then kept current from the
//! volume's USN journal, where NTFS records every change, even those made while nothing ran.
//! Both need an administrator's access to the raw volume (`\\.\C:`): the service's.

use std::time::UNIX_EPOCH;

use ntfs_reader::{HistorySize, Journal, JournalOptions, MftScan, NextUsn, NtfsAttributeType, NtfsFile, NtfsReaderError, Reason, UsnRecord, Volume};

use crate::tree::{self, Tree};

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

fn device(letter: char) -> String {
    format!(r"\\.\{letter}:")
}

fn err(what: &str, e: NtfsReaderError) -> String {
    format!("{what}: {e}")
}

fn flags(attributes: u32, is_dir: bool) -> u8 {
    let mut f = 0;
    if is_dir {
        f |= tree::DIR;
    }
    if attributes & FILE_ATTRIBUTE_HIDDEN != 0 {
        f |= tree::HIDDEN;
    }
    if attributes & FILE_ATTRIBUTE_SYSTEM != 0 {
        f |= tree::SYSTEM;
    }
    f
}

/// Every file and folder of the volume `letter`, from its MFT. Each file is held under one
/// name (Windows' own when it has several hard links).
pub fn scan(letter: char) -> Result<Tree, String> {
    let volume = Volume::new(device(letter)).map_err(|e| err("Abrir o volume", e))?;
    let mut scan = MftScan::new(volume).map_err(|e| err("Ler a MFT", e))?;
    let mut t = Tree::new(&format!("{letter}:\\"));
    let records = scan.record_count() as usize;
    t.reserve(records, records * 20);
    while let Some(chunk) = scan.next_chunk().map_err(|e| err("Ler a MFT", e))? {
        for file in chunk.files() {
            add(&mut t, &file);
        }
    }
    t.shrink();
    Ok(t)
}

fn add(t: &mut Tree, file: &NtfsFile) {
    let Some(name) = file.best_name() else { return };
    let (mut attributes, mut modified, mut size) = (0, 0, 0);
    for a in file.attributes() {
        match a.attribute_type() {
            Some(NtfsAttributeType::StandardInformation) => {
                if let Some(si) = a.standard_information() {
                    attributes = si.file_attributes();
                    modified = si.modified().unix_timestamp().clamp(0, u32::MAX as i64) as u32;
                }
            }
            Some(NtfsAttributeType::Data) if a.name().is_none() => size = a.value_size().unwrap_or(0),
            _ => {}
        }
    }
    let is_dir = file.is_directory();
    t.set(file.number() as u32, name.parent_number() as u32, &name.to_string(), flags(attributes, is_dir), if is_dir { 0 } else { size }, modified);
}

/// The volume's journal, read from now on: opened before [`scan`], so what changes during the
/// scan is read after it. Created when the volume has none.
pub fn journal(letter: char) -> Result<Journal, String> {
    let open = || Journal::new(Volume::new(device(letter))?, JournalOptions { reason_mask: Reason::ALL, next_usn: NextUsn::Next, max_history_size: HistorySize::Limited(0) });
    match open() {
        Err(NtfsReaderError::JournalNotActive) => {
            create_journal(letter)?;
            open().map_err(|e| err("Abrir o diário USN", e))
        }
        other => other.map_err(|e| err("Abrir o diário USN", e)),
    }
}

/// Turns on the volume's USN journal, as `fsutil usn createjournal` does: 32 MB, growing by 8.
fn create_journal(letter: char) -> Result<(), String> {
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{CREATE_USN_JOURNAL_DATA, FSCTL_CREATE_USN_JOURNAL};
    use windows::core::HSTRING;
    unsafe {
        let h = CreateFileW(&HSTRING::from(device(letter)), (GENERIC_READ | GENERIC_WRITE).0, FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES(0), None).map_err(|e| format!("Abrir o volume: {e}"))?;
        let data = CREATE_USN_JOURNAL_DATA { MaximumSize: 32 << 20, AllocationDelta: 8 << 20 };
        let done = DeviceIoControl(h, FSCTL_CREATE_USN_JOURNAL, Some(&data as *const _ as *const _), size_of::<CREATE_USN_JOURNAL_DATA>() as u32, None, 0, None, None);
        let _ = CloseHandle(h);
        done.map_err(|e| format!("Criar o diário USN: {e}"))
    }
}

/// A change read from the journal, ready to apply: what the file is now, or that it's gone.
pub enum Change {
    Gone(u32),
    Now { record: u32, parent: u32, name: String, flags: u8 },
}

/// What `records` say changed. NTFS gathers everything one handle did into the record written
/// when it's closed, so only those are read.
pub fn changes(records: &[UsnRecord]) -> Vec<Change> {
    let number = |id: ntfs_reader::FileId| id.as_reference().map(|r| (r & 0xFFFF_FFFF_FFFF) as u32);
    records
        .iter()
        .filter(|r| r.reason.contains(Reason::CLOSE))
        .filter_map(|r| {
            let record = number(r.file_id)?;
            if r.reason.contains(Reason::FILE_DELETE) {
                return Some(Change::Gone(record));
            }
            let is_dir = r.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
            Some(Change::Now { record, parent: number(r.parent_id)?, name: r.name.to_string_lossy().into_owned(), flags: flags(r.file_attributes, is_dir) })
        })
        .collect()
}

/// Applies `changes` to `t`. Sizes and dates aren't in the journal: once the names and folders
/// are in (a file can sit in a folder made in the same batch), each changed file is looked at by
/// its path, without holding `t`, so searches don't wait on the disk.
pub fn apply(t: &std::sync::RwLock<Tree>, changes: Vec<Change>) {
    let mut touched = Vec::new();
    {
        let mut tree = t.write().unwrap();
        for c in changes {
            match c {
                Change::Gone(record) => tree.remove(record),
                Change::Now { record, parent, name, flags } => {
                    let (size, modified) = tree.meta(record).unwrap_or((0, 0));
                    tree.set(record, parent, &name, flags, size, modified);
                    touched.push(record);
                }
            }
        }
        tree.maybe_compact();
    }
    touched.sort_unstable();
    touched.dedup();
    let paths: Vec<(u32, String)> = {
        let tree = t.read().unwrap();
        touched.into_iter().filter_map(|r| tree.path(r).map(|p| (r, p))).collect()
    };
    let stats: Vec<(u32, u64, u32)> = paths
        .into_iter()
        .filter_map(|(r, p)| {
            let m = std::fs::symlink_metadata(&p).ok()?;
            let modified = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs().min(u32::MAX as u64) as u32);
            Some((r, if m.is_dir() { 0 } else { m.len() }, modified))
        })
        .collect();
    let mut tree = t.write().unwrap();
    for (r, size, modified) in stats {
        tree.touch(r, size, modified);
    }
}

/// The NTFS volumes on fixed disks, by letter.
pub fn volumes() -> Vec<char> {
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};
    use windows::core::HSTRING;
    const DRIVE_FIXED: u32 = 3;
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| (b'A' + i) as char)
        .filter(|l| {
            let root = HSTRING::from(format!("{l}:\\"));
            let mut fs = [0u16; 32];
            unsafe { GetDriveTypeW(&root) == DRIVE_FIXED && GetVolumeInformationW(&root, None, None, None, None, Some(&mut fs)).is_ok() && String::from_utf16_lossy(&fs).trim_end_matches('\0') == "NTFS" }
        })
        .collect()
}
