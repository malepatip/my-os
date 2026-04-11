# Makefile — builds circle_usb_shim.cpp into libcircle_shim.a
#
# Depends on Circle libraries already built at $(CIRCLEHOME).
# Called automatically by the Rust build.rs script.

CIRCLEHOME ?= /home/ubuntu/circle

PREFIX64 = aarch64-linux-gnu-
CXX      = $(PREFIX64)g++
AR       = $(PREFIX64)ar

# AArch64 bare-metal flags — must match Circle's own build flags exactly
CXXFLAGS = \
    -DRASPPI=4 \
    -DAARCH=64 \
    -march=armv8-a \
    -mtune=cortex-a72 \
    -mstrict-align \
    -ffreestanding \
    -nostdinc \
    -nostdlib \
    -fno-exceptions \
    -fno-rtti \
    -fno-unwind-tables \
    -fno-asynchronous-unwind-tables \
    -std=c++17 \
    -O2 \
    -I$(CIRCLEHOME)/include \
    -I$(CIRCLEHOME)/addon/display

TARGET = libcircle_shim.a

all: $(TARGET)

circle_usb_shim.o: circle_usb_shim.cpp
	$(CXX) $(CXXFLAGS) -c -o $@ $<

$(TARGET): circle_usb_shim.o
	$(AR) rcs $@ $^

clean:
	rm -f *.o $(TARGET)

.PHONY: all clean
