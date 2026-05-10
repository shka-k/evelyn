# Build a distributable macOS .app for evelyn.
#
#   make app             — build release binary and assemble Evelyn.app
#   make app ARCH=both   — also build a universal (x86_64 + aarch64) binary
#   make icon            — regenerate Evelyn.icns from assets/icons/evelyn.png
#   make install         — copy Evelyn.app to /Applications
#   make dmg             — build .app and pack it into a draggable .dmg
#   make clean           — drop the build/ directory only (cargo clean is separate)

NAME       := Evelyn
BIN        := evelyn
BUNDLE_ID  := io.github.shka-k.evelyn
VERSION    := $(shell awk -F'"' '/^version *=/{print $$2; exit}' Cargo.toml)

# `host` (default) → release for the running arch.
# `both` → cargo builds both targets and lipo merges them.
ARCH ?= host

BUILD_DIR  := build
APP_DIR    := $(BUILD_DIR)/$(NAME).app
CONTENTS   := $(APP_DIR)/Contents
MACOS_DIR  := $(CONTENTS)/MacOS
RES_DIR    := $(CONTENTS)/Resources
ICON_PNG   := assets/icons/evelyn.png
ICONSET    := $(BUILD_DIR)/$(NAME).iconset
ICNS       := $(BUILD_DIR)/$(NAME).icns
PLIST      := $(CONTENTS)/Info.plist
DMG        := $(BUILD_DIR)/$(NAME)-$(VERSION).dmg

.PHONY: app icon install dmg clean

app: $(APP_DIR)/Contents/MacOS/$(BIN) $(ICNS) $(PLIST) | $(RES_DIR)
	@cp $(ICNS) $(RES_DIR)/$(NAME).icns
	@codesign --force --sign - --timestamp=none $(APP_DIR) >/dev/null 2>&1 || true
	@echo "→ $(APP_DIR)"

# Compile the binary, then place it inside the bundle. Two flows: a single
# host-arch release, or a universal binary stitched together with lipo.
$(APP_DIR)/Contents/MacOS/$(BIN): | $(MACOS_DIR)
ifeq ($(ARCH),both)
	cargo build --release --target x86_64-apple-darwin
	cargo build --release --target aarch64-apple-darwin
	lipo -create -output $@ \
		target/x86_64-apple-darwin/release/$(BIN) \
		target/aarch64-apple-darwin/release/$(BIN)
else
	cargo build --release
	cp target/release/$(BIN) $@
endif
	chmod +x $@

# .icns: build an iconset from the master PNG with sips, then iconutil it.
icon: $(ICNS)

$(ICNS): $(ICON_PNG) | $(BUILD_DIR)
	@rm -rf $(ICONSET)
	@mkdir -p $(ICONSET)
	@for s in 16 32 64 128 256 512 1024; do \
		sips -z $$s $$s $(ICON_PNG) --out $(ICONSET)/icon_$$s\x$$s.png >/dev/null; \
	done
	@cp $(ICONSET)/icon_32x32.png   $(ICONSET)/icon_16x16@2x.png
	@cp $(ICONSET)/icon_64x64.png   $(ICONSET)/icon_32x32@2x.png
	@cp $(ICONSET)/icon_256x256.png $(ICONSET)/icon_128x128@2x.png
	@cp $(ICONSET)/icon_512x512.png $(ICONSET)/icon_256x256@2x.png
	@cp $(ICONSET)/icon_1024x1024.png $(ICONSET)/icon_512x512@2x.png
	iconutil -c icns -o $@ $(ICONSET)

# Info.plist: regenerate every time so version / bundle id stay in sync
# with Cargo.toml without a separate template file.
$(PLIST): Cargo.toml Makefile | $(CONTENTS)
	@printf '%s\n' \
	'<?xml version="1.0" encoding="UTF-8"?>' \
	'<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
	'<plist version="1.0">' \
	'<dict>' \
	'    <key>CFBundleName</key><string>$(NAME)</string>' \
	'    <key>CFBundleDisplayName</key><string>$(NAME)</string>' \
	'    <key>CFBundleIdentifier</key><string>$(BUNDLE_ID)</string>' \
	'    <key>CFBundleVersion</key><string>$(VERSION)</string>' \
	'    <key>CFBundleShortVersionString</key><string>$(VERSION)</string>' \
	'    <key>CFBundleExecutable</key><string>$(BIN)</string>' \
	'    <key>CFBundleIconFile</key><string>$(NAME)</string>' \
	'    <key>CFBundlePackageType</key><string>APPL</string>' \
	'    <key>LSMinimumSystemVersion</key><string>11.0</string>' \
	'    <key>NSHighResolutionCapable</key><true/>' \
	'    <key>NSPrincipalClass</key><string>NSApplication</string>' \
	'</dict>' \
	'</plist>' \
	> $@

# Pack the bundle into a draggable disk image. Uses an internal symlink
# to /Applications so the user can drag-install on mount.
dmg: app
	@rm -f $(DMG)
	@mkdir -p $(BUILD_DIR)/dmg
	@rm -rf $(BUILD_DIR)/dmg/*
	@cp -R $(APP_DIR) $(BUILD_DIR)/dmg/
	@ln -s /Applications $(BUILD_DIR)/dmg/Applications
	hdiutil create -fs HFS+ -volname $(NAME) -srcfolder $(BUILD_DIR)/dmg \
		-format UDZO -ov $(DMG)
	@rm -rf $(BUILD_DIR)/dmg
	@echo "→ $(DMG)"

install: app
	rm -rf /Applications/$(NAME).app
	cp -R $(APP_DIR) /Applications/
	@echo "→ /Applications/$(NAME).app"

clean:
	rm -rf $(BUILD_DIR)

$(BUILD_DIR) $(MACOS_DIR) $(RES_DIR) $(CONTENTS):
	@mkdir -p $@
