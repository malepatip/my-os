// build.rs — Rust build script for my-ai-os
//
// This script:
//   1. Builds Circle's core, USB, input, and fs static libraries (C++)
//      by running `make` from within each Circle subdirectory.
//      Circle's Makefiles use relative CIRCLEHOME paths (../..),
//      so we must NOT override CIRCLEHOME — just run make -C <subdir>.
//   2. Builds our circle_usb_shim.cpp into libcircle_shim.a
//   3. Tells Cargo to link all of them into the final kernel binary
//
// Only runs for the bsp_rpi4 feature (Pi 4 real hardware).
// On Pi 3 / QEMU, USB is skipped — Rust stubs handle the no-op case.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // ── Only do USB work for Pi 4 ────────────────────────────────────────────
    let is_rpi4 = env::var("CARGO_FEATURE_BSP_RPI4").is_ok();

    if !is_rpi4 {
        return;
    }

    // ── Paths ────────────────────────────────────────────────────────────────
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    // Circle is a sibling of the kernel project: /home/ubuntu/circle
    let circle_home  = PathBuf::from("/home/ubuntu/circle");
    let shim_dir     = manifest_dir.join("circle_shim");

    // ── Step 1: Build Circle libraries ───────────────────────────────────────
    // Circle's Makefiles use CIRCLEHOME = ../.. (relative), so we run make
    // with -C pointing into the circle directory — no CIRCLEHOME override.
    let circle_subdirs = ["lib", "lib/usb", "lib/input", "lib/fs"];

    for subdir in &circle_subdirs {
        let target_dir = circle_home.join(subdir);
        let status = Command::new("make")
            .arg("-C")
            .arg(&target_dir)
            .arg(format!("-j{}", num_cpus()))
            .status()
            .unwrap_or_else(|e| panic!("Failed to run make in {}: {}", subdir, e));

        if !status.success() {
            panic!("Circle library build failed in {}", subdir);
        }
    }

    // ── Step 2: Build the C++ shim ───────────────────────────────────────────
    let status = Command::new("make")
        .arg("-C")
        .arg(&shim_dir)
        .arg(format!("CIRCLEHOME={}", circle_home.display()))
        .status()
        .unwrap_or_else(|e| panic!("Failed to run make in circle_shim: {}", e));

    if !status.success() {
        panic!("circle_usb_shim build failed");
    }

    // ── Step 3: Tell Cargo where to find the libraries ───────────────────────
    println!("cargo:rustc-link-search=native={}", shim_dir.display());
    println!("cargo:rustc-link-search=native={}", circle_home.join("lib").display());
    println!("cargo:rustc-link-search=native={}", circle_home.join("lib/usb").display());
    println!("cargo:rustc-link-search=native={}", circle_home.join("lib/input").display());
    println!("cargo:rustc-link-search=native={}", circle_home.join("lib/fs").display());

    // ── Step 4: Link order — shim first, then Circle libs ────────────────────
    println!("cargo:rustc-link-lib=static=circle_shim");
    println!("cargo:rustc-link-lib=static=usb");
    println!("cargo:rustc-link-lib=static=input");
    println!("cargo:rustc-link-lib=static=fs");
    println!("cargo:rustc-link-lib=static=circle");

    // ── Step 5: Re-run triggers ───────────────────────────────────────────────
    println!("cargo:rerun-if-changed=circle_shim/circle_usb_shim.cpp");
    println!("cargo:rerun-if-changed=circle_shim/Makefile");
    println!("cargo:rerun-if-changed=build.rs");
}

fn num_cpus() -> usize {
    std::fs::read_to_string("/proc/cpuinfo")
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("processor"))
        .count()
        .max(1)
}
