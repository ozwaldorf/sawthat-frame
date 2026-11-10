//! Sets the volume label of a FAT12/16/32 filesystem on a raw block device.
//!
//! The label is stored in the boot sector and in a volume label entry in the root directory;
//! hosts read the root directory entry first, so both are written.

use embedded_sdmmc::{Block, BlockDevice, BlockIdx};

/// Directory entry size and attributes
const DIR_ENTRY: usize = 32;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_LONG_NAME: u8 = 0x0f;
const ENTRY_FREE: u8 = 0xe5;
const ENTRY_END: u8 = 0x00;
/// Extended boot signature, present when the boot sector has a label field
const BOOT_SIG: u8 = 0x29;
/// MBR partition types for FAT12/16/32
const FAT_TYPES: [u8; 7] = [0x01, 0x04, 0x06, 0x0b, 0x0c, 0x0e, 0x1b];

/// Why the label could not be set
#[derive(Debug)]
pub enum LabelError {
    Device,
    /// No FAT filesystem found
    NotFat,
}

fn u16_at(b: &Block, at: usize) -> u32 {
    u16::from_le_bytes([b.contents[at], b.contents[at + 1]]) as u32
}

fn u32_at(b: &Block, at: usize) -> u32 {
    u32::from_le_bytes(b.contents[at..at + 4].try_into().unwrap())
}

fn is_boot_sector(b: &Block) -> bool {
    matches!(b.contents[0], 0xeb | 0xe9) && b.contents[510..512] == [0x55, 0xaa]
}

fn read<D: BlockDevice>(device: &D, index: u32) -> Result<Block, LabelError> {
    let mut block = Block::new();
    device
        .read(core::slice::from_mut(&mut block), BlockIdx(index))
        .map_err(|_| LabelError::Device)?;
    Ok(block)
}

fn write<D: BlockDevice>(device: &D, index: u32, block: &Block) -> Result<(), LabelError> {
    device
        .write(core::slice::from_ref(block), BlockIdx(index))
        .map_err(|_| LabelError::Device)
}

/// Set the label of the first FAT partition (or an unpartitioned FAT card) to `label`.
///
/// Returns whether anything was written.
pub fn set_label<D: BlockDevice>(device: &D, label: &[u8; 11]) -> Result<bool, LabelError> {
    // Unpartitioned card, or the first partition of an MBR
    let mbr = read(device, 0)?;
    let start = if is_boot_sector(&mbr) && u16_at(&mbr, 11) == 512 {
        0
    } else if mbr.contents[510..512] == [0x55, 0xaa] && FAT_TYPES.contains(&mbr.contents[450]) {
        u32_at(&mbr, 454)
    } else {
        return Err(LabelError::NotFat);
    };

    let mut boot = read(device, start)?;
    if !is_boot_sector(&boot) || u16_at(&boot, 11) != 512 {
        return Err(LabelError::NotFat);
    }
    let sectors_per_cluster = boot.contents[13] as u32;
    let reserved = u16_at(&boot, 14);
    let fats = boot.contents[16] as u32;
    let root_entries = u16_at(&boot, 17);
    let fat16_size = u16_at(&boot, 22);
    let fat32 = fat16_size == 0;
    let fat_size = if fat32 { u32_at(&boot, 36) } else { fat16_size };
    if sectors_per_cluster == 0 || fats == 0 || fat_size == 0 {
        return Err(LabelError::NotFat);
    }

    let mut changed = false;

    // Boot sector label field
    let (sig_at, label_at) = if fat32 { (66, 71) } else { (38, 43) };
    if boot.contents[sig_at] == BOOT_SIG && boot.contents[label_at..label_at + 11] != label[..] {
        boot.contents[label_at..label_at + 11].copy_from_slice(label);
        write(device, start, &boot)?;
        changed = true;
    }

    // Root directory: the fixed region on FAT12/16, the first root cluster on FAT32
    let fat_end = start + reserved + fats * fat_size;
    let (root, root_blocks) = if fat32 {
        let data = fat_end;
        let cluster = u32_at(&boot, 44);
        (
            data + (cluster - 2) * sectors_per_cluster,
            sectors_per_cluster,
        )
    } else {
        (
            fat_end,
            (root_entries * DIR_ENTRY as u32).div_ceil(Block::LEN_U32),
        )
    };

    // Replace the existing label entry, or use the first free entry
    let mut free: Option<(u32, usize)> = None;
    for index in root..root + root_blocks {
        let mut block = read(device, index)?;
        for at in (0..Block::LEN).step_by(DIR_ENTRY) {
            let entry = &block.contents[at..at + DIR_ENTRY];
            match entry[0] {
                ENTRY_END => {
                    free.get_or_insert((index, at));
                    return finish(device, free, label, changed);
                }
                ENTRY_FREE => {
                    free.get_or_insert((index, at));
                }
                _ if entry[11] & ATTR_LONG_NAME != ATTR_LONG_NAME
                    && entry[11] & ATTR_VOLUME_ID != 0 =>
                {
                    if entry[..11] != label[..] {
                        block.contents[at..at + 11].copy_from_slice(label);
                        write(device, index, &block)?;
                        changed = true;
                    }
                    return Ok(changed);
                }
                _ => {}
            }
        }
    }
    finish(device, free, label, changed)
}

/// Write a new label entry into the free slot, if any
fn finish<D: BlockDevice>(
    device: &D,
    free: Option<(u32, usize)>,
    label: &[u8; 11],
    changed: bool,
) -> Result<bool, LabelError> {
    let Some((index, at)) = free else {
        return Ok(changed);
    };
    let mut block = read(device, index)?;
    let entry = &mut block.contents[at..at + DIR_ENTRY];
    entry.fill(0);
    entry[..11].copy_from_slice(label);
    entry[11] = ATTR_VOLUME_ID;
    write(device, index, &block)?;
    Ok(true)
}
