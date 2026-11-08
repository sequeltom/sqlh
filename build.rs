//! The version sqlh reports (decision 44, DESIGN.md §4), the Windows version resource that
//! Explorer's Properties > Details shows (file and product version, name, description and
//! copyright), and the exe's icon, `assets/sqlh.ico` (drawn by `scripts/make-icon.py`).
//!
//! The version is `SQLH_VERSION` when the release workflow sets it (`0.1.0`, or
//! `0.1.0-dev.<run>`), otherwise `Cargo.toml`'s with `-local`, so a build made anywhere else
//! never passes for a release. It reaches the code as `SQLH_BUILT_VERSION`.
//!
//! The resource is written here as a compiled .res file, which the MSVC linker takes as an
//! input like an object file, so neither the resource compiler (rc.exe) nor a crate is needed.
//! Formats: RESOURCEHEADER and VS_VERSIONINFO, with VS_FIXEDFILEINFO, StringFileInfo and
//! VarFileInfo (Win32 documentation, "Resource file formats" and "Version information"); the
//! icon as RT_ICON images and the RT_GROUP_ICON directory that names them ("Icons").

use std::env;
use std::path::PathBuf;

#[path = "src/build_version.rs"]
mod build_version;

/// The text fields, in the US English, Unicode string table (`040904B0`). The version fields
/// are added from the build.
const STRINGS: [(&str, &str); 6] = [
    ("CompanyName", "Tom Willwerth"),
    ("FileDescription", "sqlh - SQL Server query tool for agents"),
    ("InternalName", "sqlh"),
    ("LegalCopyright", "Copyright (c) 2026 Tom Willwerth"),
    ("OriginalFilename", "sqlh.exe"),
    ("ProductName", "sqlh"),
];

fn main() {
    // The version is checked whole before anything is printed (R47): a line break in it would
    // end Cargo's directive, and the code would report a version the resource doesn't hold.
    let cargo = env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let version = match env::var("SQLH_VERSION") {
        // The release workflow's: Cargo.toml's version, or it with a pre-release part.
        Ok(given) => match build_version::checked(&given, &cargo) {
            Ok(()) => given,
            Err(why) => panic!("{why}"),
        },
        Err(env::VarError::NotPresent) => format!("{cargo}-local"),
        Err(env::VarError::NotUnicode(_)) => panic!("SQLH_VERSION isn't valid Unicode"),
    };
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/build_version.rs");
    println!("cargo:rerun-if-changed=assets/sqlh.ico");
    println!("cargo:rerun-if-env-changed=SQLH_VERSION");
    println!("cargo:rerun-if-env-changed=SQLH_BUILD");
    println!("cargo:rustc-env=SQLH_BUILT_VERSION={version}");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }
    // Windows keeps four 16-bit numbers: major.minor.patch, then the release workflow's run
    // number (SQLH_BUILD), which only grows, so a release built after its pre-releases is
    // newer than them too; 0 for any other build.
    let part = |name: &str, value: String| -> u16 {
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name}={value} isn't a number from 0 to 65535"))
    };
    let numbers = [
        part(
            "major",
            env::var("CARGO_PKG_VERSION_MAJOR").unwrap_or_default(),
        ),
        part(
            "minor",
            env::var("CARGO_PKG_VERSION_MINOR").unwrap_or_default(),
        ),
        part(
            "patch",
            env::var("CARGO_PKG_VERSION_PATCH").unwrap_or_default(),
        ),
        match env::var("SQLH_BUILD") {
            Ok(build) => part("SQLH_BUILD", build),
            Err(_) => 0,
        },
    ];
    let ico = std::fs::read("assets/sqlh.ico").expect("assets/sqlh.ico is read");
    let res = resource_file(&icon_resources(&ico), &version_info(&version, numbers));
    let path = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("sqlh.res");
    std::fs::write(&path, res).expect("the version resource is written");
    println!("cargo:rustc-link-arg-bins={}", path.display());
}

/// VS_VERSIONINFO: the fixed numbers, then the strings and the language they are in.
fn version_info(version: &str, numbers: [u16; 4]) -> Vec<u8> {
    let [major, minor, patch, build] = numbers.map(u32::from);
    let ms = (major << 16) | minor;
    let ls = (patch << 16) | build;
    // VS_FF_PRERELEASE for a version with a pre-release part (-dev.N, -local).
    let flags: u32 = if version.contains('-') { 0x2 } else { 0 };
    let fixed: Vec<u8> = [
        0xFEEF_04BD, // signature
        0x0001_0000, // structure version 1.0
        ms,          // file version
        ls,
        ms, // product version
        ls,
        0x3F,        // the flags that are valid
        flags,       //
        0x0004_0004, // VOS_NT_WINDOWS32
        0x1,         // VFT_APP
        0,           // subtype
        0,           // date
        0,
    ]
    .iter()
    .flat_map(|n: &u32| n.to_le_bytes())
    .collect();

    let mut strings: Vec<Vec<u8>> = STRINGS.iter().map(|(k, v)| string(k, v)).collect();
    strings.push(string("FileVersion", version));
    strings.push(string("ProductVersion", version));
    let table = block("040904B0", 1, 0, &[], &strings);
    let string_file_info = block("StringFileInfo", 1, 0, &[], &[table]);
    // US English (0x0409) in Unicode (1200, 0x04B0).
    let translation = block("Translation", 0, 4, &[0x09, 0x04, 0xB0, 0x04], &[]);
    let var_file_info = block("VarFileInfo", 1, 0, &[], &[translation]);
    block(
        "VS_VERSION_INFO",
        0,
        52,
        &fixed,
        &[string_file_info, var_file_info],
    )
}

/// A String: its value in UTF-16 with its terminator, its length counted in characters.
fn string(key: &str, value: &str) -> Vec<u8> {
    let text = utf16z(value);
    let characters = u16::try_from(text.len() / 2).expect("a short value");
    block(key, 1, characters, &text, &[])
}

/// One block of the version tree: `wLength`, `wValueLength`, `wType`, the key in UTF-16 with
/// its terminator, then the value and each child, each starting on a 4-byte boundary.
/// `wLength` covers the block up to the end of its last child.
fn block(key: &str, kind: u16, value_length: u16, value: &[u8], children: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![0, 0];
    out.extend(value_length.to_le_bytes());
    out.extend(kind.to_le_bytes());
    out.extend(utf16z(key));
    align(&mut out);
    out.extend(value);
    for child in children {
        align(&mut out);
        out.extend(child);
    }
    let length = u16::try_from(out.len()).expect("a version block under 64 KB");
    out[..2].copy_from_slice(&length.to_le_bytes());
    out
}

/// An icon's resources from its .ico file: each image as an RT_ICON (3), named 1 up, and the
/// RT_GROUP_ICON (14) directory, the .ico's own with each image's file offset replaced by the
/// name of its RT_ICON. The .ico is ours and committed, so a malformed one ends the build.
fn icon_resources(ico: &[u8]) -> Vec<(u16, u16, u16, Vec<u8>)> {
    let u16_at = |at: usize| u16::from_le_bytes([ico[at], ico[at + 1]]);
    let u32_at = |at: usize| u32::from_le_bytes([ico[at], ico[at + 1], ico[at + 2], ico[at + 3]]);
    assert!(
        ico.len() >= 6 && u16_at(0) == 0 && u16_at(2) == 1,
        "assets/sqlh.ico isn't an icon file"
    );
    let count = u16_at(4);
    let mut group = ico[..6].to_vec();
    let mut resources = Vec::new();
    for n in 0..count {
        let entry = 6 + 16 * usize::from(n);
        assert!(ico.len() >= entry + 16, "assets/sqlh.ico is cut short");
        let size = u32_at(entry + 8) as usize;
        let offset = u32_at(entry + 12) as usize;
        let image = ico
            .get(offset..offset + size)
            .expect("assets/sqlh.ico's images are within it");
        let name = n + 1;
        // GRPICONDIRENTRY: the ICONDIRENTRY's first 12 bytes, then the RT_ICON's name.
        group.extend(&ico[entry..entry + 12]);
        group.extend(name.to_le_bytes());
        // MOVEABLE | DISCARDABLE, as rc.exe marks an icon image.
        resources.push((3, name, 0x1010, image.to_vec()));
    }
    // MOVEABLE | PURE | DISCARDABLE, as rc.exe marks an icon directory.
    resources.push((14, 1, 0x1030, group));
    resources
}

/// A .res file: the empty entry every 32-bit .res file starts with, the icon's resources, then
/// the version resource (type RT_VERSION 16, name 1), each in US English, its data padded to 4
/// bytes.
fn resource_file(icon: &[(u16, u16, u16, Vec<u8>)], version: &[u8]) -> Vec<u8> {
    let header = |size: u32, kind: u16, name: u16, flags: u16, language: u16| {
        let mut h = Vec::with_capacity(32);
        h.extend(size.to_le_bytes()); // DataSize
        h.extend(32u32.to_le_bytes()); // HeaderSize
        h.extend(
            [0xFFFF, kind, 0xFFFF, name]
                .iter()
                .flat_map(|n: &u16| n.to_le_bytes()),
        );
        h.extend(0u32.to_le_bytes()); // DataVersion
        h.extend(flags.to_le_bytes()); // MemoryFlags
        h.extend(language.to_le_bytes()); // LanguageId
        h.extend(0u32.to_le_bytes()); // Version
        h.extend(0u32.to_le_bytes()); // Characteristics
        h
    };
    let mut out = header(0, 0, 0, 0, 0);
    let mut add = |kind: u16, name: u16, flags: u16, data: &[u8]| {
        let size = u32::try_from(data.len()).expect("a small resource");
        out.extend(header(size, kind, name, flags, 0x0409));
        out.extend(data);
        align(&mut out);
    };
    for (kind, name, flags, data) in icon {
        add(*kind, *name, *flags, data);
    }
    // MOVEABLE | PURE, as rc.exe marks a version resource.
    add(16, 1, 0x0030, version);
    out
}

fn utf16z(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn align(bytes: &mut Vec<u8>) {
    while !bytes.len().is_multiple_of(4) {
        bytes.push(0);
    }
}
