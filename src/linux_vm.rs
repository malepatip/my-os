// linux_vm.rs — EL2 stage-2 page tables + Linux VM loader
//
// Architecture: ai-os stays at EL2. Linux runs as an EL1 guest.
//
// Memory layout (Pi 4, 4GB RAM):
//   0x0008_0000  ai-os Rust kernel (EL2, ~300KB)
//   0x0020_0000  Shared memory ring buffer (4KB) — USB HID IPC
//   0x0040_0000  Linux kernel image (vmlinuz, loaded from SD card)
//   0x0200_0000  Linux initramfs (loaded from SD card)
//   0x0300_0000  Linux stack / heap
//   0x3B40_0000  Stage-2 page table pool (64KB)
//   0x3B50_0000  Linux DTB (device tree blob, 64KB)
//
// Stage-2 page tables: identity-map all 4GB physical memory 1:1.
// This means Linux at EL1 sees the same physical addresses as EL2.
// No SMMU, no IOMMU — Linux can access all hardware directly.
//
// HCR_EL2 configuration:
//   RW=1    (EL1 is AArch64)
//   VM=1    (enable stage-2 address translation)
//   SWIO=1  (software I/O coherency — required for Pi 4)
//   PTW=1   (protected table walk)
//   FMO=1   (route FIQ to EL2 — we don't use it but needed for GIC)
//   IMO=1   (route IRQ to EL2 — we don't use it but needed for GIC)
//   NOTE: We set VM=0 initially (identity map = transparent), then
//         switch to VM=1 just before ERET to Linux.
//
// Linux boot protocol (AArch64):
//   x0 = physical address of DTB
//   x1 = 0
//   x2 = 0
//   x3 = 0
//   PC = kernel entry point (vmlinuz load address)
//   EL = EL1 (via SPSR_EL2 + ELR_EL2 + ERET)

// (atomic imports removed — not needed)

// ── Memory addresses ─────────────────────────────────────────────────────────

/// Physical address where Linux kernel image is loaded
pub const LINUX_LOAD_ADDR: usize = 0x0040_0000;

/// Physical address where Linux initramfs is loaded
pub const LINUX_INITRD_ADDR: usize = 0x0200_0000;

/// Physical address of shared memory ring buffer (USB HID IPC)
pub const SHMEM_ADDR: usize = 0x0020_0000;

/// Physical address of Linux DTB (device tree blob)
pub const LINUX_DTB_ADDR: usize = 0x3B50_0000;

/// Physical address of stage-2 page table pool
const PT_POOL_ADDR: usize = 0x3B40_0000;
const PT_POOL_SIZE: usize = 64 * 1024; // 64KB

/// Shared memory magic value — Linux writes this when ready
pub const SHMEM_MAGIC: u32 = 0xAA55AA55;

// ── Shared memory layout ─────────────────────────────────────────────────────
//
// MUST match hid_daemon.c exactly (same offsets, same field sizes):
//   Offset 0:   magic (u32)      = SHMEM_MAGIC when Linux ready
//   Offset 4:   write_idx (u32)  = next slot Linux writes
//   Offset 8:   read_idx  (u32)  = next slot EL2 reads
//   Offset 12:  _pad (u32)
//   Offset 16:  ring[256] (u8)   = ASCII keystrokes
//   Offset 272: mouse_x (i32)    = accumulated X delta
//   Offset 276: mouse_y (i32)    = accumulated Y delta
//   Offset 280: mouse_btns (u8)  = bit0=left, bit1=right, bit2=mid
//   Offset 281: mouse_flags (u8) = bit0=new_event

// All fields are naturally aligned — no packing needed.
// Layout matches hid_daemon.c exactly because C also uses natural alignment.
#[repr(C)]
pub struct SharedMem {
    pub magic:       u32,       // +0
    pub write_idx:   u32,       // +4
    pub read_idx:    u32,       // +8
    pub _pad:        u32,       // +12
    pub ring:        [u8; 256], // +16  ASCII keyboard ring
    pub mouse_x:     i32,       // +272 mouse X delta
    pub mouse_y:     i32,       // +276 mouse Y delta
    pub mouse_btns:  u8,        // +280 button state
    pub mouse_flags: u8,        // +281 bit0=new event
    pub _pad2:       [u8; 2],   // +282 alignment
}

impl SharedMem {
    pub fn get() -> &'static mut SharedMem {
        unsafe { &mut *(SHMEM_ADDR as *mut SharedMem) }
    }

    pub fn is_ready(&self) -> bool {
        let m = unsafe { core::ptr::read_volatile(&self.magic as *const u32) };
        m == SHMEM_MAGIC
    }

    /// Read one ASCII character from the keyboard ring buffer, or None if empty.
    pub fn read_char(&mut self) -> Option<u8> {
        let widx = unsafe { core::ptr::read_volatile(&self.write_idx as *const u32) };
        let ridx = unsafe { core::ptr::read_volatile(&self.read_idx  as *const u32) };
        if ridx == widx {
            return None;
        }
        let idx = (ridx as usize) % 256;
        let ch = unsafe { core::ptr::read_volatile(&self.ring[idx] as *const u8) };
        unsafe { core::ptr::write_volatile(&mut self.read_idx as *mut u32, ridx.wrapping_add(1)) };
        Some(ch)
    }

    /// Read mouse delta and button state. Returns (dx, dy, buttons) if new event.
    pub fn read_mouse(&mut self) -> Option<(i32, i32, u8)> {
        let flags = unsafe { core::ptr::read_volatile(&self.mouse_flags as *const u8) };
        if flags & 0x01 == 0 {
            return None;
        }
        let dx   = unsafe { core::ptr::read_volatile(&self.mouse_x    as *const i32) };
        let dy   = unsafe { core::ptr::read_volatile(&self.mouse_y    as *const i32) };
        let btns = unsafe { core::ptr::read_volatile(&self.mouse_btns as *const u8) };
        // Consume the event — reset deltas and clear flag
        unsafe {
            core::ptr::write_volatile(&mut self.mouse_x    as *mut i32, 0);
            core::ptr::write_volatile(&mut self.mouse_y    as *mut i32, 0);
            core::ptr::write_volatile(&mut self.mouse_flags as *mut u8, flags & !0x01);
        }
        Some((dx, dy, btns))
    }
}

// ── Stage-2 page table setup ─────────────────────────────────────────────────
//
// We use a 3-level page table (L1 → L2 → L2 block entries) for a 4GB
// identity map. Each L2 block entry covers 2MB.
//
// VTCR_EL2 configuration:
//   T0SZ=32  (input address size = 4GB, 32-bit IPA)
//   SL0=1    (start at L1)
//   IRGN0=1  (inner write-back, write-allocate)
//   ORGN0=1  (outer write-back, write-allocate)
//   SH0=3    (inner shareable)
//   TG0=0    (4KB granule)
//   PS=0     (32-bit physical address space)
//
// Stage-2 block descriptor (2MB):
//   [0]     = 1 (valid)
//   [1]     = 0 (block, not table)
//   [5:2]   = MemAttr index (0 = normal, 1 = device)
//   [9:6]   = AP[2:1] = 0b01 (EL1 R/W, EL0 no access)
//   [10]    = SH[0] = 1 (inner shareable)
//   [11]    = AF = 1 (access flag, must be set or fault on first access)
//   [47:21] = output address bits [47:21]
//   [53]    = XN = 0 (executable)

const PAGE_SIZE: usize = 4096;
const BLOCK_SIZE_2MB: usize = 2 * 1024 * 1024;
const L1_ENTRIES: usize = 4;       // 4 L1 entries = 4 x 1GB = 4GB
const L2_ENTRIES: usize = 512;     // 512 L2 entries per L1 = 512 x 2MB = 1GB

// Stage-2 descriptor bits
const DESC_VALID:    u64 = 1 << 0;
const DESC_BLOCK:    u64 = 0 << 1; // block (not table)
const DESC_TABLE:    u64 = 1 << 1; // table (not block)
const DESC_AF:       u64 = 1 << 10; // access flag
const DESC_SH_INNER: u64 = 3 << 8;  // inner shareable
// Stage-2 S2AP (bits [7:6]) — NOT the same as stage-1 AP:
//   S2AP = 0b00 → no access from EL1
//   S2AP = 0b01 → read-only  from EL1
//   S2AP = 0b11 → read/write from EL1  (CORRECT for Linux guest RAM)
const DESC_AP_RW:   u64 = 3 << 6;  // S2AP = 0b11 = EL1 read/write
const DESC_AP_RO:   u64 = 1 << 6;  // S2AP = 0b01 = EL1 read-only (framebuffer protection)
// MemAttr index 0 = normal memory (MAIR_EL2 index 0)
const DESC_MEMATTR_NORMAL: u64 = 0 << 2;
// MemAttr index 1 = device nGnRnE (MAIR_EL2 index 1)
const DESC_MEMATTR_DEVICE: u64 = 1 << 2;

/// Set up stage-2 identity page tables for 4GB.
/// Called once from kernel_main before launching Linux.
pub fn setup_stage2_tables() {
    // Page table pool: L1 table (4 entries) + 4 x L2 tables (512 entries each)
    // Total: 4*8 + 4*512*8 = 32 + 16384 = 16416 bytes
    let pool = PT_POOL_ADDR as *mut u64;

    // Zero the pool
    unsafe {
        core::ptr::write_bytes(pool, 0, PT_POOL_SIZE / 8);
    }

    // L1 table is at PT_POOL_ADDR
    let l1_table = pool;

    // L2 tables start at PT_POOL_ADDR + PAGE_SIZE
    for gb in 0..4usize {
        let l2_table = unsafe { pool.add(512 + gb * 512) }; // 512 u64s per table
        // L2 table for this GB lives at PT_POOL_ADDR + PAGE_SIZE + gb*PAGE_SIZE.
        // The pointer pool.add(512 + gb*512) maps to byte offset (512+gb*512)*8
        // = 0x1000 + gb*0x1000 from pool start — must match l2_phys exactly.
        let l2_phys = PT_POOL_ADDR + PAGE_SIZE + gb * PAGE_SIZE;

        // L1 entry points to L2 table
        let l1_desc = (l2_phys as u64) | DESC_TABLE | DESC_VALID;
        unsafe { l1_table.add(gb).write_volatile(l1_desc); }

        // Fill L2 table with 2MB block entries
        for mb2 in 0..512usize {
            let phys_addr = (gb * 1024 * 1024 * 1024 + mb2 * BLOCK_SIZE_2MB) as u64;

            // Device memory for peripheral regions (above 0xFC000000)
            let memattr = if phys_addr >= 0xFC00_0000 {
                DESC_MEMATTR_DEVICE
            } else {
                DESC_MEMATTR_NORMAL
            };

            let desc = phys_addr
                | DESC_VALID
                | DESC_BLOCK
                | DESC_AF
                | DESC_SH_INNER
                | DESC_AP_RW
                | memattr;

            unsafe { l2_table.add(mb2).write_volatile(desc); }
        }
    }

    // Set up MAIR_EL2:
    //   Index 0: normal memory (0xFF = write-back, write-allocate, inner+outer)
    //   Index 1: device nGnRnE (0x00)
    let mair: u64 = 0xFF | (0x00 << 8);
    unsafe {
        core::arch::asm!(
            "msr mair_el2, {mair}",
            "isb",
            mair = in(reg) mair,
        );
    }

    // Set up VTCR_EL2
    // T0SZ=32 (4GB IPA), SL0=1 (L1 start), TG0=0 (4KB), PS=0 (32-bit PA)
    // IRGN0=1, ORGN0=1, SH0=3
    let vtcr: u64 = (32 << 0)  // T0SZ
                  | (1  << 6)  // SL0 = 1 (start at L1)
                  | (1  << 8)  // IRGN0 = write-back
                  | (1  << 10) // ORGN0 = write-back
                  | (3  << 12) // SH0 = inner shareable
                  | (0  << 14) // TG0 = 4KB
                  | (0  << 16) // PS = 32-bit
                  | (1  << 31); // RES1

    unsafe {
        core::arch::asm!(
            "msr vtcr_el2, {vtcr}",
            "isb",
            vtcr = in(reg) vtcr,
        );
    }

    // Set VTTBR_EL2 to point to our L1 table
    let vttbr: u64 = PT_POOL_ADDR as u64; // VMID=0, BADDR=L1 table
    unsafe {
        core::arch::asm!(
            "msr vttbr_el2, {vttbr}",
            "isb",
            vttbr = in(reg) vttbr,
        );
    }
}

/// Mark the 2 MB block(s) covering the EL2 framebuffer as read-only from EL1.
///
/// Linux can still read the pixels (useful for its own simplefb driver), but
/// any CPU store from EL1 will stage-2 fault.  The EL2 handler skips those
/// writes silently so our crash screen is never overwritten.
///
/// Must be called AFTER setup_stage2_tables() and hv_console::init().
pub fn protect_framebuffer(fb_phys: usize, fb_size: usize) {
    if fb_phys == 0 || fb_size == 0 { return; }

    let pool      = PT_POOL_ADDR as *mut u64;
    let block     = BLOCK_SIZE_2MB;
    let start_blk = fb_phys / block;
    // Round the end up to cover the last partial block
    let end_blk   = (fb_phys + fb_size + block - 1) / block;

    for blk in start_blk..end_blk {
        let gb   = blk / 512;
        let mb2  = blk % 512;
        if gb >= 4 { break; }

        // L2 table for this GB lives at pool + 512 (L1 skip) + gb*512 entries
        let l2_table = unsafe { pool.add(512 + gb * 512) };
        let entry_ptr = unsafe { l2_table.add(mb2) };

        // Read existing descriptor, clear S2AP bits [7:6], set read-only
        let desc = unsafe { entry_ptr.read_volatile() };
        let desc = (desc & !(3 << 6)) | DESC_AP_RO;
        unsafe { entry_ptr.write_volatile(desc); }
    }

    // Flush stage-2 TLB entries so the new permissions take effect
    unsafe {
        core::arch::asm!(
            "dsb ishst",        // ensure descriptor writes are visible
            "tlbi vmalls12e1",  // invalidate all stage-1/2 EL1 TLB entries
            "dsb ish",
            "isb",
            options(nomem, nostack),
        );
    }
}

// ── Linux kernel launcher ────────────────────────────────────────────────────

/// Launch Linux at EL1 with the given DTB address.
/// This function does NOT return — it ERETSs into Linux.
///
/// # Safety
/// Linux kernel image must be loaded at LINUX_LOAD_ADDR.
/// DTB must be set up at dtb_addr.
pub unsafe fn launch_linux(dtb_addr: usize) -> ! {
    // Enable stage-2 translation and configure HCR_EL2
    // HCR_EL2 bits:
    //   RW  (bit 31) = 1 — EL1 is AArch64
    //   VM  (bit 0)  = 1 — enable stage-2 address translation
    //   SWIO(bit 1)  = 1 — software I/O coherency
    //   PTW (bit 2)  = 1 — protected table walk
    //
    // NOTE: IMO (bit 4), FMO (bit 3), AMO (bit 5) are intentionally NOT set.
    // Setting those bits routes IRQ/FIQ/SError from EL1 to EL2. Without a
    // real GIC virtualisation handler at EL2, this causes an interrupt storm:
    // Linux fires an interrupt, EL2 gets it, our bare 'eret' returns without
    // deactivating the GIC interrupt, Linux immediately re-raises it, repeat.
    // With IMO/FMO=0, Linux handles its own interrupts at EL1 via the GIC
    // directly — which is correct for a transparent hypervisor at this stage.
    // NOTE: PTW (bit 2) is intentionally NOT set.
    // HCR_EL2.PTW=1 enables "Protected Table Walk" — stage-2 enforces
    // permissions on stage-1 page table walks. This means Linux's MMU
    // hardware walker faults when it tries to read a page table entry
    // stored in memory that stage-2 marks as non-executable (IFSC=0x0E,
    // S1PTW=1). With PTW=0, stage-2 does not restrict stage-1 table walks,
    // which is correct for a transparent type-2 hypervisor that trusts its guest.
    let hcr: u64 = (1 << 31) // RW   — EL1 is AArch64
                 | (1 << 0)  // VM   — stage-2 translation enabled
                 | (1 << 1); // SWIO — software I/O coherency

    // SPSR_EL2 for Linux entry: EL1h (0b0101), DAIF masked
    // 0x3C5 = 0b0011_1100_0101
    let spsr: u64 = 0x3C5;

    // Linux AArch64 boot protocol:
    //   x0 = DTB physical address
    //   x1 = x2 = x3 = 0
    //   PC = kernel entry (LINUX_LOAD_ADDR + text_offset)
    // For this kernel, text_offset = 0 (confirmed from Image header)
    let entry = LINUX_LOAD_ADDR as u64;

    core::arch::asm!(
        // Set HCR_EL2
        "msr hcr_el2, {hcr}",
        "isb",
        // Set SPSR_EL2 (EL1h, all interrupts masked)
        "msr spsr_el2, {spsr}",
        // Set ELR_EL2 to Linux entry point
        "msr elr_el2, {entry}",
        // Set up Linux boot registers
        "mov x0, {dtb}",   // x0 = DTB address
        "mov x1, xzr",
        "mov x2, xzr",
        "mov x3, xzr",
        // ERET into Linux at EL1
        "eret",
        hcr   = in(reg) hcr,
        spsr  = in(reg) spsr,
        entry = in(reg) entry,
        dtb   = in(reg) dtb_addr as u64,
        options(noreturn),
    );
}

// ── DTB builder ──────────────────────────────────────────────────────────────
//
// We need to tell Linux where the initramfs is. The simplest approach is
// to use the Pi 4's existing DTB from the SD card and patch it to add:
//   chosen {
//     linux,initrd-start = <LINUX_INITRD_ADDR>;
//     linux,initrd-end   = <LINUX_INITRD_ADDR + initrd_size>;
//     bootargs = "console=ttyS0,115200 mem=512M";
//   }
//
// For now, we pass the Pi 4 DTB directly from SD card (bcm2711-rpi-4-b.dtb)
// and add bootargs via the kernel command line embedded in the DTB.
// The initrd address is passed via the chosen node.

/// Patch the DTB at LINUX_DTB_ADDR to set the chosen node with:
///   linux,initrd-start = LINUX_INITRD_ADDR
///   linux,initrd-end   = LINUX_INITRD_ADDR + initrd_size
///   bootargs = "console=tty1 console=ttyAMA0,115200 rdinit=/init panic=5"
///
/// The FDT (Flattened Device Tree) format:
///   Header (40 bytes): magic, totalsize, off_dt_struct, off_dt_strings, ...
///   Memory reservation block
///   Structure block: FDT_BEGIN_NODE, FDT_PROP, FDT_END_NODE, FDT_END tokens
///   Strings block: property name strings
///
/// We scan the structure block for the "chosen" node and patch its properties.
/// If "chosen" doesn't exist, we insert it before FDT_END.
pub fn setup_dtb(initrd_size: usize) -> usize {
    let dtb = LINUX_DTB_ADDR as *mut u8;

    // FDT magic check
    let magic = unsafe { u32::from_be(*(dtb as *const u32)) };
    if magic != 0xD00DFEED {
        return LINUX_DTB_ADDR; // not a valid FDT, skip patching
    }

    let total_size = unsafe { u32::from_be(*(dtb.add(4) as *const u32)) } as usize;
    let off_struct  = unsafe { u32::from_be(*(dtb.add(8) as *const u32)) } as usize;
    let off_strings = unsafe { u32::from_be(*(dtb.add(12) as *const u32)) } as usize;
    let size_struct = unsafe { u32::from_be(*(dtb.add(24) as *const u32)) } as usize;
    let size_strings = unsafe { u32::from_be(*(dtb.add(28) as *const u32)) } as usize;

    // We'll append new strings to the strings block and new properties to the
    // chosen node in the structure block. To keep this simple and safe, we
    // write a new minimal DTB that wraps the original but adds our chosen node.
    //
    // Simpler approach: scan for existing "chosen" node and patch in-place,
    // or append a new chosen node. Since the DTB is loaded into RAM at a fixed
    // address with plenty of space, we can expand it.
    //
    // Strategy: find the FDT_END token (0x00000009) at the end of the structure
    // block, and insert our chosen node just before it.
    //
    // FDT tokens:
    const FDT_BEGIN_NODE: u32 = 1;
    const FDT_END_NODE:   u32 = 2;
    const FDT_PROP:       u32 = 3;
    const FDT_NOP:        u32 = 4;
    const FDT_END:        u32 = 9;

    // Bootargs string (null-terminated, padded to 4 bytes)
    // console=tty1      — output to HDMI framebuffer (we can see this)
    // console=ttyAMA0   — also output to UART for when serial is available
    // rdinit=/init      — use initramfs directly (not old-style /dev/ram0)
    // panic=5           — reboot after 5 s on panic so we can catch the msg
    let bootargs = b"console=tty1 console=ttyAMA0,115200 rdinit=/init panic=5\0";

    // Find the end of the structure block (FDT_END token)
    let struct_start = unsafe { dtb.add(off_struct) };
    let struct_end_offset = size_struct; // size in bytes

    // The FDT_END token is the last 4 bytes of the structure block
    let fdt_end_pos = off_struct + struct_end_offset - 4;

    // Verify it's actually FDT_END
    let end_token = unsafe { u32::from_be(*(dtb.add(fdt_end_pos) as *const u32)) };
    if end_token != FDT_END {
        return LINUX_DTB_ADDR; // malformed DTB
    }

    // We'll write our chosen node at fdt_end_pos, then write FDT_END after it.
    // First, add property name strings to the strings block.
    let strings_start = off_strings;
    let strings_end   = off_strings + size_strings;

    // Append strings: "linux,initrd-start", "linux,initrd-end", "bootargs"
    // We write them after the existing strings block.
    let str_initrd_start_name = b"linux,initrd-start\0";
    let str_initrd_end_name   = b"linux,initrd-end\0";
    let str_bootargs_name     = b"bootargs\0";
    let str_chosen_name       = b"chosen\0";

    // String offsets (relative to start of strings block)
    let off_str_initrd_start = size_strings as u32;
    let off_str_initrd_end   = off_str_initrd_start + str_initrd_start_name.len() as u32;
    let off_str_bootargs     = off_str_initrd_end   + str_initrd_end_name.len() as u32;
    let off_str_chosen       = off_str_bootargs     + str_bootargs_name.len() as u32;

    unsafe {
        // Write new strings after existing strings block
        let mut sp = dtb.add(strings_end);
        for &b in str_initrd_start_name { *sp = b; sp = sp.add(1); }
        for &b in str_initrd_end_name   { *sp = b; sp = sp.add(1); }
        for &b in str_bootargs_name     { *sp = b; sp = sp.add(1); }
        for &b in str_chosen_name       { *sp = b; sp = sp.add(1); }

        // Write chosen node into structure block at fdt_end_pos
        let mut p = dtb.add(fdt_end_pos) as *mut u32;

        // FDT_BEGIN_NODE "chosen"
        *p = u32::to_be(FDT_BEGIN_NODE); p = p.add(1);
        // Node name "chosen\0" padded to 4 bytes: 7 bytes → 8 bytes (2 words)
        let chosen_name = b"chosen\0\0"; // 8 bytes
        let np = p as *mut u8;
        for (i, &b) in chosen_name.iter().enumerate() {
            *np.add(i) = b;
        }
        p = p.add(2); // 8 bytes = 2 words

        // Property: linux,initrd-start (u32 big-endian)
        *p = u32::to_be(FDT_PROP); p = p.add(1);
        *p = u32::to_be(4); p = p.add(1);  // len = 4 bytes
        *p = u32::to_be(off_str_initrd_start); p = p.add(1);
        *p = u32::to_be(LINUX_INITRD_ADDR as u32); p = p.add(1);

        // Property: linux,initrd-end (u32 big-endian)
        *p = u32::to_be(FDT_PROP); p = p.add(1);
        *p = u32::to_be(4); p = p.add(1);
        *p = u32::to_be(off_str_initrd_end); p = p.add(1);
        *p = u32::to_be((LINUX_INITRD_ADDR + initrd_size) as u32); p = p.add(1);

        // Property: bootargs (variable length, null-terminated, padded to 4 bytes)
        let ba_len = bootargs.len(); // includes null terminator
        let ba_padded = (ba_len + 3) & !3;
        *p = u32::to_be(FDT_PROP); p = p.add(1);
        *p = u32::to_be(ba_len as u32); p = p.add(1);
        *p = u32::to_be(off_str_bootargs); p = p.add(1);
        let bp = p as *mut u8;
        for (i, &b) in bootargs.iter().enumerate() { *bp.add(i) = b; }
        // Zero-pad to 4-byte boundary
        for i in ba_len..ba_padded { *bp.add(i) = 0; }
        p = p.add(ba_padded / 4);

        // FDT_END_NODE
        *p = u32::to_be(FDT_END_NODE); p = p.add(1);

        // FDT_END
        *p = u32::to_be(FDT_END);

        // Update DTB header: totalsize, size_dt_struct, size_dt_strings
        let new_struct_end = p as usize - LINUX_DTB_ADDR + 4; // +4 for FDT_END itself
        let new_struct_size = new_struct_end - off_struct;
        let new_strings_size = size_strings
            + str_initrd_start_name.len()
            + str_initrd_end_name.len()
            + str_bootargs_name.len()
            + str_chosen_name.len();
        let new_total = off_strings + new_strings_size
            + (new_struct_end - off_struct); // rough estimate
        // Actually total = max of struct end and strings end
        let strings_new_end = strings_end
            + str_initrd_start_name.len()
            + str_initrd_end_name.len()
            + str_bootargs_name.len()
            + str_chosen_name.len();
        let new_total_size = if new_struct_end > strings_new_end {
            new_struct_end
        } else {
            strings_new_end
        };

        // Patch header fields
        *(dtb.add(4)  as *mut u32) = u32::to_be(new_total_size as u32);  // totalsize
        *(dtb.add(24) as *mut u32) = u32::to_be(new_struct_size as u32); // size_dt_struct
        *(dtb.add(28) as *mut u32) = u32::to_be(new_strings_size as u32); // size_dt_strings
    }

    LINUX_DTB_ADDR
}
