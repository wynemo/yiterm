//! AES-128-OCB, packet nonces and bounded fragment reassembly.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use ocb3::{
    Ocb3,
    aead::{Aead, KeyInit, consts::U12},
};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    time::{Duration, Instant},
};

const MAX_MESSAGE: usize = 4 * 1024 * 1024;
type Cipher = Ocb3<aes::Aes128, U12>;
pub(super) struct Wire {
    cipher: Cipher,
    sequence: u64,
    fragment_id: u64,
    highest_sequence: Option<u64>,
    timestamp: Option<(u16, Instant)>,
    assemblies: BTreeMap<u64, Assembly>,
}
struct Assembly {
    created: Instant,
    parts: BTreeMap<u16, Vec<u8>>,
    count: Option<u16>,
    bytes: usize,
}
pub(super) struct Received {
    pub message: Option<Vec<u8>>,
    pub rtt: Option<Duration>,
}
impl Wire {
    pub fn new(key: &str) -> Result<Self> {
        let key = zeroize::Zeroizing::new(STANDARD_NO_PAD.decode(key).context("Invalid Mosh key")?);
        ensure!(key.len() == 16, "Invalid Mosh key length");
        Ok(Self {
            cipher: Cipher::new_from_slice(&key)
                .map_err(|_| anyhow::anyhow!("Invalid Mosh key"))?,
            sequence: 0,
            fragment_id: 0,
            highest_sequence: None,
            timestamp: None,
            assemblies: BTreeMap::new(),
        })
    }
    pub fn encode(&mut self, message: &[u8], clock: u16) -> Result<Vec<Vec<u8>>> {
        let mut compressor =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        compressor.write_all(message)?;
        let compressed = compressor.finish()?;
        self.fragment_id = self
            .fragment_id
            .checked_add(1)
            .context("Mosh fragment ID exhausted")?;
        // Conservative IPv6-safe MTU, including IP/UDP/crypto/fragment headers.
        let chunks = compressed.chunks(1100);
        let count = chunks.len();
        ensure!(count < 32768, "Mosh input is too large");
        let mut out = Vec::new();
        for (index, part) in chunks.enumerate() {
            ensure!(self.sequence < 1 << 63, "Mosh packet sequence exhausted");
            let seq = self.sequence.to_be_bytes();
            self.sequence += 1; // never reuse a nonce, including after a failed send
            let mut nonce = [0u8; 12];
            nonce[4..].copy_from_slice(&seq);
            let reply = self
                .timestamp
                .take()
                .filter(|(_, at)| at.elapsed() < Duration::from_secs(1))
                .map_or(u16::MAX, |(stamp, at)| {
                    stamp.wrapping_add(at.elapsed().as_millis() as u16)
                });
            let mut plain = Vec::new();
            plain.extend_from_slice(&clock.to_be_bytes());
            plain.extend_from_slice(&reply.to_be_bytes());
            plain.extend_from_slice(&self.fragment_id.to_be_bytes());
            let header = index as u16 | if index + 1 == count { 0x8000 } else { 0 };
            plain.extend_from_slice(&header.to_be_bytes());
            plain.extend_from_slice(part);
            let encrypted = self
                .cipher
                .encrypt((&nonce).into(), plain.as_slice())
                .map_err(|_| anyhow::anyhow!("Mosh encryption failed"))?;
            let mut packet = seq.to_vec();
            packet.extend(encrypted);
            out.push(packet);
        }
        Ok(out)
    }
    pub fn receive(&mut self, data: &[u8], clock: u16) -> Result<Received> {
        ensure!(data.len() >= 38, "Truncated Mosh packet");
        let value = u64::from_be_bytes(data[..8].try_into()?);
        ensure!(value & (1 << 63) != 0, "Invalid Mosh direction");
        let sequence = value & !(1 << 63);
        let mut nonce = [0; 12];
        nonce[4..].copy_from_slice(&data[..8]);
        let plain = self
            .cipher
            .decrypt((&nonce).into(), &data[8..])
            .map_err(|_| anyhow::anyhow!("Mosh packet authentication failed"))?;
        // Reordered packets may contain fragments still needed by a newer
        // state; only monotonically newer packets update the timestamp reply.
        let stamp = u16::from_be_bytes(plain[..2].try_into()?);
        if self.highest_sequence.is_none_or(|n| sequence > n) {
            self.highest_sequence = Some(sequence);
            if stamp != u16::MAX {
                self.timestamp = Some((stamp, Instant::now()));
            }
        }
        let reply = u16::from_be_bytes(plain[2..4].try_into()?);
        let rtt =
            (reply != u16::MAX).then(|| Duration::from_millis(clock.wrapping_sub(reply).into()));
        let id = u64::from_be_bytes(plain[4..12].try_into()?);
        let header = u16::from_be_bytes(plain[12..14].try_into()?);
        let index = header & 0x7fff;
        self.assemblies
            .retain(|_, a| a.created.elapsed() < Duration::from_secs(10));
        if !self.assemblies.contains_key(&id) && self.assemblies.len() >= 8 {
            self.assemblies.pop_first();
        }
        let a = self.assemblies.entry(id).or_insert_with(|| Assembly {
            created: Instant::now(),
            parts: BTreeMap::new(),
            count: None,
            bytes: 0,
        });
        if header & 0x8000 != 0 {
            let count = index + 1;
            ensure!(
                a.count.is_none_or(|n| n == count),
                "Conflicting Mosh fragments"
            );
            ensure!(
                a.parts.keys().all(|n| *n < count),
                "Invalid Mosh final fragment"
            );
            a.count = Some(count);
        }
        ensure!(
            a.count.is_none_or(|n| index < n),
            "Invalid Mosh fragment index"
        );
        if let Some(previous) = a.parts.get(&index) {
            ensure!(
                previous == &plain[14..],
                "Conflicting Mosh fragment contents"
            );
        } else {
            a.bytes += plain.len() - 14;
            ensure!(a.bytes <= MAX_MESSAGE, "Mosh fragments exceed size limit");
            a.parts.insert(index, plain[14..].to_vec());
        }
        let complete = a.count.is_some_and(|n| a.parts.len() == usize::from(n));
        let message = if complete {
            let a = self.assemblies.remove(&id).unwrap();
            let compressed: Vec<u8> = a.parts.into_values().flatten().collect();
            let mut decoded = Vec::new();
            flate2::read::ZlibDecoder::new(compressed.as_slice())
                .take(MAX_MESSAGE as u64 + 1)
                .read_to_end(&mut decoded)?;
            ensure!(
                decoded.len() <= MAX_MESSAGE,
                "Mosh message exceeds size limit"
            );
            Some(decoded)
        } else {
            None
        };
        Ok(Received { message, rtt })
    }
}
