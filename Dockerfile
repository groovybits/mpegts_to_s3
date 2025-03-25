### BUILD Development Container
FROM almalinux:8.8 as builder

ARG ENABLE_DEBUG
ENV ENABLE_DEBUG=${ENABLE_DEBUG:-false}

## Base probe app in /app directory
WORKDIR /app

## Build Deps

# Update system and handle GPG key update
RUN set -eux; \
    rpm --import https://repo.almalinux.org/almalinux/RPM-GPG-KEY-AlmaLinux && \
    dnf clean all && \
    dnf makecache && \
    dnf update -yq && \
    dnf clean all

RUN dnf install -yq almalinux-release-synergy
RUN dnf groupinstall -yq "Development Tools"
RUN dnf install -yq gcc-toolset-13 scl-utils

RUN dnf install -yq zlib-devel openssl-devel clang clang-devel libpcap-devel nasm \
    libzen-devel librdkafka-devel ncurses-devel libdvbpsi-devel bzip2-devel libcurl-devel;

## Copy files needed for builds
COPY Makefile .
COPY Cargo.toml .
COPY src/main.rs src/main.rs
COPY hls-to-udp/src/main.rs hls-to-udp/src/main.rs
COPY hls-to-udp/Cargo.toml hls-to-udp/Cargo.toml

## Install Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

ARG JOBS
ENV CPUS=4
ENV JOBS=${JOBS:-${CPUS}}
ENV CARGO_BUILD_JOBS=${JOBS}

## Build programs
RUN scl enable gcc-toolset-13 -- make build -j $(nproc) && \
    scl enable gcc-toolset-13 -- make install -j $(nproc) 

## Strip binaries in /app/bin/
RUN strip /app/bin/*

###
### END OF BUILD Development Container

###
### START OF Runtime Container
FROM almalinux:8-minimal as binary

## Add the built binaries to the PATH
ENV PATH="/app/bin:${PATH}"

## Set up nvm directory and environment variable
RUN mkdir -p /app/.nvm
ENV NVM_DIR="/app/.nvm"

## Update system and install dependencies needed for nvm and node
RUN microdnf update -y > /dev/null && \
    microdnf install -y libpcap findutils curl tar gzip

## Install nvm and use it to install Node.js 20, then symlink the node binaries for runtime
RUN curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.2/install.sh | bash && \
    bash -c "source \$NVM_DIR/nvm.sh && \
    nvm install 20 && \
    nvm alias default 20 && \
    nvm use default && \
    mkdir -p /app/bin && \
    ln -sf \$(nvm which default) /app/bin/node && \
    ln -sf \$(dirname \$(nvm which default))/npm /app/bin/npm && \
    ln -sf \$(dirname \$(nvm which default))/npx /app/bin/npx"

## Copy application binaries and scripts from the builder
COPY --from=builder /app/bin/hls-to-udp /app/bin/hls-to-udp
COPY --from=builder /app/bin/udp-to-hls /app/bin/udp-to-hls

COPY scripts/entrypoint_udp-to-hls.sh /app/entrypoint_udp-to-hls.sh
COPY scripts/entrypoint_hls-to-udp.sh /app/entrypoint_hls-to-udp.sh
COPY scripts/entrypoint_recording-playback-server.sh /app/entrypoint_recording-playback-server.sh

RUN mkdir -p /app/recording-playback-server/hls
WORKDIR /app/recording-playback-server
ENV HLS_DIR=""
ENV SWAGGER_FILE="/app/recording-playback-server/swagger.yaml"
ENV HOURLY_URLS_LOG="/app/recording-playback-server/hls/index.txt"
COPY recording-playback-server/package.json /app/recording-playback-server/package.json
COPY recording-playback-server/server.js /app/recording-playback-server/server.js
COPY recording-playback-server/.env.example /app/recording-playback-server/.env
COPY recording-playback-server/.env.example /app/recording-playback-server/hls/.env
COPY recording-playback-server/swagger.yaml ${SWAGGER_FILE}

## Install recording-playback-server dependencies
RUN npm install
WORKDIR /app/recording-playback-server/hls

RUN ldconfig 2> /dev/null

## Clean up to save space
RUN rm -rf /var/cache/* && \
    rm -rf /usr/lib*/lib*.a && \
    rm -rf /usr/lib*/lib*.la && \
    rm -rf /usr/lib/.build-id && \
    microdnf clean all > /dev/null && rm -rf /var/cache/yum && \
    rm -rf /var/cache/* /var/log/* /tmp/* /var/tmp/* && \
    rm -rf /usr/games /usr/local/man/* /usr/local/doc/* && \
    microdnf clean all && \
    rm -rf /var/cache/dnf && \
    rm -rf /var/lib/rpm

## Quick confirmation that the binaries execute correctly
RUN hls-to-udp -V
RUN udp-to-hls -V
RUN node --version
RUN npm --version