# nx. Pure Go, no cgo: every target builds from any OS.

export CGO_ENABLED = 0

APP     ?= ./cmd/nx
NAME    ?= nx
DIST    ?= dist
TARGETS ?= linux/amd64 linux/arm64
VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo dev)
LDFLAGS := -s -w -X github.com/Siddhj2206/nx/internal/version.Version=$(VERSION)

.PHONY: run test vet build build-all release clean

run:
	go run $(APP) $(ARGS)

test:
	go test ./...

vet:
	go vet ./...
	@test -z "$$(gofmt -l .)" || (gofmt -l .; echo "gofmt needed"; exit 1)

build:
	go build -trimpath -ldflags "$(LDFLAGS)" -o $(DIST)/$(NAME) $(APP)

build-all:
	@for t in $(TARGETS); do \
		os=$${t%/*}; arch=$${t#*/}; \
		echo "$$os/$$arch"; \
		GOOS=$$os GOARCH=$$arch go build -trimpath -ldflags "$(LDFLAGS)" \
			-o $(DIST)/$$os-$$arch/$(NAME) $(APP) || exit 1; \
	done

# Tarballs and SHA256SUMS for a GitHub release (ADR-0005).
release: build-all
	@cd $(DIST) && rm -f SHA256SUMS && for t in $(TARGETS); do \
		os=$${t%/*}; arch=$${t#*/}; tgz=$(NAME)-$(VERSION)-$$os-$$arch.tar.gz; \
		cp ../LICENSE $$os-$$arch/ && \
		tar --sort=name --owner=0 --group=0 --mtime='@0' -czf $$tgz -C $$os-$$arch $(NAME) LICENSE && \
		sha256sum $$tgz >> SHA256SUMS; \
	done; cat SHA256SUMS

clean:
	rm -rf $(DIST)
