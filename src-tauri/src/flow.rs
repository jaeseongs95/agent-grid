use std::{collections::VecDeque, sync::{Condvar, Mutex}};

/// Bytes emitted to the renderer but not yet consumed by xterm's write callback.
/// A stalled renderer blocks the PTY reader, then the OS PTY, instead of growing
/// an unbounded event queue. Stop wakes the reader without needing the renderer.
pub const MAX_PENDING_BYTES: usize = 1024 * 1024;

#[derive(Default)]
struct Pending {
    next: u64,
    acknowledged: u64,
    bytes: usize,
    frames: VecDeque<(u64, usize)>,
    cancelled: bool,
}

#[derive(Default)]
pub struct OutputFlow {
    pending: Mutex<Pending>,
    changed: Condvar,
}

impl OutputFlow {
    pub fn reserve(&self, bytes: usize) -> Result<Option<u64>, String> {
        if bytes > MAX_PENDING_BYTES { return Err("output frame exceeds the delivery limit".into()); }
        let mut pending = self.pending.lock().map_err(|_| "output flow lock poisoned")?;
        while !pending.cancelled && pending.bytes + bytes > MAX_PENDING_BYTES {
            pending = self.changed.wait(pending).map_err(|_| "output flow lock poisoned")?;
        }
        if pending.cancelled { return Ok(None); }
        pending.next = pending.next.checked_add(1).ok_or("output sequence exhausted")?;
        let seq = pending.next;
        pending.bytes += bytes;
        pending.frames.push_back((seq, bytes));
        Ok(Some(seq))
    }

    pub fn acknowledge(&self, seq: u64) -> Result<(), String> {
        let mut pending = self.pending.lock().map_err(|_| "output flow lock poisoned")?;
        if seq > pending.next { return Err("cannot acknowledge output that has not been emitted".into()); }
        if seq <= pending.acknowledged { return Ok(()); }
        pending.acknowledged = seq;
        while pending.frames.front().is_some_and(|(frame, _)| *frame <= seq) {
            let (_, bytes) = pending.frames.pop_front().expect("front was checked");
            pending.bytes -= bytes;
        }
        self.changed.notify_all();
        Ok(())
    }

    pub fn cancel(&self) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.cancelled = true;
        pending.frames.clear();
        pending.bytes = 0;
        self.changed.notify_all();
    }

    #[cfg(test)]
    pub fn pending_bytes(&self) -> usize { self.pending.lock().unwrap().bytes }
}

/// Preserve UTF-8 split at arbitrary PTY read boundaries. Invalid bytes use the
/// Unicode replacement character; terminal escape bytes remain untouched.
#[derive(Default)]
pub struct Utf8Decoder { tail: Vec<u8> }
impl Utf8Decoder {
    pub fn decode(&mut self, input: &[u8], eof: bool) -> String {
        self.tail.extend_from_slice(input);
        let mut output = String::new();
        let mut used = 0;
        while used < self.tail.len() {
            match std::str::from_utf8(&self.tail[used..]) {
                Ok(text) => { output.push_str(text); used = self.tail.len(); }
                Err(error) => {
                    let valid = error.valid_up_to();
                    // This prefix was checked by from_utf8 above.
                    output.push_str(std::str::from_utf8(&self.tail[used..used + valid]).expect("validated prefix"));
                    used += valid;
                    match error.error_len() {
                        Some(length) => { output.push('\u{fffd}'); used += length; }
                        None if eof => { output.push('\u{fffd}'); used = self.tail.len(); }
                        None => break,
                    }
                }
            }
        }
        self.tail.drain(..used);
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::{mpsc, Arc}, time::Duration};

    #[test]
    fn delivery_is_bounded_acknowledged_and_cancellable() {
        let flow = Arc::new(OutputFlow::default());
        assert_eq!(flow.reserve(MAX_PENDING_BYTES).unwrap(), Some(1));
        let (sent, received) = mpsc::channel();
        let reader = flow.clone();
        let worker = std::thread::spawn(move || { sent.send(reader.reserve(1)).unwrap(); });
        assert!(received.recv_timeout(Duration::from_millis(40)).is_err());
        assert_eq!(flow.pending_bytes(), MAX_PENDING_BYTES);
        assert!(flow.acknowledge(2).is_err());
        flow.acknowledge(1).unwrap();
        assert_eq!(received.recv_timeout(Duration::from_secs(2)).unwrap().unwrap(), Some(2));
        worker.join().unwrap();
        flow.acknowledge(1).unwrap(); // duplicate ACK
        assert_eq!(flow.pending_bytes(), 1);
        flow.reserve(MAX_PENDING_BYTES - 1).unwrap();
        let reader = flow.clone();
        let worker = std::thread::spawn(move || reader.reserve(1));
        flow.cancel();
        assert_eq!(worker.join().unwrap().unwrap(), None);
        assert_eq!(flow.pending_bytes(), 0);
    }

    #[test]
    fn decoding_preserves_multibyte_characters_and_escape_sequences() {
        let text = "한글🦀\x1b[31mred\x1b[0m";
        for split in 0..=text.len() {
            let mut decoder = Utf8Decoder::default();
            let actual = decoder.decode(&text.as_bytes()[..split], false) + &decoder.decode(&text.as_bytes()[split..], true);
            assert_eq!(actual, text);
        }
        assert_eq!(Utf8Decoder::default().decode(&[0xf0, 0x9f], true), "\u{fffd}");
    }
}
