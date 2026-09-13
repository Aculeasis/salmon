SHELL := bash

VERSION != git describe --dirty --tags --always
COMMIT != git rev-parse HEAD
DATE != date -u +"%Y-%m-%dT%H:%M:%SZ"

GOEXE :=
ifeq ($(OS),Windows_NT)
GOEXE := .exe
endif

LDFLAGS := -s -w \
	-X 'github.com/dimonomid/salmon/version.version=$(patsubst v%,%,$(VERSION))' \
	-X 'github.com/dimonomid/salmon/version.commit=$(COMMIT)' \
	-X 'github.com/dimonomid/salmon/version.date=$(DATE)' \
	-X 'github.com/dimonomid/salmon/version.builtBy=make'

.PHONY: all
# (for now intentionally skipping salmon-watch-2 since it's super heavy to
# build and is experimental)
all: clean salmon salmon-watch-legacy

.PHONY: test
test:
	go test --count 1 --race ./...
	node --test cmd/salmon-watch-legacy/jstest/*.js
	cargo test --manifest-path cmd/salmon-watch-2/Cargo.toml

.PHONY: generate
generate:
	go generate ./...

.PHONY: salmon
salmon: generate
	@echo Building bin/salmon$(GOEXE)
	@go build \
		-trimpath \
		-o bin/salmon$(GOEXE) \
		-ldflags "$(LDFLAGS)" \
		./cmd/salmon

.PHONY: salmon-watch-legacy
salmon-watch-legacy: generate
	@echo Building bin/salmon-watch-legacy$(GOEXE)
	@go build \
		-trimpath \
		-o bin/salmon-watch-legacy$(GOEXE) \
		-ldflags "$(LDFLAGS)" \
		./cmd/salmon-watch-legacy

.PHONY: salmon-watch-2
salmon-watch-2:
	@echo Building bin/salmon-watch-2$(GOEXE)
	@SALMON_WATCH_BUILD_VERSION='$(patsubst v%,%,$(VERSION))' \
		SALMON_WATCH_BUILD_COMMIT='$(COMMIT)' \
		SALMON_WATCH_BUILD_DATE='$(DATE)' \
		SALMON_WATCH_BUILT_BY='make' \
		cargo build --release --manifest-path cmd/salmon-watch-2/Cargo.toml
	@mkdir -p bin
	@cp cmd/salmon-watch-2/target/release/salmon-watch$(GOEXE) bin/salmon-watch-2$(GOEXE)

.PHONY: salmon-watch-2-debug
salmon-watch-2-debug:
	@echo Building bin/salmon-watch-2-debug$(GOEXE)
	@SALMON_WATCH_BUILD_VERSION='$(patsubst v%,%,$(VERSION))' \
		SALMON_WATCH_BUILD_COMMIT='$(COMMIT)' \
		SALMON_WATCH_BUILD_DATE='$(DATE)' \
		SALMON_WATCH_BUILT_BY='make' \
		cargo build --manifest-path cmd/salmon-watch-2/Cargo.toml
	@mkdir -p bin
	@cp cmd/salmon-watch-2/target/debug/salmon-watch$(GOEXE) bin/salmon-watch-2-debug$(GOEXE)

.PHONY: clean
clean:
	rm -rf bin

PREFIX ?= /usr/local
DESTDIR ?=
BINDIR := $(DESTDIR)$(PREFIX)/bin
INSTALL := install
INSTALL_FLAGS := -m 755

.PHONY: install
install: install-salmon install-salmon-watch-legacy

.PHONY: install-salmon
install-salmon:
	$(INSTALL) $(INSTALL_FLAGS) -D bin/salmon$(GOEXE) $(BINDIR)/salmon$(GOEXE)

.PHONY: install-salmon-watch-legacy
install-salmon-watch-legacy:
	$(INSTALL) $(INSTALL_FLAGS) -D bin/salmon-watch-legacy$(GOEXE) $(BINDIR)/salmon-watch-legacy$(GOEXE)
