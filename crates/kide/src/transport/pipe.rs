//! An in-memory connection for `InProcess` live sessions: two directions,
//! each holding a bounded number of bytes, so a writer waits for its reader
//! as over a socket. A read waits at most its timeout.

use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// The bytes one direction holds before a write waits.
const CAPACITY: usize = 256 << 10;

#[derive(Clone, Copy, PartialEq)]
enum Flow {
    Open,
    /// The writer is gone: reads end once the bytes are taken.
    Ended,
    /// Broken: reads and writes fail.
    Broken,
}

/// One direction of a connection.
pub(super) struct Chan {
    state: Mutex<(VecDeque<u8>, Flow)>,
    changed: Condvar,
}

impl Chan {
    fn new() -> Arc<Chan> {
        Arc::new(Chan {
            state: Mutex::new((VecDeque::new(), Flow::Open)),
            changed: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, (VecDeque<u8>, Flow)> {
        // A poisoned lock only means another thread panicked; the bytes are intact.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set(&self, flow: Flow) {
        let mut s = self.lock();
        if s.1 == Flow::Open {
            s.1 = flow;
        }
        self.changed.notify_all();
    }

    /// Breaks this direction: what waits on it fails.
    pub(super) fn break_off(&self) {
        self.set(Flow::Broken);
    }
}

pub(super) struct Reader {
    chan: Arc<Chan>,
    timeout: Duration,
}

pub(super) struct Writer(Arc<Chan>);

/// A connection: each end's reader and writer, and both directions, to
/// break it.
pub(super) type Ends = ((Reader, Writer), (Reader, Writer), [Arc<Chan>; 2]);

/// A connection whose reads wait at most `timeout`.
pub(super) fn connection(timeout: Duration) -> Ends {
    let (up, down) = (Chan::new(), Chan::new());
    let reader = |chan: &Arc<Chan>| Reader {
        chan: chan.clone(),
        timeout,
    };
    (
        (reader(&down), Writer(up.clone())),
        (reader(&up), Writer(down.clone())),
        [up, down],
    )
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let chan = &self.chan;
        let s = chan.lock();
        let (mut s, wait) = chan
            .changed
            .wait_timeout_while(s, self.timeout, |s| s.0.is_empty() && s.1 == Flow::Open)
            .unwrap_or_else(|e| e.into_inner());
        if s.1 == Flow::Broken {
            return Err(ErrorKind::ConnectionReset.into());
        }
        if s.0.is_empty() {
            return match s.1 {
                Flow::Ended => Ok(0),
                _ if wait.timed_out() => Err(ErrorKind::TimedOut.into()),
                _ => Err(ErrorKind::Interrupted.into()),
            };
        }
        let n = buf.len().min(s.0.len());
        for (b, x) in buf.iter_mut().zip(s.0.drain(..n)) {
            *b = x;
        }
        chan.changed.notify_all();
        Ok(n)
    }
}

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let chan = &self.0;
        let s = chan.lock();
        let mut s = chan
            .changed
            .wait_while(s, |s| s.0.len() >= CAPACITY && s.1 == Flow::Open)
            .unwrap_or_else(|e| e.into_inner());
        if s.1 != Flow::Open {
            return Err(ErrorKind::BrokenPipe.into());
        }
        let n = buf.len().min(CAPACITY - s.0.len());
        s.0.extend(&buf[..n]);
        chan.changed.notify_all();
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.0.set(Flow::Ended);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn bytes_flow_both_ways_and_writers_wait_for_readers() {
        let ((mut ar, mut aw), (mut br, bw), _) = connection(Duration::from_secs(5));
        let big = vec![3; 3 * CAPACITY];
        let sender = thread::spawn(move || aw.write_all(&big));
        let mut got = vec![0; 3 * CAPACITY];
        br.read_exact(&mut got).unwrap();
        sender.join().unwrap().unwrap();
        assert!(got.iter().all(|&b| b == 3));
        drop(bw);
        assert_eq!(ar.read(&mut [0; 4]).unwrap(), 0, "the other end is gone");
    }

    #[test]
    fn reads_time_out_and_breaks_fail_both_ways() {
        let ((mut ar, mut aw), _other, chans) = connection(Duration::from_millis(20));
        let e = ar.read(&mut [0; 4]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::TimedOut);
        chans.iter().for_each(|c| c.break_off());
        assert_eq!(
            ar.read(&mut [0; 4]).unwrap_err().kind(),
            ErrorKind::ConnectionReset
        );
        assert!(aw.write(&[1]).is_err());
    }
}
