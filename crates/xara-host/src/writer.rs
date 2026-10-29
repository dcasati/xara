use std::io::{self, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread;

pub(crate) const QUEUE_BYTES: usize = 4 << 20;

pub struct Writer {
    tx: Sender<Option<Vec<u8>>>,
    s: TcpStream,
    queued: Arc<AtomicUsize>,
    overflowed: AtomicBool,
}

impl Writer {
    pub fn spawn(s: &TcpStream) -> io::Result<Self> {
        let mut w = s.try_clone()?;
        let (tx, rx) = channel::<Option<Vec<u8>>>();
        let queued = Arc::new(AtomicUsize::new(0));
        let q = queued.clone();
        thread::Builder::new().spawn(move || {
            for b in rx {
                match b {
                    Some(b) if w.write_all(&b).is_ok() => {
                        q.fetch_sub(b.len(), Ordering::Relaxed);
                    }
                    _ => {
                        let _ = w.shutdown(Shutdown::Both);
                        return;
                    }
                }
            }
        })?;
        Ok(Self { tx, s: s.try_clone()?, queued, overflowed: AtomicBool::new(false) })
    }

    pub fn send(&self, b: &[u8]) -> bool {
        if self.overflowed.load(Ordering::Relaxed) {
            return false;
        }
        let q = self.queued.load(Ordering::Relaxed);
        if q > 0 && q + b.len() > QUEUE_BYTES {
            if !self.overflowed.swap(true, Ordering::Relaxed) {
                let peer = self.s.peer_addr().map_or_else(|_| "client".into(), |a| a.to_string());
                crate::say(format_args!("{peer} is {q} bytes behind; closing it"));
            }
            self.close();
            return false;
        }
        self.queued.fetch_add(b.len(), Ordering::Relaxed);
        let ok = self.tx.send(Some(b.to_vec())).is_ok();
        if !ok {
            self.close();
        }
        ok
    }

    pub fn close(&self) {
        let _ = self.s.shutdown(Shutdown::Both);
    }

    pub fn finish(&self) {
        if self.tx.send(None).is_err() {
            self.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn stalled_reader_is_cut_off_by_bytes() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let s = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let _peer = l.accept().unwrap();
        let w = Writer::spawn(&s).unwrap();
        let block = vec![0u8; 65536];
        let sent = (0..1024).take_while(|_| w.send(&block)).count();
        assert!(sent < 1024);
        assert!(!w.send(b"x"));
    }
}
