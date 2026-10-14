# Installs into ~/.local by default; use `make install PREFIX=/usr/local` (as root) for
# a system-wide install.
PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
DATADIR ?= $(PREFIX)/share
ifeq ($(PREFIX),$(HOME)/.local)
SYSTEMD_USER_DIR ?= $(HOME)/.config/systemd/user
else
SYSTEMD_USER_DIR ?= $(PREFIX)/lib/systemd/user
endif
APP_ID = io.github.GoathierS.WhatDrains

.PHONY: build install uninstall check

build:
	cargo build --release

check:
	cargo fmt --all --check
	cargo clippy --all-targets -- -D warnings
	cargo test

install: build
	install -Dm755 target/release/whatdrains $(DESTDIR)$(BINDIR)/whatdrains
	install -Dm755 target/release/whatdrainsd $(DESTDIR)$(BINDIR)/whatdrainsd
	install -Dm644 data/$(APP_ID).desktop $(DESTDIR)$(DATADIR)/applications/$(APP_ID).desktop
	install -Dm644 data/$(APP_ID).metainfo.xml $(DESTDIR)$(DATADIR)/metainfo/$(APP_ID).metainfo.xml
	install -Dm644 data/$(APP_ID).svg $(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/$(APP_ID).svg
	install -d $(DESTDIR)$(SYSTEMD_USER_DIR)
	sed 's|@BINDIR@|$(BINDIR)|' data/whatdrainsd.service.in > $(DESTDIR)$(SYSTEMD_USER_DIR)/whatdrainsd.service
	@echo
	@echo "Installed. Start recording with:"
	@echo "  systemctl --user daemon-reload && systemctl --user enable --now whatdrainsd.service"

uninstall:
	-systemctl --user disable --now whatdrainsd.service
	rm -f $(DESTDIR)$(BINDIR)/whatdrains $(DESTDIR)$(BINDIR)/whatdrainsd
	rm -f $(DESTDIR)$(DATADIR)/applications/$(APP_ID).desktop
	rm -f $(DESTDIR)$(DATADIR)/metainfo/$(APP_ID).metainfo.xml
	rm -f $(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/$(APP_ID).svg
	rm -f $(DESTDIR)$(SYSTEMD_USER_DIR)/whatdrainsd.service
