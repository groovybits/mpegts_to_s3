.PHONY: all clean setup install build manager_image agent_image compose_build compose run

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
	rm -rf recording-playback-api/node_modules

distclean: clean
	rm -f recording-playback-api/hls
	rm -f hls/*
	rm -rf data/* data/.minio.sys

run: build
	podman-compose -f docker-compose_api_server.yaml up -d

compose_build: clean manager_image agent_image
	podman-compose -f docker-compose_api_server.yaml build

manager_image: clean
	podman build -t localhost/manager:latest -f Dockerfile.manager .

agent_image: clean
	podman build -t localhost/agent:latest -f Dockerfile.agent .
