# hh-tftpd design

<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- SPDX-FileCopyrightText: 2026 Howard M. Harte -->

For build and test entry points, see the [README](../README.md).

**Contents**

- [1. Purpose and scope](#1-purpose-and-scope)
- [2. Architecture](#2-architecture)
- [3. Command-line interface](#3-command-line-interface)
- [4. Protocol and filesystem contracts](#4-protocol-and-filesystem-contracts)
- [5. Verification and limits](#5-verification-and-limits)

## 1. Purpose and scope

`tftpd` serves binary boot images and optionally accepts board-produced files.
It is independent of FPGA families, CPU models and rig services. TFTP provides
neither authentication nor encryption; client-IP mappings select directories
and are not an authentication mechanism.

## 2. Architecture

One UDP listener receives requests. Each transfer runs on a separate thread
and connected UDP socket with an ephemeral server transfer identifier. The
listener dispatches RRQ reads to `serve` and permitted WRQ writes to `receive`.
Per-client directory selection occurs before spawning the transfer. `--once`
joins the first handled transfer and exits; it does not establish its success.
A refused upload does not consume `--once`, so the listener keeps running.

## 3. Command-line interface

All configured directories must already exist. Paths are canonicalized on startup.

| Option | Default | Contract |
|---|---|---|
| `--root DIR` | current directory | Default read tree. |
| `--upload DIR` | disabled | Default upload tree. |
| `--client IP=DIR` | none | Read tree for one client IP; other clients use `--root`. |
| `--client-upload IP=DIR` | none | Upload tree for one client IP; otherwise the default upload setting applies. |
| `--bind ADDR` | `0.0.0.0` | Listener IP address. |
| `--port N` | `69` | Listener UDP port. |
| `--once` | off | Exit after the first handled request/transfer. |
| `--quiet` | off | Suppress per-transfer progress; startup mappings are still printed. |
| `-h`, `--help` | — | Print usage and exit. |

Malformed or repeated IP mappings fail startup. A canonical upload directory
identical to any served root is refused. Operators must also keep the directory
trees disjoint: nested read/upload trees are not rejected by that equality check.

For example, from the repository root in Bash, with all directories present:

```sh
cargo run --locked --release -- --root /srv/boot/default --port 6969 \
  --client 192.0.2.10=/srv/boot/board-a \
  --client-upload 192.0.2.10=/srv/uploads/board-a
```

## 4. Protocol and filesystem contracts

Only `octet` transfer mode is supported; `netascii` and unknown modes are
refused. Transfers use stop-and-wait DATA/ACK, wrapping 16-bit block numbers.
A short final block terminates a transfer; exact block-size multiples require
an additional empty DATA block. Each socket accepts packets only from its client.

| Requested option | Behavior |
|---|---|
| `blksize` | Numeric values are clamped to 8–9296 bytes; default is 512. |
| `tsize` | Reads report file size; writes echo the client's supplied value. |
| `timeout` | Unsupported; omitted from OACK. Transfers retain the fixed 900 ms timer. |
| Other options | Omitted from OACK. |

The server declines timeout negotiation and uses its fixed retry interval.
When no requested option is supported, reads start with DATA 1 and uploads
with ACK 0, using the default block size and timer.

Each block has eight timed attempts with a fixed 900 ms deadline per attempt.
Unrelated packets do not reset that deadline. Duplicate ACKs do not retransmit
DATA, following [RFC 1123 section 4.2.3.1](https://www.rfc-editor.org/rfc/rfc1123#section-4.2.3.1).
Duplicate upload DATA is acknowledged again without being written twice.
This implementation targets embedded boot clients; it does not claim full RFC
compliance, use adaptive retransmission timers, or implement RFC 7440 windowing.

Reads reject parent traversal and platform path prefixes, canonicalize the
requested path, and require a regular file within the selected root. Leading
slashes are interpreted relative to that root, and backslashes are normalized.
Uploads validate the existing parent directory against the selected upload tree;
clients cannot create directories. Payload is written to a `.part` sidecar and
renamed after transfer and successful file synchronization. Interrupted transfers
close the file before attempting to remove that sidecar. A completed sidecar is
retained for operator recovery if synchronization or publication fails.

Sidecars are created exclusively. An existing sidecar, including a completed
recovery file, refuses a WRQ with TFTP error 6; it is never truncated by a new
request. Independent filenames can transfer concurrently. Upload names ending
in `.part` (case-insensitive), containing colons, or having path components
ending in a dot or space are refused to protect the sidecar namespace across
platforms. The server removes only a sidecar it successfully created.

The filesystem is operator-trusted. These checks do not provide a sandbox
against concurrent local path/symlink replacement. Final-ACK delivery
is not proof of durable publication: filesystem sync/rename errors are logged
after receipt. Replacement behavior also depends on host rename semantics.

Move a retained `.part` file aside before retrying that filename.

## 5. Verification and limits

Unit tests cover path traversal, new upload names, missing parents, empty names,
client mapping/fallback, malformed or duplicate mappings, and equal read/upload
roots, non-directory configuration paths, and truncated request fields. Loopback
process tests exercise byte-exact reads and uploads, option negotiation, lost and
duplicate acknowledgments, duplicate upload data, bounded retries despite unrelated
traffic, interrupted-upload cleanup, publication failure, and write refusal without
opening physical board interfaces.

Ownership regressions cover competing WRQs, independent concurrent uploads,
cancellation isolation and preserved recovery sidecars. Timeout-option tests
cover valid and invalid values, mixed-option OACKs, and RRQ/WRQ fallback with
no OACK when timeout is the only requested option.

The executable is used by the rig network-boot workflow. The standalone package's
host tests do not qualify every bootloader, operating system, firewall setup,
loss/reordering pattern, simultaneous-transfer load, or hostile filesystem race.
Keep deployment-specific network-boot and upload qualification separate.
