//! Function names for crash backtraces.
//!
//! The firmware reserves [`SAWTHAT_SYMTAB`] in flash, and `crates/symtab` fills it in the linked
//! ELF with the function symbols. The table keeps its size, so filling it moves no code. A build
//! that was not filled has an empty table, and lookups return `None`.
//!
//! Layout, little endian: the magic, the entry count, the string pool length, a checksum of the
//! entries and pool that identifies the build, `count` entries of (address, string offset), then
//! the string pool. Entries are sorted by address, and the name of
//! entry `i` ends where the name of entry `i + 1` begins.

/// Reserved size, header included
pub const SYMTAB_SIZE: usize = 256 * 1024;
pub const MAGIC: [u8; 16] = *b"SAWTHAT-SYMTAB-1";
const HEADER_LEN: usize = MAGIC.len() + 12;
const ENTRY_LEN: usize = 8;

#[repr(C, align(4))]
pub struct Symtab([u8; SYMTAB_SIZE]);

/// Filled after linking; see the module documentation
#[unsafe(no_mangle)]
#[used]
pub static SAWTHAT_SYMTAB: Symtab = Symtab({
    let mut table = [0u8; SYMTAB_SIZE];
    let mut i = 0;
    while i < MAGIC.len() {
        table[i] = MAGIC[i];
        i += 1;
    }
    table
});

fn table() -> &'static [u8] {
    // The pointer is opaque to the optimizer, so reads see the filled table rather than the
    // placeholder it was compiled with
    let base: *const u8 = core::hint::black_box((&raw const SAWTHAT_SYMTAB).cast());
    unsafe { core::slice::from_raw_parts(base, SYMTAB_SIZE) }
}

fn read_u32(table: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(table.get(at..at + 4)?.try_into().ok()?))
}

/// Identifies the build the table was filled from, or `None` if it was not filled
pub fn table_id() -> Option<u32> {
    let table = table();
    if table[..MAGIC.len()] != MAGIC || read_u32(table, MAGIC.len())? == 0 {
        return None;
    }
    read_u32(table, MAGIC.len() + 8)
}

/// Name of the function containing `pc`
pub fn lookup(pc: u32) -> Option<&'static str> {
    let table = table();
    if table[..MAGIC.len()] != MAGIC {
        return None;
    }
    let count = read_u32(table, MAGIC.len())? as usize;
    let strings_len = read_u32(table, MAGIC.len() + 4)? as usize;
    let strings_at = HEADER_LEN + count * ENTRY_LEN;
    if count == 0 || strings_at + strings_len > SYMTAB_SIZE {
        return None;
    }
    let entry = |i: usize| {
        let at = HEADER_LEN + i * ENTRY_LEN;
        Some((read_u32(table, at)?, read_u32(table, at + 4)? as usize))
    };

    // Last entry at or below `pc`
    let (mut lo, mut hi) = (0, count);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if entry(mid)?.0 <= pc {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let i = lo.checked_sub(1)?;
    let start = entry(i)?.1;
    let end = if i + 1 < count {
        entry(i + 1)?.1
    } else {
        strings_len
    };
    let name = table.get(strings_at + start..strings_at + end)?;
    core::str::from_utf8(name).ok()
}
