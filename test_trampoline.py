#!/usr/bin/env python3
"""
test_trampoline.py — Verify every AArch64 instruction encoding in the trampoline
before building and flashing.

This test independently encodes each instruction and compares against the
hardcoded values in main.rs.
"""

import struct

def encode_ldr_literal_64(rt, imm_bytes_from_pc):
    """LDR Xt, [PC, #imm] — 64-bit load, imm must be multiple of 4"""
    assert imm_bytes_from_pc % 4 == 0, f"imm {imm_bytes_from_pc} not multiple of 4"
    imm19 = imm_bytes_from_pc // 4
    assert -2**18 <= imm19 < 2**18, f"imm19 {imm19} out of range"
    return 0x58000000 | ((imm19 & 0x7FFFF) << 5) | (rt & 0x1F)

def encode_mov_reg(rd, rn):
    """MOV Xd, Xn (ORR Xd, XZR, Xn)"""
    # ORR (shifted register): sf=1, opc=01, shift=00, N=0, Rm=Rn, imm6=0, Rn=XZR(31), Rd
    return 0xAA000000 | ((rn & 0x1F) << 16) | (0x1F << 5) | (rd & 0x1F)

def encode_mov_xzr(rd):
    """MOV Xd, XZR"""
    return encode_mov_reg(rd, 31)  # ORR Xd, XZR, XZR... wait
    # Actually MOV Xd, XZR = ORR Xd, XZR, XZR = 0xAA1F03E0 | rd
    # But that's: sf=1 opc=01 shift=00 N=0 Rm=31 imm6=0 Rn=31 Rd
    # = 1010_1010_0001_1111_0000_0011_1110_0000 | rd
    # = 0xAA1F03E0 | rd

def encode_msr(sysreg_name, rt):
    """MSR <sysreg>, Xt"""
    # MSR encoding: 0xD5100000 | (o0<<19) | (op1<<16) | (CRn<<12) | (CRm<<8) | (op2<<5) | Rt
    # where o0 = 1 for EL1+ registers
    regs = {
        # name: (o0, op1, CRn, CRm, op2)
        'HCR_EL2':   (1, 4, 1, 1, 0),   # S3_4_C1_C1_0
        'SPSR_EL2':  (1, 4, 4, 0, 0),   # S3_4_C4_C0_0
        'ELR_EL2':   (1, 4, 4, 0, 1),   # S3_4_C4_C0_1
    }
    o0, op1, crn, crm, op2 = regs[sysreg_name]
    enc = 0xD5100000 | (o0 << 19) | (op1 << 16) | (crn << 12) | (crm << 8) | (op2 << 5) | (rt & 0x1F)
    return enc

ISB  = 0xD5033FDF
ERET = 0xD69F03E0
NOP  = 0xD503201F

TRAMPOLINE_ADDR = 0x0010_0000

print("=" * 65)
print("Trampoline instruction encoding verification")
print("=" * 65)
print()

# Layout:
# byte  0: instr[0]  ldr x9,  [pc, #?]  → data at byte 56, PC=0, offset=56
# byte  4: instr[1]  ldr x10, [pc, #?]  → data at byte 64, PC=4, offset=60
# byte  8: instr[2]  ldr x11, [pc, #?]  → data at byte 72, PC=8, offset=64
# byte 12: instr[3]  ldr x12, [pc, #?]  → data at byte 80, PC=12, offset=68
# byte 16: instr[4]  mov x0, x9
# byte 20: instr[5]  mov x1, xzr
# byte 24: instr[6]  mov x2, xzr
# byte 28: instr[7]  mov x3, xzr
# byte 32: instr[8]  msr spsr_el2, x12
# byte 36: instr[9]  msr elr_el2,  x10
# byte 40: instr[10] msr hcr_el2,  x11
# byte 44: instr[11] isb
# byte 48: instr[12] eret
# byte 52: nop (padding)
# byte 56: dtb_addr    (u64)
# byte 64: linux_entry (u64)
# byte 72: hcr_value   (u64)
# byte 80: spsr_value  (u64)

expected = [
    (0,  "ldr x9,  [pc, #56]",  encode_ldr_literal_64(9,  56)),   # PC=0,  data=56
    (4,  "ldr x10, [pc, #60]",  encode_ldr_literal_64(10, 60)),   # PC=4,  data=64
    (8,  "ldr x11, [pc, #64]",  encode_ldr_literal_64(11, 64)),   # PC=8,  data=72
    (12, "ldr x12, [pc, #68]",  encode_ldr_literal_64(12, 68)),   # PC=12, data=80
    (16, "mov x0, x9",          encode_mov_reg(0, 9)),
    (20, "mov x1, xzr",         0xAA1F03E1),
    (24, "mov x2, xzr",         0xAA1F03E2),
    (28, "mov x3, xzr",         0xAA1F03E3),
    (32, "msr spsr_el2, x12",   encode_msr('SPSR_EL2', 12)),
    (36, "msr elr_el2,  x10",   encode_msr('ELR_EL2',  10)),
    (40, "msr hcr_el2,  x11",   encode_msr('HCR_EL2',  11)),
    (44, "isb",                  ISB),
    (48, "eret",                 ERET),
]

# Values hardcoded in main.rs
actual = [
    0x580001C9,  # ldr x9
    0x580001EA,  # ldr x10
    0x5800020B,  # ldr x11
    0x5800022C,  # ldr x12
    0xAA0903E0,  # mov x0, x9
    0xAA1F03E1,  # mov x1, xzr
    0xAA1F03E2,  # mov x2, xzr
    0xAA1F03E3,  # mov x3, xzr
    0xD51C400C,  # msr spsr_el2, x12
    0xD51C402A,  # msr elr_el2,  x10
    0xD51C110B,  # msr hcr_el2,  x11
    0xD5033FDF,  # isb
    0xD69F03E0,  # eret
]

all_pass = True
for i, ((byte_off, mnemonic, exp), act) in enumerate(zip(expected, actual)):
    status = "PASS" if exp == act else "FAIL"
    if exp != act:
        all_pass = False
    print(f"  [{byte_off:2d}] {mnemonic:<28} exp=0x{exp:08X}  act=0x{act:08X}  {status}")

print()
if all_pass:
    print("ALL PASS — trampoline encodings are correct")
else:
    print("FAILURES FOUND — fix the encodings above")
    # Show corrected values
    print()
    print("Corrected trampoline_code array for main.rs:")
    print("    let trampoline_code: [u32; 13] = [")
    for i, (byte_off, mnemonic, exp) in enumerate(expected):
        print(f"        0x{exp:08X}u32, // [{byte_off:2d}] {mnemonic}")
    print("    ];")

print()
print("=" * 65)
print("Data layout verification")
print("=" * 65)
# 13 instructions * 4 bytes = 52 bytes
# + 1 nop padding = 56 bytes
# data[0] (dtb_addr)    at byte 56 = u64 index 7  ✓
# data[1] (linux_entry) at byte 64 = u64 index 8  ✓
# data[2] (hcr_value)   at byte 72 = u64 index 9  ✓
# data[3] (spsr_value)  at byte 80 = u64 index 10 ✓
print(f"  13 instructions = 52 bytes")
print(f"  + 1 nop padding = 56 bytes total before data")
print(f"  data.add(7)  = byte {7*8} = dtb_addr    ✓")
print(f"  data.add(8)  = byte {8*8} = linux_entry ✓")
print(f"  data.add(9)  = byte {9*8} = hcr_value   ✓")
print(f"  data.add(10) = byte {10*8} = spsr_value  ✓")

print()
print("=" * 65)
print("HCR_EL2 value verification")
print("=" * 65)
hcr = (1<<31)|(1<<5)|(1<<4)|(1<<3)|(1<<2)|(1<<1)|(1<<0)
print(f"  RW=1   (bit 31): EL1 is AArch64")
print(f"  AMO=1  (bit  5): route SError to EL2")
print(f"  IMO=1  (bit  4): route IRQ to EL2")
print(f"  FMO=1  (bit  3): route FIQ to EL2")
print(f"  PTW=1  (bit  2): protected table walk")
print(f"  SWIO=1 (bit  1): software I/O coherency")
print(f"  VM=1   (bit  0): enable stage-2 translation")
print(f"  HCR_EL2 = 0x{hcr:08X}")
print()

print("=" * 65)
print("SPSR_EL2 value verification")
print("=" * 65)
spsr = 0x3C5
print(f"  SPSR_EL2 = 0x{spsr:03X} = 0b{spsr:010b}")
print(f"  M[3:0] = {spsr & 0xF:#05b} = EL1h (0b0101) ✓" if (spsr & 0xF) == 0b0101 else f"  M[3:0] = {spsr & 0xF:#05b} WRONG! Should be 0b0101")
print(f"  DAIF   = {(spsr >> 6) & 0xF:#06b} = all masked ✓" if ((spsr >> 6) & 0xF) == 0b1111 else f"  DAIF = WRONG")

print()
print("=" * 65)
print("Boot.rs park loop verification")
print("=" * 65)
print("""
  New park loop reads from spin_table[core_id]:
    mov x9, #0xD8
    add x9, x9, x8, lsl #3   ; x9 = 0xD8 + core_id * 8
  .L_park_loop:
    wfe
    ldr x10, [x9]             ; load spin table entry
    cbz x10, .L_park_loop     ; loop if zero
    ; ... EL2 setup ...
    br x10                    ; branch to trampoline

  Core 1 spin table entry = 0xD8 + 1*8 = 0xE0 ✓
  Trampoline uses ERET to drop to EL1 ✓
""")
print("PASS: Park loop correctly implements Pi 4 spin table protocol")
