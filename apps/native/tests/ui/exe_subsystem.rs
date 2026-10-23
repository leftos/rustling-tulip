//! The built client exe's PE subsystem.
//!
//! An installed launch (Start menu, desktop shortcut) starts the exe directly,
//! so a console-subsystem exe opens a console window beside the app, and
//! closing that window kills the client. A build without debug assertions is
//! therefore a GUI-subsystem exe; a debug build keeps the console, so a
//! terminal launch still shows the log on stderr.
#![cfg(windows)]

/// `IMAGE_SUBSYSTEM_WINDOWS_GUI`: the exe gets no console of its own.
const IMAGE_SUBSYSTEM_WINDOWS_GUI: u16 = 2;

/// `IMAGE_SUBSYSTEM_WINDOWS_CUI`: the exe gets a console window.
const IMAGE_SUBSYSTEM_WINDOWS_CUI: u16 = 3;

#[test]
fn release_exe_is_a_gui_subsystem_exe() {
    let path = env!("CARGO_BIN_EXE_rustling-tulip-native");
    let bytes = std::fs::read(path).expect("read the built client exe");

    // The PE header sits at the offset the DOS header's `e_lfanew` names.
    let lfanew = read_u32(&bytes, 0x3C).expect("a DOS header with an e_lfanew");
    let pe_offset = usize::try_from(lfanew).expect("e_lfanew fits in a usize");
    let signature = bytes.get(pe_offset..pe_offset + 4);
    assert_eq!(
        signature,
        Some(&b"PE\0\0"[..]),
        "the header at e_lfanew carries the PE signature"
    );

    // A 20-byte COFF file header follows the signature, then the optional
    // header, whose `Subsystem` sits 68 bytes into it.
    let subsystem =
        read_u16(&bytes, pe_offset + 4 + 20 + 68).expect("an optional header with a Subsystem");
    let expected = if cfg!(debug_assertions) {
        IMAGE_SUBSYSTEM_WINDOWS_CUI
    } else {
        IMAGE_SUBSYSTEM_WINDOWS_GUI
    };
    assert_eq!(
        subsystem,
        expected,
        "subsystem {subsystem} in a build whose debug assertions are {}",
        cfg!(debug_assertions)
    );
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let raw = bytes.get(offset..offset + 2)?;
    Some(u16::from_le_bytes(raw.try_into().ok()?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let raw = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes(raw.try_into().ok()?))
}
