// build.rs — Rust build script for my-ai-os
//
// This script:
//   1. Builds Circle's core, USB, input, and fs static libraries (C++)
//      by running `make` from within each Circle subdirectory.
//      Circle's Makefiles use relative CIRCLEHOME paths (../..),
//      so we must NOT override CIRCLEHOME — just run make -C <subdir>.
//   2. Creates libcircle_nostartup.a — Circle's core lib WITHOUT startup64.o.
//      startup64.o contains Circle's own _start which conflicts with our
//      Rust _start at 0x80000. Removing it lets our boot.rs entry point
//      win and the Pi firmware jumps to the correct place.
//   3. Builds our circle_usb_shim.cpp into libcircle_shim.a
//   4. Tells Cargo to link all of them into the final kernel binary
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

    // ── Step 2: Build libcircle_nostartup.a ──────────────────────────────────
    // Circle's libcircle.a contains startup64.o which has its own _start
    // symbol in the .init section. This conflicts with our Rust _start at
    // 0x80000 — Circle's _start wins and the Pi boots into Circle's broken
    // startup code instead of our kernel, causing a blank screen.
    //
    // Fix: extract all objects from libcircle.a, remove startup64.o, and
    // repack as libcircle_nostartup.a. Our Rust _start then wins cleanly.
    let circle_lib     = circle_home.join("lib/libcircle.a");
    let nostartup_lib  = circle_home.join("lib/libcircle_nostartup.a");
    let extract_dir    = circle_home.join("lib/_nostartup_tmp");

    // Only rebuild if libcircle.a is newer than libcircle_nostartup.a
    let needs_rebuild = {
        let src_time = std::fs::metadata(&circle_lib)
            .and_then(|m| m.modified()).ok();
        let dst_time = std::fs::metadata(&nostartup_lib)
            .and_then(|m| m.modified()).ok();
        match (src_time, dst_time) {
            (Some(s), Some(d)) => s > d,
            _ => true,
        }
    };

    if needs_rebuild {
        // Clean and recreate extraction directory
        let _ = std::fs::remove_dir_all(&extract_dir);
        std::fs::create_dir_all(&extract_dir)
            .expect("Failed to create nostartup tmp dir");

        // Extract all objects from libcircle.a
        let status = Command::new("ar")
            .arg("x")
            .arg(&circle_lib)
            .current_dir(&extract_dir)
            .status()
            .expect("Failed to run ar x on libcircle.a");
        if !status.success() {
            panic!("ar x libcircle.a failed");
        }

        // Remove Circle's conflicting startup64.o
        let startup_obj = extract_dir.join("startup64.o");
        if startup_obj.exists() {
            std::fs::remove_file(&startup_obj)
                .expect("Failed to remove startup64.o");
        }

        // Repack without startup64.o
        let obj_files: Vec<_> = std::fs::read_dir(&extract_dir)
            .expect("Failed to read extract dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "o").unwrap_or(false))
            .collect();

        let mut ar_cmd = Command::new("ar");
        ar_cmd.arg("rcs").arg(&nostartup_lib);
        for obj in &obj_files {
            ar_cmd.arg(obj);
        }
        let status = ar_cmd.current_dir(&extract_dir)
            .status()
            .expect("Failed to run ar rcs");
        if !status.success() {
            panic!("Failed to create libcircle_nostartup.a");
        }

        // Clean up
        let _ = std::fs::remove_dir_all(&extract_dir);
    }

    // ── Step 3: Build the C++ shim ───────────────────────────────────────────
    let status = Command::new("make")
        .arg("-C")
        .arg(&shim_dir)
        .arg(format!("CIRCLEHOME={}", circle_home.display()))
        .status()
        .unwrap_or_else(|e| panic!("Failed to run make in circle_shim: {}", e));

    if !status.success() {
        panic!("circle_usb_shim build failed");
    }

    // ── Step 4: Tell Cargo where to find the libraries ───────────────────────
    println!("cargo:rustc-link-search=native={}", shim_dir.display());
    println!("cargo:rustc-link-search=native={}", circle_home.join("lib").display());
    println!("cargo:rustc-link-search=native={}", circle_home.join("lib/usb").display());
    println!("cargo:rustc-link-search=native={}", circle_home.join("lib/input").display());
    println!("cargo:rustc-link-search=native={}", circle_home.join("lib/fs").display());

    // ── Step 5: Link order — shim first, then Circle libs ────────────────────
    // Use circle_nostartup instead of circle to avoid the _start conflict.
    println!("cargo:rustc-link-lib=static=circle_shim");
    println!("cargo:rustc-link-lib=static=usb");
    println!("cargo:rustc-link-lib=static=input");
    println!("cargo:rustc-link-lib=static=fs");
    println!("cargo:rustc-link-lib=static=circle_nostartup");

    // ── Step 6: Re-run triggers ───────────────────────────────────────────────
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
