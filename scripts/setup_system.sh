#!/bin/bash
set -e

# check OS, only support AlmaLinux
if [ -f "/etc/centos-release" ]; then
    is_alma=$(grep -c AlmaLinux /etc/centos-release)
    if [ $is_alma -eq 0 ]; then
        echo "This script only supports AlmaLinux"
        exit 1
    fi
elif [ -f "/etc/os-release" ]; then
    is_alma=$(grep -c AlmaLinux /etc/os-release)
    if [ $is_alma -eq 0 ]; then
        echo "This script only supports AlmaLinux"
        exit 1
    fi
else
    echo "This script only supportsAlmaLinux"
    exit 1
fi

# Install required tools
echo "Installing dependencies..."
if [ -f "/usr/bin/yum" ]; then
    yum install -y -q dnf
fi
dnf groupinstall -yq "Development Tools"
dnf install -yq almalinux-release-synergy

dnf install -yq zlib-devel openssl-devel clang clang-devel \
    libpcap-devel nasm libzen-devel librdkafka-devel ncurses-devel \
    libdvbpsi-devel bzip2-devel libcurl-devel

# Install Rust if not already installed
if ! command -v cargo &> /dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source $HOME/.cargo/env
fi

