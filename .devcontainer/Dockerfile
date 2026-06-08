# Devcontainer Dockerfile using Rust devcontainers base with preinstalled Rust
FROM mcr.microsoft.com/devcontainers/rust:1

# Install system dependencies for HID support and Windows cross-compilation
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       pkg-config libudev-dev libusb-1.0-0-dev libhidapi-dev cmake git \
       gcc-mingw-w64-x86-64 \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-pc-windows-gnu

# Default workdir (the devcontainer will bind-mount the workspace here)
WORKDIR /workspace

CMD ["/bin/bash"]
