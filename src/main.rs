// SPDX-License-Identifier: MIT OR Apache-2.0
// SPDX-FileCopyrightText: 2026 Howard M. Harte
//! Standalone TFTP server for binary board boot images and optional uploads.
//!
//! The listener selects each client's read/upload directories, then dispatches
//! transfers to dedicated connected UDP sockets. Uploads are disabled by default.
//! Keep read and upload trees disjoint. Exclusive `.part` sidecars protect
//! uploads and recovery files. The local filesystem is operator-trusted.
//!
//! `doc/hh-tftpd-design.md` defines the CLI, option negotiation, and limits,
//! including the fixed retry timer and unsupported timeout negotiation.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

const OP_RRQ: u16 = 1;
const OP_WRQ: u16 = 2;
const OP_DATA: u16 = 3;
const OP_ACK: u16 = 4;
const OP_ERROR: u16 = 5;
const OP_OACK: u16 = 6;

const ERR_NOT_FOUND: u16 = 1;
const ERR_ACCESS: u16 = 2;
const ERR_ILLEGAL: u16 = 4;
const ERR_EXISTS: u16 = 6;

/// Big enough for a jumbo-framed `blksize` plus the 4-byte DATA header.
const MAX_DGRAM: usize = 9300;

/// How long to wait for an ACK before sending the block again.
const ACK_TIMEOUT: Duration = Duration::from_millis(900);
/// Maximum timed attempts for one block; unrelated packets do not extend them.
const MAX_ATTEMPTS: u32 = 8;

const USAGE: &str = "tftpd [--root DIR] [--upload DIR] [--client IP=DIR]... \
[--client-upload IP=DIR]... [--bind ADDR] [--port N] [--once] [--quiet]";

struct Args {
    root: PathBuf,
    /// Where an upload is allowed to land. `None` refuses every write, which
    /// is the default.
    upload: Option<PathBuf>,
    /// Per-board served roots, by the board's address.
    client_roots: HashMap<IpAddr, PathBuf>,
    /// Per-board upload directories, by the board's address.
    client_uploads: HashMap<IpAddr, PathBuf>,
    bind: IpAddr,
    port: u16,
    once: bool,
    quiet: bool,
}

impl Args {
    /// The tree a read from `ip` is served from.
    fn root_for(&self, ip: IpAddr) -> &Path {
        self.client_roots.get(&ip).unwrap_or(&self.root)
    }

    /// Where an upload from `ip` lands, if anywhere.
    fn upload_for(&self, ip: IpAddr) -> Option<&Path> {
        self.client_uploads
            .get(&ip)
            .or(self.upload.as_ref())
            .map(PathBuf::as_path)
    }
}

/// `192.168.0.42=C:\boot\40p` -> the address and the directory, not yet
/// canonicalized.
fn parse_mapping(flag: &str, v: Option<String>) -> Result<(IpAddr, PathBuf), String> {
    let v = v.ok_or_else(|| format!("{flag} needs IP=DIR"))?;
    let (ip, dir) = v
        .split_once('=')
        .ok_or_else(|| format!("{flag} {v}: expected IP=DIR"))?;
    let ip: IpAddr = ip.trim().parse().map_err(|e| format!("{flag} {v}: {e}"))?;
    if dir.is_empty() {
        return Err(format!("{flag} {v}: no directory"));
    }
    Ok((ip, PathBuf::from(dir)))
}

fn parse_args() -> Result<Args, String> {
    parse_args_from(std::env::args().skip(1))
}

/// Validate configuration paths before accepting any client requests.
fn canonicalize_directory(path: &Path, flag: &str) -> Result<PathBuf, String> {
    let directory =
        fs::canonicalize(path).map_err(|error| format!("{flag} {}: {error}", path.display()))?;
    if !directory.is_dir() {
        return Err(format!("{flag} {}: not a directory", path.display()));
    }
    Ok(directory)
}

fn parse_args_from(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut config = Args {
        root: PathBuf::from("."),
        upload: None,
        client_roots: HashMap::new(),
        client_uploads: HashMap::new(),
        bind: IpAddr::from([0, 0, 0, 0]),
        port: 69,
        once: false,
        quiet: false,
    };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--root" => config.root = PathBuf::from(it.next().ok_or("--root needs a directory")?),
            "--upload" => {
                config.upload = Some(PathBuf::from(
                    it.next().ok_or("--upload needs a directory")?,
                ))
            }
            "--client" | "--client-upload" => {
                let (ip, dir) = parse_mapping(&arg, it.next())?;
                let map = if arg == "--client" {
                    &mut config.client_roots
                } else {
                    &mut config.client_uploads
                };
                if map.insert(ip, dir).is_some() {
                    return Err(format!("{arg}: {ip} is mapped twice"));
                }
            }
            "--bind" => {
                config.bind = it
                    .next()
                    .ok_or("--bind needs an address")?
                    .parse()
                    .map_err(|e| format!("--bind: {e}"))?
            }
            "--port" => {
                config.port = it
                    .next()
                    .ok_or("--port needs a number")?
                    .parse()
                    .map_err(|e| format!("--port: {e}"))?
            }
            "--once" => config.once = true,
            "--quiet" => config.quiet = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    config.root = canonicalize_directory(&config.root, "--root")?;
    if let Some(upload) = &mut config.upload {
        *upload = canonicalize_directory(upload, "--upload")?;
    }
    for (ip, directory) in &mut config.client_roots {
        *directory = canonicalize_directory(directory, &format!("--client {ip}"))?;
    }
    for (ip, directory) in &mut config.client_uploads {
        *directory = canonicalize_directory(directory, &format!("--client-upload {ip}"))?;
    }
    // The whole point of a separate directory is that the files a board boots
    // from cannot be replaced by the board. Pointing an upload directory at a
    // served root gives that away silently, so refuse it instead -- across
    // boards too: one board must not be able to replace what another boots.
    let roots: Vec<&PathBuf> = std::iter::once(&config.root)
        .chain(config.client_roots.values())
        .collect();
    for u in config.upload.iter().chain(config.client_uploads.values()) {
        if roots.contains(&u) {
            return Err(format!(
                "upload directory {} is also a served root",
                u.display()
            ));
        }
    }
    Ok(config)
}

/// Split a NUL-terminated string off the front of a request.
fn take_cstr(buf: &[u8], at: &mut usize) -> Option<String> {
    let start = *at;
    let end = buf.get(start..)?.iter().position(|b| *b == 0)? + start;
    *at = end + 1;
    Some(String::from_utf8_lossy(&buf[start..end]).into_owned())
}

/// Resolve a requested name inside `root`, refusing anything that climbs out.
///
/// Check both path components and the canonical target: lexical validation
/// rejects traversal, while canonicalization catches existing symlinks that
/// point outside the tree. The local filesystem must remain operator-trusted.
fn resolve(root: &Path, name: &str) -> Result<PathBuf, (u16, String)> {
    let rel = name.replace('\\', "/");
    let rel = rel.trim_start_matches('/');
    let candidate = root.join(rel);

    // Refuse a climbing, absolute or drive-qualified name BEFORE touching the
    // filesystem. The containment check below is the backstop, but on Windows
    // it cannot be relied on to be the thing that fires: canonicalize() hands
    // back an extended-length `\?\` path, and Windows does not resolve `..`
    // inside those at all -- so a traversal fails as "no such path" by luck of
    // spelling rather than by decision. This makes the decision explicit, and
    // makes it the same decision on every platform.
    if Path::new(rel).components().any(|c| {
        matches!(
            c,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir
        )
    }) {
        return Err((ERR_ACCESS, format!("{name}: not inside the served root")));
    }

    let real = fs::canonicalize(&candidate).map_err(|e| (ERR_NOT_FOUND, format!("{name}: {e}")))?;
    if !real.starts_with(root) {
        return Err((ERR_ACCESS, format!("{name}: not inside the served root")));
    }
    if !real.is_file() {
        return Err((ERR_NOT_FOUND, format!("{name}: not a regular file")));
    }
    Ok(real)
}

/// Resolve a name an upload wants to create inside `dir`.
///
/// Unlike [`resolve`], this canonicalizes the existing parent directory because
/// the destination file may not exist yet. Clients cannot create directories.
fn resolve_for_write(dir: &Path, name: &str) -> Result<PathBuf, (u16, String)> {
    let rel = name.replace('\\', "/");
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() {
        return Err((ERR_ACCESS, format!("{name}: no file name")));
    }
    // Reserve the temporary-file namespace, including case-insensitive hosts.
    // Colons name Windows alternate streams; trailing dots/spaces can alias a
    // different name through platform APIs. Apply one portable upload policy.
    if rel
        .split('/')
        .any(|part| part.contains(':') || part.ends_with('.') || part.ends_with(' '))
        || rel.to_ascii_lowercase().ends_with(".part")
    {
        return Err((
            ERR_ACCESS,
            format!("{name}: reserved or nonportable upload name"),
        ));
    }
    if Path::new(rel).components().any(|c| {
        matches!(
            c,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir
        )
    }) {
        return Err((ERR_ACCESS, format!("{name}: not inside the upload root")));
    }
    let candidate = dir.join(rel);
    let leaf = match candidate.file_name() {
        Some(f) => f.to_os_string(),
        None => return Err((ERR_ACCESS, format!("{name}: no file name"))),
    };
    let parent = candidate.parent().unwrap_or(dir);
    // Subdirectories are served but not created: a client that can invent
    // directories can grow a tree on the workstation from a typo.
    let real = fs::canonicalize(parent).map_err(|e| (ERR_NOT_FOUND, format!("{name}: {e}")))?;
    if !real.starts_with(dir) {
        return Err((ERR_ACCESS, format!("{name}: not inside the upload root")));
    }
    Ok(real.join(leaf))
}

/// The socket is connected to the client before this is reached, so `send`
/// is both correct and portable; `send_to` on a connected UDP socket is an
/// error on some platforms.
fn send_error(sock: &UdpSocket, code: u16, msg: &str) {
    let mut pkt = Vec::with_capacity(msg.len() + 5);
    pkt.extend_from_slice(&OP_ERROR.to_be_bytes());
    pkt.extend_from_slice(&code.to_be_bytes());
    pkt.extend_from_slice(msg.as_bytes());
    pkt.push(0);
    let _ = sock.send(&pkt);
}

/// A fresh local port is what the protocol calls the server's transfer
/// identifier. Bind the unspecified address of the client's family, not the
/// client's own address -- that one is remote and cannot be bound.
fn transfer_socket(client: SocketAddr) -> Option<UdpSocket> {
    let any: IpAddr = if client.is_ipv4() {
        IpAddr::from([0, 0, 0, 0])
    } else {
        IpAddr::from([0u16; 8])
    };
    let sock = match UdpSocket::bind((any, 0)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tftpd: cannot open a transfer socket: {e}");
            return None;
        }
    };
    if let Err(error) = sock.connect(client) {
        eprintln!("tftpd: cannot connect transfer socket to {client}: {error}");
        return None;
    }
    Some(sock)
}

/// `octet` or nothing. netascii would mean CRLF translation: refuse rather than
/// move a kernel -- or a ZIP-framed capture -- with `0x0A` silently turned into
/// `0x0D 0x0A`, a corruption that looks like a bad image and not like a protocol
/// mistake.
fn mode_ok(sock: &UdpSocket, mode: &str) -> bool {
    if mode == "octet" {
        return true;
    }
    if mode == "netascii" {
        send_error(sock, ERR_ILLEGAL, "netascii would corrupt a binary");
    } else {
        send_error(sock, ERR_ILLEGAL, "only octet mode is served");
    }
    false
}

/// Read option pairs, lowercasing names and ignoring an incomplete final pair.
/// If a key is repeated, its last complete value wins.
fn take_opts(req: &[u8], at: &mut usize) -> HashMap<String, String> {
    let mut opts = HashMap::new();
    while let Some(k) = take_cstr(req, at) {
        match take_cstr(req, at) {
            Some(v) => {
                opts.insert(k.to_ascii_lowercase(), v);
            }
            None => break,
        }
    }
    opts
}

/// Encode an OACK for the options this transfer accepted.
fn oack(acked: &[(String, String)]) -> Vec<u8> {
    let mut pkt = Vec::new();
    pkt.extend_from_slice(&OP_OACK.to_be_bytes());
    for (k, v) in acked {
        pkt.extend_from_slice(k.as_bytes());
        pkt.push(0);
        pkt.extend_from_slice(v.as_bytes());
        pkt.push(0);
    }
    pkt
}

/// Accepted options are emitted in a stable order, independent of the request.
struct TransferOptions {
    block_size: usize,
    accepted: Vec<(String, String)>,
}

/// RRQs report `download_size`; WRQs echo the client's proposed transfer size.
/// Unsupported options, including `timeout`, are omitted from the OACK.
fn negotiate_options(
    opts: &HashMap<String, String>,
    download_size: Option<u64>,
) -> TransferOptions {
    let mut result = TransferOptions {
        block_size: 512,
        accepted: Vec::new(),
    };
    if let Some(size) = opts
        .get("blksize")
        .and_then(|value| value.parse::<usize>().ok())
    {
        result.block_size = size.clamp(8, MAX_DGRAM - 4);
        result
            .accepted
            .push(("blksize".into(), result.block_size.to_string()));
    }
    if let Some(size) = opts.get("tsize") {
        let size = download_size.map_or_else(|| size.clone(), |size| size.to_string());
        result.accepted.push(("tsize".into(), size));
    }
    result
}

fn ack(block: u16) -> Vec<u8> {
    let mut packet = OP_ACK.to_be_bytes().to_vec();
    packet.extend_from_slice(&block.to_be_bytes());
    packet
}

/// Send a stop-and-wait response and receive the next matching ACK or DATA.
///
/// Each attempt has a fixed deadline. Duplicate ACKs never retransmit DATA
/// (RFC 1123 section 4.2.3.1); duplicate upload DATA re-sends its previous ACK
/// without writing the payload again or extending the deadline.
fn exchange(
    sock: &UdpSocket,
    outgoing: &[u8],
    expected_opcode: u16,
    expected_block: u16,
    incoming: &mut [u8],
) -> io::Result<usize> {
    for _ in 0..MAX_ATTEMPTS {
        sock.send(outgoing)?;
        let deadline = Instant::now() + ACK_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            sock.set_read_timeout(Some(remaining))?;
            match sock.recv(incoming) {
                Ok(length) if length >= 4 => {
                    let opcode = u16::from_be_bytes([incoming[0], incoming[1]]);
                    let block = u16::from_be_bytes([incoming[2], incoming[3]]);
                    if opcode == OP_ERROR {
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            format!("client returned TFTP error {block}"),
                        ));
                    }
                    if opcode == expected_opcode && block == expected_block {
                        return Ok(length);
                    }
                    if expected_opcode == OP_DATA
                        && opcode == OP_DATA
                        && block == expected_block.wrapping_sub(1)
                    {
                        sock.send(outgoing)?;
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(error) => return Err(error),
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "transfer retry limit reached",
    ))
}

/// Receive complete blocks into a sidecar. Publication belongs to the caller.
fn receive_payload(
    sock: &UdpSocket,
    file: &mut fs::File,
    block_size: usize,
    mut reply: Vec<u8>,
) -> io::Result<u64> {
    let mut incoming = vec![0; block_size + 4];
    let mut expected_block = 1u16;
    let mut total = 0u64;
    loop {
        let length = exchange(sock, &reply, OP_DATA, expected_block, &mut incoming)?;
        let data = &incoming[4..length];
        if let Err(error) = file.write_all(data) {
            send_error(sock, ERR_ACCESS, &error.to_string());
            return Err(error);
        }
        total += data.len() as u64;
        reply = ack(expected_block);
        if data.len() < block_size {
            sock.send(&reply)?;
            return Ok(total);
        }
        expected_block = expected_block.wrapping_add(1);
    }
}

/// Serve one file to one client, on its own socket and its own port, which is
/// what the protocol means by a transfer identifier.
fn serve(root: &Path, client: SocketAddr, req: &[u8], quiet: bool) {
    let mut at = 2usize;
    let name = match take_cstr(req, &mut at) {
        Some(s) => s,
        None => return,
    };
    let mode = take_cstr(req, &mut at)
        .unwrap_or_default()
        .to_ascii_lowercase();

    let sock = match transfer_socket(client) {
        Some(s) => s,
        None => return,
    };
    if !mode_ok(&sock, &mode) {
        return;
    }

    let path = match resolve(root, &name) {
        Ok(p) => p,
        Err((code, msg)) => {
            if !quiet {
                println!("tftpd: {client} asked for {name}: refused ({msg})");
            }
            send_error(&sock, code, &msg);
            return;
        }
    };

    let mut file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            send_error(&sock, ERR_NOT_FOUND, &e.to_string());
            return;
        }
    };
    let size = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            send_error(&sock, ERR_ACCESS, &error.to_string());
            return;
        }
    };

    let options = negotiate_options(&take_opts(req, &mut at), Some(size));
    let blksize = options.block_size;

    if !quiet {
        println!(
            "tftpd: {client} <- {} ({size} bytes, blksize {blksize})",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
    }

    let started = Instant::now();
    let mut rx = [0u8; 1024];

    // An OACK is itself acknowledged with block 0, and the transfer does not
    // start until that arrives -- otherwise a client that rejected the options
    // would be sent data in a block size it never agreed to.
    if !options.accepted.is_empty() {
        if let Err(error) = exchange(&sock, &oack(&options.accepted), OP_ACK, 0, &mut rx) {
            if !quiet {
                eprintln!("tftpd: {client} did not accept the options: {error}");
            }
            return;
        }
    }

    let mut buf = vec![0u8; blksize + 4];
    let mut block: u16 = 1;
    let mut offset: u64 = 0;
    loop {
        let mut got = 0usize;
        while got < blksize {
            match file.read(&mut buf[4 + got..4 + blksize]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    send_error(&sock, ERR_ACCESS, &e.to_string());
                    return;
                }
            }
        }
        buf[0..2].copy_from_slice(&OP_DATA.to_be_bytes());
        buf[2..4].copy_from_slice(&block.to_be_bytes());

        if let Err(error) = exchange(&sock, &buf[..4 + got], OP_ACK, block, &mut rx) {
            if !quiet {
                eprintln!("tftpd: {client} did not acknowledge block {block}: {error}");
            }
            return;
        }

        offset += got as u64;
        // A transfer ends on a block SHORTER than the block size, so a file
        // that is an exact multiple needs one final empty block.
        if got < blksize {
            break;
        }
        block = block.wrapping_add(1);
    }

    if !quiet {
        let secs = started.elapsed().as_secs_f64();
        let rate = if secs > 0.0 {
            offset as f64 / secs / 1024.0
        } else {
            0.0
        };
        println!("tftpd: {client} <- {offset} bytes in {secs:.1}s ({rate:.0} KiB/s)");
    }
}

/// Receive one file from one client into `dir`.
///
/// The mirror image of [`serve`]: the server acknowledges, the client sends
/// `DATA`, and a block shorter than the block size ends it.
fn receive(dir: &Path, client: SocketAddr, req: &[u8], quiet: bool) {
    let mut at = 2usize;
    let name = match take_cstr(req, &mut at) {
        Some(s) => s,
        None => return,
    };
    let mode = take_cstr(req, &mut at)
        .unwrap_or_default()
        .to_ascii_lowercase();

    let sock = match transfer_socket(client) {
        Some(s) => s,
        None => return,
    };
    if !mode_ok(&sock, &mode) {
        return;
    }

    let path = match resolve_for_write(dir, &name) {
        Ok(p) => p,
        Err((code, msg)) => {
            if !quiet {
                println!("tftpd: {client} tried to write {name}: refused ({msg})");
            }
            send_error(&sock, code, &msg);
            return;
        }
    };

    let options = negotiate_options(&take_opts(req, &mut at), None);
    let blksize = options.block_size;

    // Exclusive creation owns this sidecar until cleanup or publication. A
    // competing WRQ or a retained recovery file must never be truncated.
    let mut tmp = path.clone().into_os_string();
    tmp.push(".part");
    let tmp = PathBuf::from(tmp);
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
    {
        Ok(f) => f,
        Err(e) => {
            let code = if e.kind() == io::ErrorKind::AlreadyExists {
                ERR_EXISTS
            } else {
                ERR_ACCESS
            };
            send_error(&sock, code, &e.to_string());
            if !quiet {
                println!("tftpd: cannot create {}: {e}", tmp.display());
            }
            return;
        }
    };

    if !quiet {
        println!(
            "tftpd: {client} -> {} (blksize {blksize})",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
    }

    let started = Instant::now();
    // The OACK replaces the initial ACK 0 when options were accepted.
    let reply = if options.accepted.is_empty() {
        ack(0)
    } else {
        oack(&options.accepted)
    };
    let total = match receive_payload(&sock, &mut file, blksize, reply) {
        Ok(total) => total,
        Err(error) => {
            // Close the file before unlinking it, including on Windows.
            drop(file);
            let _ = fs::remove_file(&tmp);
            if !quiet {
                eprintln!("tftpd: {client} upload of {name} failed: {error}");
            }
            return;
        }
    };

    if let Err(e) = file.sync_all() {
        eprintln!("tftpd: cannot sync {}: {e}", tmp.display());
        return;
    }
    drop(file);
    if let Err(e) = fs::rename(&tmp, &path) {
        eprintln!("tftpd: cannot rename into {}: {e}", path.display());
        return;
    }
    if !quiet {
        let secs = started.elapsed().as_secs_f64();
        let rate = if secs > 0.0 {
            total as f64 / secs / 1024.0
        } else {
            0.0
        };
        println!("tftpd: {client} -> {total} bytes in {secs:.1}s ({rate:.0} KiB/s)");
    }
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("tftpd: {e}");
            std::process::exit(2);
        }
    };

    let sock = match UdpSocket::bind((args.bind, args.port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tftpd: cannot bind {}:{}: {e}", args.bind, args.port);
            if args.port < 1024 {
                eprintln!("tftpd: port {} may need Administrator", args.port);
            }
            std::process::exit(1);
        }
    };
    println!(
        "tftpd: serving {} on {}:{} (read only)",
        args.root.display(),
        args.bind,
        args.port
    );
    if let Some(u) = &args.upload {
        println!("tftpd: uploads accepted into {}", u.display());
    }
    let mut ips: Vec<&IpAddr> = args
        .client_roots
        .keys()
        .chain(args.client_uploads.keys())
        .collect();
    ips.sort();
    ips.dedup();
    for ip in ips {
        let up = match args.upload_for(*ip) {
            Some(u) => u.display().to_string(),
            None => "refused".to_string(),
        };
        println!(
            "tftpd: {ip} is served {} (uploads: {up})",
            args.root_for(*ip).display()
        );
    }

    let mut req = [0u8; MAX_DGRAM];
    loop {
        let (n, from) = match sock.recv_from(&mut req) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("tftpd: recv: {e}");
                continue;
            }
        };
        if n < 4 {
            continue;
        }
        match u16::from_be_bytes([req[0], req[1]]) {
            OP_RRQ => {
                let root = args.root_for(from.ip()).to_path_buf();
                let pkt = req[..n].to_vec();
                let quiet = args.quiet;
                let h = std::thread::spawn(move || serve(&root, from, &pkt, quiet));
                if args.once {
                    let _ = h.join();
                    return;
                }
            }
            OP_WRQ => match args.upload_for(from.ip()) {
                Some(dir) => {
                    let dir = dir.to_path_buf();
                    let pkt = req[..n].to_vec();
                    let quiet = args.quiet;
                    let h = std::thread::spawn(move || receive(&dir, from, &pkt, quiet));
                    if args.once {
                        let _ = h.join();
                        return;
                    }
                }
                None => {
                    // Answering rather than ignoring: a client that gets silence
                    // retries for a minute before saying anything useful.
                    if let Ok(s) = UdpSocket::bind((args.bind, 0)) {
                        if s.connect(from).is_ok() {
                            send_error(&s, ERR_ACCESS, "this server is read only");
                        }
                    }
                    if !args.quiet {
                        println!("tftpd: {from} tried to write; refused (no --upload directory)");
                    }
                }
            },
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::Deref;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDirectory(PathBuf);

    impl Deref for TestDirectory {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tmpdir(tag: &str) -> TestDirectory {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("tftpd-test-{tag}-{}-{unique}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        TestDirectory(fs::canonicalize(directory).unwrap())
    }

    #[test]
    fn configuration_paths_must_be_directories() {
        let directory = tmpdir("not-directory");
        let file = directory.join("file");
        fs::write(&file, b"not a directory").unwrap();
        let root = directory.to_string_lossy().into_owned();
        let file = file.to_string_lossy().into_owned();
        for arguments in [
            argv(&["--root", &file]),
            argv(&["--root", &root, "--upload", &file]),
            argv(&["--root", &root, "--client", &format!("192.0.2.1={file}")]),
            argv(&[
                "--root",
                &root,
                "--client-upload",
                &format!("192.0.2.1={file}"),
            ]),
        ] {
            let error = parse_args_from(arguments)
                .err()
                .expect("file path was accepted");
            assert!(error.contains("not a directory"), "{error}");
        }
    }

    #[test]
    fn truncated_request_fields_do_not_advance_the_cursor() {
        let mut cursor = 0;
        assert!(take_cstr(b"unterminated", &mut cursor).is_none());
        assert_eq!(cursor, 0);
        cursor = 100;
        assert!(take_cstr(b"short\0", &mut cursor).is_none());
        assert_eq!(cursor, 100);
    }

    /// A name that does not exist yet must resolve -- an upload's whole point.
    /// ⚠ The read-side `resolve` canonicalises the FILE and so returns
    /// NOT_FOUND here; that difference is the reason the two are separate.
    #[test]
    fn a_new_name_resolves_for_writing_and_not_for_reading() {
        let d = tmpdir("new");
        let p = resolve_for_write(&d, "capture.sr").expect("a new name is writable");
        assert_eq!(p, d.join("capture.sr"));
        assert!(resolve(&d, "capture.sr").is_err());
    }

    /// Climbing out is refused before the filesystem is touched, with the same
    /// component test the read side uses.
    #[test]
    fn a_climbing_name_is_refused() {
        let d = tmpdir("climb");
        for name in ["../escaped.sr", r"..\escaped.sr", "sub/../../escaped.sr"] {
            let e = resolve_for_write(&d, name);
            assert!(e.is_err(), "{name} was accepted");
        }
        assert!(!d.parent().unwrap().join("escaped.sr").exists());
    }

    #[test]
    fn leading_slashes_and_backslashes_are_relative_to_the_selected_root() {
        let directory = tmpdir("root-relative");
        fs::create_dir(directory.join("sub")).unwrap();
        let path = directory.join("sub/boot.bin");
        fs::write(&path, b"boot image").unwrap();
        for name in ["/sub/boot.bin", r"\sub\boot.bin", "sub/boot.bin"] {
            assert_eq!(resolve(&directory, name).unwrap(), path);
            assert_eq!(resolve_for_write(&directory, name).unwrap(), path);
        }
    }

    /// Subdirectories are served but never created: a typo must not grow a
    /// tree on the workstation.
    #[test]
    fn a_missing_subdirectory_is_not_created() {
        let d = tmpdir("subdir");
        assert!(resolve_for_write(&d, "nope/capture.sr").is_err());
        assert!(!d.join("nope").exists());

        fs::create_dir_all(d.join("yes")).unwrap();
        let p = resolve_for_write(&d, "yes/capture.sr").expect("an existing subdirectory works");
        assert_eq!(p, d.join("yes").join("capture.sr"));
    }

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// A mapped board gets its own tree for reads and uploads; every other
    /// address keeps the defaults.
    #[test]
    fn a_mapped_client_gets_its_own_root_and_upload() {
        let d = tmpdir("map");
        for sub in ["root", "up", "root40", "up40"] {
            fs::create_dir_all(d.join(sub)).unwrap();
        }
        let s = |sub: &str| d.join(sub).to_string_lossy().into_owned();
        let a = parse_args_from(argv(&[
            "--root",
            &s("root"),
            "--upload",
            &s("up"),
            "--client",
            &format!("192.168.0.42={}", s("root40")),
            "--client-upload",
            &format!("192.168.0.42={}", s("up40")),
        ]))
        .expect("parses");
        let mapped: IpAddr = "192.168.0.42".parse().unwrap();
        let other: IpAddr = "192.168.0.43".parse().unwrap();
        assert_eq!(a.root_for(mapped), d.join("root40"));
        assert_eq!(a.upload_for(mapped), Some(d.join("up40").as_path()));
        assert_eq!(a.root_for(other), d.join("root"));
        assert_eq!(a.upload_for(other), Some(d.join("up").as_path()));
    }

    /// A board with a root but no upload mapping uploads where the rest do.
    #[test]
    fn a_client_without_an_upload_mapping_uses_the_default() {
        let d = tmpdir("noup");
        for sub in ["root", "root40"] {
            fs::create_dir_all(d.join(sub)).unwrap();
        }
        let s = |sub: &str| d.join(sub).to_string_lossy().into_owned();
        let a = parse_args_from(argv(&[
            "--root",
            &s("root"),
            "--client",
            &format!("10.0.0.5={}", s("root40")),
        ]))
        .expect("parses");
        let ip: IpAddr = "10.0.0.5".parse().unwrap();
        assert_eq!(a.root_for(ip), d.join("root40"));
        assert_eq!(a.upload_for(ip), None);
    }

    /// No upload directory may be a served root, the default's or a board's.
    #[test]
    fn an_upload_directory_that_is_any_served_root_is_refused() {
        let d = tmpdir("overlap");
        for sub in ["root", "root40"] {
            fs::create_dir_all(d.join(sub)).unwrap();
        }
        let s = |sub: &str| d.join(sub).to_string_lossy().into_owned();
        let cases = [
            argv(&["--root", &s("root"), "--upload", &s("root")]),
            argv(&[
                "--root",
                &s("root"),
                "--client",
                &format!("10.0.0.5={}", s("root40")),
                "--upload",
                &s("root40"),
            ]),
            argv(&[
                "--root",
                &s("root"),
                "--client-upload",
                &format!("10.0.0.5={}", s("root")),
            ]),
        ];
        for args in cases {
            assert!(
                parse_args_from(args.clone()).is_err(),
                "{args:?} was accepted"
            );
        }
    }

    /// A malformed or repeated mapping is an error, not a silent default.
    #[test]
    fn a_bad_mapping_is_refused() {
        let d = tmpdir("bad");
        let r = d.to_string_lossy().into_owned();
        for m in ["nodir", "notanip=C:\\x", "10.0.0.5="] {
            let e = parse_args_from(argv(&["--root", &r, "--client", m]));
            assert!(e.is_err(), "{m} was accepted");
        }
        let twice = format!("10.0.0.5={r}");
        assert!(parse_args_from(argv(&["--client", &twice, "--client", &twice])).is_err());
    }

    /// An empty name is a request for the directory itself.
    #[test]
    fn an_empty_name_is_refused() {
        let d = tmpdir("empty");
        assert!(resolve_for_write(&d, "").is_err());
        assert!(resolve_for_write(&d, "/").is_err());
    }

    #[test]
    fn upload_names_cannot_alias_sidecars_or_platform_streams() {
        let d = tmpdir("reserved-upload");
        for name in [
            "capture.bin.part",
            "capture.bin.PART",
            "capture.bin.part.",
            "capture.bin.part ",
            "capture.bin.part:stream",
            "ordinary:stream",
        ] {
            assert!(resolve_for_write(&d, name).is_err(), "accepted {name}");
        }
        assert!(resolve_for_write(&d, "capture.partial").is_ok());
    }

    #[test]
    fn unsupported_timeout_values_are_never_acknowledged() {
        for value in ["0", "1", "9", "255", "256", "4294967295", "invalid"] {
            let options = HashMap::from([("timeout".into(), value.into())]);
            for size in [None, Some(123)] {
                let negotiated = negotiate_options(&options, size);
                assert!(negotiated.accepted.is_empty(), "acknowledged {value}");
                assert_eq!(negotiated.block_size, 512);
            }
        }
    }
}
