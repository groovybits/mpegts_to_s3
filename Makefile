.PHONY: all clean setup install build container

CARGO_FEATURES = smoother

all: build

install:
	mkdir -p bin && \
		cp -f target/release/udp-to-hls hls-to-udp/target/release/hls-to-udp bin/

setup:
	sh scripts/setup_system.sh

build:
	cargo build --release && \
		cd hls-to-udp && cargo build --release --features="$(CARGO_FEATURES)"

clean:
	cargo clean
	cd hls-to-udp && cargo clean
	rm -rf bin
	rm -rf recording-playback-server/node_modules
	rm -f recording-playback-server/package-lock.json

distclean: clean
	rm -f recording-playback-server/media_jobs.db
	rm -f recording-playback-server/hourly_urls.log

container: clean
	podman-compose build


