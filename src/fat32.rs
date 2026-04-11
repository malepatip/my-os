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

/// FAT32 Directory Entry (32 bytes)
#[repr(C, packed)]
struct DirEntry {
    name:       [u8; 8],   // 8.3 filename, space-padded
    ext:        [u8; 3],   // extension, space-padded
    attr:       u8,        // file attributes
    _reserved:  u8,
    crt_time_tenth: u8,
    crt_time:   u16,
    crt_date:   u16,
    acc_date:   u16,
    cluster_hi: u16,       // high 16 bits of first cluster
    wrt_time:   u16,
    wrt_date:   u16,
    cluster_lo: u16,       // low 16 bits of first cluster
    file_size:  u32,
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
/// Returns `true` on success.
pub fn fat32_init() -> bool {
    // Read MBR (sector 0)
    if !read_sector(0) { return false; }

    // Check MBR signature
    if buf_u16(510) != 0xAA55 { return false; }

    // Read first partition entry (at offset 0x1BE)
    let part_type = unsafe { SECTOR_BUF[0x1BE + 4] };
    // FAT32 partition types: 0x0B (FAT32 CHS), 0x0C (FAT32 LBA)
    if part_type != 0x0B && part_type != 0x0C { return false; }

    let partition_lba = buf_u32(0x1BE + 8);
    unsafe { FAT32_PARTITION_LBA = partition_lba; }

    // Read Volume Boot Record (VBR) = first sector of the partition
    if !read_sector(partition_lba) { return false; }

    // Check VBR signature
    if buf_u16(510) != 0xAA55 { return false; }

    // Parse BPB fields (all little-endian)
    let bytes_per_sector    = buf_u16(11) as u32;
    let sectors_per_cluster = unsafe { SECTOR_BUF[13] } as u32;
    let reserved_sectors    = buf_u16(14) as u32;
    let num_fats            = unsafe { SECTOR_BUF[16] } as u32;
    let fat_size_32         = buf_u32(36);
    let root_cluster        = buf_u32(44);

    if bytes_per_sector != 512 { return false; } // we only support 512-byte sectors

    let fat_lba  = partition_lba + reserved_sectors;
    let data_lba = fat_lba + num_fats * fat_size_32;

    unsafe {
        FAT32_FAT_LBA      = fat_lba;
        FAT32_DATA_LBA     = data_lba;
        FAT32_ROOT_CLUSTER = root_cluster;
        FAT32_SEC_PER_CLUS = sectors_per_cluster;
        FAT32_READY        = true;
    }

    true
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

                // Build 8.3 filename string
                let mut name = [0u8; 12];
                let mut ni = 0usize;
                for i in 0..8 {
                    let c = unsafe { SECTOR_BUF[offset + i] };
                    if c == b' ' { break; }
                    name[ni] = c;
                    ni += 1;
                }
                let has_ext = unsafe { SECTOR_BUF[offset + 8] != b' ' };
                if has_ext {
                    name[ni] = b'.';
                    ni += 1;
                    for i in 8..11 {
                        let c = unsafe { SECTOR_BUF[offset + i] };
                        if c == b' ' { break; }
                        name[ni] = c;
                        ni += 1;
                    }
                }

                let cluster_hi = buf_u16(offset + 20) as u32;
                let cluster_lo = buf_u16(offset + 26) as u32;
                let file_cluster = (cluster_hi << 16) | cluster_lo;
                let file_size = buf_u32(offset + 28);

                let info = FileInfo {
                    name,
                    size: file_size,
                    is_dir: attr & ATTR_DIRECTORY != 0,
                    cluster: file_cluster,
                };
                callback(&info);
            }
        }

        // Follow FAT chain to next cluster
        let next = fat_next_cluster(cluster);
        if next >= 0x0FFF_FFF8 { break; } // end of chain
        cluster = next;
    }
}

/// Read a file from the root directory by name (8.3 format, e.g. "MODEL   BIN").
/// Calls `callback` with each 512-byte chunk and the actual byte count in that chunk.
/// Returns `true` if the file was found and read successfully.
pub fn fat32_read_file<F: FnMut(&[u8], usize)>(
    name83: &[u8; 11],
    mut callback: F
) -> bool {
    if !unsafe { FAT32_READY } { return false; }

    // Find the file in the root directory
    let mut found_cluster = 0u32;
    let mut found_size = 0u32;
    let mut found = false;

    fat32_list_root(|info| {
        if found { return; }
        // Compare 8.3 name (first 11 bytes of info.name vs name83)
        let mut match_name = [b' '; 11];
        let mut ni = 0;
        let mut dot_seen = false;
        for &c in info.name.iter() {
            if c == 0 { break; }
            if c == b'.' { dot_seen = true; ni = 8; continue; }
            if ni < 11 { match_name[ni] = c.to_ascii_uppercase(); ni += 1; }
            if !dot_seen && ni == 8 { ni = 8; }
        }
        if !dot_seen { /* extension stays as spaces */ }

        if &match_name == name83 {
            found_cluster = info.cluster;
            found_size = info.size;
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
