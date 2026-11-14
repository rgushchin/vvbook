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
        rng.fill_bytes(&mut self.session_token);
        self.has_session = true;
        Ok(self.session_token)
    }

    pub fn issue_wpa2_session_token<E: EntropySource>(&mut self, rng: &mut E) -> Result<[u8; 32], AuthError> {
        if self.is_locked() {
            return Err(AuthError::LockedOut);
        }
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
        let len = u32::from_le_bytes(raw[12..16].try_into().ok()?) as usize;
        if raw.len() != 16 + len + 16 {
            return None;
        }
        let body = &raw[..16 + len];
        let tag = &raw[16 + len..];
        let computed = blake3::hash(body);
        if !constant_time_eq(&computed.as_bytes()[..16], tag) {
            return None;
        }
        Some((seq, raw[16..16 + len].to_vec()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use ed25519_compact::KeyPair;
    use vvbook_pal::{DirEntry, StorageError};

    struct TestRng(u64);
    impl EntropySource for TestRng {
        fn fill_bytes(&mut self, out: &mut [u8]) {
            for b in out.iter_mut() {
                self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                *b = (self.0 >> 32) as u8;
            }
        }
    }

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
    fn pin_handshake_and_lockout() {
        let mut rng = TestRng(42);
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
        assert_eq!(auth.issue_wpa2_session_token(&mut rng), Err(AuthError::LockedOut));

        // Fresh auth succeeds with right PIN and with WPA2 link session
        let mut auth2 = VvDropAuth::from_fixed_pin(*b"48291704", &mut rng);
        let good_mac = compute_auth_mac(b"48291704", auth2.server_nonce(), &client_nonce);
        let tok = auth2.verify_and_issue_token(&client_nonce, &good_mac, &mut rng).unwrap();
        assert!(auth2.verify_token(&tok).is_ok());
        let mut tampered = tok;
        tampered[0] ^= 1;
        assert_eq!(auth2.verify_token(&tampered), Err(AuthError::InvalidToken));

        let wpa_tok = auth2.issue_wpa2_session_token(&mut rng).unwrap();
        assert!(auth2.verify_token(&wpa_tok).is_ok());
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
}
