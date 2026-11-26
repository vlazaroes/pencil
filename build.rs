//! Build-time resources for the executable, with no extra crates or SDK tools:
//!
//! - An application manifest that enables Common Controls v6, so the native
//!   settings window uses modern visual styles.
//! - A compiled resource file (.res) with the app icon (assets/pencil.ico) and the
//!   version information. Windows shows its `FileDescription` ("Pencil") in Task
//!   Manager and uses the icon for the .exe, taskbar, Start menu and Apps list.
//!   The .res is written directly in its binary format and handed to the linker.

use std::path::Path;

const ICON: &str = "assets/pencil.ico";
/// Resource id of the icon group; `LoadIcon(hinst, 1)` in the app uses it.
const ICON_ID: u16 = 1;
const LANG_EN_US: u16 = 0x0409;

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={ICON}");
    println!("cargo:rerun-if-changed=Cargo.toml");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTDEPENDENCY:type='win32' \
         name='Microsoft.Windows.Common-Controls' version='6.0.0.0' \
         processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
    );

    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("pencil.res");
    std::fs::write(&out, resources()).expect("write pencil.res");
    // link.exe accepts compiled .res files as plain inputs.
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

fn resources() -> Vec<u8> {
    let mut res = Vec::new();
    // A 32-bit .res file starts with an empty entry.
    entry(&mut res, 0, 0, 0, &[]);
    icons(&mut res);
    entry(&mut res, RT_VERSION, 1, LANG_EN_US, &version_info());
    res
}

/// Splits the .ico into one RT_ICON per image plus the RT_GROUP_ICON directory.
fn icons(res: &mut Vec<u8>) {
    let ico = std::fs::read(ICON).expect("read assets/pencil.ico");
    let u16_at = |o: usize| u16::from_le_bytes([ico[o], ico[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes([ico[o], ico[o + 1], ico[o + 2], ico[o + 3]]);
    assert!(u16_at(2) == 1, "assets/pencil.ico is not an icon file");
    let count = u16_at(4) as usize;

    let mut group = Vec::new();
    group.extend_from_slice(&[0, 0, 1, 0]); // reserved, type = icon
    group.extend_from_slice(&(count as u16).to_le_bytes());
    for i in 0..count {
        let e = 6 + i * 16;
        let (size, offset) = (u32_at(e + 8) as usize, u32_at(e + 12) as usize);
        let id = i as u16 + 1;
        entry(res, RT_ICON, id, LANG_EN_US, &ico[offset..offset + size]);
        // Same 12 leading bytes as the .ico entry, then the RT_ICON id.
        group.extend_from_slice(&ico[e..e + 12]);
        group.extend_from_slice(&id.to_le_bytes());
    }
    entry(res, RT_GROUP_ICON, ICON_ID, LANG_EN_US, &group);
}

/// One resource: RESOURCEHEADER (numeric type and name) followed by its data.
fn entry(res: &mut Vec<u8>, kind: u16, name: u16, lang: u16, data: &[u8]) {
    let mut h = Vec::new();
    h.extend_from_slice(&(data.len() as u32).to_le_bytes());
    h.extend_from_slice(&32u32.to_le_bytes()); // header size
    h.extend_from_slice(&[0xFF, 0xFF]);
    h.extend_from_slice(&kind.to_le_bytes());
    h.extend_from_slice(&[0xFF, 0xFF]);
    h.extend_from_slice(&name.to_le_bytes());
    h.extend_from_slice(&0u32.to_le_bytes()); // data version
    h.extend_from_slice(&0x1030u16.to_le_bytes()); // memory flags: moveable, pure, discardable
    h.extend_from_slice(&lang.to_le_bytes());
    h.extend_from_slice(&0u32.to_le_bytes()); // version
    h.extend_from_slice(&0u32.to_le_bytes()); // characteristics
    if kind == 0 {
        // The leading empty entry has zero type, name, flags and language.
        h = vec![0; 32];
        h[4] = 32;
        h[8..12].copy_from_slice(&[0xFF, 0xFF, 0, 0]);
        h[12..16].copy_from_slice(&[0xFF, 0xFF, 0, 0]);
    }
    res.extend_from_slice(&h);
    res.extend_from_slice(data);
    pad4(res);
}

/// VS_VERSIONINFO with the fixed version numbers and the English string table.
fn version_info() -> Vec<u8> {
    let v = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts = v.split('.').map(|p| p.parse::<u16>().unwrap_or(0));
    let (major, minor, patch) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    let ms = ((major as u32) << 16) | minor as u32;
    let ls = (patch as u32) << 16;

    let mut fixed = Vec::new();
    for value in [
        0xFEEF_04BD, // signature
        0x0001_0000, // structure version
        ms,          // file version
        ls,
        ms, // product version
        ls,
        0x3F,      // flags mask
        0,         // flags
        0x0004_0004, // VOS_NT_WINDOWS32
        1,         // VFT_APP
        0,         // subtype
        0,         // date
        0,
    ] {
        fixed.extend_from_slice(&u32::to_le_bytes(value));
    }

    let version = format!("{major}.{minor}.{patch}.0");
    let strings = [
        ("CompanyName", "vlazaroes"),
        ("FileDescription", "Pencil"),
        ("FileVersion", version.as_str()),
        ("InternalName", "pencil"),
        ("LegalCopyright", "Copyright (c) vlazaroes"),
        ("OriginalFilename", "pencil.exe"),
        ("ProductName", "Pencil"),
        ("ProductVersion", version.as_str()),
    ];
    let table: Vec<Vec<u8>> = strings.iter().map(|(k, val)| text_block(k, val)).collect();
    let string_table = block("040904b0", &[], 1, &table); // en-US, Unicode
    let string_info = block("StringFileInfo", &[], 1, &[string_table]);
    let translation = block("Translation", &[0x09, 0x04, 0xB0, 0x04], 0, &[]);
    let var_info = block("VarFileInfo", &[], 1, &[translation]);
    block("VS_VERSION_INFO", &fixed, 0, &[string_info, var_info])
}

/// A version resource node: wLength, wValueLength, wType, key, value, children.
/// `value_len` is in bytes for binary values (wType 0) and in WCHARs for text (wType 1).
fn block(key: &str, value: &[u8], kind: u16, children: &[Vec<u8>]) -> Vec<u8> {
    node(key, value, if kind == 0 { value.len() } else { value.len() / 2 }, kind, children)
}

fn text_block(key: &str, value: &str) -> Vec<u8> {
    let bytes: Vec<u8> = value.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
    node(key, &bytes, bytes.len() / 2, 1, &[])
}

fn node(key: &str, value: &[u8], value_len: usize, kind: u16, children: &[Vec<u8>]) -> Vec<u8> {
    let mut b = vec![0u8; 6]; // wLength, wValueLength, wType (filled below)
    b.extend(key.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
    pad4(&mut b);
    b.extend_from_slice(value);
    for child in children {
        pad4(&mut b);
        b.extend_from_slice(child);
    }
    let len = b.len() as u16;
    b[0..2].copy_from_slice(&len.to_le_bytes());
    b[2..4].copy_from_slice(&(value_len as u16).to_le_bytes());
    b[4..6].copy_from_slice(&kind.to_le_bytes());
    b
}

fn pad4(b: &mut Vec<u8>) {
    while !b.len().is_multiple_of(4) {
        b.push(0);
    }
}
