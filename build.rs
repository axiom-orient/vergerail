#![allow(missing_docs)]

use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let supported_target = target_os == "macos" && target_arch == "aarch64";
    let clang_target = match (target_os.as_str(), target_arch.as_str()) {
        ("macos", "aarch64") => Some("arm64-apple-macosx"),
        ("macos", "x86_64") => Some("x86_64-apple-macosx"),
        _ => None,
    };
    let source = if supported_target {
        PathBuf::from("src/native/vergerail_guardian.c")
    } else {
        PathBuf::from("src/native/vergerail_guardian_stub.c")
    };
    println!("cargo:rerun-if-changed=src/native/vergerail_guardian.c");
    println!("cargo:rerun-if-changed=src/native/vergerail_guardian_stub.c");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let object = out_dir.join("vergerail_guardian.o");
    let binary = out_dir.join("vergerail-guardian");
    // The guardian is a security boundary.  Do not inherit CFLAGS, CPPFLAGS,
    // or compiler extra arguments from the caller's environment: an injected
    // define or linker flag must not change the embedded production helper.
    // /usr/bin/clang is the supported macOS toolchain used for this artifact.
    const CLANG: &str = "/usr/bin/clang";

    let mut compile = Command::new(CLANG);
    compile
        .args([
            "-std=c11",
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fno-common",
        ])
        .args(["-ffunction-sections", "-fdata-sections", "-fPIC"])
        .arg("-c")
        .arg(&source)
        .arg("-o")
        .arg(&object);
    if let Some(target) = clang_target {
        compile.arg(format!("--target={target}"));
        compile.arg("-mmacosx-version-min=26.5");
    }
    run(&mut compile, "compile macOS guardian");

    let mut link = Command::new(CLANG);
    link.args(["-O2", "-ffunction-sections", "-fdata-sections", "-fPIC"]);
    if let Some(target) = clang_target {
        link.arg(format!("--target={target}"));
        link.arg("-mmacosx-version-min=26.5");
    }
    link.arg(&object).arg("-o").arg(&binary);
    run(&mut link, "link macOS guardian");

    if supported_target {
        let mut strip = Command::new("strip");
        strip.args(["-S"]).arg(&binary);
        run(&mut strip, "strip guardian symbols");
        normalize_macho_uuid(&binary).expect("normalize guardian Mach-O UUID");
        let mut sign = Command::new("codesign");
        sign.args([
            "--force",
            "--sign",
            "-",
            "--timestamp=none",
            "--identifier=com.axiom-orient.vergerail.guardian",
        ])
        .arg(&binary);
        run(&mut sign, "apply deterministic ad-hoc guardian signature");
    }

    let bytes = fs::read(&binary).expect("guardian binary was produced");
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    let digest = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    println!(
        "cargo:rustc-env=VERGERAIL_GUARDIAN_PATH={}",
        binary.display()
    );
    println!("cargo:rustc-env=VERGERAIL_GUARDIAN_SHA256={digest}");
}

fn normalize_macho_uuid(path: &PathBuf) -> std::io::Result<()> {
    const MACH_HEADER_64: u32 = 0xfeed_facf;
    const LC_UUID: u32 = 0x1b;
    const UUID_OFFSET: usize = 8;
    const LOAD_COMMANDS_OFFSET: usize = 32;
    const LOAD_COMMAND_HEADER_BYTES: usize = 8;
    const FIXED_UUID: [u8; 16] = [
        0x56, 0x45, 0x52, 0x47, 0x45, 0x52, 0x41, 0x49, 0x4c, 0x2d, 0x47, 0x55, 0x41, 0x52, 0x44,
        0x31,
    ];

    let mut bytes = fs::read(path)?;
    if bytes.len() < LOAD_COMMANDS_OFFSET {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "guardian Mach-O header is truncated",
        ));
    }
    fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
        bytes
            .get(offset..offset + 4)
            .map(|raw| u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    if read_u32(&bytes, 0) != Some(MACH_HEADER_64) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "guardian is not a little-endian 64-bit Mach-O",
        ));
    }
    let command_count = read_u32(&bytes, 16).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "guardian Mach-O command count is missing",
        )
    })? as usize;
    let mut offset = LOAD_COMMANDS_OFFSET;
    let mut found = false;
    for _ in 0..command_count {
        let command = read_u32(&bytes, offset).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "guardian Mach-O load command is truncated",
            )
        })?;
        let command_size = read_u32(&bytes, offset + 4).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "guardian Mach-O load command size is missing",
            )
        })? as usize;
        if command_size < LOAD_COMMAND_HEADER_BYTES || offset + command_size > bytes.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "guardian Mach-O load command has an invalid size",
            ));
        }
        if command == LC_UUID {
            if command_size < UUID_OFFSET + FIXED_UUID.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "guardian Mach-O UUID command is truncated",
                ));
            }
            bytes[offset + UUID_OFFSET..offset + UUID_OFFSET + FIXED_UUID.len()]
                .copy_from_slice(&FIXED_UUID);
            found = true;
        }
        offset += command_size;
    }
    if !found {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "guardian Mach-O UUID command is missing",
        ));
    }
    fs::write(path, bytes)
}

fn run(command: &mut Command, operation: &str) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("{operation} failed to start: {error}"));
    assert!(status.success(), "{operation} failed with status {status}");
}
