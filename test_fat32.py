#!/usr/bin/env python3
"""
test_fat32.py — Simulates the exact FAT32 parsing logic from fat32.rs
Run against the SD card image to see what the kernel actually sees.

Usage:
    python3 test_fat32.py /tmp/sdcard.img
    python3 test_fat32.py /dev/sdb   (if you have direct SD card access)

This mirrors fat32.rs exactly:
  - Reads MBR sector 0, checks 0xAA55 signature
  - Checks partition type byte (0x0B, 0x0C, 0x0E, 0x06)
  - Reads VBR, parses BPB
  - Walks root directory cluster chain
  - Prints raw 11-byte 8.3 names as the kernel sees them
  - Tests whether "VMLINUZ RPI", "INITRD  GZ ", "BCM2711 DTB" match
"""

import sys
import struct

def read_sector(f, lba):
    f.seek(lba * 512)
    return f.read(512)

def buf_u16(sector, offset):
    return struct.unpack_from('<H', sector, offset)[0]

def buf_u32(sector, offset):
    return struct.unpack_from('<I', sector, offset)[0]

def fat32_test(image_path):
    print(f"=== FAT32 Kernel Simulator ===")
    print(f"Image: {image_path}")
    print()

    with open(image_path, 'rb') as f:
        # ── Step 1: Read MBR ──────────────────────────────────────────────
        mbr = read_sector(f, 0)
        sig = buf_u16(mbr, 510)
        print(f"[MBR] Signature: 0x{sig:04X} {'OK' if sig == 0xAA55 else 'FAIL - expected 0xAA55'}")
        if sig != 0xAA55:
            return

        part_type = mbr[0x1BE + 4]
        partition_lba = buf_u32(mbr, 0x1BE + 8)
        partition_size = buf_u32(mbr, 0x1BE + 12)
        print(f"[MBR] Partition type: 0x{part_type:02X}", end=" ")
        if part_type in (0x0B, 0x0C, 0x0E, 0x06):
            print("OK (FAT32/FAT16)")
        else:
            print(f"FAIL - kernel rejects this type!")
            print(f"       Kernel only accepts: 0x0B, 0x0C, 0x0E, 0x06")
            print(f"       This will cause fat32_init() to return false!")
            return
        print(f"[MBR] Partition LBA start: {partition_lba} (byte offset: {partition_lba*512})")
        print(f"[MBR] Partition size: {partition_size} sectors ({partition_size*512//1024//1024} MB)")
        print()

        # ── Step 2: Read VBR (Volume Boot Record) ────────────────────────
        vbr = read_sector(f, partition_lba)
        sig2 = buf_u16(vbr, 510)
        print(f"[VBR] Signature: 0x{sig2:04X} {'OK' if sig2 == 0xAA55 else 'FAIL'}")
        if sig2 != 0xAA55:
            return

        # Parse BPB
        bytes_per_sector    = buf_u16(vbr, 11)
        sectors_per_cluster = vbr[13]
        reserved_sectors    = buf_u16(vbr, 14)
        num_fats            = vbr[16]
        fat_size_32         = buf_u32(vbr, 36)
        root_cluster        = buf_u32(vbr, 44)
        fs_type             = vbr[82:90].decode('ascii', errors='replace').strip()

        print(f"[BPB] Bytes/sector:      {bytes_per_sector} {'OK' if bytes_per_sector==512 else 'FAIL - kernel requires 512!'}")
        print(f"[BPB] Sectors/cluster:   {sectors_per_cluster}")
        print(f"[BPB] Reserved sectors:  {reserved_sectors}")
        print(f"[BPB] Num FATs:          {num_fats}")
        print(f"[BPB] FAT size (sectors):{fat_size_32}")
        print(f"[BPB] Root cluster:      {root_cluster}")
        print(f"[BPB] FS type string:    '{fs_type}'")

        if bytes_per_sector != 512:
            print("FAIL: kernel requires 512 bytes/sector")
            return

        fat_lba  = partition_lba + reserved_sectors
        data_lba = fat_lba + num_fats * fat_size_32

        print(f"[BPB] FAT LBA:           {fat_lba}")
        print(f"[BPB] Data LBA:          {data_lba}")
        print()

        # ── Step 3: Walk root directory ───────────────────────────────────
        ATTR_DIRECTORY = 0x10
        ATTR_VOLUME_ID = 0x08
        ATTR_LFN       = 0x0F

        def cluster_to_lba(cluster):
            return data_lba + (cluster - 2) * sectors_per_cluster

        def fat_next_cluster(cluster):
            fat_offset = cluster * 4
            sector_lba = fat_lba + fat_offset // 512
            offset_in_sector = fat_offset % 512
            fat_sector = read_sector(f, sector_lba)
            val = buf_u32(fat_sector, offset_in_sector)
            return val & 0x0FFFFFFF

        print("[DIR] Root directory entries (raw 8.3 names as kernel sees them):")
        print(f"      {'Raw 8.3 (11 bytes)':15s}  {'Hex':33s}  {'Size':10s}  {'Type'}")
        print(f"      {'-'*15}  {'-'*33}  {'-'*10}  {'-'*10}")

        entries = []
        cluster = root_cluster
        done = False

        while not done:
            lba = cluster_to_lba(cluster)
            for s in range(sectors_per_cluster):
                if done:
                    break
                sector = read_sector(f, lba + s)
                for entry_idx in range(16):
                    offset = entry_idx * 32
                    first_byte = sector[offset]

                    if first_byte == 0x00:
                        done = True
                        break
                    if first_byte == 0xE5:
                        continue  # deleted

                    attr = sector[offset + 11]
                    if attr == ATTR_LFN:
                        continue  # LFN entry
                    if attr & ATTR_VOLUME_ID:
                        continue  # volume label

                    # Raw 11-byte 8.3 name
                    name83 = bytes(sector[offset:offset+11])
                    cluster_hi = buf_u16(sector, offset + 20)
                    cluster_lo = buf_u16(sector, offset + 26)
                    file_cluster = (cluster_hi << 16) | cluster_lo
                    file_size = buf_u32(sector, offset + 28)
                    is_dir = bool(attr & ATTR_DIRECTORY)

                    # Display: show spaces as '_' for clarity
                    display = ''.join(chr(c) if c != ord(' ') else '_' for c in name83)
                    hex_str = ' '.join(f'{c:02X}' for c in name83)
                    ftype = '<DIR>' if is_dir else 'file'

                    print(f"      [{display}]  {hex_str}  {file_size:10d}  {ftype}")
                    entries.append((name83, file_cluster, file_size))

            next_c = fat_next_cluster(cluster)
            if next_c >= 0x0FFFFFF8:
                break
            cluster = next_c

        print()

        # ── Step 4: Test name matching ────────────────────────────────────
        search_names = [
            (b"VMLINUZ RPI", "vmlinuz.rpi (Linux kernel)"),
            (b"INITRD  GZ ", "initrd.gz (initramfs)"),
            (b"BCM2711 DTB", "bcm2711.dtb (device tree)"),
        ]

        print("[MATCH] Testing kernel search names:")
        all_ok = True
        for target, desc in search_names:
            found = False
            for (name83, cluster, size) in entries:
                if name83 == target:
                    found = True
                    print(f"  OK    {desc}")
                    print(f"        target:  [{target.decode('ascii')}]")
                    print(f"        matched: [{name83.decode('ascii')}]  cluster={cluster}  size={size}")
                    break
            if not found:
                all_ok = False
                print(f"  FAIL  {desc}")
                print(f"        target:  [{target.decode('ascii')}]")
                print(f"        NOT FOUND in directory!")
                # Show closest matches
                for (name83, cluster, size) in entries:
                    if name83[:3] == target[:3]:
                        display = ''.join(chr(c) if c != ord(' ') else '_' for c in name83)
                        print(f"        closest: [{display}]  {' '.join(f'{c:02X}' for c in name83)}")

        print()
        if all_ok:
            print("ALL FILES FOUND — kernel should load them successfully!")
        else:
            print("SOME FILES MISSING — kernel will fail to load Linux VM")

if __name__ == '__main__':
    if len(sys.argv) < 2:
        print(f"Usage: {sys.argv[0]} <image_or_device>")
        print(f"Example: {sys.argv[0]} /tmp/sdcard.img")
        sys.exit(1)
    fat32_test(sys.argv[1])
