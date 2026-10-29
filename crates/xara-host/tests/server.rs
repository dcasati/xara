use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};
use xara_host::command::Command;
use xara_host::kiss::{Deframer, FEND, KissServer};
use xara_host::server::{DATA_CHUNK, Host, INPUT_QUEUE, Input, MAX_LINE};

#[test]
fn a_client_that_stops_reading_does_not_block_the_host() {
    let h = Host::bind("127.0.0.1", 0).unwrap();
    let _c = TcpStream::connect(h.data_addr).unwrap();
    assert!(matches!(h.input.recv_timeout(Duration::from_secs(1)), Ok(Input::Connected)));
    let t0 = Instant::now();
    let block = vec![0u8; 65536];
    while h.has_data_client() {
        h.data(&block);
        assert!(t0.elapsed() < Duration::from_secs(5), "host send blocked");
    }
    assert!(matches!(h.input.recv_timeout(Duration::from_secs(1)), Ok(Input::Closed)));
}

#[test]
fn a_kiss_client_that_stops_reading_is_dropped() {
    let k = KissServer::bind("127.0.0.1", 0).unwrap();
    let _c = TcpStream::connect(k.addr).unwrap();
    let t0 = Instant::now();
    while !k.has_clients() {
        assert!(t0.elapsed() < Duration::from_secs(1));
    }
    let frame = vec![0x55u8; 4096];
    while k.has_clients() {
        k.send(0, &frame);
        assert!(t0.elapsed() < Duration::from_secs(5), "kiss send blocked");
    }
}

fn command_host() -> (Host, TcpStream) {
    let h = Host::bind("127.0.0.1", 0).unwrap();
    let c = TcpStream::connect(h.cmd_addr).unwrap();
    assert!(matches!(h.input.recv_timeout(Duration::from_secs(1)), Ok(Input::Connected)));
    (h, c)
}

#[test]
fn an_over_long_command_line_is_dropped() {
    use std::io::Write;
    let (h, mut c) = command_host();
    let mut long = vec![b'A'; MAX_LINE + 1];
    long.extend(b"\rBW2300\r");
    c.write_all(&long).unwrap();
    let got = h.input.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(matches!(got, Input::Command(Command::Bw(_))), "{got:?}");
    let mut edge = vec![b'A'; MAX_LINE];
    edge.push(b'\r');
    c.write_all(&edge).unwrap();
    assert!(matches!(h.input.recv_timeout(Duration::from_secs(1)), Ok(Input::Wrong(l)) if l.len() == MAX_LINE));
}

#[test]
fn a_second_client_is_refused_and_the_port_stays_ours() {
    use std::io::{Read, Write};
    let (h, mut first) = command_host();
    for _ in 0..3 {
        let e = TcpStream::connect(h.cmd_addr).map(drop).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::ConnectionRefused);
    }
    first.write_all(b"BW500\r").unwrap();
    assert!(matches!(h.input.recv_timeout(Duration::from_secs(1)), Ok(Input::Command(Command::Bw(_)))));
    for a in [h.cmd_addr, h.data_addr] {
        assert_eq!(std::net::TcpListener::bind(a).map(drop).unwrap_err().kind(), std::io::ErrorKind::AddrInUse);
    }
    drop(first);
    assert!(matches!(h.input.recv_timeout(Duration::from_secs(1)), Ok(Input::Closed)));
    let t0 = Instant::now();
    let mut third = loop {
        let mut c = connect_soon(h.cmd_addr);
        if let Ok(Input::Connected) = h.input.recv_timeout(Duration::from_millis(200)) {
            break c;
        }
        c.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let _ = c.read(&mut [0; 1]);
        assert!(t0.elapsed() < Duration::from_secs(2));
    };
    third.write_all(b"BW2750\r").unwrap();
    assert!(matches!(h.input.recv_timeout(Duration::from_secs(1)), Ok(Input::Command(Command::Bw(_)))));
}

#[test]
fn the_top_port_has_no_room_for_the_data_port() {
    let e = Host::bind("127.0.0.1", 65535).err().expect("port 65535 cannot have a data port above it");
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn a_dropped_host_frees_its_ports() {
    let fds = open_fds();
    let mut last = None;
    for _ in 0..300 {
        let h = Host::bind("127.0.0.1", 0).unwrap();
        last = Some((h.cmd_addr, h.data_addr));
    }
    let (cmd, data) = last.unwrap();
    let t0 = Instant::now();
    while open_fds() > fds + 32 || [cmd, data].iter().any(|&a| std::net::TcpListener::bind(a).is_err()) {
        assert!(t0.elapsed() < Duration::from_secs(5), "{} fds open, {fds} before", open_fds());
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_host_dropped_with_clients_lets_go_when_they_do() {
    let (h, _c) = command_host();
    let d = TcpStream::connect(h.data_addr).unwrap();
    assert!(matches!(h.input.recv_timeout(Duration::from_secs(1)), Ok(Input::Connected)));
    let addrs = [h.cmd_addr, h.data_addr];
    drop(h);
    thread::sleep(Duration::from_millis(50));
    for a in addrs {
        assert_eq!(std::net::TcpListener::bind(a).map(drop).unwrap_err().kind(), std::io::ErrorKind::AddrInUse);
    }
    drop(d);
    let t0 = Instant::now();
    while addrs.iter().any(|&a| std::net::TcpListener::bind(a).is_err()) {
        assert!(t0.elapsed() < Duration::from_secs(5), "{addrs:?} still bound");
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

#[cfg(not(target_os = "linux"))]
fn open_fds() -> usize {
    0
}

#[test]
fn an_over_long_kiss_frame_is_dropped() {
    let mut d = Deframer::default();
    let long = [vec![FEND, 0x02], vec![0x41; 6000], vec![FEND]].concat();
    assert!(d.feed(&long).is_empty());
    let edge = [vec![FEND, 0x02], vec![0x41; 4999], vec![FEND]].concat();
    assert_eq!(d.feed(&edge), vec![(2, vec![0x41; 4999])]);
    let next = [FEND, 0x02, 0x42, FEND];
    assert_eq!(d.feed(&next), vec![(2, vec![0x42])]);
}

#[test]
fn ports_reopen_after_xara_closes_the_other_port() {
    let h = Host::bind("127.0.0.1", 0).unwrap();
    for _ in 0..3 {
        let (c, d) = (connect_soon(h.cmd_addr), connect_soon(h.data_addr));
        for _ in 0..2 {
            assert!(matches!(h.input.recv_timeout(Duration::from_secs(1)), Ok(Input::Connected)));
        }
        drop(d);
        assert!(matches!(h.input.recv_timeout(Duration::from_secs(1)), Ok(Input::Closed)));
        let mut probe = [0u8; 1];
        c.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        assert_eq!(std::io::Read::read(&mut &c, &mut probe).unwrap_or(0), 0);
    }
}

fn connect_soon(a: std::net::SocketAddr) -> TcpStream {
    let t0 = Instant::now();
    loop {
        match TcpStream::connect(a) {
            Ok(s) => return s,
            Err(e) => assert!(t0.elapsed() < Duration::from_secs(2), "{a}: {e}"),
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn pattern(at: usize, n: usize) -> Vec<u8> {
    (at..at + n).map(|i| (i % 251) as u8).collect()
}

fn flood_until_stalled(c: &mut TcpStream, limit: usize) -> usize {
    use std::io::Write;
    c.set_write_timeout(Some(Duration::from_millis(300))).unwrap();
    let mut sent = 0;
    while sent < limit {
        match c.write(&pattern(sent, DATA_CHUNK)) {
            Ok(n) => sent += n,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => break,
            Err(e) => panic!("{e}"),
        }
    }
    sent
}

fn take_data(h: &Host, got: &mut Vec<u8>, upto: usize) {
    while got.len() < upto {
        match h.input.recv_timeout(Duration::from_secs(2)) {
            Ok(Input::Data(d)) => got.extend(d),
            other => panic!("{other:?} after {} of {upto} bytes", got.len()),
        }
    }
}

const SOCKET_SLACK: usize = 32 << 20;

#[test]
fn a_data_flood_is_held_back_without_loss() {
    let h = Host::bind("127.0.0.1", 0).unwrap();
    let mut c = TcpStream::connect(h.data_addr).unwrap();
    assert!(matches!(h.input.recv_timeout(Duration::from_secs(1)), Ok(Input::Connected)));
    let limit = 256 << 20;
    let sent = flood_until_stalled(&mut c, limit);
    assert!(sent < INPUT_QUEUE * DATA_CHUNK + SOCKET_SLACK, "the client was not pushed back: {sent} bytes taken");
    let mut got = Vec::new();
    take_data(&h, &mut got, sent);
    assert_eq!(got.len(), sent);
    assert!(got == pattern(0, sent), "data reordered or lost");
}

#[test]
fn a_held_data_port_stops_reading() {
    let h = Host::bind("127.0.0.1", 0).unwrap();
    let mut c = TcpStream::connect(h.data_addr).unwrap();
    assert!(matches!(h.input.recv_timeout(Duration::from_secs(1)), Ok(Input::Connected)));
    h.hold_data(true);
    let sent = flood_until_stalled(&mut c, 256 << 20);
    assert!(sent < SOCKET_SLACK, "{sent} bytes taken while held");
    thread::sleep(Duration::from_millis(100));
    let mut got: Vec<u8> =
        h.input.try_iter().flat_map(|i| if let Input::Data(d) = i { d } else { panic!("{i:?}") }).collect();
    assert!(got.len() <= DATA_CHUNK, "{} bytes read while held", got.len());
    h.hold_data(false);
    take_data(&h, &mut got, sent);
    assert_eq!(got.len(), sent);
    assert!(got == pattern(0, sent), "data reordered or lost");
}
