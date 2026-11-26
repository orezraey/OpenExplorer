//! Embeds `assets/icon.ico` as the Windows executable's application icon resource (ID 1).

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let ico_path = manifest_dir.join("assets").join("icon.ico");
    let ico = std::fs::read(&ico_path).expect("failed to read assets/icon.ico");
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));

    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
        let rc_path = out_dir.join("openexplorer.rc");
        let obj_path = out_dir.join("openexplorer_res.o");
        let escaped = ico_path.to_string_lossy().replace('\\', "/");
        std::fs::write(&rc_path, format!("1 ICON \"{escaped}\"\n")).expect("failed to write .rc");
        let status = std::process::Command::new("windres")
            .args([rc_path.to_str().unwrap(), "-O", "coff", "-o", obj_path.to_str().unwrap()])
            .status()
            .expect("failed to run windres");
        assert!(status.success(), "windres failed with {status}");
        println!("cargo:rustc-link-arg-bin=openexplorer={}", obj_path.display());
        return;
    }

    let res_path = out_dir.join("openexplorer.res");
    let res = build_win32_res(&ico);
    std::fs::write(&res_path, res).expect("failed to write openexplorer.res");
    println!("cargo:rustc-link-arg-bin=openexplorer={}", res_path.display());
}

fn push_res_entry(out: &mut Vec<u8>, res_type: u16, res_id: u16, memory_flags: u16, language_id: u16, data: &[u8]) {
    let data_size = u32::try_from(data.len()).expect("resource data too large");
    let header_size: u32 = 32;
    out.extend_from_slice(&data_size.to_le_bytes());
    out.extend_from_slice(&header_size.to_le_bytes());
    // Type: numeric ordinal (0xFFFF, res_type)
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.extend_from_slice(&res_type.to_le_bytes());
    // Name: numeric ordinal (0xFFFF, res_id)
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.extend_from_slice(&res_id.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    out.extend_from_slice(&memory_flags.to_le_bytes());
    out.extend_from_slice(&language_id.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // Version
    out.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
    out.extend_from_slice(data);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

fn build_win32_res(ico: &[u8]) -> Vec<u8> {
    assert!(ico.len() >= 6, "invalid .ico header");
    let reserved = u16::from_le_bytes([ico[0], ico[1]]);
    let res_type = u16::from_le_bytes([ico[2], ico[3]]);
    let count = u16::from_le_bytes([ico[4], ico[5]]);
    assert_eq!(reserved, 0, "invalid .ico reserved field");
    assert_eq!(res_type, 1, "expected ICO type 1");
    assert!(count > 0, "empty .ico");

    let dir_end = 6 + usize::from(count) * 16;
    assert!(ico.len() >= dir_end, "truncated .ico directory");

    let mut out = Vec::with_capacity(ico.len() + 256);
    // Standard 32-byte null resource header at the start of a Win32 .res file.
    push_res_entry(&mut out, 0, 0, 0, 0, &[]);

    let mut grp = Vec::with_capacity(6 + usize::from(count) * 14);
    grp.extend_from_slice(&0u16.to_le_bytes());
    grp.extend_from_slice(&1u16.to_le_bytes());
    grp.extend_from_slice(&count.to_le_bytes());

    for i in 0..usize::from(count) {
        let off = 6 + i * 16;
        let entry = &ico[off..off + 16];
        let bytes_in_res = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize;
        let image_offset = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize;
        let img = &ico[image_offset..image_offset + bytes_in_res];

        let icon_id = u16::try_from(i + 1).expect("too many icon entries");
        // RT_ICON = 3, MOVEABLE | PURE | DISCARDABLE = 0x1030, en-US = 0x0409
        push_res_entry(&mut out, 3, icon_id, 0x1030, 0x0409, img);

        grp.extend_from_slice(&entry[0..4]);
        let planes = u16::from_le_bytes([entry[4], entry[5]]);
        let bit_count = u16::from_le_bytes([entry[6], entry[7]]);
        grp.extend_from_slice(&(if planes == 0 { 1 } else { planes }).to_le_bytes());
        grp.extend_from_slice(&(if bit_count == 0 { 32 } else { bit_count }).to_le_bytes());
        grp.extend_from_slice(&entry[8..12]);
        grp.extend_from_slice(&icon_id.to_le_bytes());
    }

    // RT_GROUP_ICON = 14, resource ID = 1
    push_res_entry(&mut out, 14, 1, 0x1030, 0x0409, &grp);
    let _ = Path::new("");
    out
}
