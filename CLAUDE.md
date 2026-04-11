# my-ai-os: Bare-Metal AI Operating System

## Project Context
This is a bare-metal AI Operating System written in Rust, targeting the AArch64 architecture (Raspberry Pi 3/4). The goal is to build a true OS kernel from scratch—not a Layer 7 wrapper—inspired by macOS (XNU/Darwin) architecture. The system runs directly on hardware to achieve zero-copy data pipelines and deterministic autonomy for AI inference.

## Architecture & Design Philosophy
- **Language:** Rust (`no_std`, `no_main`) using the stable toolchain.
- **Boot Process:** Firmware loads kernel at `0x80000`. `boot.rs` (assembly) parks cores 1-3, sets up the stack for core 0, zeros the BSS section, and jumps to `kernel_main`.
- **Hardware Interaction:** Direct Memory-Mapped I/O (MMIO). No external crates for hardware abstraction; we write directly to registers.
- **Feature Flags:** 
  - `bsp_rpi3` (default): For QEMU development (UART base `0x3F201000`).
  - `bsp_rpi4`: For physical hardware (UART base `0xFE201000`).

## Current State
The kernel successfully boots in QEMU and on physical hardware. It features:
1. A custom PL011 UART driver (`src/uart.rs`).
2. A minimal interactive shell (`src/main.rs`) with commands: `help`, `info`, `echo`, `halt`.
3. Exception Level detection (currently running at EL2).

## Build & Run Commands
- **Build for QEMU (Pi 3):** `cargo build --release`
- **Build for Hardware (Pi 4):** `cargo build --release --no-default-features --features bsp_rpi4`
- **Create Binary Image:** `rust-objcopy --strip-all -O binary target/aarch64-unknown-none-softfloat/release/kernel kernel8.img`
- **Run in QEMU:** `qemu-system-aarch64 -M raspi3b -serial stdio -display none -kernel kernel8.img`

## Roadmap & Next Steps
When assisting with this project, prioritize the following roadmap:
1. **System Timer:** Implement a driver for the ARM Generic Timer to measure time and enable delays.
2. **Interrupts & Exceptions:** Set up the exception vector table to handle synchronous exceptions and IRQs.
3. **Preemptive Scheduler:** Use the timer interrupt to implement context switching between multiple kernel threads.
4. **Memory Management:** Implement a physical memory allocator and configure the MMU (page tables) for virtual memory.
5. **AI Inference Engine:** Port a minimal inference engine (e.g., `llama2.c` style) into the `no_std` kernel environment.

## Coding Guidelines
- Do not add external dependencies unless absolutely necessary. We are building from scratch.
- Always use `unsafe` blocks explicitly and document the hardware invariants being relied upon.
- Maintain the `kprint!` and `kprintln!` macros for all console output.
- When modifying hardware registers, use `read_volatile` and `write_volatile`.
