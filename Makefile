# DriveSync (dsync) — https://github.com/scaleninja/drivesync
#
# Native:        make build | make release | make test | make install
# Cross-compile: make all-targets            (all four targets into dist/)
#                make linux-x86_64 linux-arm64 macos-x86_64 macos-arm64
# One-time:      make setup                  (rustup targets + cargo-zigbuild; needs `zig` on PATH)
#
# Linux builds use cargo-zigbuild (Zig as the C cross-compiler for bundled SQLite) and target musl,
# so the binaries are fully static. macOS targets build natively with cargo (run on a Mac).

BIN      := dsync
VERSION  := $(shell sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
DIST     := dist

LINUX_X86   := x86_64-unknown-linux-musl
LINUX_ARM   := aarch64-unknown-linux-musl
MAC_X86     := x86_64-apple-darwin
MAC_ARM     := aarch64-apple-darwin
ALL_TARGETS := $(LINUX_X86) $(LINUX_ARM) $(MAC_X86) $(MAC_ARM)

.PHONY: build release test check fmt clean install setup all-targets \
        linux-x86_64 linux-arm64 macos-x86_64 macos-arm64

build:
	cargo build

release:
	cargo build --release

test:
	cargo test

check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

fmt:
	cargo fmt

install:
	cargo install --path . --locked

clean:
	cargo clean
	rm -rf $(DIST)

setup:
	rustup target add $(ALL_TARGETS)
	cargo install cargo-zigbuild --locked
	@command -v zig >/dev/null || echo "zig not found: install it (e.g. brew install zig) for Linux builds"

all-targets: linux-x86_64 linux-arm64 macos-x86_64 macos-arm64
	@echo; ls -l $(DIST)

linux-x86_64:   ; $(call zig_build,$(LINUX_X86),linux-x86_64,)
linux-arm64:    ; $(call zig_build,$(LINUX_ARM),linux-arm64,)
macos-x86_64:   ; $(call cargo_build,$(MAC_X86),macos-x86_64,)
macos-arm64:    ; $(call cargo_build,$(MAC_ARM),macos-arm64,)

# $(1) rust target, $(2) artifact suffix, $(3) extension
define zig_build
	cargo zigbuild --release --target $(1)
	$(call package,$(1),$(2),$(3))
endef

define cargo_build
	cargo build --release --target $(1)
	$(call package,$(1),$(2),$(3))
endef

define package
	mkdir -p $(DIST)
	cp target/$(1)/release/$(BIN)$(3) $(DIST)/$(BIN)-$(VERSION)-$(2)$(3)
	@echo "==> $(DIST)/$(BIN)-$(VERSION)-$(2)$(3)"
endef
