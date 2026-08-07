# Cross compilation guide

## Setup zigbuild

Used to use `cross` for that, but with all the native static linking deps `cargo-zigbuild` was better and faster.

```sh
brew install zig
cargo install cargo-zigbuild
rustup target add x86_64-unknown-linux-gnu
rustup target add x86_64-pc-windows-gnu
rustup target add arm-unknown-linux-gnueabihf
```

Run these from the project root dir:

## Build Mac (Apple Silicon)

```sh
cargo build --release
```

## Build Linux x86_64

```sh
cargo zigbuild --target x86_64-unknown-linux-gnu --release
```

## Build Windows x86_64

```sh
cargo zigbuild --target x86_64-pc-windows-gnu --release
```

## Build ARM

```sh
cargo zigbuild --target arm-unknown-linux-gnueabihf --release
```

## Collect binaries

```sh
mkdir -p x_builds && cp target/release/ilert x_builds/ilert_mac && cp target/x86_64-unknown-linux-gnu/release/ilert x_builds/ilert_linux && cp target/x86_64-pc-windows-gnu/release/ilert.exe x_builds/ilert.exe && cp target/arm-unknown-linux-gnueabihf/release/ilert x_builds/ilert_arm
```