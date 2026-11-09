// SPDX-License-Identifier: MIT OR Apache-2.0
// SPDX-FileCopyrightText: 2026 Howard M. Harte
//! Exercise the standalone executable on loopback, without board services.

use std::fs;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Server {
    child: Child,
    directory: PathBuf,
    address: SocketAddr,
}

impl Server {
    fn start(upload: bool) -> Self {
        Self::start_mode(upload, true)
    }

    fn start_mode(upload: bool, once: bool) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("hh-tftpd-{}-{unique}", std::process::id()));
        fs::create_dir_all(directory.join("boot")).unwrap();
        fs::create_dir_all(directory.join("uploads")).unwrap();
        // Reserve a free loopback port, then release it for the child listener.
        let reservation = UdpSocket::bind("127.0.0.1:0").unwrap();
        let address = reservation.local_addr().unwrap();
        drop(reservation);
        let mut command = Command::new(env!("CARGO_BIN_EXE_tftpd"));
        command.args([
            "--bind",
            "127.0.0.1",
            "--port",
            &address.port().to_string(),
            "--quiet",
        ]);
        if once {
            command.arg("--once");
        }
        command.arg("--root").arg(directory.join("boot"));
        if upload {
            command.arg("--upload").arg(directory.join("uploads"));
        }
        let child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut server = Self {
            child,
            directory,
            address,
        };
        let stdout = server.child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let mut reader = BufReader::new(stdout);
            let result = reader.read_line(&mut line);
            let _ = sender.send((result, line));
            // Keep draining startup mappings/progress until the child exits.
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
        });
        let (result, line) = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("server startup timed out");
        result.unwrap();
        assert!(line.contains("serving"), "server did not start: {line}");
        server
    }

    fn finish(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(
                Instant::now() < deadline,
                "server did not exit after transfer"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // This is the unique temporary tree created by Server::start.
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn client() -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket
}

fn request(op: u16, name: &str, options: &[(&str, &str)]) -> Vec<u8> {
    let mut packet = op.to_be_bytes().to_vec();
    for field in [name, "octet"]
        .into_iter()
        .chain(options.iter().flat_map(|(k, v)| [*k, *v]))
    {
        packet.extend_from_slice(field.as_bytes());
        packet.push(0);
    }
    packet
}

fn download(data: &[u8], options: bool) {
    let mut server = Server::start(false);
    fs::write(server.directory.join("boot/image.bin"), data).unwrap();
    let socket = client();
    let opts = if options {
        vec![("blksize", "64"), ("tsize", "0"), ("timeout", "9")]
    } else {
        vec![]
    };
    socket
        .send_to(&request(1, "image.bin", &opts), server.address)
        .unwrap();
    let mut received = Vec::new();
    let mut block = 1u16;
    let mut buffer = [0; 1024];
    let block_size = if options { 64 } else { 512 };
    loop {
        let (length, peer) = socket.recv_from(&mut buffer).unwrap();
        let op = u16::from_be_bytes([buffer[0], buffer[1]]);
        if op == 6 {
            assert!(!buffer[2..length]
                .windows(8)
                .any(|bytes| bytes == b"timeout\0"));
            let fields: Vec<_> = buffer[2..length]
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .collect();
            assert!(fields
                .chunks_exact(2)
                .any(|p| p == [b"blksize".as_slice(), b"64".as_slice()]));
            let size = data.len().to_string();
            assert!(fields
                .chunks_exact(2)
                .any(|p| p == [b"tsize".as_slice(), size.as_bytes()]));
            socket.send_to(&[0, 4, 0, 0], peer).unwrap();
            continue;
        }
        assert_eq!(op, 3);
        assert_eq!(u16::from_be_bytes([buffer[2], buffer[3]]), block);
        received.extend_from_slice(&buffer[4..length]);
        let mut ack = vec![0, 4];
        ack.extend_from_slice(&block.to_be_bytes());
        socket.send_to(&ack, peer).unwrap();
        if length - 4 < block_size {
            break;
        }
        block = block.wrapping_add(1);
    }
    server.finish();
    assert_eq!(received, data);
}

#[test]
fn rrq_preserves_bytes_and_negotiates_exact_multiple_termination() {
    download(&(0..128).map(|n| n as u8).collect::<Vec<_>>(), true);
}

#[test]
fn rrq_handles_empty_and_default_block_transfers() {
    download(&[], false);
    download(&vec![0xa5; 513], false);
}

#[test]
fn wrq_requires_explicit_upload_permission() {
    let mut server = Server::start(false);
    let socket = client();
    socket
        .send_to(&request(2, "capture.bin", &[]), server.address)
        .unwrap();
    let mut buffer = [0; 1024];
    let (length, _) = socket.recv_from(&mut buffer).unwrap();
    assert!(length >= 5);
    assert_eq!(&buffer[..4], &[0, 5, 0, 2]);
    // Refused uploads do not consume --once; the listener remains available.
    assert!(server.child.try_wait().unwrap().is_none());
    assert!(!server.directory.join("boot/capture.bin").exists());
}

#[test]
fn wrq_preserves_payload_in_the_separate_upload_tree() {
    let mut server = Server::start(true);
    let socket = client();
    socket
        .send_to(&request(2, "capture.bin", &[]), server.address)
        .unwrap();
    let mut buffer = [0; 1024];
    let (length, peer) = socket.recv_from(&mut buffer).unwrap();
    assert_eq!(&buffer[..length], &[0, 4, 0, 0]);
    let payload = [0, 0xff, 0x0a, 0x0d, 0x80];
    let mut packet = vec![0, 3, 0, 1];
    packet.extend_from_slice(&payload);
    socket.send_to(&packet, peer).unwrap();
    let (length, _) = socket.recv_from(&mut buffer).unwrap();
    assert_eq!(&buffer[..length], &[0, 4, 0, 1]);
    server.finish();
    assert_eq!(
        fs::read(server.directory.join("uploads/capture.bin")).unwrap(),
        payload
    );
    assert!(!server.directory.join("boot/capture.bin").exists());
    assert!(!server.directory.join("uploads/capture.bin.part").exists());
}

fn receive_packet(socket: &UdpSocket) -> (Vec<u8>, SocketAddr) {
    let mut bytes = [0; 2048];
    let (length, peer) = socket.recv_from(&mut bytes).unwrap();
    (bytes[..length].to_vec(), peer)
}

fn acknowledge(socket: &UdpSocket, peer: SocketAddr, block: u16) {
    let mut packet = vec![0, 4];
    packet.extend_from_slice(&block.to_be_bytes());
    socket.send_to(&packet, peer).unwrap();
}

#[test]
fn rrq_retransmits_the_same_block_when_its_ack_is_lost() {
    let mut server = Server::start(false);
    fs::write(server.directory.join("boot/image.bin"), b"boot image").unwrap();
    let socket = client();
    socket
        .send_to(&request(1, "image.bin", &[]), server.address)
        .unwrap();
    let (first, peer) = receive_packet(&socket);
    let (retry, retry_peer) = receive_packet(&socket);
    assert_eq!(retry_peer, peer);
    assert_eq!(first, retry);
    assert_eq!(&first[..4], &[0, 3, 0, 1]);
    acknowledge(&socket, peer, 1);
    server.finish();
}

#[test]
fn rrq_ignores_a_duplicate_ack_without_resending_current_data() {
    let mut server = Server::start(false);
    let payload = vec![0xa5; 600];
    fs::write(server.directory.join("boot/image.bin"), &payload).unwrap();
    let socket = client();
    socket
        .send_to(&request(1, "image.bin", &[]), server.address)
        .unwrap();
    let (first, peer) = receive_packet(&socket);
    assert_eq!(&first[..4], &[0, 3, 0, 1]);
    acknowledge(&socket, peer, 1);
    let (last, last_peer) = receive_packet(&socket);
    assert_eq!(last_peer, peer);
    assert_eq!(&last[..4], &[0, 3, 0, 2]);

    acknowledge(&socket, peer, 1);
    socket
        .set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    let error = socket.recv_from(&mut [0; 1024]).unwrap_err();
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    acknowledge(&socket, peer, 2);
    server.finish();
    assert_eq!([&first[4..], &last[4..]].concat(), payload);
}

#[test]
fn unrelated_acks_cannot_keep_a_download_alive_past_its_retry_budget() {
    let mut server = Server::start(false);
    fs::write(server.directory.join("boot/image.bin"), b"boot image").unwrap();
    let socket = client();
    socket
        .send_to(&request(1, "image.bin", &[]), server.address)
        .unwrap();
    let (_, peer) = receive_packet(&socket);
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        if let Some(status) = server.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "unrelated ACKs extended the retry budget"
        );
        // ACK 0 cannot acknowledge DATA 1. Keeping packets flowing must not
        // reset the block's retry timer or provoke immediate DATA responses.
        let _ = socket.send_to(&[0, 4, 0, 0], peer);
        std::thread::sleep(Duration::from_millis(10));
    }
    server.finish();
}

#[test]
fn wrq_negotiates_options_and_never_writes_duplicate_blocks_twice() {
    let mut server = Server::start(true);
    let socket = client();
    socket
        .send_to(
            &request(
                2,
                "capture.bin",
                &[("tsize", "128"), ("blksize", "64"), ("timeout", "9")],
            ),
            server.address,
        )
        .unwrap();
    let (options, peer) = receive_packet(&socket);
    assert_eq!(&options[..2], &[0, 6]);
    for option in [b"blksize\x0064\x00".as_slice(), b"tsize\x00128\x00"] {
        assert!(options.windows(option.len()).any(|bytes| bytes == option));
    }
    assert!(!options.windows(8).any(|bytes| bytes == b"timeout\0"));
    let payload: Vec<_> = (0..128).map(|byte| byte as u8).collect();
    for (block, data) in [
        (1u16, &payload[..64]),
        (1, &payload[..64]),
        (2, &payload[64..]),
        (3, &[]),
    ] {
        let mut packet = vec![0, 3];
        packet.extend_from_slice(&block.to_be_bytes());
        packet.extend_from_slice(data);
        socket.send_to(&packet, peer).unwrap();
        let (ack, ack_peer) = receive_packet(&socket);
        assert_eq!(ack_peer, peer);
        let mut expected = vec![0, 4];
        expected.extend_from_slice(&block.to_be_bytes());
        assert_eq!(ack, expected);
    }
    server.finish();
    assert_eq!(
        fs::read(server.directory.join("uploads/capture.bin")).unwrap(),
        payload
    );
}

#[test]
fn cancelled_upload_closes_and_removes_its_partial_file() {
    let mut server = Server::start(true);
    let socket = client();
    socket
        .send_to(&request(2, "capture.bin", &[]), server.address)
        .unwrap();
    let (ack, peer) = receive_packet(&socket);
    assert_eq!(ack, [0, 4, 0, 0]);
    let mut data = vec![0, 3, 0, 1];
    data.extend_from_slice(&[0xa5; 512]);
    socket.send_to(&data, peer).unwrap();
    assert_eq!(receive_packet(&socket).0, [0, 4, 0, 1]);
    socket
        .send_to(b"\x00\x05\x00\x00cancelled\x00", peer)
        .unwrap();
    server.finish();
    assert!(!server.directory.join("uploads/capture.bin").exists());
    assert!(!server.directory.join("uploads/capture.bin.part").exists());
}

#[test]
fn failed_publication_keeps_the_completed_sidecar_for_recovery() {
    let mut server = Server::start(true);
    // A nonempty directory cannot be replaced by a file on the supported hosts.
    let destination = server.directory.join("uploads/capture.bin");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("preserve"), b"original").unwrap();
    let socket = client();
    socket
        .send_to(&request(2, "capture.bin", &[]), server.address)
        .unwrap();
    let (_, peer) = receive_packet(&socket);
    socket.send_to(b"\x00\x03\x00\x01payload", peer).unwrap();
    assert_eq!(receive_packet(&socket).0, [0, 4, 0, 1]);
    server.finish();
    assert_eq!(fs::read(destination.join("preserve")).unwrap(), b"original");
    assert_eq!(
        fs::read(server.directory.join("uploads/capture.bin.part")).unwrap(),
        b"payload"
    );
}

fn begin_upload(server: &Server, socket: &UdpSocket, name: &str) -> SocketAddr {
    socket
        .send_to(&request(2, name, &[]), server.address)
        .unwrap();
    let (reply, peer) = receive_packet(socket);
    assert_eq!(reply, [0, 4, 0, 0]);
    peer
}

fn upload_block(socket: &UdpSocket, peer: SocketAddr, block: u16, bytes: &[u8]) {
    let mut packet = vec![0, 3];
    packet.extend_from_slice(&block.to_be_bytes());
    packet.extend_from_slice(bytes);
    socket.send_to(&packet, peer).unwrap();
    let mut expected = vec![0, 4];
    expected.extend_from_slice(&block.to_be_bytes());
    assert_eq!(receive_packet(socket).0, expected);
}

fn wait_for_file(path: &std::path::Path, expected: &[u8]) {
    // The final ACK precedes sync/rename. Wait on publication, never a fixed
    // scheduling delay, and retain a deadline if the transfer did not finish.
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs::read(path).ok().as_deref() != Some(expected) {
        assert!(
            Instant::now() < deadline,
            "file was not published: {path:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn an_existing_upload_sidecar_is_refused_without_truncating_recovery_data() {
    let mut server = Server::start(true);
    let destination = server.directory.join("uploads/capture.bin");
    let sidecar = server.directory.join("uploads/capture.bin.part");
    fs::write(&destination, b"original destination").unwrap();
    fs::write(&sidecar, b"recover this payload").unwrap();
    let socket = client();
    socket
        .send_to(&request(2, "capture.bin", &[]), server.address)
        .unwrap();
    let (reply, _) = receive_packet(&socket);
    assert_eq!(&reply[..4], &[0, 5, 0, 6]);
    server.finish();
    assert_eq!(fs::read(destination).unwrap(), b"original destination");
    assert_eq!(fs::read(sidecar).unwrap(), b"recover this payload");
}

#[test]
fn a_competing_upload_cannot_truncate_or_remove_the_active_transfers_sidecar() {
    let server = Server::start_mode(true, false);
    let first = client();
    let peer = begin_upload(&server, &first, "capture.bin");
    let first_block = [0xa5; 512];
    upload_block(&first, peer, 1, &first_block);
    let second = client();
    second
        .send_to(&request(2, "capture.bin", &[]), server.address)
        .unwrap();
    let (reply, refused_peer) = receive_packet(&second);
    assert_eq!(&reply[..4], &[0, 5, 0, 6]);
    // A client abandoning the refused request cannot clean up the first one.
    second
        .send_to(b"\x00\x05\x00\x00cancelled\x00", refused_peer)
        .unwrap();
    assert!(!server.directory.join("uploads/capture.bin").exists());
    upload_block(&first, peer, 2, b"tail");
    let expected = [&first_block[..], b"tail"].concat();
    wait_for_file(&server.directory.join("uploads/capture.bin"), &expected);
    assert!(!server.directory.join("uploads/capture.bin.part").exists());
}

#[test]
fn independent_uploads_progress_and_one_cancellation_cannot_delete_the_other() {
    let server = Server::start_mode(true, false);
    let first = client();
    let second = client();
    let a = begin_upload(&server, &first, "a.bin");
    let b = begin_upload(&server, &second, "b.bin");
    upload_block(&first, a, 1, &[0x11; 512]);
    upload_block(&second, b, 1, &[0x22; 512]);
    first.send_to(b"\x00\x05\x00\x00cancelled\x00", a).unwrap();
    upload_block(&second, b, 2, b"complete");
    wait_for_file(
        &server.directory.join("uploads/b.bin"),
        &[&[0x22; 512][..], b"complete"].concat(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.directory.join("uploads/a.bin.part").exists() {
        assert!(
            Instant::now() < deadline,
            "cancelled sidecar was not removed"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!server.directory.join("uploads/a.bin").exists());
}

#[test]
fn timeout_only_requests_use_default_data_or_ack_without_an_oack() {
    for op in [1, 2] {
        let mut server = Server::start(op == 2);
        fs::write(server.directory.join("boot/image.bin"), b"image").unwrap();
        let socket = client();
        socket
            .send_to(
                &request(op, "image.bin", &[("timeout", "9")]),
                server.address,
            )
            .unwrap();
        let (reply, peer) = receive_packet(&socket);
        if op == 1 {
            assert_eq!(reply, b"\x00\x03\x00\x01image");
            acknowledge(&socket, peer, 1);
        } else {
            assert_eq!(reply, [0, 4, 0, 0]);
            upload_block(&socket, peer, 1, b"image");
        }
        server.finish();
    }
}
