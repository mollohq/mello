/// XOR-based Forward Error Correction.
///
/// For every N data packets, one parity packet is produced.
/// The parity is the XOR of all N payloads (variable-length; shorter
/// payloads are zero-padded to the length of the longest).
///
/// This recovers any single packet loss within a group at zero added latency.

/// Encoder: host side. Accumulates data packets and emits parity.
pub struct FecEncoder {
    n: usize,
    group: Vec<Vec<u8>>,
}

impl FecEncoder {
    pub fn new(n: usize) -> Self {
        assert!(n >= 2, "FEC group size must be >= 2");
        Self {
            n,
            group: Vec::with_capacity(n),
        }
    }

    pub fn group_size(&self) -> usize {
        self.n
    }

    /// Push a data packet's payload. Returns `Some(parity_payload)` when the
    /// group is complete (after N data packets have been pushed).
    pub fn push(&mut self, payload: &[u8]) -> Option<Vec<u8>> {
        self.group.push(payload.to_vec());
        if self.group.len() == self.n {
            let parity = xor_payloads(&self.group);
            self.group.clear();
            Some(parity)
        } else {
            None
        }
    }

    /// Reset the group (call on keyframe boundary).
    pub fn reset(&mut self) {
        self.group.clear();
    }
}

/// Decoder: viewer side. Feeds received packets and recovers losses.
pub struct FecDecoder {
    n: usize,
    /// Payloads received in the current group, indexed by position (0..n-1).
    received: Vec<Option<Vec<u8>>>,
    parity: Option<Vec<u8>>,
    received_count: usize,
    /// Sequence number of the first packet in the current group.
    group_base_seq: Option<u16>,
}

impl FecDecoder {
    pub fn new(n: usize) -> Self {
        assert!(n >= 2, "FEC group size must be >= 2");
        Self {
            n,
            received: vec![None; n],
            parity: None,
            received_count: 0,
            group_base_seq: None,
        }
    }

    pub fn group_size(&self) -> usize {
        self.n
    }

    /// Reset for a new group starting at `base_seq`.
    pub fn reset(&mut self, base_seq: u16) {
        self.received.iter_mut().for_each(|s| *s = None);
        self.parity = None;
        self.received_count = 0;
        self.group_base_seq = Some(base_seq);
    }

    /// Feed a data packet. `seq` is the absolute sequence number.
    /// If the group base hasn't been set yet, this packet establishes it.
    ///
    /// Returns `Some(recovered_payload)` if feeding this packet allowed
    /// recovery of a previously missing packet.
    pub fn feed_data(&mut self, seq: u16, payload: &[u8]) -> Option<Vec<u8>> {
        let base = match self.group_base_seq {
            Some(b) => b,
            None => {
                self.group_base_seq = Some(seq);
                seq
            }
        };

        let idx = seq.wrapping_sub(base) as usize;
        if idx >= self.n {
            return None;
        }

        if self.received[idx].is_none() {
            self.received[idx] = Some(payload.to_vec());
            self.received_count += 1;
        }

        self.try_recover()
    }

    /// Feed a parity (FEC) packet for the current group.
    pub fn feed_parity(&mut self, payload: &[u8]) -> Option<Vec<u8>> {
        self.parity = Some(payload.to_vec());
        self.try_recover()
    }

    /// Returns true if the current group has unrecoverable losses
    /// (2+ missing data packets, or 1 missing but no parity).
    pub fn is_unrecoverable(&self) -> bool {
        let missing = self.n - self.received_count;
        missing >= 2 || (missing == 1 && self.parity.is_none())
    }

    /// Try to recover a single missing packet if we have N-1 data + parity.
    fn try_recover(&mut self) -> Option<Vec<u8>> {
        if self.received_count != self.n - 1 {
            return None;
        }
        let parity = self.parity.as_ref()?;

        let missing_idx = self.received.iter().position(|s| s.is_none())?;

        let mut present: Vec<&[u8]> = Vec::with_capacity(self.n - 1);
        for (i, slot) in self.received.iter().enumerate() {
            if i != missing_idx {
                present.push(slot.as_ref().unwrap());
            }
        }

        // recovered = XOR(parity, all present packets)
        let mut all_for_xor: Vec<&[u8]> = present;
        all_for_xor.push(parity);
        let recovered = xor_slices(&all_for_xor);

        self.received[missing_idx] = Some(recovered.clone());
        self.received_count += 1;

        Some(recovered)
    }
}

/// XOR a list of payloads. Shorter payloads are zero-padded.
fn xor_payloads(payloads: &[Vec<u8>]) -> Vec<u8> {
    let refs: Vec<&[u8]> = payloads.iter().map(|p| p.as_slice()).collect();
    xor_slices(&refs)
}

fn xor_slices(slices: &[&[u8]]) -> Vec<u8> {
    let max_len = slices.iter().map(|s| s.len()).max().unwrap_or(0);
    let mut result = vec![0u8; max_len];
    for slice in slices {
        for (i, &b) in slice.iter().enumerate() {
            result[i] ^= b;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_emits_parity_after_n() {
        let mut enc = FecEncoder::new(3);
        assert!(enc.push(&[1, 2, 3]).is_none());
        assert!(enc.push(&[4, 5, 6]).is_none());
        let parity = enc.push(&[7, 8, 9]).unwrap();
        // parity = 1^4^7, 2^5^8, 3^6^9
        assert_eq!(parity, vec![1 ^ 4 ^ 7, 2 ^ 5 ^ 8, 3 ^ 6 ^ 9]);
    }

    #[test]
    fn encoder_reset_clears_group() {
        let mut enc = FecEncoder::new(3);
        enc.push(&[1, 2, 3]);
        enc.reset();
        // Should need 3 more pushes to emit parity
        assert!(enc.push(&[10, 20]).is_none());
        assert!(enc.push(&[30, 40]).is_none());
        let parity = enc.push(&[50, 60]).unwrap();
        assert_eq!(parity, vec![10 ^ 30 ^ 50, 20 ^ 40 ^ 60]);
    }

    #[test]
    fn decoder_recovers_single_loss() {
        let payloads: Vec<Vec<u8>> = vec![vec![1, 2], vec![3, 4], vec![5, 6]];
        let parity = xor_payloads(&payloads);

        let mut dec = FecDecoder::new(3);
        // Feed packet 0 and 2, skip packet 1
        assert!(dec.feed_data(0, &payloads[0]).is_none());
        assert!(dec.feed_data(2, &payloads[2]).is_none());
        // Feed parity -> should recover packet 1
        let recovered = dec.feed_parity(&parity).unwrap();
        assert_eq!(recovered, payloads[1]);
    }

    #[test]
    fn decoder_no_loss_no_recovery() {
        let mut dec = FecDecoder::new(3);
        assert!(dec.feed_data(0, &[1]).is_none());
        assert!(dec.feed_data(1, &[2]).is_none());
        assert!(dec.feed_data(2, &[3]).is_none());
        // All received, parity won't trigger recovery
        assert!(dec.feed_parity(&[1 ^ 2 ^ 3]).is_none());
    }

    #[test]
    fn decoder_two_losses_unrecoverable() {
        let mut dec = FecDecoder::new(5);
        dec.feed_data(0, &[1]);
        dec.feed_data(1, &[2]);
        dec.feed_data(4, &[5]);
        // Missing seq 2 and 3 -> unrecoverable
        assert!(dec.is_unrecoverable());
    }

    #[test]
    fn variable_length_payloads() {
        let payloads: Vec<Vec<u8>> = vec![vec![1, 2, 3], vec![4, 5], vec![6]];
        let parity = xor_payloads(&payloads);
        assert_eq!(parity.len(), 3); // max length

        let mut dec = FecDecoder::new(3);
        dec.feed_data(0, &payloads[0]);
        dec.feed_data(2, &payloads[2]);
        let recovered = dec.feed_parity(&parity).unwrap();
        // recovered[0] = parity[0] ^ 1 ^ 6 = (1^4^6) ^ 1 ^ 6 = 4
        // recovered[1] = parity[1] ^ 2 ^ 0 = (2^5^0) ^ 2 ^ 0 = 5
        // recovered[2] = parity[2] ^ 3 ^ 0 = (3^0^0) ^ 3 ^ 0 = 0
        assert_eq!(recovered, vec![4, 5, 0]);
    }
}
