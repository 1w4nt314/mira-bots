//! Fixed-capacity byte ring buffer for PTY output. Oldest bytes are overwritten first.

pub struct RingBuffer {
    buf: Vec<u8>,
    cap: usize,
    /// Index of the oldest byte.
    start: usize,
    len: usize,
    /// Total number of bytes ever pushed.
    seq: u64,
}

impl RingBuffer {
    /// Creates an empty buffer. A capacity of 0 is bumped to 1.
    pub fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            buf: vec![0; cap],
            cap,
            start: 0,
            len: 0,
            seq: 0,
        }
    }

    /// Appends `bytes`, overwriting the oldest data when full. `seq` grows by `bytes.len()`.
    pub fn push(&mut self, bytes: &[u8]) {
        self.seq += bytes.len() as u64;
        // Only the last `cap` bytes can survive.
        let bytes = &bytes[bytes.len().saturating_sub(self.cap)..];
        for &b in bytes {
            let end = (self.start + self.len) % self.cap;
            self.buf[end] = b;
            if self.len < self.cap {
                self.len += 1;
            } else {
                self.start = (self.start + 1) % self.cap;
            }
        }
    }

    /// `(seq, bytes)` with the retained bytes in chronological order.
    pub fn snapshot(&self) -> (u64, Vec<u8>) {
        let mut out = Vec::with_capacity(self.len);
        let first = (self.cap - self.start).min(self.len);
        out.extend_from_slice(&self.buf[self.start..self.start + first]);
        out.extend_from_slice(&self.buf[..self.len - first]);
        (self.seq, out)
    }

    /// Total bytes ever pushed (the `seq` of `agent-output`).
    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_under_capacity_keeps_everything() {
        let mut rb = RingBuffer::new(8);
        assert!(rb.is_empty());
        rb.push(b"abc");
        rb.push(b"de");
        assert_eq!(rb.snapshot(), (5, b"abcde".to_vec()));
        assert_eq!(rb.len(), 5);
    }

    #[test]
    fn push_over_capacity_drops_oldest_in_order() {
        let mut rb = RingBuffer::new(4);
        rb.push(b"abc");
        rb.push(b"def");
        assert_eq!(rb.snapshot(), (6, b"cdef".to_vec()));
        rb.push(b"g");
        assert_eq!(rb.snapshot(), (7, b"defg".to_vec()));
        assert_eq!(rb.len(), 4);
    }

    #[test]
    fn push_larger_than_capacity_keeps_tail() {
        let mut rb = RingBuffer::new(4);
        rb.push(b"x");
        rb.push(b"0123456789");
        assert_eq!(rb.snapshot(), (11, b"6789".to_vec()));
    }

    #[test]
    fn seq_counts_all_bytes_including_empty_pushes() {
        let mut rb = RingBuffer::new(2);
        rb.push(b"");
        assert_eq!(rb.seq(), 0);
        for _ in 0..1000 {
            rb.push(b"ab");
        }
        assert_eq!(rb.seq(), 2000);
        assert_eq!(rb.snapshot().1, b"ab".to_vec());
    }

    #[test]
    fn wraps_repeatedly_like_a_1mib_buffer() {
        let mut rb = RingBuffer::new(1 << 20);
        let chunk: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
        for _ in 0..5 {
            rb.push(&chunk);
        }
        let (seq, data) = rb.snapshot();
        assert_eq!(seq, 1_500_000);
        assert_eq!(data.len(), 1 << 20);
        // Last byte is the last byte of the last chunk.
        assert_eq!(*data.last().unwrap(), *chunk.last().unwrap());
    }

    #[test]
    fn zero_capacity_is_bumped() {
        let mut rb = RingBuffer::new(0);
        rb.push(b"xy");
        assert_eq!(rb.capacity(), 1);
        assert_eq!(rb.snapshot(), (2, b"y".to_vec()));
    }
}
