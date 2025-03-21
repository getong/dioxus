use anyhow::{Context, Result};
use itertools::Itertools;
use memmap::{Mmap, MmapOptions};
use object::{
    macho,
    read::File,
    write::{MachOBuildVersion, StandardSection, Symbol, SymbolSection},
    Architecture, BinaryFormat, Endianness, Object, ObjectSection, ObjectSymbol, ObjectSymbolTable,
    Relocation, RelocationTarget, SectionIndex, SectionKind, SymbolKind, SymbolScope,
};
use std::io::Write;
use std::{cmp::Ordering, ffi::OsStr, fs, ops::Deref, path::PathBuf};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
};
pub use subsecond_types::*;
use target_lexicon::{OperatingSystem, Triple};
use tokio::process::Command;

/// Resolve the undefined symbols in the incrementals against the original binary, returning an object
/// file that can be linked along the incrementals.
///
/// This makes it possible to dlopen the resulting object file and use the original binary's symbols
/// bypassing the dynamic linker.
///
/// This is very similar to malware :) but it's not!
pub fn resolve_undefined(
    source: &Path,
    incrementals: &[PathBuf],
    triple: &Triple,
    aslr_reference: u64,
) -> Result<Vec<u8>> {
    let sorted: Vec<_> = incrementals.iter().sorted().collect();

    // Find all the undefined symbols in the incrementals
    let mut undefined_symbols = HashSet::new();
    let mut defined_symbols = HashSet::new();
    for path in sorted {
        let bytes = std::fs::read(&path).with_context(|| format!("failed to read {:?}", path))?;
        let file = File::parse(bytes.deref() as &[u8])?;
        for symbol in file.symbols() {
            if symbol.is_undefined() {
                undefined_symbols.insert(symbol.name()?.to_string());
            } else {
                if symbol.is_global() {
                    defined_symbols.insert(symbol.name()?.to_string());
                }
            }
        }
    }
    let undefined_symbols: Vec<_> = undefined_symbols
        .difference(&defined_symbols)
        .cloned()
        .collect();

    // Create a new object file (architecture doesn't matter much for our purposes)
    let mut obj = object::write::Object::new(
        match triple.binary_format {
            target_lexicon::BinaryFormat::Elf => object::BinaryFormat::Elf,
            target_lexicon::BinaryFormat::Macho => object::BinaryFormat::MachO,
            target_lexicon::BinaryFormat::Coff => todo!(),
            target_lexicon::BinaryFormat::Wasm => todo!(),
            target_lexicon::BinaryFormat::Xcoff => todo!(),
            _ => todo!(),
        },
        match triple.architecture {
            target_lexicon::Architecture::Aarch64(_) => object::Architecture::Aarch64,
            _ => todo!(),
        },
        match triple.endianness() {
            Ok(target_lexicon::Endianness::Little) => Endianness::Little,
            Ok(target_lexicon::Endianness::Big) => Endianness::Big,
            _ => Endianness::Little,
        },
    );

    // Write the headers so we load properly in ios/macos
    match triple.operating_system {
        target_lexicon::OperatingSystem::Darwin(_) => {
            obj.set_macho_build_version({
                let mut build_version = MachOBuildVersion::default();
                build_version.platform = macho::PLATFORM_MACOS;
                build_version.minos = (11 << 16) | (0 << 8) | 0;
                build_version.sdk = (11 << 16) | (0 << 8) | 0;
                build_version
            });
        }
        target_lexicon::OperatingSystem::IOS(_) => {
            obj.set_macho_build_version({
                let mut build_version = MachOBuildVersion::default();
                build_version.platform = match triple.environment {
                    target_lexicon::Environment::Sim => macho::PLATFORM_IOSSIMULATOR,
                    _ => macho::PLATFORM_IOS,
                };
                build_version.minos = (14 << 16) | (0 << 8) | 0; // 14.0.0
                build_version.sdk = (14 << 16) | (0 << 8) | 0; // SDK 14.0.0
                build_version
            });
        }

        _ => {}
    }

    // Load the original binary
    let bytes = std::fs::read(&source).with_context(|| format!("failed to read {:?}", source))?;
    let file = File::parse(bytes.deref() as &[u8])?;
    let symbol_table = file
        .symbols()
        .flat_map(|s| Some((s.name().ok()?, s)))
        .collect::<HashMap<_, _>>();

    let aslr_offset = aslr_reference - symbol_table.get("_aslr_reference").unwrap().address();

    // let ref_sym = symbol_table
    //     .get("_aslr_reference")
    //     .or_else(|| symbol_table.get("aslr_reference"))
    //     .expect("failed to find aslr_reference");

    // // fix the tag
    // let aslr_reference = aslr_reference;
    // // let aslr_reference = aslr_reference & 0x00FFFFFFFFFFFFFF;
    // let aslr_offset = aslr_reference - ref_sym.address();
    // tracing::error!("aslr_offset: {:#x}", aslr_offset);

    // we need to assemble a PLT/GOT so direct calls to the patch symbols work
    // for each symbol we either write the address directly (as a symbol) or create a PLT/GOT entry
    let text_section = obj.section_id(StandardSection::Text);

    for name in undefined_symbols {
        if let Some(sym) = symbol_table.get(name.as_str()) {
            let abs_addr = sym.address() + aslr_offset;

            let name_offset = match triple.operating_system {
                target_lexicon::OperatingSystem::Darwin(_) => 1,
                _ => 0,
            };
            if sym.kind() == SymbolKind::Text || sym.kind() == SymbolKind::Unknown {
                let jump_code = match triple.architecture {
                    target_lexicon::Architecture::X86_64 => {
                        // Use JMP instruction to absolute address: FF 25 followed by 32-bit offset
                        // Then the 64-bit absolute address
                        let mut code = vec![0xFF, 0x25, 0x00, 0x00, 0x00, 0x00]; // jmp [rip+0]
                                                                                 // Append the 64-bit address
                        code.extend_from_slice(&abs_addr.to_le_bytes());
                        code
                    }
                    target_lexicon::Architecture::Aarch64(_) => {
                        // For ARM64, we load the address into a register and branch
                        let mut code = Vec::new();
                        // LDR X16, [PC, #0]  ; Load from the next instruction
                        code.extend_from_slice(&[0x50, 0x00, 0x00, 0x58]);
                        // BR X16            ; Branch to the address in X16
                        code.extend_from_slice(&[0x00, 0x02, 0x1F, 0xD6]);
                        // Store the 64-bit address
                        code.extend_from_slice(&abs_addr.to_le_bytes());
                        code
                    }
                    // Add other architectures as needed
                    _ => todo!(),
                };

                // Add the jump code to the text section
                let offset = obj.append_section_data(text_section, &jump_code, 8);

                obj.add_symbol(Symbol {
                    name: name.as_bytes()[name_offset..].to_vec(),
                    value: offset,
                    size: jump_code.len() as u64,
                    scope: SymbolScope::Linkage,
                    kind: SymbolKind::Text,
                    weak: false,
                    section: SymbolSection::Section(text_section),
                    flags: object::SymbolFlags::None,
                });
            } else {
                obj.add_symbol(Symbol {
                    name: name.as_bytes()[name_offset..].to_vec(),
                    value: abs_addr,
                    size: 0,
                    scope: SymbolScope::Linkage,
                    kind: sym.kind(),
                    weak: sym.is_weak(),
                    section: SymbolSection::Absolute,
                    flags: object::SymbolFlags::None,
                });
            }
        } else {
            println!("Symbol not found: {}", name);
        }
    }

    // Write the object to a file
    let bytes = obj.write()?;
    Ok(bytes)
}

#[test]
fn test_resolve_undefined() {
    let incremental_dir = "/Users/jonkelley/Development/dioxus/packages/subsecond/data/linux/rcgu";
    let incrementals = std::fs::read_dir(incremental_dir)
        .unwrap()
        .map(|x| x.unwrap().path())
        .collect::<Vec<_>>();

    let source_file: PathBuf =
        "/Users/jonkelley/Development/dioxus/packages/subsecond/data/linux/subsecond-harness"
            .into();

    let bytes = resolve_undefined(
        &source_file,
        &incrementals,
        &"aarch64-android-linux".parse().unwrap(),
        0,
    )
    .unwrap();
}

// 03-20 23:53:24.703 20073 20095 I RustStdoutStderr: Could not find detour for.. 0x719aaa1c5c
// 03-20 23:53:24.703 20073 20095 I RustStdoutStderr: Could not find detour for.. 0x719aadf02c
// 03-20 23:53:24.703 20073 20095 I RustStdoutStderr: Could not find detour for 0x7199d4724c
// 03-20 23:53:24.704 20073 20095 I RustStdoutStderr: Could not find detour for.. 0x7199d6a258
// 03-20 23:53:24.704 20073 20095 I RustStdoutStderr: Could not find detour for.. 0x7199d6a258

#[test]
fn test_endian() {
    fix_endian();
}

fn fix_endian() {
    // let num: usize = 0x7199d6a258;

    let offset: usize = 0x719c546000;
    let notag = offset & 0x00FFFFFFFFFFFFFF;
    println!("{:#x}", notag);

    // 487992464852 aslr reference

    // let num: usize = 0x719aaa1c5c;
    // let num = u64::from_be_bytes(num.to_le_bytes());
    // println!("{num:x}");
}
