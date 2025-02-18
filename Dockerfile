### BUILD Development Container
FROM almalinux:8.8 as builder

ARG ENABLE_DEBUG
ENV ENABLE_DEBUG=${ENABLE_DEBUG:-false}

## Base probe app in /app directory
WORKDIR /app

## Build Deps

# Update system and handle GPG key update
RUN set -eux; \
    # Import the AlmaLinux GPG key
    rpm --import https://repo.almalinux.org/almalinux/RPM-GPG-KEY-AlmaLinux && \
    # Clean up the DNF cache and update packages
    dnf clean all && \
    dnf makecache && \
    dnf update -yq && \
    # Clean up to reduce image size
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

# avoid resource issues
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

## Set up the environment for the RPM probes location of the binaries
ENV PATH="/app/bin:${PATH}"

## Alma Release Synergy GRPC and Protobuf
RUN microdnf update -y > /dev/null
RUN microdnf install -y libpcap  > /dev/null

WORKDIR /app/hls

## Probe binaries and tools / scripts
COPY --from=builder /app/bin/hls-to-udp /app/bin/hls-to-udp
COPY --from=builder /app/bin/udp-to-hls /app/bin/udp-to-hls

COPY scripts/entrypoint_udp-to-hls.sh /app/entrypoint_udp-to-hls.sh
COPY scripts/entrypoint_hls-to-udp.sh /app/entrypoint_hls-to-udp.sh

# Make directory for eBPF program binary
RUN rm -rf /usr/share/*
RUN rm -rf /usr/libexec/*
RUN ldconfig 2> /dev/null

## Clean up and save space
RUN rm -rf /var/cache/* && \
        rm -rf /usr/lib*/lib*.a && \
        rm -rf /usr/lib*/lib*.la && \
        rm -rf /usr/lib/.build-id;

RUN microdnf clean all > /dev/null && rm -rf /var/cache/yum

# remove unnecessary files
RUN cd /usr/bin && \
    rm -f addgnupghome agetty alias ambiguous_words arch aserver awk \
        b2sum base32 base64 bashbug* bg brotli bunzip2 busctl bzcat \
        bzcmp bzdiff bzegrep bzfgrep bzgrep bzip2* bzless bzmore \
        ca-legacy cairo-sphinx cal captoinfo catchsegv cd chacl \
        chage chcon chgrp chmem chmod chown chrt cksum clear cmp \
        cntraining col colcrt colrm column combine_* comm coredumpctl \
        csplit curl cut date dawg2wordlist db_* dbus-* dd debuginfod-find \
        df diff* dir* dmesg dnf-3 du dumpiso \
        fc* fgrep fincore fmt fold fribidi g13 gawk gdbm* gdbus gencat \
        getconf getent getfacl getopt getopts gio* glib* glx* \
        gpasswd gpg* gr2fonttest grep grpc_* gst-* gunzip gzexe gzip \
        head hexdump hostid i386 iconv id info* ionice ipc* join journalctl \
        last* ld.so ldd libinput* libwacom* link linux* look lstmeval \
        lstmtraining make* mc* md5sum merge* mftraining modulemd* more mount* \
        namei newgid* newgrp newuid* nice nl nohup nproc nsenter numfmt od \
        openssl orc* pango* paste pathchk pinky pldd pr printenv printf \
        protoc ptx pwd* python* raw read* realpath rename resolvectl \
        runcon script* sdiff sed sendiso seq set_unicharset setarch && \
    cd /usr/sbin && rm -f * && \
    # Remove development files and libraries
    rm -rf /usr/lib/debug/* \
           /usr/lib/games/* \
           /usr/lib/fontconfig/* \
           /usr/lib/environment.d/* \
           /usr/include/* \
           /usr/lib64/gcc-* \
           /usr/lib64/cmake/* \
           /usr/lib64/X11/* \
           /usr/lib64/libQt5* \
           /usr/lib64/gconv/* \
           /usr/lib/.build-id/* \
           /usr/lib64/.build-id/* && \
    # Remove documentation, locale, and other non-essential data
    rm -rf /usr/share/doc/* \
           /usr/share/info/* \
           /usr/share/man/* \
           /usr/share/gnupg/* \
           /usr/share/i18n/* \
           /usr/share/backgrounds/* \
           /usr/share/icons/* \
           /usr/share/fonts/* \
           /usr/share/cracklib/* \
           /usr/share/applications/* \
           /usr/share/locale/* \
           /usr/share/X11/* && \
    # Remove package management and temp files
    rm -rf /var/cache/* \
           /var/log/* \
           /tmp/* \
           /var/tmp/* && \
    # Remove non-essential system files
    rm -rf /usr/games \
           /usr/local/share/* \
           /usr/local/man/* \
           /usr/local/doc/* && \
    # Final cleanup
    microdnf clean all && \
    rm -rf /var/cache/dnf && \
    rm -rf /var/lib/rpm

# Library Cleanup - preserve only needed libraries
RUN cd /usr/lib && \
    rm -rf binfmt.d environment.d debug fontconfig games && \
    cd /usr/lib64 && \
    rm -f libasound* libpulse* libjack* libsndfile* \
        libicu* libsqlite* libgirepository* \
        libgphoto* libgpm* libgusb* libmagic* \
        libmozjs* libwrap* libxslt* libgst* && \
    rm -rf /usr/lib/.build-id/* \
           /usr/lib64/.build-id/* \
           /usr/lib64/gstreamer-1.0/*

## Quick confirmation the binary executes
RUN hls-to-udp -V
RUN udp-to-hls -V

