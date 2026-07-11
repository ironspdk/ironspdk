# Makefile for building SPDK (submodule) and the Rust crate.
# If SPDK is not set in the environment, use extern/spdk.
SPDK ?= extern/spdk
SPDK := $(abspath $(SPDK))

# Build PKG_CONFIG_PATH so it includes SPDK's pkgconfig dir.
PKG_CONFIG_PATH := $(SPDK)/build/lib/pkgconfig

export SPDK PKG_CONFIG_PATH

# Number of parallel jobs for make
NPROC := $(shell nproc 2>/dev/null || echo 1)

.PHONY: all release spdk check-spdk clean distclean

# Default target: ensure SPDK built (if needed), then build the crate
all: check-spdk cargo-build-debug

cargo-build-debug:
	@echo "Building ironspdk rust crate (debug)"
	SPDK=$(SPDK) PKG_CONFIG_PATH=$(PKG_CONFIG_PATH) cargo build

release: check-spdk
	@echo "Building ironspdk rust crate (release)"
	SPDK=$(SPDK) PKG_CONFIG_PATH=$(PKG_CONFIG_PATH) cargo build --release

test-build: build-test-spdk cargo-build-debug

# check-spdk: checks if SPDK build outputs look present; if not, invoke spdk
check-spdk:
	@if [ -d "$(SPDK)/build/lib/pkgconfig" ] && [ "$$(ls -A $(SPDK)/build/lib/pkgconfig 2>/dev/null)" ]; then \
		echo "SPDK appears built in $(SPDK)/build; skipping SPDK build"; \
	else \
		$(MAKE) spdk; \
	fi

# spdk: build the SPDK submodule (configure+make run from the source dir)
spdk:
	@echo "Building SPDK from $(SPDK) (configure+make run in $(SPDK))..."
	@if [ ! -d "$(SPDK)" ]; then \
		echo "ERROR: SPDK not found at $(SPDK). Try: git submodule update --init --recursive"; \
		exit 1; \
	fi
	@# Ensure an out-of-source build dir exists; SPDK configure may populate build/ under the source tree.
	mkdir -p $(SPDK)/build
	cd $(SPDK) && ./configure --with-vhost --with-ublk --with-uring --with-raid5f
	cd $(SPDK) && make -j $(NPROC)

build-test-spdk:
	@echo "Building SPDK from $(SPDK) (configure+make run in $(SPDK))..."
	mkdir -p $(SPDK)/build
	cd $(SPDK) && ./configure --with-raid5f
	cd $(SPDK) && make -j $(NPROC)

clean-spdk:
	@if [ -d "$(SPDK)/build" ]; then \
		echo "Removing SPDK build directory $(SPDK)/build..."; \
		rm -rf "$(SPDK)/build"; \
	fi

clean:
	@echo "Cleaning Rust build artifacts..."
	SPDK=$(SPDK) PKG_CONFIG_PATH=$(PKG_CONFIG_PATH) cargo clean

distclean: clean clean-spdk
