//! Protocol-v2 field numbers, following mymosh's MoshProto.swift.
use anyhow::{Result, bail, ensure};

#[derive(Default)]
pub(super) struct Instruction {
    pub version: u64,
    pub old: u64,
    pub new: u64,
    pub ack: u64,
    pub throwaway: u64,
    pub diff: Vec<u8>,
}

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 128 {
        out.push(value as u8 | 128);
        value >>= 7;
    }
    out.push(value as u8);
}
fn number(out: &mut Vec<u8>, field: u64, value: u64) {
    varint(out, field << 3);
    varint(out, value);
}
fn bytes(out: &mut Vec<u8>, field: u64, value: &[u8]) {
    varint(out, field << 3 | 2);
    varint(out, value.len() as u64);
    out.extend_from_slice(value);
}

impl Instruction {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (field, value) in [
            (1, 2),
            (2, self.old),
            (3, self.new),
            (4, self.ack),
            (5, self.throwaway),
        ] {
            number(&mut out, field, value);
        }
        bytes(&mut out, 6, &self.diff);
        out
    }
    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut result = Self::default();
        for field in Fields(data) {
            let (n, value) = field?;
            match (n, value) {
                (1, Value::Number(v)) => result.version = v,
                (2, Value::Number(v)) => result.old = v,
                (3, Value::Number(v)) => result.new = v,
                (4, Value::Number(v)) => result.ack = v,
                (5, Value::Number(v)) => result.throwaway = v,
                (6, Value::Bytes(v)) => result.diff = v.to_vec(),
                _ => {}
            }
        }
        ensure!(result.version == 2, "Unsupported Mosh protocol version");
        Ok(result)
    }
}

// Each local event is already a complete repeated UserMessage.instruction.
pub(super) fn input(data: &[u8]) -> Vec<u8> {
    let mut key = Vec::new();
    bytes(&mut key, 4, data);
    let mut instruction = Vec::new();
    bytes(&mut instruction, 2, &key);
    let mut out = Vec::new();
    bytes(&mut out, 1, &instruction);
    out
}
pub(super) fn resize(cols: u16, rows: u16) -> Vec<u8> {
    let mut size = Vec::new();
    number(&mut size, 5, cols.into());
    number(&mut size, 6, rows.into());
    let mut instruction = Vec::new();
    bytes(&mut instruction, 3, &size);
    let mut out = Vec::new();
    bytes(&mut out, 1, &instruction);
    out
}

pub(super) enum Operation<'a> {
    Host(&'a [u8]),
    Resize(u16, u16),
}
pub(super) fn host(data: &[u8]) -> Result<Vec<Operation<'_>>> {
    let mut out = Vec::new();
    for field in Fields(data) {
        if let (1, Value::Bytes(instruction)) = field? {
            for field in Fields(instruction) {
                match field? {
                    (2, Value::Bytes(message)) => {
                        for field in Fields(message) {
                            if let (4, Value::Bytes(data)) = field? {
                                out.push(Operation::Host(data));
                            }
                        }
                    }
                    (3, Value::Bytes(message)) => {
                        let (mut cols, mut rows) = (0, 0);
                        for field in Fields(message) {
                            match field? {
                                (5, Value::Number(n)) => cols = n,
                                (6, Value::Number(n)) => rows = n,
                                _ => {}
                            }
                        }
                        // Bound allocations even for an authenticated peer.
                        ensure!(
                            (1..=1000).contains(&cols)
                                && (1..=1000).contains(&rows)
                                && cols * rows <= 100_000,
                            "Invalid Mosh terminal size"
                        );
                        out.push(Operation::Resize(cols as u16, rows as u16));
                    }
                    _ => {} // echo acknowledgement is used only for prediction
                }
            }
        }
    }
    Ok(out)
}

enum Value<'a> {
    Number(u64),
    Bytes(&'a [u8]),
    Other,
}
struct Fields<'a>(&'a [u8]);
impl<'a> Fields<'a> {
    fn varint(&mut self) -> Result<u64> {
        let mut value = 0;
        for shift in (0..70).step_by(7) {
            let byte = *self
                .0
                .first()
                .ok_or_else(|| anyhow::anyhow!("Truncated protobuf"))?;
            self.0 = &self.0[1..];
            ensure!(shift < 63 || byte <= 1, "Invalid protobuf varint");
            value |= u64::from(byte & 127) << shift;
            if byte & 128 == 0 {
                return Ok(value);
            }
        }
        bail!("Invalid protobuf varint")
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(n <= self.0.len(), "Truncated protobuf field");
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }
    fn field(&mut self) -> Result<(u64, Value<'a>)> {
        let key = self.varint()?;
        ensure!(key >> 3 != 0, "Invalid protobuf field");
        let value = match key & 7 {
            0 => Value::Number(self.varint()?),
            2 => {
                let len = usize::try_from(self.varint()?)?;
                Value::Bytes(self.take(len)?)
            }
            1 => {
                self.take(8)?;
                Value::Other
            }
            5 => {
                self.take(4)?;
                Value::Other
            }
            _ => bail!("Invalid protobuf wire type"),
        };
        Ok((key >> 3, value))
    }
}
impl<'a> Iterator for Fields<'a> {
    type Item = Result<(u64, Value<'a>)>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.0.is_empty() {
            return None;
        }
        let result = self.field();
        if result.is_err() {
            self.0 = &[];
        }
        Some(result)
    }
}
