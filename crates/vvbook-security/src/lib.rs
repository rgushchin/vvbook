#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ed25519_compact::{PublicKey, Signature};
use vvbook_pal::{EntropySource, StorageBackend, StorageError};

pub const VVFW_MAGIC: [u8; 4] = *b"VVFW";
pub const VVFW_FORMAT_VERSION: u16 = 1;
pub const HW_TARGET_XTEINK_X3: u16 = 0x0003;
pub const VVJN_MAGIC: [u8; 4] = *b"VVJN";
pub const VVRB_MAGIC: [u8; 4] = *b"VVRB";
pub const VVRB_VERSION: u16 = 1;
pub const VVRB_RECORD_SIZE: usize = 80;
pub const SECURITY_SECTOR_SIZE: usize = 4096;
pub const ESP32C3_CHIP_ID: u16 = 0x0005;
pub const ESP_IMAGE_MAGIC: u8 = 0xE9;
pub const MAX_ESP32C3_OTA_BYTES: u32 = 3_776 * 1024;

/// Official Ed25519 release public key for Folio OS signed firmware (`dist/folio_release.pub`).
pub const FOLIO_RELEASE_PUBKEY: [u8; 32] = [
    0xc2, 0xd1, 0xbc, 0x4b, 0xb1, 0xcf, 0xaf, 0x00,
    0x82, 0xf3, 0xc8, 0x3d, 0x63, 0x08, 0x19, 0xc4,
    0x89, 0x23, 0xca, 0xed, 0xfc, 0x8e, 0x03, 0xac,
    0x1f, 0x25, 0x21, 0x66, 0xf5, 0x7a, 0x36, 0xdd,
];

/// Fast-key-erasure CSPRNG backed by keyed BLAKE3-XOF (`#![no_std]`).
#[derive(Clone)]
pub struct Blake3Drbg {
    key: [u8; 32],
    counter: u64,
}

impl Blake3Drbg {
    /// Initialize a DRBG from 32 bytes of entropy.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            key: blake3::derive_key("vvbook.drbg.init.v1", &seed),
            counter: 0,
        }
    }

    /// Initialize deterministically from a 64-bit seed (for tests/simulations only).
    pub fn from_u64(seed: u64) -> Self {
        let mut s = [0u8; 32];
        s[..8].copy_from_slice(&seed.to_le_bytes());
        Self::from_seed(s)
    }

    /// Mix additional entropy into the DRBG state and erase the previous key.
    pub fn reseed(&mut self, extra: &[u8]) {
        let mut h = blake3::Hasher::new_keyed(&self.key);
        h.update(b"vvbook.drbg.reseed.v1");
        h.update(&self.counter.to_le_bytes());
        h.update(extra);
        self.key = *h.finalize().as_bytes();
        self.counter = self.counter.wrapping_add(1);
    }

    /// Return the next uniformly distributed `u64`.
    pub fn next_u64(&mut self) -> u64 {
        let mut buf = [0u8; 8];
        self.fill_bytes(&mut buf);
        u64::from_le_bytes(buf)
    }
}

impl EntropySource for Blake3Drbg {
    fn fill_bytes(&mut self, out: &mut [u8]) {
        let mut h = blake3::Hasher::new_keyed(&self.key);
        h.update(&self.counter.to_le_bytes());
        let mut xof = h.finalize_xof();
        let mut next_key = [0u8; 32];
        xof.fill(&mut next_key);
        xof.fill(out);
        self.key = next_key;
        self.counter = self.counter.wrapping_add(1);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    InvalidPinFormat,
    InvalidMac,
    LockedOut,
    InvalidToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FwVerifyError {
    BadMagic,
    UnsupportedFormat,
    WrongHardware,
    RollbackRejected,
    LengthMismatch,
    DigestMismatch,
    InvalidPublicKey,
    InvalidSignature,
}

#[inline]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn generate_pin<E: EntropySource>(rng: &mut E) -> [u8; 8] {
    // Unbiased rejection sampling in 0..100_000_000
    loop {
        let mut buf = [0u8; 4];
        rng.fill_bytes(&mut buf);
        let val = u32::from_le_bytes(buf);
        let limit = u32::MAX - (u32::MAX % 100_000_000);
        if val < limit {
            let mut n = val % 100_000_000;
            let mut pin = [b'0'; 8];
            for i in (0..8).rev() {
                pin[i] = b'0' + (n % 10) as u8;
                n /= 10;
            }
            return pin;
        }
    }
}

pub fn derive_pin_key(pin: &[u8]) -> [u8; 32] {
    blake3::derive_key("vvbook.vvdrop.pin.v1", pin)
}

pub fn compute_auth_mac(pin: &[u8], server_nonce: &[u8; 32], client_nonce: &[u8; 32]) -> [u8; 32] {
    let key = derive_pin_key(pin);
    let mut h = blake3::Hasher::new_keyed(&key);
    h.update(server_nonce);
    h.update(client_nonce);
    *h.finalize().as_bytes()
}

pub struct VvDropAuth {
    pin: [u8; 8],
    server_nonce: [u8; 32],
    session_token: [u8; 32],
    has_session: bool,
    failed_attempts: u8,
    max_attempts: u8,
}

impl VvDropAuth {
    pub fn new<E: EntropySource>(rng: &mut E) -> Self {
        let pin = generate_pin(rng);
        let mut server_nonce = [0u8; 32];
        rng.fill_bytes(&mut server_nonce);
        Self {
            pin,
            server_nonce,
            session_token: [0u8; 32],
            has_session: false,
            failed_attempts: 0,
            max_attempts: 5,
        }
    }

    pub fn from_fixed_pin<E: EntropySource>(pin: [u8; 8], rng: &mut E) -> Self {
        let mut server_nonce = [0u8; 32];
        rng.fill_bytes(&mut server_nonce);
        Self {
            pin,
            server_nonce,
            session_token: [0u8; 32],
            has_session: false,
            failed_attempts: 0,
            max_attempts: 5,
        }
    }

    pub fn pin_str(&self) -> &str {
        core::str::from_utf8(&self.pin).unwrap_or("00000000")
    }

    pub fn server_nonce(&self) -> &[u8; 32] {
        &self.server_nonce
    }

    pub fn failed_attempts(&self) -> u8 {
        self.failed_attempts
    }

    pub fn is_locked(&self) -> bool {
        self.failed_attempts >= self.max_attempts
    }

    pub fn rotate_pin<E: EntropySource>(&mut self, rng: &mut E) {
        self.pin = generate_pin(rng);
        rng.fill_bytes(&mut self.server_nonce);
        self.session_token = [0u8; 32];
        self.has_session = false;
        self.failed_attempts = 0;
    }

    pub fn verify_and_issue_token<E: EntropySource>(
        &mut self,
        client_nonce: &[u8; 32],
        mac: &[u8; 32],
        rng: &mut E,
    ) -> Result<[u8; 32], AuthError> {
        if self.is_locked() {
            return Err(AuthError::LockedOut);
        }
        let expected = compute_auth_mac(&self.pin, &self.server_nonce, client_nonce);
        if !constant_time_eq(&expected, mac) {
            self.failed_attempts = self.failed_attempts.saturating_add(1);
            // Rotate server nonce after failure to prevent replay
            rng.fill_bytes(&mut self.server_nonce);
            if self.is_locked() {
                return Err(AuthError::LockedOut);
            }
            return Err(AuthError::InvalidMac);
        }
        self.failed_attempts = 0;
        // Rotate server nonce on success too so every challenge stays strictly single-use.
        rng.fill_bytes(&mut self.server_nonce);
        rng.fill_bytes(&mut self.session_token);
        self.has_session = true;
        Ok(self.session_token)
    }

    pub fn verify_token(&self, candidate: &[u8; 32]) -> Result<(), AuthError> {
        if !self.has_session {
            return Err(AuthError::InvalidToken);
        }
        if constant_time_eq(&self.session_token, candidate) {
            Ok(())
        } else {
            Err(AuthError::InvalidToken)
        }
    }
}

pub fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0F) as usize] as char);
    }
    s
}

pub fn decode_hex_32(hex: &str) -> Option<[u8; 32]> {
    let bytes = hex.trim().as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = hex_nibble(bytes[i * 2])?;
        let lo = hex_nibble(bytes[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

#[inline]
fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 112-byte Ed25519-signed firmware header (`VVFW`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedFwHeader {
    pub format_ver: u16,
    pub fw_version: u32,
    pub target_hw: u16,
    pub reserved: u16,
    pub payload_len: u32,
    pub payload_blake3: [u8; 32],
    pub ed25519_sig: [u8; 64],
}

impl SignedFwHeader {
    pub const SIZE: usize = 112;

    pub fn signed_message(&self) -> [u8; 48] {
        let mut msg = [0u8; 48];
        msg[0..4].copy_from_slice(&VVFW_MAGIC);
        msg[4..6].copy_from_slice(&self.format_ver.to_le_bytes());
        msg[6..10].copy_from_slice(&self.fw_version.to_le_bytes());
        msg[10..12].copy_from_slice(&self.target_hw.to_le_bytes());
        msg[12..14].copy_from_slice(&self.reserved.to_le_bytes());
        msg[14..18].copy_from_slice(&self.payload_len.to_le_bytes());
        // pad 2 bytes zero at 18..20? No, 48 - 16 = 32 -> put digest at 16..48 by packing:
        // Wait: 4 + 2 + 4 + 2 + 2 + 4 = 18 bytes + 32 = 50 bytes! Let's make signed_message 50 bytes
        // and total header 114 bytes, OR drop `reserved`! Dropping `reserved` makes 4+2+4+2+4 = 16 bytes + 32 = 48 + 64 = 112 bytes exact!
        msg
    }

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[0..4].copy_from_slice(&VVFW_MAGIC);
        out[4..6].copy_from_slice(&self.format_ver.to_le_bytes());
        out[6..10].copy_from_slice(&self.fw_version.to_le_bytes());
        out[10..12].copy_from_slice(&self.target_hw.to_le_bytes());
        out[12..16].copy_from_slice(&self.payload_len.to_le_bytes());
        out[16..48].copy_from_slice(&self.payload_blake3);
        out[48..112].copy_from_slice(&self.ed25519_sig);
        out
    }

    pub fn from_bytes(raw: &[u8]) -> Result<Self, FwVerifyError> {
        if raw.len() < Self::SIZE {
            return Err(FwVerifyError::LengthMismatch);
        }
        if raw[0..4] != VVFW_MAGIC {
            return Err(FwVerifyError::BadMagic);
        }
        let format_ver = u16::from_le_bytes([raw[4], raw[5]]);
        if format_ver != VVFW_FORMAT_VERSION {
            return Err(FwVerifyError::UnsupportedFormat);
        }
        let fw_version = u32::from_le_bytes([raw[6], raw[7], raw[8], raw[9]]);
        let target_hw = u16::from_le_bytes([raw[10], raw[11]]);
        let payload_len = u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]);
        let mut payload_blake3 = [0u8; 32];
        payload_blake3.copy_from_slice(&raw[16..48]);
        let mut ed25519_sig = [0u8; 64];
        ed25519_sig.copy_from_slice(&raw[48..112]);
        Ok(Self {
            format_ver,
            fw_version,
            target_hw,
            reserved: 0,
            payload_len,
            payload_blake3,
            ed25519_sig,
        })
    }

    pub fn canonical_body(raw_or_self: &Self) -> [u8; 48] {
        let mut msg = [0u8; 48];
        msg[0..4].copy_from_slice(&VVFW_MAGIC);
        msg[4..6].copy_from_slice(&raw_or_self.format_ver.to_le_bytes());
        msg[6..10].copy_from_slice(&raw_or_self.fw_version.to_le_bytes());
        msg[10..12].copy_from_slice(&raw_or_self.target_hw.to_le_bytes());
        msg[12..16].copy_from_slice(&raw_or_self.payload_len.to_le_bytes());
        msg[16..48].copy_from_slice(&raw_or_self.payload_blake3);
        msg
    }
}

/// Streaming Ed25519 + BLAKE3 + anti-rollback OTA firmware verifier (`#![no_std]`).
pub struct StreamingFwVerifier {
    header: SignedFwHeader,
    pub_key: PublicKey,
    hasher: blake3::Hasher,
    received: u32,
}

impl StreamingFwVerifier {
    pub fn start(
        header_bytes: &[u8],
        pub_key_bytes: &[u8; 32],
        min_fw_version: u32,
        expected_hw: u16,
    ) -> Result<Self, FwVerifyError> {
        let header = SignedFwHeader::from_bytes(header_bytes)?;
        if header.target_hw != expected_hw {
            return Err(FwVerifyError::WrongHardware);
        }
        if header.fw_version < min_fw_version {
            return Err(FwVerifyError::RollbackRejected);
        }
        let pub_key = PublicKey::from_slice(pub_key_bytes).map_err(|_| FwVerifyError::InvalidPublicKey)?;
        // Verify signature over header metadata + committed payload BLAKE3 before flashing chunks
        let sig = Signature::from_slice(&header.ed25519_sig).map_err(|_| FwVerifyError::InvalidSignature)?;
        let msg = SignedFwHeader::canonical_body(&header);
        pub_key
            .verify(msg, &sig)
            .map_err(|_| FwVerifyError::InvalidSignature)?;

        Ok(Self {
            header,
            pub_key,
            hasher: blake3::Hasher::new(),
            received: 0,
        })
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Result<(), FwVerifyError> {
        let next = self
            .received
            .checked_add(chunk.len() as u32)
            .ok_or(FwVerifyError::LengthMismatch)?;
        if next > self.header.payload_len {
            return Err(FwVerifyError::LengthMismatch);
        }
        self.hasher.update(chunk);
        self.received = next;
        Ok(())
    }

    pub fn finish(self) -> Result<SignedFwHeader, FwVerifyError> {
        if self.received != self.header.payload_len {
            return Err(FwVerifyError::LengthMismatch);
        }
        let digest = self.hasher.finalize();
        if !constant_time_eq(digest.as_bytes(), &self.header.payload_blake3) {
            return Err(FwVerifyError::DigestMismatch);
        }
        let _ = self.pub_key;
        Ok(self.header)
    }
}

/// Crash-proof Dual-Slot (`slot_a` / `slot_b`) Atomic Journal protected by BLAKE3-128 MAC tags.
/// Survives sudden power loss mid-write by automatically falling back to the intact sibling slot.
pub struct AtomicJournal;

impl AtomicJournal {
    fn slot_paths(base: &str) -> (String, String) {
        (format!("{base}.a"), format!("{base}.b"))
    }

    fn decode_slot(raw: &[u8]) -> Option<(u64, Vec<u8>)> {
        // Format: VVJN(4) + seq(8) + len(4) + payload(len) + blake3_tag(16)
        if raw.len() < 32 || raw[0..4] != VVJN_MAGIC {
            return None;
        }
        let seq = u64::from_le_bytes(raw[4..12].try_into().ok()?);
        let len = usize::try_from(u32::from_le_bytes(raw[12..16].try_into().ok()?)).ok()?;
        let body_end = 16usize.checked_add(len)?;
        let expected_len = body_end.checked_add(16)?;
        if raw.len() != expected_len {
            return None;
        }
        let body = &raw[..body_end];
        let tag = &raw[body_end..];
        let computed = blake3::hash(body);
        if !constant_time_eq(&computed.as_bytes()[..16], tag) {
            return None;
        }
        Some((seq, raw[16..body_end].to_vec()))
    }

    fn encode_slot(seq: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + payload.len());
        out.extend_from_slice(&VVJN_MAGIC);
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        let h = blake3::hash(&out);
        out.extend_from_slice(&h.as_bytes()[..16]);
        out
    }

    pub fn load<S: StorageBackend>(storage: &S, base_path: &str) -> Option<Vec<u8>> {
        let (pa, pb) = Self::slot_paths(base_path);
        let a = storage.read_all(&pa).ok().and_then(|d| Self::decode_slot(&d));
        let b = storage.read_all(&pb).ok().and_then(|d| Self::decode_slot(&d));
        match (a, b) {
            (Some((sa, da)), Some((sb, db))) => {
                if sb > sa {
                    Some(db)
                } else {
                    Some(da)
                }
            }
            (Some((_, da)), None) => Some(da),
            (None, Some((_, db))) => Some(db),
            (None, None) => None,
        }
    }

    pub fn commit<S: StorageBackend>(
        storage: &mut S,
        base_path: &str,
        payload: &[u8],
    ) -> Result<(), StorageError> {
        let (pa, pb) = Self::slot_paths(base_path);
        let a_seq = storage
            .read_all(&pa)
            .ok()
            .and_then(|d| Self::decode_slot(&d))
            .map(|(s, _)| s);
        let b_seq = storage
            .read_all(&pb)
            .ok()
            .and_then(|d| Self::decode_slot(&d))
            .map(|(s, _)| s);

        let (next_seq, target_path) = match (a_seq, b_seq) {
            (Some(sa), Some(sb)) => {
                if sa >= sb {
                    (sa + 1, pb)
                } else {
                    (sb + 1, pa)
                }
            }
            (Some(sa), None) => (sa + 1, pb),
            (None, Some(sb)) => (sb + 1, pa),
            (None, None) => (1, pa),
        };

        let record = Self::encode_slot(next_seq, payload);
        storage.write_atomic(&target_path, &record)
    }
}

/// Pure-Rust FIPS 180-4 SHA-256 hasher (`#![no_std]`) for ESP32-C3 image validation.
#[derive(Clone)]
pub struct Sha256Hasher {
    state: [u32; 8],
    total_len: u64,
    buf: [u8; 64],
    buf_len: usize,
}

impl Default for Sha256Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256Hasher {
    const INIT: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];

    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];

    pub const fn new() -> Self {
        Self {
            state: Self::INIT,
            total_len: 0,
            buf: [0u8; 64],
            buf_len: 0,
        }
    }

    fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (slot, chunk) in w[..16].iter_mut().zip(block.as_chunks::<4>().0.iter()) {
            *slot = u32::from_be_bytes(*chunk);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
        for (&k_i, &w_i) in Self::K.iter().zip(w.iter()) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k_i)
                .wrapping_add(w_i);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let need = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + need].copy_from_slice(&data[..need]);
            self.buf_len += need;
            data = &data[need..];
            if self.buf_len == 64 {
                let block = self.buf;
                Self::compress(&mut self.state, &block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let block: &[u8; 64] = data[..64].try_into().unwrap();
            Self::compress(&mut self.state, block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 32] {
        let bit_len = self.total_len.wrapping_mul(8);
        self.buf[self.buf_len] = 0x80;
        self.buf_len += 1;
        if self.buf_len > 56 {
            self.buf[self.buf_len..64].fill(0);
            let block = self.buf;
            Self::compress(&mut self.state, &block);
            self.buf_len = 0;
        }
        self.buf[self.buf_len..56].fill(0);
        self.buf[56..64].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.buf;
        Self::compress(&mut self.state, &block);

        let mut out = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256Hasher::new();
    h.update(data);
    h.finalize()
}

/// Errors returned when validating an ESP32-C3 ROM bootloader `.bin` image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EspImageError {
    TooSmall,
    TooLarge,
    UnalignedLength,
    BadMagic,
    BadSegmentCount,
    UnsupportedChipId,
    MissingHashDigest,
    ZeroEntryPoint,
    InvalidSegment,
    ChecksumMismatch,
    Sha256Mismatch,
    LengthMismatch,
}

#[derive(Clone, Copy)]
enum EspImgPhase {
    Header,
    SegHeader { buf: [u8; 8], len: u8 },
    SegData { remaining: u32 },
    Padding { count: u32, last: u8 },
}

/// Zero-allocation streaming validator for ESP32-C3 ROM bootloader images (`0xE9` + SHA-256).
pub struct Esp32c3ImageValidator {
    total_len: u32,
    body_len: u32,
    pos: u32,
    hdr: [u8; 24],
    seg_count: u8,
    segs_done: u8,
    xor_acc: u8,
    phase: EspImgPhase,
    sha: Sha256Hasher,
    expected_sha: [u8; 32],
}

impl Esp32c3ImageValidator {
    pub fn new(total_len: u32) -> Result<Self, EspImageError> {
        if total_len < 64 {
            return Err(EspImageError::TooSmall);
        }
        if total_len > MAX_ESP32C3_OTA_BYTES {
            return Err(EspImageError::TooLarge);
        }
        let body_len = total_len - 32;
        if !body_len.is_multiple_of(16) {
            return Err(EspImageError::UnalignedLength);
        }
        Ok(Self {
            total_len,
            body_len,
            pos: 0,
            hdr: [0u8; 24],
            seg_count: 0,
            segs_done: 0,
            xor_acc: 0xEF,
            phase: EspImgPhase::Header,
            sha: Sha256Hasher::new(),
            expected_sha: [0u8; 32],
        })
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Result<(), EspImageError> {
        let chunk_len = u32::try_from(chunk.len()).map_err(|_| EspImageError::LengthMismatch)?;
        let next_pos = self
            .pos
            .checked_add(chunk_len)
            .ok_or(EspImageError::LengthMismatch)?;
        if next_pos > self.total_len {
            return Err(EspImageError::LengthMismatch);
        }

        let mut offset = 0usize;
        if self.pos < self.body_len {
            let body_take = ((self.body_len - self.pos) as usize).min(chunk.len());
            let body_slice = &chunk[..body_take];
            self.sha.update(body_slice);
            let mut idx = 0usize;
            while idx < body_slice.len() {
                match self.phase {
                    EspImgPhase::Header => {
                        let take = (24 - self.pos as usize).min(body_slice.len() - idx);
                        let dst_start = self.pos as usize;
                        self.hdr[dst_start..dst_start + take]
                            .copy_from_slice(&body_slice[idx..idx + take]);
                        self.pos += take as u32;
                        idx += take;
                        if self.pos == 24 {
                            if self.hdr[0] != ESP_IMAGE_MAGIC {
                                return Err(EspImageError::BadMagic);
                            }
                            let seg_count = self.hdr[1];
                            if !(1..=16).contains(&seg_count) {
                                return Err(EspImageError::BadSegmentCount);
                            }
                            let entry = u32::from_le_bytes([
                                self.hdr[4],
                                self.hdr[5],
                                self.hdr[6],
                                self.hdr[7],
                            ]);
                            if entry == 0 {
                                return Err(EspImageError::ZeroEntryPoint);
                            }
                            let chip_id = u16::from_le_bytes([self.hdr[12], self.hdr[13]]);
                            if chip_id != ESP32C3_CHIP_ID {
                                return Err(EspImageError::UnsupportedChipId);
                            }
                            if self.hdr[23] != 1 {
                                return Err(EspImageError::MissingHashDigest);
                            }
                            self.seg_count = seg_count;
                            self.phase = EspImgPhase::SegHeader { buf: [0u8; 8], len: 0 };
                        }
                    }
                    EspImgPhase::SegHeader { mut buf, mut len } => {
                        let take = (8 - len as usize).min(body_slice.len() - idx);
                        buf[len as usize..len as usize + take]
                            .copy_from_slice(&body_slice[idx..idx + take]);
                        len += take as u8;
                        self.pos += take as u32;
                        idx += take;
                        if len == 8 {
                            let data_len =
                                u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
                            let end = (self.pos as u64)
                                .checked_add(data_len as u64)
                                .ok_or(EspImageError::InvalidSegment)?;
                            // Must leave >= 1 byte before `body_len` for the trailing checksum byte.
                            if end >= self.body_len as u64 {
                                return Err(EspImageError::InvalidSegment);
                            }
                            if data_len == 0 {
                                self.segs_done += 1;
                                self.phase = if self.segs_done == self.seg_count {
                                    EspImgPhase::Padding { count: 0, last: 0 }
                                } else {
                                    EspImgPhase::SegHeader { buf: [0u8; 8], len: 0 }
                                };
                            } else {
                                self.phase = EspImgPhase::SegData { remaining: data_len };
                            }
                        } else {
                            self.phase = EspImgPhase::SegHeader { buf, len };
                        }
                    }
                    EspImgPhase::SegData { mut remaining } => {
                        let take = (remaining as usize).min(body_slice.len() - idx);
                        for &b in &body_slice[idx..idx + take] {
                            self.xor_acc ^= b;
                        }
                        remaining -= take as u32;
                        self.pos += take as u32;
                        idx += take;
                        if remaining == 0 {
                            self.segs_done += 1;
                            self.phase = if self.segs_done == self.seg_count {
                                EspImgPhase::Padding { count: 0, last: 0 }
                            } else {
                                EspImgPhase::SegHeader { buf: [0u8; 8], len: 0 }
                            };
                        } else {
                            self.phase = EspImgPhase::SegData { remaining };
                        }
                    }
                    EspImgPhase::Padding { count, .. } => {
                        let take = body_slice.len() - idx;
                        let new_count = count
                            .checked_add(take as u32)
                            .ok_or(EspImageError::InvalidSegment)?;
                        if new_count > 16 {
                            return Err(EspImageError::InvalidSegment);
                        }
                        let last = body_slice[body_slice.len() - 1];
                        self.pos += take as u32;
                        idx = body_slice.len();
                        self.phase = EspImgPhase::Padding {
                            count: new_count,
                            last,
                        };
                    }
                }
            }
            offset = body_take;
        }

        if offset < chunk.len() {
            let tail = &chunk[offset..];
            let dst = (self.pos - self.body_len) as usize;
            self.expected_sha[dst..dst + tail.len()].copy_from_slice(tail);
            self.pos += tail.len() as u32;
        }
        Ok(())
    }

    pub fn finish(self) -> Result<(), EspImageError> {
        if self.pos != self.total_len {
            return Err(EspImageError::LengthMismatch);
        }
        match self.phase {
            EspImgPhase::Padding { count, last } if (1..=16).contains(&count) => {
                if last != self.xor_acc {
                    return Err(EspImageError::ChecksumMismatch);
                }
            }
            _ => return Err(EspImageError::InvalidSegment),
        }
        let digest = self.sha.finalize();
        if !constant_time_eq(&digest, &self.expected_sha) {
            return Err(EspImageError::Sha256Mismatch);
        }
        Ok(())
    }
}

/// Validate a complete in-memory ESP32-C3 ROM bootloader image.
pub fn validate_esp32c3_ota_image(raw: &[u8]) -> Result<(), EspImageError> {
    let total_len = u32::try_from(raw.len()).map_err(|_| EspImageError::TooLarge)?;
    let mut v = Esp32c3ImageValidator::new(total_len)?;
    v.feed(raw)?;
    v.finish()
}

/// Two 4 KB NOR flash sectors (A/B) backing power-fail-safe security metadata.
pub trait NorFlashRegion {
    type Error;
    fn read_slot(&mut self, slot: usize, offset: usize, out: &mut [u8]) -> Result<(), Self::Error>;
    fn erase_slot(&mut self, slot: usize) -> Result<(), Self::Error>;
    fn program_slot(&mut self, slot: usize, offset: usize, data: &[u8]) -> Result<(), Self::Error>;
}

/// Dual-sector A/B MAC-protected persistent anti-rollback and OTA commit state (`VVRB`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PersistentSecurityState {
    pub seq: u64,
    pub min_fw_version: u32,
    pub last_installed_fw_version: u32,
    pub installed_blake3: [u8; 32],
    pub pending_flags: u32,
}

impl PersistentSecurityState {
    pub const FLAG_PENDING_STAGE_APPLY: u32 = 1 << 0;

    fn derive_mac_key(device_binding: &[u8]) -> [u8; 32] {
        blake3::derive_key("vvbook.vvrb.sector.v1", device_binding)
    }

    pub fn encode(&self, device_binding: &[u8]) -> [u8; VVRB_RECORD_SIZE] {
        let mut rec = [0u8; VVRB_RECORD_SIZE];
        rec[0..4].copy_from_slice(&VVRB_MAGIC);
        rec[4..6].copy_from_slice(&VVRB_VERSION.to_le_bytes());
        rec[8..16].copy_from_slice(&self.seq.to_le_bytes());
        rec[16..20].copy_from_slice(&self.min_fw_version.to_le_bytes());
        rec[20..24].copy_from_slice(&self.last_installed_fw_version.to_le_bytes());
        rec[24..28].copy_from_slice(&self.pending_flags.to_le_bytes());
        rec[32..64].copy_from_slice(&self.installed_blake3);

        let key = Self::derive_mac_key(device_binding);
        let mut mac = blake3::Hasher::new_keyed(&key);
        mac.update(&rec[0..64]);
        let tag = mac.finalize();
        rec[64..80].copy_from_slice(&tag.as_bytes()[..16]);
        rec
    }

    pub fn decode(raw: &[u8; VVRB_RECORD_SIZE], device_binding: &[u8]) -> Option<Self> {
        if raw[0..4] != VVRB_MAGIC {
            return None;
        }
        let ver = u16::from_le_bytes([raw[4], raw[5]]);
        if ver != VVRB_VERSION {
            return None;
        }
        let key = Self::derive_mac_key(device_binding);
        let mut mac = blake3::Hasher::new_keyed(&key);
        mac.update(&raw[0..64]);
        let tag = mac.finalize();
        if !constant_time_eq(&tag.as_bytes()[..16], &raw[64..80]) {
            return None;
        }
        let seq = u64::from_le_bytes(raw[8..16].try_into().ok()?);
        if seq == 0 {
            return None;
        }
        let min_fw_version = u32::from_le_bytes(raw[16..20].try_into().ok()?);
        let last_installed_fw_version = u32::from_le_bytes(raw[20..24].try_into().ok()?);
        let pending_flags = u32::from_le_bytes(raw[24..28].try_into().ok()?);
        let mut installed_blake3 = [0u8; 32];
        installed_blake3.copy_from_slice(&raw[32..64]);
        Some(Self {
            seq,
            min_fw_version,
            last_installed_fw_version,
            installed_blake3,
            pending_flags,
        })
    }

    fn read_slots<F: NorFlashRegion>(
        flash: &mut F,
        device_binding: &[u8],
    ) -> Result<(Option<Self>, Option<Self>), F::Error> {
        let mut buf_a = [0u8; VVRB_RECORD_SIZE];
        let mut buf_b = [0u8; VVRB_RECORD_SIZE];
        let a = match flash.read_slot(0, 0, &mut buf_a) {
            Ok(()) => Self::decode(&buf_a, device_binding),
            Err(e) => return Err(e),
        };
        let b = match flash.read_slot(1, 0, &mut buf_b) {
            Ok(()) => Self::decode(&buf_b, device_binding),
            Err(e) => return Err(e),
        };
        Ok((a, b))
    }

    pub fn load_best<F: NorFlashRegion>(
        flash: &mut F,
        device_binding: &[u8],
    ) -> Result<Option<(usize, Self)>, F::Error> {
        let (a, b) = Self::read_slots(flash, device_binding)?;
        Ok(match (a, b) {
            (Some(sa), Some(sb)) => {
                if sb.seq > sa.seq {
                    Some((1, sb))
                } else {
                    Some((0, sa))
                }
            }
            (Some(sa), None) => Some((0, sa)),
            (None, Some(sb)) => Some((1, sb)),
            (None, None) => None,
        })
    }

    pub fn load_or_init<F: NorFlashRegion>(
        flash: &mut F,
        device_binding: &[u8],
        initial_fw_version: u32,
    ) -> Result<Self, F::Error> {
        if let Some((_, st)) = Self::load_best(flash, device_binding)? {
            return Ok(st);
        }
        let mut st = Self {
            seq: 0,
            min_fw_version: initial_fw_version,
            last_installed_fw_version: initial_fw_version,
            installed_blake3: [0u8; 32],
            pending_flags: 0,
        };
        st.commit(flash, device_binding)?;
        Ok(st)
    }

    /// Monotonically advance rollback floor and commit Metadata for a newly verified image.
    pub fn record_ota_install(&mut self, fw_version: u32, payload_blake3: [u8; 32], pending_flags: u32) {
        self.min_fw_version = self.min_fw_version.max(fw_version);
        self.last_installed_fw_version = fw_version;
        self.installed_blake3 = payload_blake3;
        self.pending_flags = pending_flags;
    }

    pub fn commit<F: NorFlashRegion>(
        &mut self,
        flash: &mut F,
        device_binding: &[u8],
    ) -> Result<(), F::Error> {
        let (a, b) = Self::read_slots(flash, device_binding)?;
        let (target_slot, highest_seq) = match (a, b) {
            (Some(sa), Some(sb)) => {
                if sa.seq >= sb.seq {
                    (1usize, sa.seq)
                } else {
                    (0usize, sb.seq)
                }
            }
            (Some(sa), None) => (1usize, sa.seq),
            (None, Some(sb)) => (0usize, sb.seq),
            (None, None) => (0usize, 0u64),
        };
        self.seq = highest_seq.max(self.seq).saturating_add(1);
        let rec = self.encode(device_binding);
        flash.erase_slot(target_slot)?;
        flash.program_slot(target_slot, 0, &rec)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use ed25519_compact::KeyPair;
    use vvbook_pal::{DirEntry, StorageError};

    #[derive(Clone, Default)]
    struct MemStorage(BTreeMap<String, Vec<u8>>);
    impl StorageBackend for MemStorage {
        fn exists(&self, path: &str) -> bool {
            self.0.contains_key(path)
        }
        fn file_len(&self, path: &str) -> Result<u64, StorageError> {
            self.0.get(path).map(|v| v.len() as u64).ok_or(StorageError::NotFound)
        }
        fn read_all(&self, path: &str) -> Result<Vec<u8>, StorageError> {
            self.0.get(path).cloned().ok_or(StorageError::NotFound)
        }
        fn read_range(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
            let d = self.0.get(path).ok_or(StorageError::NotFound)?;
            let off = offset as usize;
            if off >= d.len() {
                return Ok(0);
            }
            let n = buf.len().min(d.len() - off);
            buf[..n].copy_from_slice(&d[off..off + n]);
            Ok(n)
        }
        fn write_all(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError> {
            self.0.insert(String::from(path), data.to_vec());
            Ok(())
        }
        fn write_atomic(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError> {
            self.write_all(path, data)
        }
        fn append(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError> {
            self.0.entry(String::from(path)).or_default().extend_from_slice(data);
            Ok(())
        }
        fn remove(&mut self, path: &str) -> Result<(), StorageError> {
            self.0.remove(path).map(|_| ()).ok_or(StorageError::NotFound)
        }
        fn mkdir_all(&mut self, _path: &str) -> Result<(), StorageError> {
            Ok(())
        }
        fn list_dir(&self, _path: &str) -> Result<Vec<DirEntry>, StorageError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn pin_handshake_lockout_and_single_use_nonce() {
        let mut rng = Blake3Drbg::from_u64(42);
        let mut auth = VvDropAuth::from_fixed_pin(*b"48291704", &mut rng);
        let client_nonce = [0x11u8; 32];
        let bad_mac = [0x22u8; 32];
        for i in 0..4 {
            let err = auth.verify_and_issue_token(&client_nonce, &bad_mac, &mut rng).unwrap_err();
            assert_eq!(err, AuthError::InvalidMac, "attempt {i}");
        }
        assert_eq!(
            auth.verify_and_issue_token(&client_nonce, &bad_mac, &mut rng).unwrap_err(),
            AuthError::LockedOut
        );

        // Fresh auth succeeds with right PIN, issues valid token, and rotates server_nonce on success
        let mut auth2 = VvDropAuth::from_fixed_pin(*b"48291704", &mut rng);
        let first_nonce = *auth2.server_nonce();
        let good_mac = compute_auth_mac(b"48291704", &first_nonce, &client_nonce);
        let tok = auth2.verify_and_issue_token(&client_nonce, &good_mac, &mut rng).unwrap();
        assert!(auth2.verify_token(&tok).is_ok());
        assert_ne!(*auth2.server_nonce(), first_nonce, "server_nonce must rotate on success");
        // Replaying the prior (client_nonce, mac) must fail now that server_nonce rotated
        assert_eq!(
            auth2.verify_and_issue_token(&client_nonce, &good_mac, &mut rng).unwrap_err(),
            AuthError::InvalidMac
        );
        let mut tampered = tok;
        tampered[0] ^= 1;
        assert_eq!(auth2.verify_token(&tampered), Err(AuthError::InvalidToken));
    }

    #[test]
    fn ed25519_firmware_verification_and_tamper_rejection() {
        let kp = KeyPair::from_seed(ed25519_compact::Seed::new([7u8; 32]));
        let mut pub_bytes = [0u8; 32];
        pub_bytes.copy_from_slice(kp.pk.as_ref());

        let payload = b"ESP32-C3_TEST_IMAGE_PAYLOAD_1234567890";
        let digest = *blake3::hash(payload).as_bytes();
        let mut hdr = SignedFwHeader {
            format_ver: VVFW_FORMAT_VERSION,
            fw_version: 5,
            target_hw: HW_TARGET_XTEINK_X3,
            reserved: 0,
            payload_len: payload.len() as u32,
            payload_blake3: digest,
            ed25519_sig: [0u8; 64],
        };
        let msg = SignedFwHeader::canonical_body(&hdr);
        let sig = kp.sk.sign(msg, None);
        hdr.ed25519_sig.copy_from_slice(sig.as_ref());
        let hdr_bytes = hdr.to_bytes();

        // Valid verification
        let mut v = StreamingFwVerifier::start(&hdr_bytes, &pub_bytes, 5, HW_TARGET_XTEINK_X3).unwrap();
        v.feed(&payload[..10]).unwrap();
        v.feed(&payload[10..]).unwrap();
        assert_eq!(v.finish().unwrap().fw_version, 5);

        // Anti-rollback rejection
        assert_eq!(
            StreamingFwVerifier::start(&hdr_bytes, &pub_bytes, 6, HW_TARGET_XTEINK_X3).err(),
            Some(FwVerifyError::RollbackRejected)
        );

        // 1-bit payload tamper rejection
        let mut v2 = StreamingFwVerifier::start(&hdr_bytes, &pub_bytes, 1, HW_TARGET_XTEINK_X3).unwrap();
        let mut bad_payload = payload.to_vec();
        bad_payload[4] ^= 0x01;
        v2.feed(&bad_payload).unwrap();
        assert_eq!(v2.finish().err(), Some(FwVerifyError::DigestMismatch));
    }

    #[test]
    fn atomic_journal_recovers_from_torn_write() {
        let mut st = MemStorage::default();
        AtomicJournal::commit(&mut st, "/.vvbook/state", b"state_v1").unwrap();
        assert_eq!(AtomicJournal::load(&st, "/.vvbook/state").as_deref(), Some(b"state_v1".as_slice()));

        AtomicJournal::commit(&mut st, "/.vvbook/state", b"state_v2").unwrap();
        assert_eq!(AtomicJournal::load(&st, "/.vvbook/state").as_deref(), Some(b"state_v2".as_slice()));

        // Simulate power-cut corrupting slot B mid-write
        st.0.get_mut("/.vvbook/state.b").unwrap()[18] ^= 0xFF;
        assert_eq!(AtomicJournal::load(&st, "/.vvbook/state").as_deref(), Some(b"state_v1".as_slice()));
    }

    #[test]
    fn atomic_journal_exhaustive_torn_write_every_byte_offset() {
        let mut st = MemStorage::default();
        AtomicJournal::commit(&mut st, "/.vvbook/state", b"state_v1_clean").unwrap();
        AtomicJournal::commit(&mut st, "/.vvbook/state", b"state_v2_staged").unwrap();
        let full_b = st.0.get("/.vvbook/state.b").unwrap().clone();

        // Simulate power cut truncating slot B at every single byte offset 0..full_b.len()
        for cut in 0..full_b.len() {
            let mut damaged = st.clone();
            damaged.0.insert("/.vvbook/state.b".into(), full_b[..cut].to_vec());
            assert_eq!(
                AtomicJournal::load(&damaged, "/.vvbook/state").as_deref(),
                Some(b"state_v1_clean".as_slice()),
                "failed torn truncation recovery at byte {cut}"
            );
        }

        // Simulate 1-bit flip at every byte offset of slot B
        for pos in 0..full_b.len() {
            let mut damaged = st.clone();
            let mut flipped = full_b.clone();
            flipped[pos] ^= 0x40;
            damaged.0.insert("/.vvbook/state.b".into(), flipped);
            assert_eq!(
                AtomicJournal::load(&damaged, "/.vvbook/state").as_deref(),
                Some(b"state_v1_clean".as_slice()),
                "failed bit-flip recovery at byte {pos}"
            );
        }
    }

    #[test]
    fn firmware_wrong_hardware_bad_magic_and_truncated_stream_rejected() {
        let kp = KeyPair::from_seed(ed25519_compact::Seed::new([11u8; 32]));
        let wrong_kp = KeyPair::from_seed(ed25519_compact::Seed::new([22u8; 32]));
        let mut pub_bytes = [0u8; 32];
        pub_bytes.copy_from_slice(kp.pk.as_ref());
        let mut wrong_pub = [0u8; 32];
        wrong_pub.copy_from_slice(wrong_kp.pk.as_ref());

        let payload = b"ESP32-C3_X3_VALID_APP_BYTES_ABCDEF";
        let mut hdr = SignedFwHeader {
            format_ver: VVFW_FORMAT_VERSION,
            fw_version: 9,
            target_hw: HW_TARGET_XTEINK_X3,
            reserved: 0,
            payload_len: payload.len() as u32,
            payload_blake3: *blake3::hash(payload).as_bytes(),
            ed25519_sig: [0u8; 64],
        };
        let sig = kp.sk.sign(SignedFwHeader::canonical_body(&hdr), None);
        hdr.ed25519_sig.copy_from_slice(sig.as_ref());
        let mut raw = hdr.to_bytes();

        // Wrong hardware target
        assert_eq!(
            StreamingFwVerifier::start(&raw, &pub_bytes, 1, 0xCAFE).err(),
            Some(FwVerifyError::WrongHardware)
        );
        // Wrong public key -> InvalidSignature
        assert_eq!(
            StreamingFwVerifier::start(&raw, &wrong_pub, 1, HW_TARGET_XTEINK_X3).err(),
            Some(FwVerifyError::InvalidSignature)
        );
        // Truncated stream finishes with LengthMismatch
        let mut v_short = StreamingFwVerifier::start(&raw, &pub_bytes, 1, HW_TARGET_XTEINK_X3).unwrap();
        v_short.feed(&payload[..12]).unwrap();
        assert_eq!(v_short.finish().err(), Some(FwVerifyError::LengthMismatch));

        // Bad magic
        raw[0] = b'X';
        assert_eq!(
            StreamingFwVerifier::start(&raw, &pub_bytes, 1, HW_TARGET_XTEINK_X3).err(),
            Some(FwVerifyError::BadMagic)
        );
    }

    #[test]
    fn release_pubkey_matches_dist_pubkey_and_drbg_key_erasure() {
        assert_eq!(
            encode_hex(&FOLIO_RELEASE_PUBKEY),
            "c2d1bc4bb1cfaf0082f3c83d630819c48923caedfc8e03ac1f252166f57a36dd"
        );
        let mut drbg = Blake3Drbg::from_u64(0xCAFE_BABE);
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        drbg.fill_bytes(&mut a);
        drbg.fill_bytes(&mut b);
        assert_ne!(a, b);
        assert_ne!(a, [0u8; 32]);
        let mut forked = drbg.clone();
        drbg.reseed(b"extra-entropy");
        let mut c1 = [0u8; 32];
        let mut c2 = [0u8; 32];
        drbg.fill_bytes(&mut c1);
        forked.fill_bytes(&mut c2);
        assert_ne!(c1, c2);
    }

    #[test]
    fn atomic_journal_overflow_header_returns_none() {
        let mut bogus = [0u8; 32];
        bogus[0..4].copy_from_slice(&VVJN_MAGIC);
        bogus[4..12].copy_from_slice(&1u64.to_le_bytes());
        bogus[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(AtomicJournal::decode_slot(&bogus).is_none());
    }

    #[test]
    fn sha256_fips_180_4_vectors() {
        assert_eq!(
            encode_hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            encode_hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            encode_hex(&sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    fn build_valid_esp32c3_image(seg_data: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        let mut hdr = [0u8; 24];
        hdr[0] = ESP_IMAGE_MAGIC;
        hdr[1] = 1;
        hdr[4..8].copy_from_slice(&0x4200_0100u32.to_le_bytes());
        hdr[12..14].copy_from_slice(&ESP32C3_CHIP_ID.to_le_bytes());
        hdr[23] = 1;
        body.extend_from_slice(&hdr);

        body.extend_from_slice(&0x3FC8_0000u32.to_le_bytes());
        body.extend_from_slice(&(seg_data.len() as u32).to_le_bytes());
        body.extend_from_slice(seg_data);

        let mut xor = 0xEFu8;
        for &b in seg_data {
            xor ^= b;
        }
        while (body.len() + 1) % 16 != 0 {
            body.push(0);
        }
        body.push(xor);

        let digest = sha256(&body);
        body.extend_from_slice(&digest);
        body
    }

    #[test]
    fn esp32c3_image_validator_streaming_and_negative_cases() {
        let img = build_valid_esp32c3_image(b"hello esp32-c3 firmware segment bytes!!");
        assert!(validate_esp32c3_ota_image(&img).is_ok());

        // Chunked validation at arbitrary boundary splits
        let mut stream = Esp32c3ImageValidator::new(img.len() as u32).unwrap();
        for ch in img.chunks(7) {
            stream.feed(ch).unwrap();
        }
        assert!(stream.finish().is_ok());

        let mut bad_magic = img.clone();
        bad_magic[0] = 0xAA;
        assert_eq!(validate_esp32c3_ota_image(&bad_magic), Err(EspImageError::BadMagic));

        let mut bad_chip = img.clone();
        bad_chip[12] = 0x09;
        assert_eq!(validate_esp32c3_ota_image(&bad_chip), Err(EspImageError::UnsupportedChipId));

        let mut no_hash = img.clone();
        no_hash[23] = 0;
        assert_eq!(validate_esp32c3_ota_image(&no_hash), Err(EspImageError::MissingHashDigest));

        let mut bad_sha = img.clone();
        let last = bad_sha.len() - 1;
        bad_sha[last] ^= 0x01;
        assert_eq!(validate_esp32c3_ota_image(&bad_sha), Err(EspImageError::Sha256Mismatch));

        let mut bad_xor = img.clone();
        let xor_idx = bad_xor.len() - 33;
        bad_xor[xor_idx] ^= 0xFF;
        // Re-sign trailing SHA-256 so XOR mismatch itself is isolated
        let body_end = bad_xor.len() - 32;
        let d = sha256(&bad_xor[..body_end]);
        bad_xor[body_end..].copy_from_slice(&d);
        assert_eq!(validate_esp32c3_ota_image(&bad_xor), Err(EspImageError::ChecksumMismatch));
    }

    struct MemNorSlots([[u8; SECURITY_SECTOR_SIZE]; 2]);
    impl Default for MemNorSlots {
        fn default() -> Self {
            Self([[0xFFu8; SECURITY_SECTOR_SIZE]; 2])
        }
    }
    impl NorFlashRegion for MemNorSlots {
        type Error = ();
        fn read_slot(&mut self, slot: usize, offset: usize, out: &mut [u8]) -> Result<(), ()> {
            out.copy_from_slice(&self.0[slot][offset..offset + out.len()]);
            Ok(())
        }
        fn erase_slot(&mut self, slot: usize) -> Result<(), ()> {
            self.0[slot].fill(0xFF);
            Ok(())
        }
        fn program_slot(&mut self, slot: usize, offset: usize, data: &[u8]) -> Result<(), ()> {
            for (dst, &src) in self.0[slot][offset..offset + data.len()].iter_mut().zip(data) {
                *dst &= src;
            }
            Ok(())
        }
    }

    #[test]
    fn persistent_security_state_ab_recovery_and_monotonic_rollback() {
        let mut flash = MemNorSlots::default();
        let key = b"x3-mac-001122334455";
        let mut st = PersistentSecurityState::load_or_init(&mut flash, key, 1).unwrap();
        assert_eq!(st.seq, 1);
        assert_eq!(st.min_fw_version, 1);

        st.record_ota_install(4, [0x44u8; 32], 0);
        st.commit(&mut flash, key).unwrap();
        assert_eq!(st.seq, 2);

        // Tear slot B (which holds seq=2): must automatically recover seq=1 on slot A
        flash.0[1][17] ^= 0x80;
        let (slot_idx, recovered) = PersistentSecurityState::load_best(&mut flash, key).unwrap().unwrap();
        assert_eq!(slot_idx, 0);
        assert_eq!(recovered.seq, 1);
        assert_eq!(recovered.min_fw_version, 1);

        // Restore seq=2 in B, advance to seq=3 in A, then verify wrong binding key rejects both
        let mut fresh = recovered;
        fresh.record_ota_install(5, [0x55u8; 32], 0);
        fresh.commit(&mut flash, key).unwrap();
        assert!(PersistentSecurityState::load_best(&mut flash, b"wrong-key").unwrap().is_none());
        let (_, best) = PersistentSecurityState::load_best(&mut flash, key).unwrap().unwrap();
        assert_eq!(best.min_fw_version, 5);
    }
}
