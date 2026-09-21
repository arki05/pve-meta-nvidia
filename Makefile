PREFIX ?= /usr
DESTDIR ?=
CARGO ?= cargo

.PHONY: build test check deb install clean

build:
	$(CARGO) build --release --locked

test:
	$(CARGO) test --locked

check:
	$(CARGO) fmt --check
	$(CARGO) clippy --all-targets --locked -- -D warnings
	$(CARGO) test --locked

install: build
	install -D -m 0755 target/release/pve-meta-nvidia $(DESTDIR)$(PREFIX)/sbin/pve-meta-nvidia
	install -D -m 0644 pve-meta-nvidia.service $(DESTDIR)$(PREFIX)/lib/systemd/system/pve-meta-nvidia.service

deb:
	dpkg-buildpackage -b -us -uc

clean:
	$(CARGO) clean
	rm -rf debian/pve-meta-nvidia debian/.debhelper debian/files debian/*.substvars debian/*.log debian/debhelper-build-stamp
