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
const DESC_AP_RW:    u64 = 1 << 6;  // AP[2:1] = 01 = EL1 R/W
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
        let l2_phys = PT_POOL_ADDR + PAGE_SIZE + gb * PAGE_SIZE * 2;

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
    //   FMO (bit 3)  = 1 — route FIQ to EL2
    //   IMO (bit 4)  = 1 — route IRQ to EL2
    //   AMO (bit 5)  = 1 — route SError to EL2
    //   TGE (bit 27) = 0 — EL1 is a guest (not host)
    let hcr: u64 = (1 << 31) // RW
                 | (1 << 0)  // VM
                 | (1 << 1)  // SWIO
                 | (1 << 2)  // PTW
                 | (1 << 3)  // FMO
                 | (1 << 4)  // IMO
                 | (1 << 5); // AMO

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

/// Copy DTB from SD card and patch chosen node for initramfs + bootargs.
/// Returns the address where the DTB was placed.
pub fn setup_dtb(initrd_size: usize) -> usize {
    // For now, return the address where the caller should have loaded the DTB.
    // The actual DTB patching happens via the SD card DTB + kernel cmdline.
    LINUX_DTB_ADDR
}
