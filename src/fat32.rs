// fat32.rs — Bare-metal FAT32 filesystem parser
// Reads the MBR, BPB, FAT table, and directory entries from the SD card.
// Supports: listing root directory, reading files by name (8.3 format).
//
// Layout on SD card:
//   Sector 0:       MBR (partition table)
//   Sector N:       Volume Boot Record (VBR) with BPB
//   Sector N+R:     FAT table(s)
//   Sector N+R+F:   Root directory cluster
//   Sector N+R+F+D: Data clusters
//
// IMPORTANT: fat32_read_file() compares raw 8.3 directory bytes directly.
// Do NOT pass a human-readable name — pass the exact 11-byte FAT32 8.3 name
// as stored on disk (space-padded, uppercase, no dot).
// Example: "VMLINUZ RPI" -> b"VMLINUZ RPI" (8 name + 3 ext, spaces as padding)

use crate::emmc::{sd_read_block};

// ─── FAT32 on-disk structures ─────────────────────────────────────────────────

/// MBR Partition Entry (16 bytes at offset 0x1BE, 0x1CE, 0x1DE, 0x1EE)
#[repr(C, packed)]
struct MbrPartition {
    status:     u8,
    chs_first:  [u8; 3],
    part_type:  u8,
    chs_last:   [u8; 3],
    lba_start:  u32,
    lba_size:   u32,
}

/// FAT32 BIOS Parameter Block (BPB) — starts at byte 11 of the VBR
#[repr(C, packed)]
struct Bpb {
    bytes_per_sector:       u16,  // almost always 512
    sectors_per_cluster:    u8,
    reserved_sectors:       u16,  // sectors before first FAT
    num_fats:               u8,   // usually 2
    root_entry_count:       u16,  // 0 for FAT32
    total_sectors_16:       u16,  // 0 for FAT32
    media_type:             u8,
    fat_size_16:            u16,  // 0 for FAT32
    sectors_per_track:      u16,
    num_heads:              u16,
    hidden_sectors:         u32,
    total_sectors_32:       u32,
    // FAT32 extended BPB
    fat_size_32:            u32,  // sectors per FAT
    ext_flags:              u16,
    fs_version:             u16,
    root_cluster:           u32,  // cluster number of root directory
    fs_info:                u16,
    backup_boot_sector:     u16,
    _reserved:              [u8; 12],
    drive_number:           u8,
    _reserved2:             u8,
    boot_signature:         u8,
    volume_id:              u32,
    volume_label:           [u8; 11],
    fs_type:                [u8; 8],  // "FAT32   "
}

const ATTR_DIRECTORY:   u8 = 0x10;
const ATTR_VOLUME_ID:   u8 = 0x08;
const ATTR_LFN:         u8 = 0x0F; // Long File Name entry (skip)

// ─── Global FAT32 state ───────────────────────────────────────────────────────

static mut FAT32_PARTITION_LBA: u32 = 0;
static mut FAT32_FAT_LBA:       u32 = 0;
static mut FAT32_DATA_LBA:      u32 = 0;
static mut FAT32_ROOT_CLUSTER:  u32 = 0;
static mut FAT32_SEC_PER_CLUS:  u32 = 0;
static mut FAT32_READY:         bool = false;

// ─── Sector buffer (static, no heap needed) ──────────────────────────────────
static mut SECTOR_BUF: [u8; 512] = [0u8; 512];

fn read_sector(lba: u32) -> bool {
    unsafe { sd_read_block(lba, &mut SECTOR_BUF) }
}

fn buf_u16(offset: usize) -> u16 {
    unsafe {
        (SECTOR_BUF[offset] as u16) | ((SECTOR_BUF[offset + 1] as u16) << 8)
    }
}

fn buf_u32(offset: usize) -> u32 {
    unsafe {
        (SECTOR_BUF[offset] as u32)
            | ((SECTOR_BUF[offset + 1] as u32) << 8)
            | ((SECTOR_BUF[offset + 2] as u32) << 16)
            | ((SECTOR_BUF[offset + 3] as u32) << 24)
    }
}

// ─── FAT32 cluster → LBA conversion ──────────────────────────────────────────

fn cluster_to_lba(cluster: u32) -> u32 {
    unsafe { FAT32_DATA_LBA + (cluster - 2) * FAT32_SEC_PER_CLUS }
}

/// Follow the FAT chain to get the next cluster number.
fn fat_next_cluster(cluster: u32) -> u32 {
    let fat_lba = unsafe { FAT32_FAT_LBA };
    let fat_offset = cluster * 4; // each FAT32 entry is 4 bytes
    let sector = fat_lba + fat_offset / 512;
    let offset = (fat_offset % 512) as usize;
    if !read_sector(sector) { return 0x0FFF_FFFF; }
    buf_u32(offset) & 0x0FFF_FFFF // mask top 4 bits (reserved)
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Initialize the FAT32 filesystem by reading the MBR and BPB.
/// Prints detailed diagnostics at every step.
/// Returns `true` on success.
pub fn fat32_init() -> bool {
    // Read MBR (sector 0)
    if !read_sector(0) {
        crate::kprintln!("[fat32] FAIL: sd_read_block(0) failed");
        return false;
    }

    // Check MBR signature
    let mbr_sig = buf_u16(510);
    crate::kprintln!("[fat32] MBR sig: 0x{:04X}", mbr_sig);
    if mbr_sig != 0xAA55 {
        crate::kprintln!("[fat32] FAIL: bad MBR signature (expected 0xAA55)");
        return false;
    }

    // Read first partition entry (at offset 0x1BE)
    let part_type   = unsafe { SECTOR_BUF[0x1BE + 4] };
    let partition_lba = buf_u32(0x1BE + 8);
    crate::kprintln!("[fat32] Part type: 0x{:02X}  LBA: {}", part_type, partition_lba);

    // Accept any FAT partition type — don't reject unknown types
    // (Pi SD cards can have 0x0B, 0x0C, 0x0E, 0x06, or even 0x0A)
    // Only reject if it's clearly not a FAT partition (e.g. Linux ext4 = 0x83)
    if part_type == 0x00 {
        crate::kprintln!("[fat32] FAIL: partition type 0x00 (empty)");
        return false;
    }
    if part_type == 0x82 || part_type == 0x83 || part_type == 0x8E {
        crate::kprintln!("[fat32] FAIL: partition type 0x{:02X} is Linux, not FAT", part_type);
        return false;
    }

    unsafe { FAT32_PARTITION_LBA = partition_lba; }

    // Read Volume Boot Record (VBR) = first sector of the partition
    if !read_sector(partition_lba) {
        crate::kprintln!("[fat32] FAIL: sd_read_block({}) failed", partition_lba);
        return false;
    }

    // Check VBR signature
    let vbr_sig = buf_u16(510);
    crate::kprintln!("[fat32] VBR sig: 0x{:04X}", vbr_sig);
    if vbr_sig != 0xAA55 {
        crate::kprintln!("[fat32] FAIL: bad VBR signature");
        return false;
    }

    // Parse BPB fields (all little-endian)
    let bytes_per_sector    = buf_u16(11) as u32;
    let sectors_per_cluster = unsafe { SECTOR_BUF[13] } as u32;
    let reserved_sectors    = buf_u16(14) as u32;
    let num_fats            = unsafe { SECTOR_BUF[16] } as u32;
    let fat_size_32         = buf_u32(36);
    let root_cluster        = buf_u32(44);

    crate::kprintln!("[fat32] bps={} spc={} res={} nfat={} fatsz={} rootclus={}",
        bytes_per_sector, sectors_per_cluster, reserved_sectors,
        num_fats, fat_size_32, root_cluster);

    if bytes_per_sector != 512 {
        crate::kprintln!("[fat32] FAIL: bytes_per_sector={} (need 512)", bytes_per_sector);
        return false;
    }
    if sectors_per_cluster == 0 {
        crate::kprintln!("[fat32] FAIL: sectors_per_cluster=0");
        return false;
    }

    let fat_lba  = partition_lba + reserved_sectors;
    let data_lba = fat_lba + num_fats * fat_size_32;

    crate::kprintln!("[fat32] fat_lba={} data_lba={}", fat_lba, data_lba);

    unsafe {
        FAT32_FAT_LBA      = fat_lba;
        FAT32_DATA_LBA     = data_lba;
        FAT32_ROOT_CLUSTER = root_cluster;
        FAT32_SEC_PER_CLUS = sectors_per_cluster;
        FAT32_READY        = true;
    }

    true
}

/// Iterate over raw directory entries in the root directory.
/// Calls `callback` with the raw 11-byte 8.3 name, cluster, size, and attr.
/// The 11-byte name is exactly as stored on disk (space-padded, uppercase, no dot).
fn iter_root_dir<F: FnMut(&[u8; 11], u32, u32, u8)>(mut callback: F) {
    if !unsafe { FAT32_READY } { return; }

    let mut cluster = unsafe { FAT32_ROOT_CLUSTER };

    'outer: loop {
        let lba = cluster_to_lba(cluster);
        let spc = unsafe { FAT32_SEC_PER_CLUS };

        for s in 0..spc {
            if !read_sector(lba + s) { break 'outer; }

            for entry_idx in 0..16usize { // 16 entries per 512-byte sector
                let offset = entry_idx * 32;
                let first_byte = unsafe { SECTOR_BUF[offset] };

                if first_byte == 0x00 { break 'outer; } // no more entries
                if first_byte == 0xE5 { continue; }     // deleted entry

                let attr = unsafe { SECTOR_BUF[offset + 11] };
                if attr == ATTR_LFN { continue; }       // skip LFN entries
                if attr & ATTR_VOLUME_ID != 0 { continue; } // skip volume label

                // Extract raw 11-byte 8.3 name exactly as stored on disk
                let mut name83 = [b' '; 11];
                for i in 0..11 {
                    name83[i] = unsafe { SECTOR_BUF[offset + i] };
                }

                let cluster_hi = buf_u16(offset + 20) as u32;
                let cluster_lo = buf_u16(offset + 26) as u32;
                let file_cluster = (cluster_hi << 16) | cluster_lo;
                let file_size = buf_u32(offset + 28);

                callback(&name83, file_cluster, file_size, attr);
            }
        }

        // Follow FAT chain to next cluster
        let next = fat_next_cluster(cluster);
        if next >= 0x0FFF_FFF8 { break; } // end of chain
        cluster = next;
    }
}

/// A simple file info structure returned by directory listing.
pub struct FileInfo {
    pub name: [u8; 12], // "FILENAME.EXT\0"
    pub size: u32,
    pub is_dir: bool,
    pub cluster: u32,
}

/// List up to `max` entries in the root directory.
/// Calls `callback` for each valid entry.
pub fn fat32_list_root<F: FnMut(&FileInfo)>(mut callback: F) {
    iter_root_dir(|name83, cluster, size, attr| {
        // Build human-readable name from raw 8.3 bytes
        let mut name = [0u8; 12];
        let mut ni = 0usize;
        for i in 0..8 {
            if name83[i] == b' ' { break; }
            name[ni] = name83[i];
            ni += 1;
        }
        let has_ext = name83[8] != b' ';
        if has_ext {
            name[ni] = b'.';
            ni += 1;
            for i in 8..11 {
                if name83[i] == b' ' { break; }
                name[ni] = name83[i];
                ni += 1;
            }
        }

        let info = FileInfo {
            name,
            size,
            is_dir: attr & ATTR_DIRECTORY != 0,
            cluster,
        };
        callback(&info);
    });
}

/// Print all files in the root directory to UART/framebuffer for diagnostics.
/// Shows the raw 11-byte 8.3 name as hex so we can see exactly what's stored.
pub fn fat32_list_files_debug() {
    use core::fmt::Write;
    crate::kprintln!("[fat32] Root directory listing:");
    let mut count = 0u32;
    iter_root_dir(|name83, _cluster, size, attr| {
        count += 1;
        // Print as ASCII (show spaces as '_')
        let _ = write!(crate::uart::UartWriter, "[fat32]   [");
        let _ = write!(crate::framebuffer::FbWriter, "[fat32]   [");
        for i in 0..11 {
            let c = if name83[i] == b' ' { b'_' } else { name83[i] };
            let _ = write!(crate::uart::UartWriter, "{}", c as char);
            let _ = write!(crate::framebuffer::FbWriter, "{}", c as char);
        }
        let is_dir = attr & ATTR_DIRECTORY != 0;
        crate::kprintln!("] {} {} bytes", if is_dir { "<DIR>" } else { "     " }, size);
    });
    if count == 0 {
        crate::kprintln!("[fat32]   (no entries found)");
    }
    crate::kprintln!("[fat32] {} entries total", count);
}

/// Read a file from the root directory by its raw 11-byte FAT32 8.3 name.
///
/// `name83` must be exactly 11 bytes, space-padded, uppercase, no dot.
/// This is compared DIRECTLY against the raw bytes in the directory entry.
///
/// Examples:
///   "VMLINUZ RPI" -> b"VMLINUZ RPI"  (vmlinuz.rpi on disk)
///   "INITRD  GZ " -> b"INITRD  GZ "  (initrd.gz on disk)
///   "BCM2711 DTB" -> b"BCM2711 DTB"  (bcm2711.dtb on disk)
///
/// Calls `callback` with each 512-byte chunk and the actual byte count.
/// Returns `true` if the file was found and read successfully.
pub fn fat32_read_file<F: FnMut(&[u8], usize)>(
    name83: &[u8; 11],
    mut callback: F
) -> bool {
    if !unsafe { FAT32_READY } { return false; }

    let mut found_cluster = 0u32;
    let mut found_size = 0u32;
    let mut found = false;

    // Direct raw byte comparison — no string conversion
    iter_root_dir(|entry_name83, cluster, size, _attr| {
        if found { return; }
        if entry_name83 == name83 {
            found_cluster = cluster;
            found_size = size;
            found = true;
        }
    });

    if !found { return false; }

    // Read the file cluster by cluster
    let spc = unsafe { FAT32_SEC_PER_CLUS };
    let mut remaining = found_size as usize;
    let mut cluster = found_cluster;

    loop {
        let lba = cluster_to_lba(cluster);
        for s in 0..spc {
            if remaining == 0 { return true; }
            if !read_sector(lba + s) { return false; }
            let chunk_size = remaining.min(512);
            let chunk = unsafe { &SECTOR_BUF[..chunk_size] };
            callback(chunk, chunk_size);
            remaining = remaining.saturating_sub(512);
        }

        let next = fat_next_cluster(cluster);
        if next >= 0x0FFF_FFF8 { break; }
        cluster = next;
    }

    true
}
