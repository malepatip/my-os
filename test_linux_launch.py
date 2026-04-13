#!/usr/bin/env python3
"""
test_linux_launch.py — Tests the Linux VM launch trampoline encoding
and validates the Pi 4 spin table protocol before flashing.

Tests:
1. Trampoline instruction encoding (ldr x0/x18 PC-relative offsets)
2. Pi 4 spin table address (0xE0 vs correct address)
3. DTB patching for initramfs
4. SPSR_EL2 value for EL1h entry
"""

import struct

# ── Constants matching main.rs ────────────────────────────────────────────────
TRAMPOLINE_ADDR   = 0x0010_0000
CORE1_SPIN_TABLE  = 0xe0          # Current value in code — IS THIS RIGHT?
LINUX_LOAD_ADDR   = 0x0040_0000
LINUX_DTB_ADDR    = 0x3B50_0000
LINUX_INITRD_ADDR = 0x0200_0000

# Pi 4 spin table — where start4.elf parks secondary cores
# From: https://github.com/raspberrypi/tools/blob/master/armstubs/armstub8.S
# The Pi 4 firmware (start4.elf) parks cores using the spin table at:
#   Core 0: 0x000000D8  (but core 0 is already running)
#   Core 1: 0x000000E0
#   Core 2: 0x000000E8
#   Core 3: 0x000000F0
# This IS the correct address for Pi 4 with start4.elf in 64-bit mode.
# The firmware writes 0 to these addresses and cores spin on WFE+LDR.
# Writing a non-zero address + SEV releases the core.
PI4_CORE1_RELEASE_ADDR = 0xE0   # This is correct for Pi 4 with start4.elf

print("=" * 60)
print("Test 1: Pi 4 spin table address")
print("=" * 60)
print(f"Code uses:     0x{CORE1_SPIN_TABLE:08X}")
print(f"Pi 4 correct:  0x{PI4_CORE1_RELEASE_ADDR:08X}")
if CORE1_SPIN_TABLE == PI4_CORE1_RELEASE_ADDR:
    print("PASS: Spin table address is correct")
else:
    print("FAIL: Wrong spin table address!")
print()

# ── Test 2: Trampoline instruction encoding ───────────────────────────────────
print("=" * 60)
print("Test 2: Trampoline AArch64 instruction encoding")
print("=" * 60)

# The trampoline in main.rs:
#   [0] 0x58000300  ldr x0, [pc, #24]   ; loads dtb_addr from offset +24
#   [1] 0x58000312  ldr x18, [pc, #24]  ; loads linux_entry from offset +28
#   [2] 0xAA1F03E1  mov x1, xzr
#   [3] 0xAA1F03E2  mov x2, xzr
#   [4] 0xAA1F03E3  mov x3, xzr
#   [5] 0xD61F0240  br x18
#   [6] (data) dtb_addr   (u64 at byte offset 24)
#   [7] (data) linux_entry (u64 at byte offset 32)

# LDR (literal) encoding: 0x58 | (imm19 << 5) | Rt
# imm19 is the signed offset in units of 4 bytes from the PC of the instruction
# PC of instr[0] = TRAMPOLINE_ADDR + 0
# Data at instr[6] = TRAMPOLINE_ADDR + 24
# Offset from instr[0] = +24 bytes = +6 instructions = imm19=6
# ldr x0, [pc, #24]: 0x58 | (6 << 5) | 0 = 0x580000C0... wait let me recalculate

def encode_ldr_literal(rt, imm_bytes):
    """Encode LDR Xt, [PC, #imm] — imm must be multiple of 4"""
    assert imm_bytes % 4 == 0
    imm19 = imm_bytes // 4
    # LDR (literal) 64-bit: 0x58000000 | (imm19 << 5) | Rt
    return 0x58000000 | ((imm19 & 0x7FFFF) << 5) | (rt & 0x1F)

# Instruction 0 is at byte 0, data[0] (dtb_addr) is at byte 24
# PC of instruction 0 = TRAMPOLINE_ADDR + 0
# Data at byte 24 from start of trampoline
# Offset from PC of instr[0] = 24 bytes
instr0_expected = encode_ldr_literal(0, 24)   # ldr x0, [pc, #24]
instr0_actual   = 0x58000300

# Instruction 1 is at byte 4, data[1] (linux_entry) is at byte 32
# PC of instruction 1 = TRAMPOLINE_ADDR + 4
# Data at byte 32 from start of trampoline
# Offset from PC of instr[1] = 32 - 4 = 28 bytes
instr1_expected = encode_ldr_literal(18, 28)  # ldr x18, [pc, #28]
instr1_actual   = 0x58000312

print(f"ldr x0, [pc, #24]:")
print(f"  Expected: 0x{instr0_expected:08X}")
print(f"  Actual:   0x{instr0_actual:08X}")
if instr0_expected == instr0_actual:
    print("  PASS")
else:
    print(f"  FAIL! Correct encoding: 0x{instr0_expected:08X}")

print(f"ldr x18, [pc, #28]:")
print(f"  Expected: 0x{instr1_expected:08X}")
print(f"  Actual:   0x{instr1_actual:08X}")
if instr1_expected == instr1_actual:
    print("  PASS")
else:
    print(f"  FAIL! Correct encoding: 0x{instr1_expected:08X}")

# Verify data placement
# 6 instructions * 4 bytes = 24 bytes → data[0] at offset 24 ✓
# data[1] at offset 32 (data.add(4) where data is *mut u64 = 8 bytes each → offset 32)
print(f"\nData placement:")
print(f"  dtb_addr at byte offset:    24 (data.add(3) * 8 = 24) ✓")
print(f"  linux_entry at byte offset: 32 (data.add(4) * 8 = 32)")
print(f"  instr[0] reads from PC+24 = byte 0+24 = 24 ✓")
print(f"  instr[1] reads from PC+28 = byte 4+28 = 32 ✓")
print()

# ── Test 3: SPSR_EL2 value ────────────────────────────────────────────────────
print("=" * 60)
print("Test 3: SPSR_EL2 for EL1h entry")
print("=" * 60)
# SPSR_EL2 for entering EL1h (AArch64, EL1 with SP_EL1):
# M[3:0] = 0b0101 = EL1h
# DAIF   = 0b1111 (all interrupts masked during boot)
# 0x3C5 = 0b0011_1100_0101
spsr_actual = 0x3C5
spsr_el1h_daif_masked = (0b1111 << 6) | 0b0101  # DAIF=1111, M=EL1h
print(f"  Code uses:    0x{spsr_actual:03X} = 0b{spsr_actual:09b}")
print(f"  EL1h+DAIF:    0x{spsr_el1h_daif_masked:03X} = 0b{spsr_el1h_daif_masked:09b}")
if spsr_actual == spsr_el1h_daif_masked:
    print("  PASS: SPSR_EL2 is correct for EL1h entry")
else:
    print(f"  FAIL! Should be 0x{spsr_el1h_daif_masked:03X}")
print()

# ── Test 4: BUT WAIT — the trampoline runs at EL2, not EL1! ──────────────────
print("=" * 60)
print("Test 4: CRITICAL — Does the trampoline enter EL1?")
print("=" * 60)
print("""
The current trampoline code:
  ldr x0, [pc, #24]   ; dtb_addr
  ldr x18, [pc, #24]  ; linux entry
  mov x1, xzr
  mov x2, xzr
  mov x3, xzr
  br x18              ; ← JUMPS DIRECTLY, STAYS AT CURRENT EL!

PROBLEM: Core 1 is parked at EL2 (same as core 0).
  'br x18' jumps to LINUX_LOAD_ADDR while STILL AT EL2.
  Linux will crash immediately because it expects EL1.

FIX: The trampoline must use ERET to drop to EL1:
  Set SPSR_EL2 = 0x3C5 (EL1h, DAIF masked)
  Set ELR_EL2  = LINUX_LOAD_ADDR
  ERET          ; drops to EL1 and jumps to Linux

ALSO: HCR_EL2 must be set with RW=1 (EL1 is AArch64)
  before ERET, otherwise EL1 is AArch32.
""")
print("FAIL: Current trampoline does NOT drop to EL1 before jumping to Linux!")
print()

# ── Test 5: Core 1 park state ─────────────────────────────────────────────────
print("=" * 60)
print("Test 5: What EL is core 1 parked at?")
print("=" * 60)
print("""
From boot.rs:
  All cores enter _start at the same EL (EL2 on Pi 4 with start4.elf).
  Cores 1-3 hit '.L_park: wfe / b .L_park' BEFORE any EL setup.
  They are parked at EL2 with NO stack, NO VBAR, NO HCR_EL2 configured.

When core 0 writes TRAMPOLINE_ADDR to 0xE0 and sends SEV:
  Core 1 wakes from WFE, reads 0xE0, sees non-zero... 
  
  WAIT — the park loop in boot.rs is:
    .L_park:
      wfe
      b .L_park
  
  This is a SIMPLE WFE SPIN LOOP. It does NOT read from 0xE0!
  The Pi 4 spin table protocol requires the park loop to:
    1. Load from the spin table address
    2. If non-zero, branch to that address
  
  The current park loop IGNORES the spin table entirely.
  Core 1 will never wake up from 'b .L_park' no matter what we write to 0xE0!
""")
print("FAIL: Park loop in boot.rs ignores the spin table — core 1 never wakes!")
print()

print("=" * 60)
print("SUMMARY OF BUGS:")
print("=" * 60)
print("""
BUG 1 (CRITICAL): boot.rs park loop is a simple WFE spin.
  It does NOT read from the spin table at 0xE0.
  Core 1 will NEVER wake up.
  FIX: Change park loop to read from spin_table[core_id] and branch if non-zero.

BUG 2 (CRITICAL): Trampoline uses 'br x18' — stays at EL2.
  Linux expects to be entered at EL1.
  FIX: Trampoline must set SPSR_EL2, ELR_EL2, HCR_EL2 and use ERET.

BUG 3 (IMPORTANT): DTB has no initramfs info.
  Linux will boot but won't find hid_daemon in initramfs.
  FIX: Patch DTB chosen node with linux,initrd-start/end and bootargs.
""")
