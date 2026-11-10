//! Fills the symbol table the firmware reserves (`SAWTHAT_SYMTAB`, see `symbols.rs` in the
//! firmware) with the function names of its linked ELF, so crash reports can name functions.
//!
//! Usage: `sawthat-symtab <firmware.elf>`. The ELF is rewritten in place. The table keeps its
//! size, so no address in the image changes.

use std::process::ExitCode;

use object::{Object, ObjectSection, ObjectSymbol, SymbolKind};

const TABLE_SYMBOL: &str = "SAWTHAT_SYMTAB";
const MAGIC: &[u8; 16] = b"SAWTHAT-SYMTAB-1";
const HEADER_LEN: usize = MAGIC.len() + 12;
const ENTRY_LEN: usize = 8;
/// Longest name kept, in characters; longer names keep their end
const MAX_NAME: usize = 120;

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: sawthat-symtab <firmware.elf>");
        return ExitCode::FAILURE;
    };
    match run(&path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sawthat-symtab: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(path: &str) -> Result<(), String> {
    let mut data = std::fs::read(path).map_err(|e| format!("reading {path}: {e}"))?;
    let (offset, capacity, functions) = {
        let file = object::File::parse(&*data).map_err(|e| format!("parsing {path}: {e}"))?;
        let (offset, capacity) = table_location(&file)?;
        (offset, capacity, functions(&file))
    };

    let table = &mut data[offset..offset + capacity];
    if table[..MAGIC.len()] != MAGIC[..] {
        return Err(format!("{TABLE_SYMBOL} does not start with its magic"));
    }
    let blob = encode(&functions);
    if blob.len() > capacity {
        return Err(format!(
            "{} functions need {} bytes, but {TABLE_SYMBOL} has {capacity}",
            functions.len(),
            blob.len()
        ));
    }
    table.fill(0);
    table[..blob.len()].copy_from_slice(&blob);

    std::fs::write(path, &data).map_err(|e| format!("writing {path}: {e}"))?;
    println!(
        "sawthat-symtab: {} functions, {} of {capacity} bytes",
        functions.len(),
        blob.len()
    );
    Ok(())
}

/// File offset and size of the reserved table
fn table_location(file: &object::File) -> Result<(usize, usize), String> {
    let symbol = file
        .symbols()
        .find(|s| s.name() == Ok(TABLE_SYMBOL))
        .ok_or(format!(
            "{TABLE_SYMBOL} not found; is this the firmware ELF?"
        ))?;
    let section = symbol
        .section_index()
        .and_then(|i| file.section_by_index(i).ok())
        .ok_or(format!("{TABLE_SYMBOL} has no section"))?;
    let (section_offset, _) = section
        .file_range()
        .ok_or(format!("{TABLE_SYMBOL} is not stored in the file"))?;
    let offset = section_offset + (symbol.address() - section.address());
    Ok((offset as usize, symbol.size() as usize))
}

/// Function addresses and short names, sorted and unique by address
fn functions(file: &object::File) -> Vec<(u32, String)> {
    let mut functions: Vec<(u32, String)> = file
        .symbols()
        .filter(|s| s.kind() == SymbolKind::Text && s.is_definition())
        .filter_map(|s| {
            let name = s.name().ok()?;
            Some((s.address() as u32, short_name(name)))
        })
        .collect();
    functions.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.len().cmp(&b.1.len())));
    functions.dedup_by_key(|f| f.0);
    functions
}

/// Demangled name without its hash, with turbofish generic arguments removed and nested ones
/// collapsed to `<..>`. Names still too long keep their last path segments.
fn short_name(mangled: &str) -> String {
    let demangled = format!("{:#}", rustc_demangle::demangle(mangled));
    let mut name = String::with_capacity(demangled.len());
    // Depth of the generic arguments being dropped, and of those being kept
    let (mut dropped, mut kept) = (0, 0);
    let mut prev = '\0';
    let mut rest = demangled.as_str();
    while let Some(c) = rest.chars().next() {
        rest = &rest[c.len_utf8()..];
        // `->` in a function signature is not a closing bracket
        let (open, close) = (c == '<', c == '>' && prev != '-');
        prev = c;
        if dropped > 0 {
            dropped += i32::from(open) - i32::from(close);
            if dropped == 0 && !name.ends_with("<..") {
                continue;
            }
            if dropped == 0 {
                name.push('>');
            }
            continue;
        }
        if c == ':' && rest.starts_with(":<") {
            // Turbofish: drop `::<...>` entirely
            rest = &rest[2..];
            dropped = 1;
            continue;
        }
        if open && kept >= 1 {
            // Arguments nested in a qualified path's type: keep a marker
            name.push_str("<..");
            dropped = 1;
            continue;
        }
        kept += i32::from(open) - i32::from(close);
        name.push(c);
    }

    if name.chars().count() > MAX_NAME {
        // Cut at a path separator outside generic arguments, so the rest still parses
        let mut depth = 0;
        let cut = name
            .char_indices()
            .filter_map(|(i, c)| {
                match c {
                    '<' => depth += 1,
                    '>' => depth -= 1,
                    ':' if depth == 0 && name[i..].starts_with("::") => return Some(i + 2),
                    _ => {}
                }
                None
            })
            .find(|&i| name[i..].chars().count() <= MAX_NAME);
        if let Some(i) = cut {
            name = format!("..::{}", &name[i..]);
        }
    }
    name
}

/// Table layout: magic, entry count, string pool length, checksum of the entries and pool,
/// (address, string offset) entries, then the string pool, all little endian
fn encode(functions: &[(u32, String)]) -> Vec<u8> {
    let pool_len: usize = functions.iter().map(|(_, name)| name.len()).sum();
    let mut blob = Vec::with_capacity(HEADER_LEN + functions.len() * ENTRY_LEN + pool_len);
    blob.extend_from_slice(MAGIC);
    blob.extend_from_slice(&(functions.len() as u32).to_le_bytes());
    blob.extend_from_slice(&(pool_len as u32).to_le_bytes());
    let checksum_at = blob.len();
    blob.extend_from_slice(&[0; 4]);
    let mut offset = 0u32;
    for (address, name) in functions {
        blob.extend_from_slice(&address.to_le_bytes());
        blob.extend_from_slice(&offset.to_le_bytes());
        offset += name.len() as u32;
    }
    for (_, name) in functions {
        blob.extend_from_slice(name.as_bytes());
    }
    let checksum = fnv1a(&blob[HEADER_LEN..]);
    blob[checksum_at..checksum_at + 4].copy_from_slice(&checksum.to_le_bytes());
    blob
}

/// 32-bit FNV-1a hash
fn fnv1a(data: &[u8]) -> u32 {
    data.iter().fold(0x811c_9dc5, |hash, &b| {
        (hash ^ b as u32).wrapping_mul(0x0100_0193)
    })
}
