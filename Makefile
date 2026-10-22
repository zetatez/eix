# eix workspace shortcuts.
#
#   make                  # show help
#   make build            # debug build
#   make release          # release build + package for this OS/arch
#   make dist             # all package formats supported by this OS
#   make test             # all workspace tests
#   make run ARGS=doctor  # run eix with arguments
#   make install          # install to ~/.local/bin (PREFIX overridable)
#   make clean            # clean build and packaging artifacts

CARGO ?= cargo
CARGO_TARGET_DIR ?= target
export CARGO_TARGET_DIR

PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
ARGS ?=
DIST_DIR ?= dist
STAGE_DIR = $(DIST_DIR)/stage
MAKEPKG_BUILDDIR = $(abspath $(DIST_DIR))/makepkg

VERSION ?= $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)

UNAME_S := $(shell uname -s 2>/dev/null || echo unknown)
UNAME_M := $(shell uname -m 2>/dev/null || echo unknown)

# Normalize host architecture for artifact names and Debian control files.
PKG_ARCH := $(if $(filter x86_64 amd64,$(UNAME_M)),x86_64,$(if $(filter aarch64 arm64,$(UNAME_M)),arm64,$(if $(filter armv7l armv7,$(UNAME_M)),armv7,$(UNAME_M))))
DEB_ARCH := $(if $(filter x86_64 amd64,$(UNAME_M)),amd64,$(if $(filter aarch64 arm64,$(UNAME_M)),arm64,$(if $(filter armv7l armv7,$(UNAME_M)),armhf,$(UNAME_M))))
OS_NAME := $(if $(filter Linux,$(UNAME_S)),linux,$(if $(filter Darwin,$(UNAME_S)),macos,$(shell echo '$(UNAME_S)' | tr '[:upper:]' '[:lower:]')))

ifeq ($(UNAME_S),Darwin)
MACOSX_DEPLOYMENT_TARGET ?= 11.0
export MACOSX_DEPLOYMENT_TARGET
endif

ARCHIVE = $(DIST_DIR)/eix-$(VERSION)-$(OS_NAME)-$(PKG_ARCH).tar.gz
BREW_FORMULA = $(DIST_DIR)/eix-$(PKG_ARCH).rb
BREW_CLASS := $(if $(filter x86_64,$(PKG_ARCH)),EixX8664,$(if $(filter arm64,$(PKG_ARCH)),EixArm64,$(if $(filter armv7,$(PKG_ARCH)),EixArmv7,Eix)))
DEB_FILE = $(DIST_DIR)/eix_$(VERSION)_$(DEB_ARCH).deb

.DEFAULT_GOAL := help

.PHONY: help build build-release test run install release package package-arch \
	package-deb package-darwin package-brew package-tar \
	dist dist-linux dist-macos clean

help: ## Show this help
	@printf 'Usage: make [target]\n\nTargets:\n'
	@awk 'BEGIN {FS = ":.*##"} /^[a-z0-9-]+:.*##/ {printf "  make %-15s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@printf '\nVersion: %s\nHost: %s %s\nArtifacts: %s\nInstall prefix: %s\n' \
		'$(VERSION)' '$(OS_NAME)' '$(PKG_ARCH)' '$(abspath $(DIST_DIR))' '$(PREFIX)'

build: ## Build debug binaries (eix + eixd)
	$(CARGO) build --workspace

build-release:
	$(CARGO) build --workspace --release --locked

test: ## Run all workspace tests
	$(CARGO) test --workspace

run: ## Run the eix TUI (ARGS="..." for CLI flags)
	$(CARGO) run -p eix-tui --bin eix -- $(ARGS)

release: package ## Build release and package for this OS/arch

install: build-release ## Build release and install eix/eixd to PREFIX/bin
	@mkdir -p '$(BINDIR)'
	install -m 0755 '$(CARGO_TARGET_DIR)/release/eix' '$(BINDIR)/eix'
	install -m 0755 '$(CARGO_TARGET_DIR)/release/eixd' '$(BINDIR)/eixd'
	@printf 'installed eix and eixd to %s\n' '$(BINDIR)'

package: ## Build a package for this OS/arch
	@case "$(UNAME_S)" in \
		Linux) \
			if command -v makepkg >/dev/null 2>&1 && command -v pacman >/dev/null 2>&1; then \
				$(MAKE) package-arch; \
			elif command -v dpkg-deb >/dev/null 2>&1; then \
				$(MAKE) package-deb; \
			else \
				$(MAKE) package-tar; \
			fi ;; \
		Darwin) $(MAKE) package-darwin ;; \
		*) printf 'unsupported OS: %s\n' "$(UNAME_S)" >&2; exit 1 ;; \
	esac

package-arch: build-release ## Build an Arch Linux package with makepkg
	@command -v makepkg >/dev/null 2>&1 || { \
		printf 'makepkg not found (Arch Linux only)\n' >&2; exit 1; \
	}
	@mkdir -p '$(DIST_DIR)' '$(MAKEPKG_BUILDDIR)'
	BUILDDIR='$(MAKEPKG_BUILDDIR)' PKGDEST='$(abspath $(DIST_DIR))' makepkg -f --noconfirm
	@printf 'Arch package: %s\n' "$$(PKGDEST='$(abspath $(DIST_DIR))' makepkg --packagelist | head -n 1)"

package-deb: build-release ## Build a Debian/Ubuntu .deb for this arch
	@command -v dpkg-deb >/dev/null 2>&1 || { \
		printf 'dpkg-deb not found (install dpkg or run on Debian/Ubuntu)\n' >&2; exit 1; \
	}
	@rm -rf '$(STAGE_DIR)/deb'
	@mkdir -p '$(STAGE_DIR)/deb/DEBIAN' \
		'$(STAGE_DIR)/deb/usr/bin' \
		'$(STAGE_DIR)/deb/usr/share/doc/eix'
	install -m 0755 '$(CARGO_TARGET_DIR)/release/eix' '$(STAGE_DIR)/deb/usr/bin/eix'
	install -m 0755 '$(CARGO_TARGET_DIR)/release/eixd' '$(STAGE_DIR)/deb/usr/bin/eixd'
	install -m 0644 README.md '$(STAGE_DIR)/deb/usr/share/doc/eix/README.md'
	install -m 0644 DESIGN.md '$(STAGE_DIR)/deb/usr/share/doc/eix/DESIGN.md'
	@printf 'Package: eix\nVersion: %s\nArchitecture: %s\nMaintainer: eix contributors <noreply@github.com>\nDepends: libc6, libgcc-s1\nSection: devel\nPriority: optional\nDescription: Coding agent with a multi-session TUI\n eix is a daemon-backed coding agent with a native multi-session TUI\n and a Neovim frontend.\n' \
		'$(VERSION)' '$(DEB_ARCH)' > '$(STAGE_DIR)/deb/DEBIAN/control'
	@chmod 0644 '$(STAGE_DIR)/deb/DEBIAN/control'
	dpkg-deb --root-owner-group --build '$(STAGE_DIR)/deb' '$(DEB_FILE)'
	@rm -rf '$(STAGE_DIR)/deb'
	@rmdir '$(STAGE_DIR)' 2>/dev/null || true
	@printf 'Debian package: %s\n' '$(DEB_FILE)'

package-tar: build-release ## Build a portable tar.gz for this OS/arch
	@mkdir -p '$(DIST_DIR)'
	@rm -rf '$(STAGE_DIR)/tar' '$(ARCHIVE)'
	@mkdir -p '$(STAGE_DIR)/tar/eix-$(VERSION)'
	install -m 0755 '$(CARGO_TARGET_DIR)/release/eix' '$(STAGE_DIR)/tar/eix-$(VERSION)/eix'
	install -m 0755 '$(CARGO_TARGET_DIR)/release/eixd' '$(STAGE_DIR)/tar/eix-$(VERSION)/eixd'
	install -m 0644 README.md '$(STAGE_DIR)/tar/eix-$(VERSION)/README.md'
	install -m 0644 DESIGN.md '$(STAGE_DIR)/tar/eix-$(VERSION)/DESIGN.md'
	tar -czf '$(ARCHIVE)' -C '$(STAGE_DIR)/tar' 'eix-$(VERSION)'
	@rm -rf '$(STAGE_DIR)/tar'
	@rmdir '$(STAGE_DIR)' 2>/dev/null || true
	@printf 'Portable archive: %s\n' '$(ARCHIVE)'

package-darwin: ## Build macOS tar.gz + Homebrew binary formula
	@[ "$(UNAME_S)" = "Darwin" ] || { \
		printf 'macOS packages must be built on macOS (current host: %s)\n' '$(UNAME_S)' >&2; exit 1; \
	}
	@$(MAKE) package-tar
	@command -v shasum >/dev/null 2>&1 || command -v sha256sum >/dev/null 2>&1 || { \
		printf 'shasum or sha256sum is required to build the Homebrew formula\n' >&2; exit 1; \
	}
	@sha=$$(if command -v sha256sum >/dev/null 2>&1; then \
		sha256sum '$(ARCHIVE)' | awk '{print $$1}'; \
	else \
		shasum -a 256 '$(ARCHIVE)' | awk '{print $$1}'; \
	fi); \
	sed -e 's|@CLASS@|$(BREW_CLASS)|g' \
		-e 's|@VERSION@|$(VERSION)|g' \
		-e 's|@TARBALL@|$(notdir $(ARCHIVE))|g' \
		-e "s|@SHA256@|$$sha|g" \
		packaging/homebrew/eix.rb.in > '$(BREW_FORMULA)'; \
	if command -v ruby >/dev/null 2>&1; then ruby -c '$(BREW_FORMULA)'; fi; \
	printf 'macOS package: %s\nHomebrew formula: %s\nInstall: brew install --formula %s\n' \
		'$(ARCHIVE)' '$(BREW_FORMULA)' '$(BREW_FORMULA)'

package-brew: package-darwin
	@:

dist: ## Build every package format supported by this OS/arch
	@case "$(UNAME_S)" in \
		Linux) $(MAKE) dist-linux ;; \
		Darwin) $(MAKE) dist-macos ;; \
		*) printf 'unsupported OS: %s\n' "$(UNAME_S)" >&2; exit 1 ;; \
	esac

dist-linux:
	@if command -v makepkg >/dev/null 2>&1 && command -v pacman >/dev/null 2>&1; then \
		$(MAKE) package-arch; \
	fi
	@if command -v dpkg-deb >/dev/null 2>&1; then \
		$(MAKE) package-deb; \
	fi
	@$(MAKE) package-tar

dist-macos: package-darwin
	@:

clean: ## Remove build and packaging artifacts
	$(CARGO) clean
	rm -rf '$(DIST_DIR)' ./src ./pkg
	rm -f ./*.pkg.tar.*
