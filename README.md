# hh-tftpd

<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- SPDX-FileCopyrightText: 2026 Howard M. Harte -->

A standalone Rust TFTP server for FPGA and embedded-board network boot,
with optional uploads and per-client directory mappings. The executable is
named `tftpd`.

## Documentation

| Document | Contents |
|---|---|
| [Design](doc/hh-tftpd-design.md) | Protocol behavior, file handling, options, and verification limits. |

## Repository layout

| Path | Contents |
|---|---|
| `src/main.rs` | Server, command-line parsing, and unit tests. |
| `tests/` | Loopback protocol regression. |
| `doc/` | Design and operating contracts. |
| `.githooks/` | Formatting, Clippy, Rustdoc, and test gate. |

## Requirements

Stable Rust/Cargo with `rustfmt` and `clippy`. The server uses only Rust's
standard library: no external crates, sibling repositories, or native libraries
are required. The binary is built from this repository, not published to crates.io.

## Getting started

From the repository root, with an existing directory of boot files:

```sh
cargo build --locked --release
cargo run --locked --release -- --root /path/to/boot --bind 127.0.0.1 --port 6969
```

For a physical board, select the host's reachable network address with `--bind`
and configure the client for the same port. The default is `0.0.0.0:69`.
On Windows, the executable is `target/release/tftpd.exe`; on Unix it is
`target/release/tftpd`. The [design](doc/hh-tftpd-design.md#3-command-line-interface)
defines uploads, client mappings and other options.

TFTP has no authentication or encryption. Run it on a trusted bench network
and allow only the intended network interfaces/clients through the firewall.
Uploads are disabled unless an upload directory is selected. Keep upload and
served directory trees disjoint. An active or retained `.part` sidecar refuses
another upload to that filename. Move recovery files aside before retrying.
The [protocol and filesystem contracts](doc/hh-tftpd-design.md#4-protocol-and-filesystem-contracts)
define option fallback, upload ownership and remaining deployment limits.

## Verification

From the repository root:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo doc --locked --no-deps
cargo test --locked
```

Enable the same checks at commit time with
`git config core.hooksPath .githooks`. Tests use temporary directories and
loopback UDP ports; they do not access physical boards or the live TFTP service.
A successful test exits zero. The [verification scope](doc/hh-tftpd-design.md#5-verification-and-limits)
distinguishes unit/protocol checks from deployment qualification.

## License

Licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE),
at your option. Original attribution is retained in the source.
