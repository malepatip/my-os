// build.rs — ai-os v0.4.0
//
// New architecture: EL2 Thin Hypervisor + Linux Driver VM
// No more Circle library — USB is handled by Linux at EL1.
// The linker script is specified in .cargo/config.toml.

fn main() {
    // Re-run if linker script or build script changes
    println!("cargo:rerun-if-changed=src/linker.ld");
    println!("cargo:rerun-if-changed=build.rs");
}
