.PHONY: build check fmt install-local install clean

build:
	cargo build --release

check:
	cargo check

fmt:
	cargo fmt --all

install-local: build
	mkdir -p ~/.local/bin
	cp target/release/cx ~/.local/bin/
	@echo "已安装: ~/.local/bin/cx"

install: build
	sudo cp target/release/cx /usr/local/bin/
	@echo "已安装: /usr/local/bin/cx"

clean:
	cargo clean
	rm -f ~/.local/bin/cx 2>/dev/null || true
