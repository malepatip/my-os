TARGET     = aarch64-unknown-none-softfloat
KERNEL_ELF = target/$(TARGET)/release/kernel
KERNEL_BIN = kernel8.img

# QEMU (Pi 3 emulation — default for local dev)
QEMU      = qemu-system-aarch64
QEMU_ARGS = -M raspi3b -serial stdio -display none -kernel $(KERNEL_BIN)

# SD card boot partition (Chromebook via Crostini share)
SD_BOOT   = /mnt/chromeos/removable/UNTITLED
# Note: SD card has 2 partitions. Pi 4 boots from the first one (UNTITLED).
# Second partition (UNTITLED 1) is unused. After sharing with Linux via
# ChromeOS Files app, the first partition mounts at the path above.

# UART console device (USB-to-TTL adapter)
UART_DEV  = /dev/ttyUSB0
UART_BAUD = 115200

.PHONY: all build run build-pi4 flash console clean

# ── QEMU build (Pi 3, bsp_rpi3 feature) ──────────────────────────────────────
all: build

OBJCOPY = rust-objcopy

build:
	cargo build --release
	$(OBJCOPY) --strip-all -O binary $(KERNEL_ELF) $(KERNEL_BIN)
	@echo "[qemu] $(KERNEL_BIN) $$(ls -lh $(KERNEL_BIN) | awk '{print $$5}')"

run: build
	$(QEMU) $(QEMU_ARGS)

# ── Pi 4 build (bsp_rpi4 feature) ────────────────────────────────────────────
build-pi4:
	cargo build --release \
	    --no-default-features --features bsp_rpi4
	$(OBJCOPY) --strip-all -O binary $(KERNEL_ELF) $(KERNEL_BIN)
	@echo "[pi4]  $(KERNEL_BIN) $$(ls -lh $(KERNEL_BIN) | awk '{print $$5}')"

# ── Flash kernel + firmware to SD card ───────────────────────────────────────
# Uses python fsync on each file + directory to force ChromeOS 9p bridge to
# flush writes to the physical FAT32 filesystem before eject.

flash: build-pi4
	@test -d "$(SD_BOOT)" || { \
	    echo "ERROR: SD card not found at $(SD_BOOT)"; \
	    echo "       Insert SD card, then in ChromeOS Files app:"; \
	    echo "       right-click the SD card → Share with Linux"; \
	    exit 1; }
	@python3 -c "\
import os; \
src='sdcard'; dst='$(SD_BOOT)'; \
files=['config.txt','start4.elf','fixup4.dat','bcm2711-rpi-4-b.dtb','kernel8.img']; \
[( \
  d:=open(os.path.join(dst,f),'wb'), \
  d.write(open(os.path.join(src,f),'rb').read()), \
  d.flush(), os.fsync(d.fileno()), d.close(), \
  print(f'  {f}: ok') \
) for f in files]; \
fd=os.open(dst,os.O_RDONLY); os.fsync(fd); os.close(fd) \
"
	@echo "[flash] Done — eject SD card from ChromeOS Files app, then boot Pi 4."

# ── Open UART console (requires USB-to-TTL adapter) ──────────────────────────
console:
	@echo "[uart] $(UART_DEV) at $(UART_BAUD) baud — Ctrl+A then K to quit"
	screen $(UART_DEV) $(UART_BAUD)

clean:
	cargo clean
	rm -f $(KERNEL_BIN)
