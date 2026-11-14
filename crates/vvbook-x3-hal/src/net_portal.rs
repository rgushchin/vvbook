extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use smoltcp::iface::{
    Config as NetConfig, Interface, PollResult, SocketHandle, SocketSet, SocketStorage,
};
use smoltcp::phy::Device;
use smoltcp::socket::{tcp, udp};
use smoltcp::time::{Duration as NetDuration, Instant};
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address, Ipv4Cidr};
use vvbook_pal::{DateTime, EntropySource, StorageBackend};
use vvbook_security::{
    HW_TARGET_XTEINK_X3, SignedFwHeader, StreamingFwVerifier, VvDropAuth, compute_auth_mac,
    decode_hex_32, encode_hex,
};

pub const AP_IPV4: [u8; 4] = [192, 168, 4, 1];
pub const AP_NETMASK: [u8; 4] = [255, 255, 255, 0];
pub const AP_BROADCAST: [u8; 4] = [192, 168, 4, 255];
pub const AP_SSID: &str = "VVBook-Folio";
pub const AP_URL: &str = "http://192.168.4.1";
pub const HTTP_PORT: u16 = 80;
pub const HTTP_SLOT_COUNT: usize = 6;
pub const VVDROP_WEB_HTML: &str = include_str!("../../vvbook-emu/assets/vvdrop_web.html");

const DHCP_MAGIC_COOKIE: [u8; 4] = [99, 130, 83, 99];
const LEASE_SECONDS: u32 = 3600;
const MAX_HTTP_HEADER_BYTES: usize = 4096;
const MAX_UPLOAD_BYTES: u64 = 512 * 1024 * 1024;
const MAX_OTA_BYTES: u64 = 7 * 1024 * 1024;
const OTA_STAGE_PATH: &str = "/.vvbook/ota_staged.bin";

#[derive(Clone, Copy)]
struct LeaseSlot {
    mac: [u8; 6],
    last_octet: u8,
    active: bool,
}

/// Minimal `#![no_std]` DHCPv4 server for the ESP32-C3 SoftAP subnet `192.168.4.0/24`.
pub struct DhcpServer {
    leases: [LeaseSlot; 16],
}

impl Default for DhcpServer {
    fn default() -> Self {
        Self::new()
    }
}

impl DhcpServer {
    pub const fn new() -> Self {
        Self {
            leases: [LeaseSlot {
                mac: [0; 6],
                last_octet: 0,
                active: false,
            }; 16],
        }
    }

    pub fn reset(&mut self) {
        self.leases.fill(LeaseSlot {
            mac: [0; 6],
            last_octet: 0,
            active: false,
        });
    }

    pub fn active_lease_count(&self) -> u8 {
        self.leases.iter().filter(|s| s.active).count() as u8
    }

    fn assign_ip_for(&mut self, mac: [u8; 6], requested: Option<[u8; 4]>) -> [u8; 4] {
        if let Some(slot) = self.leases.iter().find(|s| s.active && s.mac == mac) {
            return [AP_IPV4[0], AP_IPV4[1], AP_IPV4[2], slot.last_octet];
        }
        if let Some(req) = requested
            && req[0] == AP_IPV4[0]
            && req[1] == AP_IPV4[1]
            && req[2] == AP_IPV4[2]
            && (2..=19).contains(&req[3])
            && !self.leases.iter().any(|s| s.active && s.last_octet == req[3])
        {
            self.store_lease(mac, req[3]);
            return req;
        }
        for oct in 2u8..=19 {
            if !self.leases.iter().any(|s| s.active && s.last_octet == oct) {
                self.store_lease(mac, oct);
                return [AP_IPV4[0], AP_IPV4[1], AP_IPV4[2], oct];
            }
        }
        // Evict slot 0 if table saturated
        let oct = self.leases[0].last_octet.max(2);
        self.leases[0] = LeaseSlot {
            mac,
            last_octet: oct,
            active: true,
        };
        [AP_IPV4[0], AP_IPV4[1], AP_IPV4[2], oct]
    }

    fn store_lease(&mut self, mac: [u8; 6], oct: u8) {
        if let Some(empty) = self.leases.iter_mut().find(|s| !s.active) {
            *empty = LeaseSlot {
                mac,
                last_octet: oct,
                active: true,
            };
        } else {
            self.leases[0] = LeaseSlot {
                mac,
                last_octet: oct,
                active: true,
            };
        }
    }

    fn release_mac(&mut self, mac: [u8; 6]) {
        for slot in &mut self.leases {
            if slot.active && slot.mac == mac {
                slot.active = false;
            }
        }
    }

    /// Process a raw DHCPv4 BOOTP UDP payload (`port 67`) and write a reply into `out`.
    pub fn handle_packet(&mut self, req: &[u8], out: &mut [u8]) -> Option<usize> {
        if req.len() < 240 || out.len() < 320 {
            return None;
        }
        if req[0] != 1 || req[1] != 1 || req[2] != 6 {
            return None;
        }
        if req[236..240] != DHCP_MAGIC_COOKIE {
            return None;
        }

        let mut msg_type = 0u8;
        let mut req_ip: Option<[u8; 4]> = None;
        let mut server_id: Option<[u8; 4]> = None;
        let mut i = 240usize;
        while i < req.len() {
            let code = req[i];
            i += 1;
            if code == 255 {
                break;
            }
            if code == 0 {
                continue;
            }
            if i >= req.len() {
                break;
            }
            let len = usize::from(req[i]);
            i += 1;
            if i + len > req.len() {
                break;
            }
            match (code, len) {
                (53, 1) => msg_type = req[i],
                (50, 4) => req_ip = Some([req[i], req[i + 1], req[i + 2], req[i + 3]]),
                (54, 4) => server_id = Some([req[i], req[i + 1], req[i + 2], req[i + 3]]),
                _ => {}
            }
            i += len;
        }

        let mut mac = [0u8; 6];
        mac.copy_from_slice(&req[28..34]);
        let ciaddr = [req[12], req[13], req[14], req[15]];

        let (reply_type, requested_for_lease) = match msg_type {
            1 => (2u8, req_ip), // DISCOVER -> OFFER
            3 => {
                if let Some(sid) = server_id
                    && sid != AP_IPV4
                {
                    return None;
                }
                let target_ip = req_ip.or((ciaddr != [0, 0, 0, 0]).then_some(ciaddr));
                if let Some(ip) = target_ip
                    && (ip[0..3] != AP_IPV4[0..3] || !(2..=19).contains(&ip[3]))
                {
                    // Foreign subnet in INIT-REBOOT/RENEW -> immediate DHCPNAK so client re-discovers
                    out.fill(0);
                    out[0] = 2; // BOOTREPLY
                    out[1] = 1;
                    out[2] = 6;
                    out[3] = 0;
                    out[4..8].copy_from_slice(&req[4..8]); // xid
                    out[10..12].copy_from_slice(&[0x80, 0x00]); // broadcast bit for NAK per RFC 2131
                    out[20..24].copy_from_slice(&AP_IPV4); // siaddr
                    out[28..44].copy_from_slice(&req[28..44]); // chaddr
                    out[236..240].copy_from_slice(&DHCP_MAGIC_COOKIE);

                    let mut p = 240usize;
                    out[p..p + 3].copy_from_slice(&[53, 1, 6]); // DHCPNAK
                    p += 3;
                    out[p] = 54;
                    out[p + 1] = 4;
                    out[p + 2..p + 6].copy_from_slice(&AP_IPV4);
                    p += 6;
                    out[p] = 255;
                    p += 1;
                    return Some(p.max(300));
                }
                (5u8, target_ip) // REQUEST -> ACK
            }
            7 => {
                self.release_mac(mac);
                return None;
            }
            8 => (5u8, None), // INFORM -> ACK
            _ => return None,
        };

        let yiaddr = if msg_type == 8 {
            [0; 4]
        } else {
            self.assign_ip_for(mac, requested_for_lease)
        };

        out.fill(0);
        out[0] = 2; // BOOTREPLY
        out[1] = 1;
        out[2] = 6;
        out[3] = 0;
        out[4..8].copy_from_slice(&req[4..8]); // xid
        out[10..12].copy_from_slice(&req[10..12]); // flags (broadcast bit preserved)
        out[12..16].copy_from_slice(&req[12..16]); // ciaddr
        out[16..20].copy_from_slice(&yiaddr);
        out[20..24].copy_from_slice(&AP_IPV4); // siaddr
        out[28..44].copy_from_slice(&req[28..44]); // chaddr
        out[236..240].copy_from_slice(&DHCP_MAGIC_COOKIE);

        let mut p = 240usize;
        // Option 53: Message Type
        out[p..p + 3].copy_from_slice(&[53, 1, reply_type]);
        p += 3;
        // Option 54: Server Identifier
        out[p] = 54;
        out[p + 1] = 4;
        out[p + 2..p + 6].copy_from_slice(&AP_IPV4);
        p += 6;
        // RFC 2131 §4.3.5: DHCPINFORM (msg_type == 8) must omit Lease/T1/T2 options.
        if msg_type != 8 {
            // Option 51: Lease Time
            out[p] = 51;
            out[p + 1] = 4;
            out[p + 2..p + 6].copy_from_slice(&LEASE_SECONDS.to_be_bytes());
            p += 6;
            // Option 58: Renewal (T1 = 1800s)
            out[p] = 58;
            out[p + 1] = 4;
            out[p + 2..p + 6].copy_from_slice(&(LEASE_SECONDS / 2).to_be_bytes());
            p += 6;
            // Option 59: Rebinding (T2 = 3150s)
            out[p] = 59;
            out[p + 1] = 4;
            out[p + 2..p + 6].copy_from_slice(&((LEASE_SECONDS * 7) / 8).to_be_bytes());
            p += 6;
        }
        // Option 1: Subnet Mask
        out[p] = 1;
        out[p + 1] = 4;
        out[p + 2..p + 6].copy_from_slice(&AP_NETMASK);
        p += 6;
        // Option 3: Default Router (192.168.4.1 so Android netd / iOS complete IPv4 provisioning)
        out[p] = 3;
        out[p + 1] = 4;
        out[p + 2..p + 6].copy_from_slice(&AP_IPV4);
        p += 6;
        // Option 6: DNS Server (192.168.4.1 -> instant NXDOMAIN for probes + vvbook.local A record)
        out[p] = 6;
        out[p + 1] = 4;
        out[p + 2..p + 6].copy_from_slice(&AP_IPV4);
        p += 6;
        // Option 15: Domain Name ("local")
        out[p] = 15;
        out[p + 1] = 5;
        out[p + 2..p + 7].copy_from_slice(b"local");
        p += 7;
        // Option 28: Broadcast Address
        out[p] = 28;
        out[p + 1] = 4;
        out[p + 2..p + 6].copy_from_slice(&AP_BROADCAST);
        p += 6;
        // End option
        out[p] = 255;
        p += 1;

        Some(p.max(300))
    }
}

/// Minimal `#![no_std]` DNS responder for `vvbook.local` -> `192.168.4.1`,
/// returning immediate `NXDOMAIN` for external captive-portal checks so phones stay responsive.
pub struct DnsResponder;

impl DnsResponder {
    pub fn handle_query(req: &[u8], out: &mut [u8]) -> Option<usize> {
        if req.len() < 17 || out.len() < req.len() + 16 {
            return None;
        }
        // Parse QNAME labels starting at offset 12
        let mut qname = String::new();
        let mut pos = 12usize;
        loop {
            let len = usize::from(*req.get(pos)?);
            pos += 1;
            if len == 0 {
                break;
            }
            if len >= 64 || pos + len > req.len() || qname.len() + len + 1 > 253 {
                return None;
            }
            if !qname.is_empty() {
                qname.push('.');
            }
            let lbl = core::str::from_utf8(&req[pos..pos + len]).ok()?;
            qname.push_str(lbl);
            pos += len;
        }
        if pos + 4 > req.len() {
            return None;
        }
        let qtype = u16::from_be_bytes([req[pos], req[pos + 1]]);
        let qclass = u16::from_be_bytes([req[pos + 2], req[pos + 3]]);
        let q_end = pos + 4;

        let lower = qname.to_ascii_lowercase();
        let is_local_name = matches!(
            lower.as_str(),
            "vvbook.local" | "vvbook" | "vvbook.lan" | "folio.local" | "vvdrop.local"
        );

        out[..q_end].copy_from_slice(&req[..q_end]);
        let rd = req[2] & 0x01;
        out[2] = 0x84 | rd; // QR=1, AA=1
        out[4..6].copy_from_slice(&1u16.to_be_bytes()); // QDCOUNT=1
        out[8..12].copy_from_slice(&[0; 4]); // NSCOUNT=0, ARCOUNT=0

        if is_local_name && qclass == 1 && (qtype == 1 || qtype == 255) {
            out[3] = 0x00; // RCODE=0 NOERROR
            out[6..8].copy_from_slice(&1u16.to_be_bytes()); // ANCOUNT=1
            // Pointer to QNAME at 0xC00C, TYPE=A (1), CLASS=IN (1), TTL=60, RDLEN=4, 192.168.4.1
            let ans = [
                0xC0, 0x0C, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x3C, 0x00, 0x04,
                AP_IPV4[0], AP_IPV4[1], AP_IPV4[2], AP_IPV4[3],
            ];
            out[q_end..q_end + ans.len()].copy_from_slice(&ans);
            Some(q_end + ans.len())
        } else if is_local_name && qclass == 1 && qtype == 28 {
            // AAAA NODATA so dual-stack clients immediately pick IPv4 A record
            out[3] = 0x00;
            out[6..8].copy_from_slice(&0u16.to_be_bytes());
            Some(q_end)
        } else {
            out[3] = 0x03; // NXDOMAIN
            out[6..8].copy_from_slice(&0u16.to_be_bytes());
            Some(q_end)
        }
    }
}

pub fn percent_decode_str(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(raw_hex) = core::str::from_utf8(&b[i + 1..i + 3])
            && let Ok(val) = u8::from_str_radix(raw_hex, 16)
        {
            out.push(val);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Sanitize a user-supplied upload filename into a safe leaf suitable for FAT32 `/books/<name>`.
pub fn sanitize_book_filename(raw: &str) -> String {
    let decoded = percent_decode_str(raw);
    let leaf = decoded.rsplit(['/', '\\']).next().unwrap_or("book.txt").trim();
    let mut cleaned = String::new();
    for ch in leaf.chars() {
        if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            cleaned.push('_');
        } else {
            cleaned.push(ch);
        }
        if cleaned.len() >= 96 {
            break;
        }
    }
    let trimmed = cleaned.trim_matches(['.', ' ']);
    let mut out = if trimmed.is_empty() {
        String::from("upload.txt")
    } else {
        String::from(trimmed)
    };
    if !out.contains('.') {
        out.push_str(".txt");
    }
    out
}

/// Parse `YYYY-MM-DD HH:MM:SS` or `YYYY-MM-DDTHH:MM:SS` from `X-VVDrop-Datetime`.
pub fn parse_datetime_header(s: &str) -> Option<DateTime> {
    let t = s.trim();
    if t.len() < 19 {
        return None;
    }
    let year: u16 = t.get(0..4)?.parse().ok()?;
    let month: u8 = t.get(5..7)?.parse().ok()?;
    let day: u8 = t.get(8..10)?.parse().ok()?;
    let sep = t.as_bytes()[10];
    if sep != b' ' && sep != b'T' {
        return None;
    }
    let hour: u8 = t.get(11..13)?.parse().ok()?;
    let minute: u8 = t.get(14..16)?.parse().ok()?;
    let second: u8 = t.get(17..19)?.parse().ok()?;
    if !(2024..=2099).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    Some(DateTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
    })
}

struct XorShift64(u64);

impl EntropySource for XorShift64 {
    fn fill_bytes(&mut self, out: &mut [u8]) {
        for b in out.iter_mut() {
            let mut x = self.0.max(1);
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            *b = (x >> 24) as u8;
        }
    }
}

/// Shared VVDrop portal session, access key auth, OTA verification key, and upload status.
pub struct VvDropServerState {
    pub active: bool,
    pub pin: [u8; 8],
    pub wpa2_authenticated_link: bool,
    pub auth: VvDropAuth,
    pub rng_state: u64,
    pub ota_pub_key: [u8; 32],
    pub min_fw_version: u32,
    pub clients_connected: u8,
    pub rx_bytes: u64,
    pub total_bytes: u64,
    pub last_received_file: Option<String>,
    pub busy_message: Option<String>,
    pub completed_uploads: Vec<String>,
    pub synced_datetime: Option<DateTime>,
    pub status_dirty: bool,
    pub active_upload_path: Option<String>,
}

impl VvDropServerState {
    pub fn new(ota_pub_key: [u8; 32], min_fw_version: u32, seed: u64) -> Self {
        let mut rng = XorShift64(seed ^ 0x9E37_79B9_7F4A_7C15);
        let auth = VvDropAuth::from_fixed_pin(*b"00000000", &mut rng);
        Self {
            active: false,
            pin: *b"00000000",
            wpa2_authenticated_link: false,
            auth,
            rng_state: rng.0,
            ota_pub_key,
            min_fw_version,
            clients_connected: 0,
            rx_bytes: 0,
            total_bytes: 0,
            last_received_file: None,
            busy_message: None,
            completed_uploads: Vec::new(),
            synced_datetime: None,
            status_dirty: false,
            active_upload_path: None,
        }
    }

    pub fn start(&mut self, pin: &str, seed_mix: u64) {
        let mut pb = [b'0'; 8];
        for (i, b) in pin
            .bytes()
            .filter(|b| !b.is_ascii_whitespace() && *b != b'-')
            .take(8)
            .enumerate()
        {
            pb[i] = b;
        }
        self.pin = pb;
        self.rng_state = self.rng_state.wrapping_add(seed_mix ^ 0xA5A5_5A5A);
        let mut rng = XorShift64(self.rng_state);
        self.auth = VvDropAuth::from_fixed_pin(pb, &mut rng);
        self.rng_state = rng.0;
        self.active = true;
        self.busy_message = None;
        self.status_dirty = true;
    }

    pub fn stop(&mut self) {
        self.active = false;
        self.clients_connected = 0;
        self.rx_bytes = 0;
        self.total_bytes = 0;
        self.busy_message = None;
        self.active_upload_path = None;
    }
}

enum ConnStage {
    ReadingHeaders,
    StreamingUpload {
        dest_path: String,
        filename: String,
        remaining: u64,
        total: u64,
        received: u64,
        last_pct_bucket: u8,
    },
    StreamingOtaHeader {
        remaining_total: u64,
        hdr_buf: [u8; SignedFwHeader::SIZE],
        hdr_len: usize,
    },
    StreamingOtaBody {
        verifier: Box<StreamingFwVerifier>,
        remaining: u64,
    },
    DrainingBadBody {
        remaining: u64,
        status: u16,
        reason: &'static str,
        msg: Vec<u8>,
    },
    WritingResponse,
    Closed,
}

/// Zero-payload-buffering HTTP/1.1 connection state machine for `#![no_std]` sockets.
pub struct StreamingHttpConn {
    redirect_only: bool,
    hdr_buf: Vec<u8>,
    stage: ConnStage,
    tx_head: Vec<u8>,
    tx_head_pos: usize,
    tx_static_body: &'static [u8],
    tx_dyn_body: Vec<u8>,
    tx_body_pos: usize,
}

impl StreamingHttpConn {
    pub fn new(redirect_only: bool) -> Self {
        Self {
            redirect_only,
            hdr_buf: Vec::new(),
            stage: ConnStage::ReadingHeaders,
            tx_head: Vec::new(),
            tx_head_pos: 0,
            tx_static_body: &[],
            tx_dyn_body: Vec::new(),
            tx_body_pos: 0,
        }
    }

    pub fn reset(&mut self) {
        self.hdr_buf.clear();
        self.stage = ConnStage::ReadingHeaders;
        self.tx_head.clear();
        self.tx_head_pos = 0;
        self.tx_static_body = &[];
        self.tx_dyn_body.clear();
        self.tx_body_pos = 0;
    }

    pub fn abort(&mut self, state: &mut VvDropServerState, storage: &mut dyn StorageBackend) {
        match &self.stage {
            ConnStage::StreamingUpload { dest_path, .. } => {
                let _ = storage.remove(dest_path);
                if state.active_upload_path.as_deref() == Some(dest_path.as_str()) {
                    state.active_upload_path = None;
                }
                state.rx_bytes = 0;
                state.total_bytes = 0;
                state.busy_message = None;
                state.status_dirty = true;
            }
            ConnStage::StreamingOtaHeader { .. } | ConnStage::StreamingOtaBody { .. } => {
                let _ = storage.remove(OTA_STAGE_PATH);
                state.busy_message = None;
                state.status_dirty = true;
            }
            _ => {}
        }
        self.reset();
    }

    pub fn wants_rx(&self) -> bool {
        !matches!(self.stage, ConnStage::WritingResponse | ConnStage::Closed)
    }

    pub fn is_idle_header_wait(&self) -> bool {
        matches!(self.stage, ConnStage::ReadingHeaders) && self.hdr_buf.is_empty()
    }

    pub fn has_tx(&self) -> bool {
        self.tx_head_pos < self.tx_head.len()
            || self.tx_body_pos < self.tx_static_body.len()
            || self.tx_body_pos < self.tx_dyn_body.len()
    }

    pub fn should_close(&self) -> bool {
        matches!(self.stage, ConnStage::Closed) && !self.has_tx()
    }

    fn queue_response_bytes(
        &mut self,
        code: u16,
        reason: &str,
        ctype: &str,
        static_body: Option<&'static [u8]>,
        dyn_body: &[u8],
    ) {
        let len = static_body.map_or(dyn_body.len(), <[u8]>::len);
        let head = format!(
            "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {len}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: *\r\nConnection: close\r\n\r\n"
        );
        self.tx_head = head.into_bytes();
        self.tx_head_pos = 0;
        self.tx_static_body = static_body.unwrap_or(&[]);
        self.tx_dyn_body = if static_body.is_some() {
            Vec::new()
        } else {
            dyn_body.to_vec()
        };
        self.tx_body_pos = 0;
        self.stage = ConnStage::WritingResponse;
    }

    fn queue_head_response(&mut self, code: u16, reason: &str, ctype: &str, content_len: usize) {
        let head = format!(
            "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {content_len}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: *\r\nConnection: close\r\n\r\n"
        );
        self.tx_head = head.into_bytes();
        self.tx_head_pos = 0;
        self.tx_static_body = &[];
        self.tx_dyn_body.clear();
        self.tx_body_pos = 0;
        self.stage = ConnStage::WritingResponse;
    }

    fn queue_redirect_root(&mut self) {
        let head = b"HTTP/1.1 302 Found\r\nLocation: http://192.168.4.1/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        self.tx_head = head.to_vec();
        self.tx_head_pos = 0;
        self.tx_static_body = &[];
        self.tx_dyn_body.clear();
        self.tx_body_pos = 0;
        self.stage = ConnStage::WritingResponse;
    }

    fn queue_options(&mut self) {
        let head = b"HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, HEAD, POST, OPTIONS\r\nAccess-Control-Allow-Headers: *\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        self.tx_head = head.to_vec();
        self.tx_head_pos = 0;
        self.tx_static_body = &[];
        self.tx_dyn_body.clear();
        self.tx_body_pos = 0;
        self.stage = ConnStage::WritingResponse;
    }

    pub fn peek_tx(&self, out: &mut [u8]) -> usize {
        if out.is_empty() {
            return 0;
        }
        let mut written = 0usize;
        if self.tx_head_pos < self.tx_head.len() {
            let rem = &self.tx_head[self.tx_head_pos..];
            let take = rem.len().min(out.len());
            out[..take].copy_from_slice(&rem[..take]);
            written += take;
        }
        if written < out.len() {
            let body: &[u8] = if !self.tx_static_body.is_empty() {
                self.tx_static_body
            } else {
                &self.tx_dyn_body
            };
            if self.tx_body_pos < body.len() {
                let rem = &body[self.tx_body_pos..];
                let take = rem.len().min(out.len() - written);
                out[written..written + take].copy_from_slice(&rem[..take]);
                written += take;
            }
        }
        written
    }

    pub fn advance_tx(&mut self, mut n: usize) {
        if self.tx_head_pos < self.tx_head.len() {
            let avail = self.tx_head.len() - self.tx_head_pos;
            let used = n.min(avail);
            self.tx_head_pos += used;
            n -= used;
        }
        if n > 0 {
            let body_len = if !self.tx_static_body.is_empty() {
                self.tx_static_body.len()
            } else {
                self.tx_dyn_body.len()
            };
            if self.tx_body_pos < body_len {
                let avail = body_len - self.tx_body_pos;
                self.tx_body_pos += n.min(avail);
            }
        }
        if matches!(self.stage, ConnStage::WritingResponse) && !self.has_tx() {
            self.stage = ConnStage::Closed;
        }
    }

    pub fn on_rx(
        &mut self,
        data: &[u8],
        state: &mut VvDropServerState,
        storage: &mut dyn StorageBackend,
    ) -> usize {
        if data.is_empty() {
            return 0;
        }
        let mut consumed = 0usize;
        while consumed < data.len() && self.wants_rx() {
            let chunk = &data[consumed..];
            match &mut self.stage {
                ConnStage::ReadingHeaders => {
                    let take = chunk.len().min(MAX_HTTP_HEADER_BYTES.saturating_sub(self.hdr_buf.len()));
                    self.hdr_buf.extend_from_slice(&chunk[..take]);
                    consumed += take;

                    if let Some(pos) = self.hdr_buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let extra = self.hdr_buf[pos + 4..].len();
                        consumed = consumed.saturating_sub(extra);
                        let raw_hdr = core::mem::take(&mut self.hdr_buf);
                        self.handle_parsed_header(&raw_hdr[..pos], state, storage);
                    } else if self.hdr_buf.len() >= MAX_HTTP_HEADER_BYTES {
                        self.hdr_buf.clear();
                        self.queue_response_bytes(431, "Header Too Large", "text/plain", None, b"header too large");
                    }
                }
                ConnStage::StreamingUpload {
                    dest_path,
                    filename,
                    remaining,
                    total,
                    received,
                    last_pct_bucket,
                } => {
                    let take = chunk.len().min(*remaining as usize);
                    if take > 0 {
                        if storage.append(dest_path, &chunk[..take]).is_err() {
                            let dp = dest_path.clone();
                            let _ = storage.remove(&dp);
                            if state.active_upload_path.as_deref() == Some(dp.as_str()) {
                                state.active_upload_path = None;
                            }
                            state.rx_bytes = 0;
                            state.total_bytes = 0;
                            state.busy_message = None;
                            state.status_dirty = true;
                            let rem_after = *remaining - take as u64;
                            consumed += take;
                            if rem_after > 0 {
                                self.stage = ConnStage::DrainingBadBody {
                                    remaining: rem_after,
                                    status: 507,
                                    reason: "Insufficient Storage",
                                    msg: b"microSD write failed".to_vec(),
                                };
                            } else {
                                self.queue_response_bytes(
                                    507,
                                    "Insufficient Storage",
                                    "text/plain",
                                    None,
                                    b"microSD write failed",
                                );
                            }
                            continue;
                        }
                        consumed += take;
                        *remaining -= take as u64;
                        *received += take as u64;
                        state.rx_bytes = *received;
                        state.total_bytes = *total;

                        let pct = (*received * 100)
                            .checked_div(*total)
                            .map_or(100, |v| v.min(100)) as u8;
                        let bucket = pct / 25;
                        if bucket > *last_pct_bucket && *remaining > 0 {
                            *last_pct_bucket = bucket;
                            state.busy_message = Some(format!("{filename} ({pct}%)"));
                            state.status_dirty = true;
                        }
                    }
                    if *remaining == 0 {
                        let done_path = dest_path.clone();
                        let done_name = filename.clone();
                        state.active_upload_path = None;
                        state.rx_bytes = 0;
                        state.total_bytes = 0;
                        state.busy_message = None;
                        state.last_received_file = Some(done_name);
                        state.completed_uploads.push(done_path);
                        state.status_dirty = true;
                        self.queue_response_bytes(200, "OK", "text/plain", None, b"stored to microSD");
                    }
                }
                ConnStage::StreamingOtaHeader {
                    remaining_total,
                    hdr_buf,
                    hdr_len,
                } => {
                    let need = SignedFwHeader::SIZE - *hdr_len;
                    let take = chunk.len().min(need).min(*remaining_total as usize);
                    hdr_buf[*hdr_len..*hdr_len + take].copy_from_slice(&chunk[..take]);
                    *hdr_len += take;
                    *remaining_total -= take as u64;
                    consumed += take;

                    if *hdr_len == SignedFwHeader::SIZE {
                        let rem = *remaining_total;
                        match StreamingFwVerifier::start(
                            hdr_buf,
                            &state.ota_pub_key,
                            state.min_fw_version,
                            HW_TARGET_XTEINK_X3,
                        ) {
                            Ok(verifier) => {
                                let _ = storage.mkdir_all("/.vvbook");
                                let _ = storage.remove(OTA_STAGE_PATH);
                                if storage.write_atomic(OTA_STAGE_PATH, &[]).is_err() {
                                    if rem > 0 {
                                        self.stage = ConnStage::DrainingBadBody {
                                            remaining: rem,
                                            status: 507,
                                            reason: "Insufficient Storage",
                                            msg: b"OTA stage failed".to_vec(),
                                        };
                                    } else {
                                        self.queue_response_bytes(
                                            507,
                                            "Insufficient Storage",
                                            "text/plain",
                                            None,
                                            b"OTA stage failed",
                                        );
                                    }
                                    continue;
                                }
                                if rem == 0 {
                                    match verifier.finish() {
                                        Ok(hdr) => {
                                            state.min_fw_version = hdr.fw_version;
                                            state.busy_message = None;
                                            state.status_dirty = true;
                                            self.queue_response_bytes(
                                                200,
                                                "OK",
                                                "text/plain",
                                                None,
                                                b"OTA verified and staged",
                                            );
                                        }
                                        Err(e) => {
                                            let _ = storage.remove(OTA_STAGE_PATH);
                                            let msg = format!("OTA verify failed: {e:?}");
                                            self.queue_response_bytes(
                                                403,
                                                "Forbidden",
                                                "text/plain",
                                                None,
                                                msg.as_bytes(),
                                            );
                                        }
                                    }
                                } else {
                                    self.stage = ConnStage::StreamingOtaBody {
                                        verifier: Box::new(verifier),
                                        remaining: rem,
                                    };
                                }
                            }
                            Err(e) => {
                                let msg = format!("OTA header rejected: {e:?}");
                                if rem > 0 {
                                    self.stage = ConnStage::DrainingBadBody {
                                        remaining: rem,
                                        status: 403,
                                        reason: "Forbidden",
                                        msg: msg.into_bytes(),
                                    };
                                } else {
                                    self.queue_response_bytes(
                                        403,
                                        "Forbidden",
                                        "text/plain",
                                        None,
                                        msg.as_bytes(),
                                    );
                                }
                            }
                        }
                    }
                }
                ConnStage::StreamingOtaBody { .. } => {
                    let prev = core::mem::replace(&mut self.stage, ConnStage::Closed);
                    if let ConnStage::StreamingOtaBody {
                        mut verifier,
                        mut remaining,
                    } = prev
                    {
                        let take = chunk.len().min(remaining as usize);
                        consumed += take;
                        remaining -= take as u64;
                        let feed_res = verifier.feed(&chunk[..take]);
                        let sd_res = storage.append(OTA_STAGE_PATH, &chunk[..take]);
                        if feed_res.is_err() || sd_res.is_err() {
                            let _ = storage.remove(OTA_STAGE_PATH);
                            if remaining > 0 {
                                self.stage = ConnStage::DrainingBadBody {
                                    remaining,
                                    status: 400,
                                    reason: "Bad Request",
                                    msg: b"OTA stream error".to_vec(),
                                };
                            } else {
                                self.queue_response_bytes(
                                    400,
                                    "Bad Request",
                                    "text/plain",
                                    None,
                                    b"OTA stream error",
                                );
                            }
                            continue;
                        }
                        if remaining == 0 {
                            match (*verifier).finish() {
                                Ok(hdr) => {
                                    state.min_fw_version = hdr.fw_version;
                                    state.busy_message = None;
                                    state.last_received_file = Some(String::from("ota_staged.bin"));
                                    state.status_dirty = true;
                                    self.queue_response_bytes(
                                        200,
                                        "OK",
                                        "text/plain",
                                        None,
                                        b"OTA verified and staged",
                                    );
                                }
                                Err(e) => {
                                    let _ = storage.remove(OTA_STAGE_PATH);
                                    let msg = format!("OTA verify failed: {e:?}");
                                    self.queue_response_bytes(
                                        403,
                                        "Forbidden",
                                        "text/plain",
                                        None,
                                        msg.as_bytes(),
                                    );
                                }
                            }
                        } else {
                            self.stage = ConnStage::StreamingOtaBody {
                                verifier,
                                remaining,
                            };
                        }
                    }
                }
                ConnStage::DrainingBadBody {
                    remaining,
                    status,
                    reason,
                    msg,
                } => {
                    let take = chunk.len().min(*remaining as usize);
                    consumed += take;
                    *remaining -= take as u64;
                    if *remaining == 0 {
                        let code = *status;
                        let r = *reason;
                        let body = core::mem::take(msg);
                        self.queue_response_bytes(code, r, "text/plain", None, &body);
                    }
                }
                ConnStage::WritingResponse | ConnStage::Closed => break,
            }
        }
        consumed
    }

    fn handle_parsed_header(
        &mut self,
        raw_hdr: &[u8],
        state: &mut VvDropServerState,
        storage: &mut dyn StorageBackend,
    ) {
        if self.redirect_only {
            self.queue_redirect_root();
            return;
        }
        let hdr_text = String::from_utf8_lossy(raw_hdr);
        let mut lines = hdr_text.lines();
        let Some(req_line) = lines.next() else {
            self.queue_response_bytes(400, "Bad Request", "text/plain", None, b"bad request");
            return;
        };
        let mut parts = req_line.split_whitespace();
        let Some(method) = parts.next() else {
            self.queue_response_bytes(400, "Bad Request", "text/plain", None, b"bad method");
            return;
        };
        let raw_path = parts.next().unwrap_or("/");
        let path = raw_path.split('?').next().unwrap_or(raw_path);

        let mut headers: Vec<(String, String)> = Vec::new();
        let mut content_len: Option<u64> = None;
        let mut expect_continue = false;
        for ln in lines {
            if let Some((k, v)) = ln.split_once(':') {
                let kl = k.trim().to_ascii_lowercase();
                let val = String::from(v.trim());
                if kl == "content-length" {
                    content_len = val.parse::<u64>().ok();
                } else if kl == "expect" && val.eq_ignore_ascii_case("100-continue") {
                    expect_continue = true;
                }
                headers.push((kl, val));
            }
        }

        if method == "OPTIONS" {
            self.queue_options();
            return;
        }

        if !state.active && path != "/api/ota" {
            self.queue_response_bytes(
                503,
                "Service Unavailable",
                "text/plain",
                None,
                b"VVDrop closed on device",
            );
            return;
        }

        let hdr_val = |k: &str| -> Option<&str> {
            headers.iter().find(|(hk, _)| hk == k).map(|(_, v)| v.as_str())
        };

        match (method, path) {
            ("GET", "/") | ("GET", "/index.html") => {
                self.queue_response_bytes(
                    200,
                    "OK",
                    "text/html; charset=utf-8",
                    Some(VVDROP_WEB_HTML.as_bytes()),
                    &[],
                );
            }
            ("HEAD", "/") | ("HEAD", "/index.html") => {
                self.queue_head_response(
                    200,
                    "OK",
                    "text/html; charset=utf-8",
                    VVDROP_WEB_HTML.len(),
                );
            }
            ("GET", "/api/health") => {
                self.queue_response_bytes(
                    200,
                    "OK",
                    "application/json",
                    None,
                    br#"{"ok":true,"app":"vvdrop"}"#,
                );
            }
            ("GET", "/favicon.ico") => {
                self.queue_response_bytes(204, "No Content", "image/x-icon", None, &[]);
            }
            ("GET", "/api/nonce") => {
                let hex = encode_hex(state.auth.server_nonce());
                self.queue_response_bytes(200, "OK", "text/plain", None, hex.as_bytes());
            }
            ("POST", "/api/auth") => {
                if let Some(dt_hdr) = hdr_val("x-vvdrop-datetime")
                    && let Some(dt) = parse_datetime_header(dt_hdr)
                {
                    state.synced_datetime = Some(dt);
                }
                if hdr_val("x-vvdrop-wpa2-session") == Some("1") {
                    if !state.wpa2_authenticated_link {
                        self.queue_response_bytes(
                            401,
                            "Unauthorized",
                            "text/plain",
                            None,
                            b"wpa2 session requires key",
                        );
                        return;
                    }
                    let mut rng = XorShift64(state.rng_state.wrapping_add(1337));
                    let res = state.auth.issue_wpa2_session_token(&mut rng);
                    state.rng_state = rng.0;
                    match res {
                        Ok(tok) => {
                            self.queue_response_bytes(
                                200,
                                "OK",
                                "text/plain",
                                None,
                                encode_hex(&tok).as_bytes(),
                            );
                        }
                        Err(_) => {
                            self.queue_response_bytes(
                                423,
                                "Locked",
                                "text/plain",
                                None,
                                b"locked out",
                            );
                        }
                    }
                    return;
                }
                let Some(cn_hex) = hdr_val("x-vvdrop-client-nonce") else {
                    self.queue_response_bytes(400, "Bad Request", "text/plain", None, b"missing client nonce");
                    return;
                };
                let Some(cn) = decode_hex_32(cn_hex) else {
                    self.queue_response_bytes(400, "Bad Request", "text/plain", None, b"bad client nonce");
                    return;
                };
                let mac = if let Some(mac_hex) = hdr_val("x-vvdrop-mac") {
                    match decode_hex_32(mac_hex) {
                        Some(m) => m,
                        None => {
                            self.queue_response_bytes(400, "Bad Request", "text/plain", None, b"bad mac hex");
                            return;
                        }
                    }
                } else if let Some(pin) = hdr_val("x-vvdrop-pin") {
                    let clean_pin: Vec<u8> = pin
                        .bytes()
                        .filter(|b| !b.is_ascii_whitespace() && *b != b'-')
                        .collect();
                    compute_auth_mac(&clean_pin, state.auth.server_nonce(), &cn)
                } else {
                    self.queue_response_bytes(400, "Bad Request", "text/plain", None, b"missing credentials");
                    return;
                };

                let mut rng = XorShift64(state.rng_state.wrapping_add(1337));
                let res = state.auth.verify_and_issue_token(&cn, &mac, &mut rng);
                state.rng_state = rng.0;
                match res {
                    Ok(tok) => {
                        self.queue_response_bytes(
                            200,
                            "OK",
                            "text/plain",
                            None,
                            encode_hex(&tok).as_bytes(),
                        );
                    }
                    Err(vvbook_security::AuthError::LockedOut) => {
                        self.queue_response_bytes(
                            423,
                            "Locked",
                            "text/plain",
                            None,
                            b"locked out",
                        );
                    }
                    Err(_) => {
                        self.queue_response_bytes(
                            403,
                            "Forbidden",
                            "text/plain",
                            None,
                            b"invalid pin",
                        );
                    }
                }
            }
            ("POST", "/api/upload") => {
                let total = content_len.unwrap_or(0);
                let Some(tok_hex) = hdr_val("x-vvdrop-token") else {
                    self.reject_with_drain(total, 401, "Unauthorized", b"unauthorized");
                    return;
                };
                let Some(tok) = decode_hex_32(tok_hex) else {
                    self.reject_with_drain(total, 401, "Unauthorized", b"unauthorized");
                    return;
                };
                if state.auth.verify_token(&tok).is_err() {
                    self.reject_with_drain(total, 401, "Unauthorized", b"unauthorized");
                    return;
                }
                if total == 0 || total > MAX_UPLOAD_BYTES {
                    self.reject_with_drain(total, 400, "Bad Request", b"invalid content length");
                    return;
                }
                if state.active_upload_path.is_some() {
                    self.reject_with_drain(total, 409, "Conflict", b"another upload in progress");
                    return;
                }
                let raw_name = hdr_val("x-vvdrop-filename").unwrap_or("book.txt");
                let safe_name = sanitize_book_filename(raw_name);
                let dest_path = format!("/books/{safe_name}");
                let _ = storage.mkdir_all("/books");
                let _ = storage.remove(&dest_path);
                if storage.write_atomic(&dest_path, &[]).is_err() {
                    self.reject_with_drain(total, 507, "Insufficient Storage", b"microSD write failed");
                    return;
                }
                state.active_upload_path = Some(dest_path.clone());
                state.rx_bytes = 0;
                state.total_bytes = total;
                state.busy_message = Some(format!("{safe_name} (0%)"));
                state.status_dirty = true;
                if expect_continue {
                    self.tx_head = b"HTTP/1.1 100 Continue\r\n\r\n".to_vec();
                    self.tx_head_pos = 0;
                }
                self.stage = ConnStage::StreamingUpload {
                    dest_path,
                    filename: safe_name,
                    remaining: total,
                    total,
                    received: 0,
                    last_pct_bucket: 0,
                };
            }
            ("POST", "/api/ota") => {
                let total = content_len.unwrap_or(0);
                if total < SignedFwHeader::SIZE as u64 || total > MAX_OTA_BYTES {
                    self.reject_with_drain(total, 400, "Bad Request", b"invalid ota size");
                    return;
                }
                if expect_continue {
                    self.tx_head = b"HTTP/1.1 100 Continue\r\n\r\n".to_vec();
                    self.tx_head_pos = 0;
                }
                self.stage = ConnStage::StreamingOtaHeader {
                    remaining_total: total,
                    hdr_buf: [0u8; SignedFwHeader::SIZE],
                    hdr_len: 0,
                };
            }
            _ => {
                // Unknown HTTP paths redirect gently or return 404
                if method == "GET" && !path.starts_with("/api/") {
                    self.queue_redirect_root();
                } else {
                    self.queue_response_bytes(404, "Not Found", "text/plain", None, b"not found");
                }
            }
        }
    }

    fn reject_with_drain(
        &mut self,
        body_len: u64,
        status: u16,
        reason: &'static str,
        msg: &[u8],
    ) {
        if body_len > 0 && body_len <= 1024 * 1024 {
            self.stage = ConnStage::DrainingBadBody {
                remaining: body_len,
                status,
                reason,
                msg: msg.to_vec(),
            };
        } else {
            self.queue_response_bytes(status, reason, "text/plain", None, msg);
        }
    }
}

struct TcpSlot {
    handle: SocketHandle,
    conn: StreamingHttpConn,
    was_active: bool,
    last_io_ms: u64,
    fin_wait_since_ms: Option<u64>,
    time_wait_since_ms: Option<u64>,
    last_tx_queued: usize,
}

impl TcpSlot {
    fn new(handle: SocketHandle) -> Self {
        Self {
            handle,
            conn: StreamingHttpConn::new(false),
            was_active: false,
            last_io_ms: 0,
            fin_wait_since_ms: None,
            time_wait_since_ms: None,
            last_tx_queued: 0,
        }
    }

    fn clear_timers(&mut self) {
        self.was_active = false;
        self.fin_wait_since_ms = None;
        self.time_wait_since_ms = None;
        self.last_tx_queued = 0;
    }

    fn step(
        &mut self,
        sock: &mut tcp::Socket<'_>,
        listen_count: &mut usize,
        now_ms: u64,
        state: &mut VvDropServerState,
        storage: &mut dyn StorageBackend,
    ) -> bool {
        match sock.state() {
            tcp::State::Closed => {
                self.conn.abort(state, storage);
                self.clear_timers();
                let _ = sock.listen(HTTP_PORT);
                *listen_count += 1;
                true
            }
            tcp::State::Listen => {
                self.clear_timers();
                false
            }
            tcp::State::TimeWait => {
                self.conn.abort(state, storage);
                self.fin_wait_since_ms = None;
                self.last_tx_queued = 0;
                let t0 = *self.time_wait_since_ms.get_or_insert(now_ms);
                if *listen_count < 3 || now_ms.saturating_sub(t0) >= 800 {
                    let _ = sock.listen(HTTP_PORT);
                    self.clear_timers();
                    *listen_count += 1;
                    true
                } else {
                    false
                }
            }
            tcp::State::SynReceived | tcp::State::SynSent => {
                self.fin_wait_since_ms = None;
                self.time_wait_since_ms = None;
                if !self.was_active {
                    self.was_active = true;
                    self.last_io_ms = now_ms;
                    false
                } else if now_ms.saturating_sub(self.last_io_ms) >= 3_000 {
                    self.conn.abort(state, storage);
                    sock.abort();
                    self.clear_timers();
                    let _ = sock.listen(HTTP_PORT);
                    *listen_count += 1;
                    true
                } else {
                    false
                }
            }
            tcp::State::FinWait1 | tcp::State::FinWait2 | tcp::State::Closing | tcp::State::LastAck => {
                self.time_wait_since_ms = None;
                let q = sock.send_queue();
                if q < self.last_tx_queued {
                    self.last_tx_queued = q;
                    self.fin_wait_since_ms = Some(now_ms);
                }
                let t0 = *self.fin_wait_since_ms.get_or_insert(now_ms);
                let limit_ms = if q > 0 {
                    10_000
                } else if sock.state() == tcp::State::FinWait2 {
                    1_500
                } else {
                    3_000
                };
                if now_ms.saturating_sub(t0) >= limit_ms {
                    self.conn.abort(state, storage);
                    sock.abort();
                    self.clear_timers();
                    let _ = sock.listen(HTTP_PORT);
                    *listen_count += 1;
                    true
                } else {
                    false
                }
            }
            tcp::State::Established | tcp::State::CloseWait => {
                self.fin_wait_since_ms = None;
                self.time_wait_since_ms = None;
                if !self.was_active {
                    self.was_active = true;
                    self.last_io_ms = now_ms;
                }
                let mut did_io = false;
                while sock.can_recv() && self.conn.wants_rx() {
                    let consumed = sock
                        .recv(|buf| {
                            let n = self.conn.on_rx(buf, state, storage);
                            (n, n)
                        })
                        .unwrap_or(0);
                    if consumed == 0 {
                        break;
                    }
                    did_io = true;
                }
                while sock.can_send() && self.conn.has_tx() {
                    let sent = sock
                        .send(|buf| {
                            let n = self.conn.peek_tx(buf);
                            (n, n)
                        })
                        .unwrap_or(0);
                    if sent == 0 {
                        break;
                    }
                    self.conn.advance_tx(sent);
                    did_io = true;
                }
                if did_io {
                    self.last_io_ms = now_ms;
                }
                if self.conn.should_close() {
                    sock.close();
                    self.conn.reset();
                    self.fin_wait_since_ms = Some(now_ms);
                    self.last_tx_queued = sock.send_queue();
                    true
                } else if !sock.may_recv() && !sock.can_recv() && !self.conn.has_tx() {
                    self.conn.abort(state, storage);
                    sock.close();
                    self.fin_wait_since_ms = Some(now_ms);
                    self.last_tx_queued = sock.send_queue();
                    true
                } else {
                    let idle_limit = if self.conn.is_idle_header_wait() {
                        if *listen_count == 0 { 1_200 } else { 4_000 }
                    } else {
                        15_000
                    };
                    if now_ms.saturating_sub(self.last_io_ms) >= idle_limit {
                        self.conn.abort(state, storage);
                        sock.abort();
                        self.clear_timers();
                        let _ = sock.listen(HTTP_PORT);
                        *listen_count += 1;
                        true
                    } else {
                        did_io
                    }
                }
            }
        }
    }
}

/// Shared `#![no_std]` `smoltcp` SoftAP portal stack (DHCPv4 + DNS + 6x HTTP/80 TCP slots).
pub struct SmoltcpPortalEngine {
    iface: Interface,
    sockets: SocketSet<'static>,
    dhcp_handle: SocketHandle,
    dns_handle: SocketHandle,
    slots: [TcpSlot; HTTP_SLOT_COUNT],
}

impl SmoltcpPortalEngine {
    /// Construct and bind the SoftAP IPv4 interface (`192.168.4.1/24`), UDP `67`/`53`, and 6 HTTP port-80 sockets.
    pub fn new<D: Device + ?Sized>(device: &mut D, mac: [u8; 6], seed: u64, now_ms: u64) -> Self {
        let mut net_cfg = NetConfig::new(EthernetAddress(mac).into());
        net_cfg.random_seed = seed | 1;

        let mut iface = Interface::new(net_cfg, device, Instant::from_millis(now_ms as i64));
        iface.update_ip_addrs(|addrs| {
            let _ = addrs.push(IpCidr::Ipv4(Ipv4Cidr::new(
                Ipv4Address::new(AP_IPV4[0], AP_IPV4[1], AP_IPV4[2], AP_IPV4[3]),
                24,
            )));
        });

        let sock_storage = Box::leak(Box::new([const { SocketStorage::EMPTY }; 2 + HTTP_SLOT_COUNT]));
        let mut sockets = SocketSet::new(&mut sock_storage[..]);

        let make_udp = |rx_pkts: usize, rx_bytes: usize, tx_pkts: usize, tx_bytes: usize, port: u16| {
            let mut s = udp::Socket::new(
                udp::PacketBuffer::new(
                    Box::leak(vec![udp::PacketMetadata::EMPTY; rx_pkts].into_boxed_slice()),
                    Box::leak(vec![0u8; rx_bytes].into_boxed_slice()),
                ),
                udp::PacketBuffer::new(
                    Box::leak(vec![udp::PacketMetadata::EMPTY; tx_pkts].into_boxed_slice()),
                    Box::leak(vec![0u8; tx_bytes].into_boxed_slice()),
                ),
            );
            let _ = s.bind(port);
            s
        };
        let dhcp_handle = sockets.add(make_udp(6, 2048, 6, 2048, 67));
        let dns_handle = sockets.add(make_udp(8, 1024, 8, 1024, 53));

        let make_tcp = |rx_len: usize, tx_len: usize| {
            let mut s = tcp::Socket::new(
                tcp::SocketBuffer::new(Box::leak(vec![0u8; rx_len].into_boxed_slice())),
                tcp::SocketBuffer::new(Box::leak(vec![0u8; tx_len].into_boxed_slice())),
            );
            s.set_nagle_enabled(false);
            s.set_ack_delay(None);
            s.set_timeout(Some(NetDuration::from_secs(30)));
            let _ = s.listen(HTTP_PORT);
            s
        };
        let slots = [
            TcpSlot::new(sockets.add(make_tcp(4096, 8704))),
            TcpSlot::new(sockets.add(make_tcp(4096, 8704))),
            TcpSlot::new(sockets.add(make_tcp(1536, 2048))),
            TcpSlot::new(sockets.add(make_tcp(1536, 2048))),
            TcpSlot::new(sockets.add(make_tcp(1536, 2048))),
            TcpSlot::new(sockets.add(make_tcp(1536, 2048))),
        ];

        Self {
            iface,
            sockets,
            dhcp_handle,
            dns_handle,
            slots,
        }
    }

    /// Update the SoftAP Ethernet hardware MAC address after AP startup.
    pub fn set_hardware_addr(&mut self, mac: [u8; 6]) {
        self.iface.set_hardware_addr(EthernetAddress(mac).into());
    }

    /// Abort active TCP connections cleanly (cleaning up any staged upload via `storage`) and re-listen.
    pub fn reset_listeners(
        &mut self,
        abort_sockets: bool,
        state: &mut VvDropServerState,
        storage: &mut dyn StorageBackend,
    ) {
        for slot in &mut self.slots {
            slot.conn.abort(state, storage);
            slot.clear_timers();
            let sock = self.sockets.get_mut::<tcp::Socket<'_>>(slot.handle);
            if abort_sockets {
                sock.abort();
            }
            if !sock.is_open() {
                let _ = sock.listen(HTTP_PORT);
            }
        }
    }

    /// Reset all TCP sockets and listener slots without touching external storage (used around AP start/stop).
    pub fn reset_sockets_only(&mut self, abort_sockets: bool) {
        for slot in &mut self.slots {
            slot.conn.reset();
            slot.clear_timers();
            let sock = self.sockets.get_mut::<tcp::Socket<'_>>(slot.handle);
            if abort_sockets {
                sock.abort();
            }
            if !sock.is_open() {
                let _ = sock.listen(HTTP_PORT);
            }
        }
    }

    /// Pump up to 16 ingress/socket/egress iterations across DHCP, DNS, and HTTP slots.
    pub fn poll_io<D: Device + ?Sized>(
        &mut self,
        device: &mut D,
        state: &mut VvDropServerState,
        dhcp: &mut DhcpServer,
        storage: &mut dyn StorageBackend,
        now_ms: u64,
    ) -> bool {
        if !state.active {
            return false;
        }

        let mut rx_buf = [0u8; 1536];
        let mut tx_buf = [0u8; 1536];
        let bcast = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::BROADCAST), 68);
        let now = Instant::from_millis(now_ms as i64);

        for _ in 0..16 {
            let polled = matches!(
                self.iface.poll(now, device, &mut self.sockets),
                PollResult::SocketStateChanged
            );
            let mut did_any_io = false;

            {
                let sock = self.sockets.get_mut::<udp::Socket<'_>>(self.dhcp_handle);
                while let Ok((n, _meta)) = sock.recv_slice(&mut rx_buf) {
                    did_any_io = true;
                    if let Some(out_n) = dhcp.handle_packet(&rx_buf[..n], &mut tx_buf) {
                        let _ = sock.send_slice(&tx_buf[..out_n], bcast);
                    }
                    let leases = dhcp.active_lease_count();
                    if state.clients_connected != leases {
                        state.clients_connected = leases;
                        state.status_dirty = true;
                    }
                }
            }

            {
                let sock = self.sockets.get_mut::<udp::Socket<'_>>(self.dns_handle);
                while let Ok((n, meta)) = sock.recv_slice(&mut rx_buf) {
                    did_any_io = true;
                    if let Some(out_n) = DnsResponder::handle_query(&rx_buf[..n], &mut tx_buf) {
                        let _ = sock.send_slice(&tx_buf[..out_n], meta.endpoint);
                    }
                }
            }

            let mut listen_count = self
                .slots
                .iter()
                .filter(|slot| {
                    self.sockets.get::<tcp::Socket<'_>>(slot.handle).state() == tcp::State::Listen
                })
                .count();

            for slot in &mut self.slots {
                let sock = self.sockets.get_mut::<tcp::Socket<'_>>(slot.handle);
                did_any_io |= slot.step(sock, &mut listen_count, now_ms, state, storage);
            }

            let flushed = matches!(
                self.iface.poll(now, device, &mut self.sockets),
                PollResult::SocketStateChanged
            );
            if !polled && !did_any_io && !flushed {
                break;
            }
        }

        core::mem::replace(&mut state.status_dirty, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::string::ToString;
    use alloc::vec;
    use ed25519_compact::KeyPair;
    use vvbook_pal::{DirEntry, StorageError};

    #[derive(Default)]
    struct MemStore(BTreeMap<String, Vec<u8>>);

    impl StorageBackend for MemStore {
        fn exists(&self, path: &str) -> bool {
            self.0.contains_key(path)
        }
        fn file_len(&self, path: &str) -> Result<u64, StorageError> {
            self.0
                .get(path)
                .map(|v| v.len() as u64)
                .ok_or(StorageError::NotFound)
        }
        fn read_all(&self, path: &str) -> Result<Vec<u8>, StorageError> {
            self.0.get(path).cloned().ok_or(StorageError::NotFound)
        }
        fn read_range(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
            let v = self.0.get(path).ok_or(StorageError::NotFound)?;
            let off = offset as usize;
            if off >= v.len() {
                return Ok(0);
            }
            let n = buf.len().min(v.len() - off);
            buf[..n].copy_from_slice(&v[off..off + n]);
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

    fn drain_tx(conn: &mut StreamingHttpConn) -> String {
        let mut out = Vec::new();
        let mut tmp = [0u8; 256];
        while conn.has_tx() {
            let n = conn.peek_tx(&mut tmp);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&tmp[..n]);
            conn.advance_tx(n);
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn find_dhcp_opt(pkt: &[u8], opt: u8) -> Option<&[u8]> {
        let mut i = 240usize;
        while i < pkt.len() {
            let code = pkt[i];
            i += 1;
            if code == 255 {
                break;
            }
            if code == 0 {
                continue;
            }
            let len = usize::from(*pkt.get(i)?);
            i += 1;
            let val = pkt.get(i..i + len)?;
            if code == opt {
                return Some(val);
            }
            i += len;
        }
        None
    }

    #[test]
    fn dhcp_offer_and_ack_lease_lifecycle() {
        let mut dhcp = DhcpServer::new();
        let mut req = [0u8; 300];
        req[0] = 1;
        req[1] = 1;
        req[2] = 6;
        req[4..8].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        req[28..34].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0x11, 0x22, 0x33]);
        req[236..240].copy_from_slice(&DHCP_MAGIC_COOKIE);
        req[240..244].copy_from_slice(&[53, 1, 1, 255]); // DISCOVER

        let mut out = [0u8; 512];
        let n = dhcp.handle_packet(&req, &mut out).expect("OFFER expected");
        assert!(n >= 299);
        assert_eq!(out[0], 2);
        assert_eq!(out[16..20], [192, 168, 4, 2]);
        assert_eq!(dhcp.active_lease_count(), 1);
        assert_eq!(find_dhcp_opt(&out[..n], 1), Some(&AP_NETMASK[..]));
        assert_eq!(find_dhcp_opt(&out[..n], 3), Some(&AP_IPV4[..]));
        assert_eq!(find_dhcp_opt(&out[..n], 6), Some(&AP_IPV4[..]));
        assert_eq!(find_dhcp_opt(&out[..n], 15), Some(&b"local"[..]));
        assert_eq!(find_dhcp_opt(&out[..n], 28), Some(&AP_BROADCAST[..]));
        assert!(find_dhcp_opt(&out[..n], 51).is_some());

        // REQUEST for offered IP -> ACK with same IP, Router & DNS options
        req[240..251].copy_from_slice(&[53, 1, 3, 50, 4, 192, 168, 4, 2, 255, 0]);
        let n2 = dhcp.handle_packet(&req, &mut out).expect("ACK expected");
        assert!(n2 >= 299);
        assert_eq!(out[16..20], [192, 168, 4, 2]);
        assert_eq!(&out[240..243], &[53, 1, 5]);
        assert_eq!(find_dhcp_opt(&out[..n2], 3), Some(&AP_IPV4[..]));
        assert_eq!(find_dhcp_opt(&out[..n2], 6), Some(&AP_IPV4[..]));

        // DHCPINFORM -> ACK without Option 51/58/59 (RFC 2131 §4.3.5), preserving Router + DNS
        let mut inf_req = [0u8; 300];
        inf_req[0] = 1;
        inf_req[1] = 1;
        inf_req[2] = 6;
        inf_req[4..8].copy_from_slice(&0x0102_0304u32.to_be_bytes());
        inf_req[12..16].copy_from_slice(&[192, 168, 4, 2]);
        inf_req[28..34].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0x11, 0x22, 0x33]);
        inf_req[236..240].copy_from_slice(&DHCP_MAGIC_COOKIE);
        inf_req[240..244].copy_from_slice(&[53, 1, 8, 255]);
        let n_inf = dhcp.handle_packet(&inf_req, &mut out).expect("DHCPINFORM ACK expected");
        assert_eq!(out[16..20], [0, 0, 0, 0]);
        assert!(find_dhcp_opt(&out[..n_inf], 51).is_none());
        assert_eq!(find_dhcp_opt(&out[..n_inf], 3), Some(&AP_IPV4[..]));
        assert_eq!(find_dhcp_opt(&out[..n_inf], 6), Some(&AP_IPV4[..]));

        // Foreign-subnet INIT-REBOOT DHCPREQUEST (e.g. phone moving from home router) -> DHCPNAK
        let mut foreign_req = [0u8; 300];
        foreign_req[0] = 1;
        foreign_req[1] = 1;
        foreign_req[2] = 6;
        foreign_req[4..8].copy_from_slice(&0xCAFE_BABEu32.to_be_bytes());
        foreign_req[28..34].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01]);
        foreign_req[236..240].copy_from_slice(&DHCP_MAGIC_COOKIE);
        foreign_req[240..251].copy_from_slice(&[53, 1, 3, 50, 4, 192, 168, 1, 50, 255, 0]);
        let n_nak = dhcp.handle_packet(&foreign_req, &mut out).expect("DHCPNAK expected");
        assert_eq!(find_dhcp_opt(&out[..n_nak], 53), Some(&[6u8][..]));
        assert_eq!(out[16..20], [0, 0, 0, 0]);
        assert_eq!(dhcp.active_lease_count(), 1, "NAK must not allocate a lease");
    }

    #[test]
    fn dns_resolves_vvbook_local_and_nxdomains_captive_checks() {
        let build_q = |name: &str, qtype: u16| -> Vec<u8> {
            let mut p = Vec::new();
            p.extend_from_slice(&[0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
            for lbl in name.split('.') {
                p.push(lbl.len() as u8);
                p.extend_from_slice(lbl.as_bytes());
            }
            p.push(0);
            p.extend_from_slice(&qtype.to_be_bytes());
            p.extend_from_slice(&1u16.to_be_bytes());
            p
        };

        let mut out = [0u8; 512];
        let q1 = build_q("vvbook.local", 1);
        let n1 = DnsResponder::handle_query(&q1, &mut out).unwrap();
        assert_eq!(out[3] & 0x0F, 0);
        assert_eq!(&out[n1 - 4..n1], &AP_IPV4);

        let q2 = build_q("captive.apple.com", 1);
        let _n2 = DnsResponder::handle_query(&q2, &mut out).unwrap();
        assert_eq!(out[3] & 0x0F, 3); // NXDOMAIN
    }

    #[test]
    fn streaming_http_auth_upload_chunks_abort_and_ed25519_ota() {
        let kp = KeyPair::from_seed(ed25519_compact::Seed::new([9u8; 32]));
        let mut pub_k = [0u8; 32];
        pub_k.copy_from_slice(kp.pk.as_ref());

        let mut state = VvDropServerState::new(pub_k, 1, 42);
        state.start("48291704", 99);
        let mut store = MemStore::default();
        let mut conn = StreamingHttpConn::new(false);

        // 1. GET / serves vvdrop_web.html directly from static slice
        let req_get = b"GET / HTTP/1.1\r\nHost: 192.168.4.1\r\n\r\n";
        assert_eq!(conn.on_rx(req_get, &mut state, &mut store), req_get.len());
        let resp_get = drain_tx(&mut conn);
        assert!(resp_get.starts_with("HTTP/1.1 200 OK"));
        assert!(resp_get.contains("VVBook-Folio"));

        // 1b. GET /favicon.ico -> 204 No Content (no 302 redirect)
        conn.reset();
        conn.on_rx(b"GET /favicon.ico HTTP/1.1\r\nHost: 192.168.4.1\r\n\r\n", &mut state, &mut store);
        let resp_fav = drain_tx(&mut conn);
        assert!(resp_fav.starts_with("HTTP/1.1 204 No Content"));

        // 1c. X-VVDrop-Wpa2-Session rejected (401) when wpa2_authenticated_link == false, accepted when true
        conn.reset();
        conn.on_rx(
            b"POST /api/auth HTTP/1.1\r\nX-VVDrop-Wpa2-Session: 1\r\nContent-Length: 0\r\n\r\n",
            &mut state,
            &mut store,
        );
        let wpa_denied = drain_tx(&mut conn);
        assert!(wpa_denied.starts_with("HTTP/1.1 401 Unauthorized"));
        assert_eq!(state.auth.failed_attempts(), 0);

        state.wpa2_authenticated_link = true;
        conn.reset();
        conn.on_rx(
            b"POST /api/auth HTTP/1.1\r\nX-VVDrop-Wpa2-Session: 1\r\nContent-Length: 0\r\n\r\n",
            &mut state,
            &mut store,
        );
        let wpa_ok = drain_tx(&mut conn);
        assert!(wpa_ok.starts_with("HTTP/1.1 200 OK"));
        let wpa_tok = decode_hex_32(wpa_ok.split("\r\n\r\n").nth(1).unwrap().trim()).unwrap();
        assert!(state.auth.verify_token(&wpa_tok).is_ok());

        // 2. GET /api/nonce -> POST /api/auth with X-VVDrop-Datetime
        conn.reset();
        conn.on_rx(b"GET /api/nonce HTTP/1.1\r\n\r\n", &mut state, &mut store);
        let resp_nonce = drain_tx(&mut conn);
        let server_nonce_hex = resp_nonce.split("\r\n\r\n").nth(1).unwrap().trim();
        let s_nonce = decode_hex_32(server_nonce_hex).unwrap();
        let c_nonce = [0x55u8; 32];
        let mac = compute_auth_mac(b"48291704", &s_nonce, &c_nonce);

        conn.reset();
        let auth_req = format!(
            "POST /api/auth HTTP/1.1\r\nX-VVDrop-Client-Nonce: {}\r\nX-VVDrop-Mac: {}\r\nX-VVDrop-Datetime: 2026-11-13 14:22:09\r\nContent-Length: 0\r\n\r\n",
            encode_hex(&c_nonce),
            encode_hex(&mac)
        );
        conn.on_rx(auth_req.as_bytes(), &mut state, &mut store);
        let auth_resp = drain_tx(&mut conn);
        assert!(auth_resp.starts_with("HTTP/1.1 200 OK"));
        let tok_hex = auth_resp.split("\r\n\r\n").nth(1).unwrap().trim().to_string();
        assert_eq!(state.synced_datetime.unwrap().hour, 14);
        assert_eq!(state.synced_datetime.unwrap().minute, 22);

        // 3. Multi-chunk streaming upload (4 KB in 512-byte pieces)
        let book_data: Vec<u8> = (0..4096).map(|i| b'A' + (i % 26) as u8).collect();
        conn.reset();
        let up_hdr = format!(
            "POST /api/upload HTTP/1.1\r\nX-VVDrop-Token: {tok_hex}\r\nX-VVDrop-Filename: War%20%26%20Peace.epub\r\nContent-Length: {}\r\n\r\n",
            book_data.len()
        );
        let mut full_req = up_hdr.into_bytes();
        full_req.extend_from_slice(&book_data);
        for slice in full_req.chunks(512) {
            let mut off = 0;
            while off < slice.len() {
                off += conn.on_rx(&slice[off..], &mut state, &mut store);
            }
        }
        let up_resp = drain_tx(&mut conn);
        assert!(up_resp.starts_with("HTTP/1.1 200 OK"));
        assert_eq!(state.rx_bytes, 0, "rx_bytes must reset after completed upload");
        assert_eq!(state.total_bytes, 0);
        assert_eq!(state.completed_uploads, vec![String::from("/books/War & Peace.epub")]);
        assert_eq!(store.read_all("/books/War & Peace.epub").unwrap(), book_data);

        // 4. Aborted partial upload cleans up file cleanly
        conn.reset();
        let abort_hdr = format!(
            "POST /api/upload HTTP/1.1\r\nX-VVDrop-Token: {tok_hex}\r\nX-VVDrop-Filename: ../../evil.epub\r\nContent-Length: 1000\r\n\r\npartial"
        );
        conn.on_rx(abort_hdr.as_bytes(), &mut state, &mut store);
        assert!(store.exists("/books/evil.epub"));
        conn.abort(&mut state, &mut store);
        assert!(!store.exists("/books/evil.epub"));
        assert!(state.active_upload_path.is_none());

        // 5. Signed Ed25519 OTA upload
        let fw_payload = b"ESP32C3_FW_V2_STREAM_VERIFIED_OK";
        let mut hdr = SignedFwHeader {
            format_ver: vvbook_security::VVFW_FORMAT_VERSION,
            fw_version: 2,
            target_hw: HW_TARGET_XTEINK_X3,
            reserved: 0,
            payload_len: fw_payload.len() as u32,
            payload_blake3: *blake3::hash(fw_payload).as_bytes(),
            ed25519_sig: [0u8; 64],
        };
        let sig = kp.sk.sign(SignedFwHeader::canonical_body(&hdr), None);
        hdr.ed25519_sig.copy_from_slice(sig.as_ref());
        let mut ota_body = hdr.to_bytes().to_vec();
        ota_body.extend_from_slice(fw_payload);

        conn.reset();
        let ota_hdr = format!(
            "POST /api/ota HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            ota_body.len()
        );
        conn.on_rx(ota_hdr.as_bytes(), &mut state, &mut store);
        for slice in ota_body.chunks(37) {
            conn.on_rx(slice, &mut state, &mut store);
        }
        let ota_resp = drain_tx(&mut conn);
        assert!(ota_resp.starts_with("HTTP/1.1 200 OK"), "got {ota_resp}");
        assert_eq!(state.min_fw_version, 2);
        assert_eq!(store.read_all(OTA_STAGE_PATH).unwrap(), fw_payload);
    }

    #[test]
    fn smoltcp_softap_dhcp_dns_concurrent_browser_and_upload_gauntlet() {
        use alloc::collections::VecDeque;
        use alloc::rc::Rc;
        use core::cell::RefCell;
        use smoltcp::phy::{self, DeviceCapabilities, Medium};
        use smoltcp::socket::{dhcpv4, dns};
        use smoltcp::wire::DnsQueryType;

        struct LanQueues {
            to_ap: VecDeque<Vec<u8>>,
            to_sta: VecDeque<Vec<u8>>,
            max_queue_len: usize,
            ap_max_burst: Option<usize>,
            drop_every_nth_ap_tx: usize,
            ap_tx_counter: usize,
        }

        struct LanRxToken(Vec<u8>);
        impl phy::RxToken for LanRxToken {
            fn consume<R, F>(self, f: F) -> R
            where
                F: FnOnce(&[u8]) -> R,
            {
                f(&self.0)
            }
        }

        struct LanTxToken {
            lan: Rc<RefCell<LanQueues>>,
            from_ap: bool,
        }
        impl phy::TxToken for LanTxToken {
            fn consume<R, F>(self, len: usize, f: F) -> R
            where
                F: FnOnce(&mut [u8]) -> R,
            {
                let mut frame = vec![0u8; len];
                let res = f(&mut frame);
                let mut q = self.lan.borrow_mut();
                if self.from_ap {
                    q.ap_tx_counter += 1;
                    let drop_this = q.drop_every_nth_ap_tx > 0
                        && q.ap_tx_counter.is_multiple_of(q.drop_every_nth_ap_tx);
                    if !drop_this && q.to_sta.len() < q.max_queue_len {
                        q.to_sta.push_back(frame);
                    }
                } else if q.to_ap.len() < q.max_queue_len {
                    q.to_ap.push_back(frame);
                }
                res
            }
        }

        struct ApEndpoint(Rc<RefCell<LanQueues>>);
        impl phy::Device for ApEndpoint {
            type RxToken<'a> = LanRxToken;
            type TxToken<'a> = LanTxToken;

            fn receive(&mut self, _ts: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
                let pkt = self.0.borrow_mut().to_ap.pop_front()?;
                Some((
                    LanRxToken(pkt),
                    LanTxToken {
                        lan: Rc::clone(&self.0),
                        from_ap: true,
                    },
                ))
            }

            fn transmit(&mut self, _ts: Instant) -> Option<Self::TxToken<'_>> {
                Some(LanTxToken {
                    lan: Rc::clone(&self.0),
                    from_ap: true,
                })
            }

            fn capabilities(&self) -> DeviceCapabilities {
                let mut caps = DeviceCapabilities::default();
                caps.max_transmission_unit = 1492;
                caps.max_burst_size = self.0.borrow().ap_max_burst;
                caps.medium = Medium::Ethernet;
                caps
            }
        }

        struct StaEndpoint(Rc<RefCell<LanQueues>>);
        impl phy::Device for StaEndpoint {
            type RxToken<'a> = LanRxToken;
            type TxToken<'a> = LanTxToken;

            fn receive(&mut self, _ts: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
                let pkt = self.0.borrow_mut().to_sta.pop_front()?;
                Some((
                    LanRxToken(pkt),
                    LanTxToken {
                        lan: Rc::clone(&self.0),
                        from_ap: false,
                    },
                ))
            }

            fn transmit(&mut self, _ts: Instant) -> Option<Self::TxToken<'_>> {
                Some(LanTxToken {
                    lan: Rc::clone(&self.0),
                    from_ap: false,
                })
            }

            fn capabilities(&self) -> DeviceCapabilities {
                let mut caps = DeviceCapabilities::default();
                caps.max_transmission_unit = 1492;
                caps.medium = Medium::Ethernet;
                caps
            }
        }

        let lan = Rc::new(RefCell::new(LanQueues {
            to_ap: VecDeque::new(),
            to_sta: VecDeque::new(),
            max_queue_len: 16,
            ap_max_burst: Some(8),
            drop_every_nth_ap_tx: 0,
            ap_tx_counter: 0,
        }));
        let mut ap_dev = ApEndpoint(Rc::clone(&lan));
        let mut sta_dev = StaEndpoint(Rc::clone(&lan));

        let ap_mac = [0x02, 0x11, 0x22, 0x33, 0x44, 0x01];
        let sta_mac = [0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0x02];
        let mut now_ms = 100u64;

        let mut engine = SmoltcpPortalEngine::new(&mut ap_dev, ap_mac, 777, now_ms);
        let mut state = VvDropServerState::new([1u8; 32], 1, 42);
        state.start("48291037", 99);
        state.wpa2_authenticated_link = true;
        let mut dhcp = DhcpServer::new();
        let mut store = MemStore::default();

        let mut sta_cfg = NetConfig::new(EthernetAddress(sta_mac).into());
        sta_cfg.random_seed = 999;
        let mut sta_iface = Interface::new(sta_cfg, &mut sta_dev, Instant::from_millis(now_ms as i64));
        let mut sta_storage = [const { SocketStorage::EMPTY }; 12];
        let mut sta_sockets = SocketSet::new(&mut sta_storage[..]);

        let dhcp_cli = sta_sockets.add(dhcpv4::Socket::new());
        let dns_cli = sta_sockets.add(dns::Socket::new(&[], vec![None, None, None, None]));

        let step = |now_ms: &mut u64,
                    dt_ms: u64,
                    engine: &mut SmoltcpPortalEngine,
                    ap_dev: &mut ApEndpoint,
                    state: &mut VvDropServerState,
                    dhcp: &mut DhcpServer,
                    store: &mut MemStore,
                    sta_iface: &mut Interface,
                    sta_dev: &mut StaEndpoint,
                    sta_sockets: &mut SocketSet<'_>| {
            *now_ms += dt_ms;
            for _ in 0..4 {
                let ts = Instant::from_millis(*now_ms as i64);
                let _ = sta_iface.poll(ts, sta_dev, sta_sockets);
                engine.poll_io(ap_dev, state, dhcp, store, *now_ms);
                let _ = sta_iface.poll(ts, sta_dev, sta_sockets);
            }
        };

        // 1. DHCP lease acquisition via real smoltcp dhcpv4::Socket client
        let mut acquired = false;
        for _ in 0..40 {
            step(
                &mut now_ms,
                50,
                &mut engine,
                &mut ap_dev,
                &mut state,
                &mut dhcp,
                &mut store,
                &mut sta_iface,
                &mut sta_dev,
                &mut sta_sockets,
            );
            if let Some(dhcpv4::Event::Configured(cfg)) =
                sta_sockets.get_mut::<dhcpv4::Socket<'_>>(dhcp_cli).poll()
            {
                assert_eq!(cfg.address.address(), Ipv4Address::new(192, 168, 4, 2));
                assert_eq!(cfg.router, Some(Ipv4Address::new(192, 168, 4, 1)));
                assert!(cfg.dns_servers.contains(&Ipv4Address::new(192, 168, 4, 1)));
                sta_iface.update_ip_addrs(|addrs| {
                    addrs.clear();
                    let _ = addrs.push(IpCidr::Ipv4(cfg.address));
                });
                let _ = sta_iface
                    .routes_mut()
                    .add_default_ipv4_route(cfg.router.unwrap());
                sta_sockets
                    .get_mut::<dns::Socket<'_>>(dns_cli)
                    .update_servers(&[IpAddress::Ipv4(Ipv4Address::new(192, 168, 4, 1))]);
                acquired = true;
                break;
            }
        }
        assert!(acquired, "DHCP client failed to acquire lease with Router+DNS");
        assert_eq!(state.clients_connected, 1);

        // 2. DNS resolution of vvbook.local + fast NXDOMAIN for captive.apple.com
        let (q_local, q_captive) = {
            let cx = sta_iface.context();
            let s = sta_sockets.get_mut::<dns::Socket<'_>>(dns_cli);
            let q1 = s.start_query(cx, "vvbook.local", DnsQueryType::A).unwrap();
            let q2 = s.start_query(cx, "captive.apple.com", DnsQueryType::A).unwrap();
            (q1, q2)
        };
        let mut local_res = None;
        let mut captive_failed = false;
        for _ in 0..20 {
            step(
                &mut now_ms,
                20,
                &mut engine,
                &mut ap_dev,
                &mut state,
                &mut dhcp,
                &mut store,
                &mut sta_iface,
                &mut sta_dev,
                &mut sta_sockets,
            );
            let s = sta_sockets.get_mut::<dns::Socket<'_>>(dns_cli);
            if local_res.is_none()
                && let Ok(ips) = s.get_query_result(q_local)
            {
                local_res = Some(ips);
            }
            if !captive_failed
                && let Err(dns::GetQueryResultError::Failed) = s.get_query_result(q_captive)
            {
                captive_failed = true;
            }
            if local_res.is_some() && captive_failed {
                break;
            }
        }
        assert_eq!(
            local_res.unwrap().as_slice(),
            &[IpAddress::Ipv4(Ipv4Address::new(192, 168, 4, 1))]
        );
        assert!(captive_failed, "captive portal probe must fast-fail with NXDOMAIN");

        // Helper for TCP socket creation & HTTP roundtrips
        let make_cli_tcp = |sockets: &mut SocketSet<'_>| -> SocketHandle {
            let mut s = tcp::Socket::new(
                tcp::SocketBuffer::new(vec![0u8; 16384]),
                tcp::SocketBuffer::new(vec![0u8; 8192]),
            );
            s.set_nagle_enabled(false);
            s.set_ack_delay(None);
            sockets.add(s)
        };
        let remote = (IpAddress::Ipv4(Ipv4Address::new(192, 168, 4, 1)), HTTP_PORT);

        // 3. Open 6 simultaneous browser connections (2 speculative preconnects + HEAD / + GET / + favicon + POST /api/auth)
        struct CliConn {
            h: SocketHandle,
            req: Option<Vec<u8>>,
            sent_pos: usize,
            rx_buf: Vec<u8>,
            done: bool,
        }
        let mut conns: Vec<CliConn> = Vec::new();
        let reqs: [Option<&[u8]>; 6] = [
            None,
            None,
            Some(b"HEAD / HTTP/1.1\r\nHost: 192.168.4.1\r\n\r\n"),
            Some(b"GET / HTTP/1.1\r\nHost: 192.168.4.1\r\n\r\n"),
            Some(b"GET /favicon.ico HTTP/1.1\r\nHost: 192.168.4.1\r\n\r\n"),
            Some(
                b"POST /api/auth HTTP/1.1\r\nHost: 192.168.4.1\r\nX-VVDrop-Wpa2-Session: 1\r\nX-VVDrop-Datetime: 2026-11-14 01:30:00\r\nContent-Length: 0\r\n\r\n",
            ),
        ];
        for (i, req) in reqs.iter().enumerate() {
            let h = make_cli_tcp(&mut sta_sockets);
            let local_port = 50_000 + i as u16;
            sta_sockets
                .get_mut::<tcp::Socket<'_>>(h)
                .connect(sta_iface.context(), remote, local_port)
                .unwrap();
            conns.push(CliConn {
                h,
                req: req.map(|b| b.to_vec()),
                sent_pos: 0,
                rx_buf: Vec::new(),
                done: false,
            });
        }

        for _ in 0..80 {
            step(
                &mut now_ms,
                15,
                &mut engine,
                &mut ap_dev,
                &mut state,
                &mut dhcp,
                &mut store,
                &mut sta_iface,
                &mut sta_dev,
                &mut sta_sockets,
            );
            for c in &mut conns {
                let s = sta_sockets.get_mut::<tcp::Socket<'_>>(c.h);
                if let Some(req_bytes) = &c.req
                    && c.sent_pos < req_bytes.len()
                    && s.can_send()
                {
                    let n = s.send_slice(&req_bytes[c.sent_pos..]).unwrap();
                    c.sent_pos += n;
                }
                while s.can_recv() {
                    let _ = s.recv(|b| {
                        c.rx_buf.extend_from_slice(b);
                        (b.len(), ())
                    });
                }
                if c.req.is_some() && !s.may_recv() && !s.can_recv() {
                    assert!(
                        s.state() != tcp::State::Closed || !c.rx_buf.is_empty(),
                        "unexpected TCP RST / empty close"
                    );
                    s.close();
                    c.done = true;
                }
            }
            if conns[2..].iter().all(|c| c.done) {
                break;
            }
        }
        assert!(conns[2].done && conns[3].done && conns[4].done && conns[5].done);

        let head_str = core::str::from_utf8(&conns[2].rx_buf).unwrap();
        let expected_cl = format!("Content-Length: {}", VVDROP_WEB_HTML.len());
        assert!(head_str.starts_with("HTTP/1.1 200 OK"));
        assert!(head_str.contains(&expected_cl));
        assert_eq!(head_str.split("\r\n\r\n").nth(1).unwrap(), "");

        let get_str = core::str::from_utf8(&conns[3].rx_buf).unwrap();
        assert!(get_str.starts_with("HTTP/1.1 200 OK"));
        assert_eq!(get_str.split_once("\r\n\r\n").unwrap().1, VVDROP_WEB_HTML);

        let fav_str = core::str::from_utf8(&conns[4].rx_buf).unwrap();
        assert!(fav_str.starts_with("HTTP/1.1 204 No Content"));

        let auth_str = core::str::from_utf8(&conns[5].rx_buf).unwrap();
        assert!(auth_str.starts_with("HTTP/1.1 200 OK"));
        let tok_hex = auth_str.split("\r\n\r\n").nth(1).unwrap().trim().to_string();
        assert_eq!(tok_hex.len(), 64);

        // 4. Keep 2 preconnects open + open 7th/8th connections (24 KB streaming upload + second GET /)
        let epub_bytes: Vec<u8> = (0..24_576).map(|i| b'a' + (i % 26) as u8).collect();
        let up_req = {
            let hdr = format!(
                "POST /api/upload HTTP/1.1\r\nHost: 192.168.4.1\r\nX-VVDrop-Token: {tok_hex}\r\nX-VVDrop-Filename: War%20and%20Peace.epub\r\nContent-Length: {}\r\n\r\n",
                epub_bytes.len()
            );
            let mut v = hdr.into_bytes();
            v.extend_from_slice(&epub_bytes);
            v
        };
        let h_up = make_cli_tcp(&mut sta_sockets);
        sta_sockets
            .get_mut::<tcp::Socket<'_>>(h_up)
            .connect(sta_iface.context(), remote, 50_100)
            .unwrap();
        let mut up_pos = 0usize;
        let mut up_rx = Vec::new();
        let mut up_done = false;

        let h_get2 = make_cli_tcp(&mut sta_sockets);
        sta_sockets
            .get_mut::<tcp::Socket<'_>>(h_get2)
            .connect(sta_iface.context(), remote, 50_101)
            .unwrap();
        let get2_req = b"GET /index.html HTTP/1.1\r\nHost: 192.168.4.1\r\n\r\n";
        let mut get2_pos = 0usize;
        let mut get2_rx = Vec::new();
        let mut get2_done = false;

        for _ in 0..160 {
            step(
                &mut now_ms,
                25,
                &mut engine,
                &mut ap_dev,
                &mut state,
                &mut dhcp,
                &mut store,
                &mut sta_iface,
                &mut sta_dev,
                &mut sta_sockets,
            );
            let s_up = sta_sockets.get_mut::<tcp::Socket<'_>>(h_up);
            if up_pos < up_req.len() && s_up.can_send() {
                up_pos += s_up.send_slice(&up_req[up_pos..]).unwrap();
            }
            while s_up.can_recv() {
                let _ = s_up.recv(|b| {
                    up_rx.extend_from_slice(b);
                    (b.len(), ())
                });
            }
            if !up_done && !s_up.may_recv() && !s_up.can_recv() {
                s_up.close();
                up_done = true;
            }

            let s_g2 = sta_sockets.get_mut::<tcp::Socket<'_>>(h_get2);
            if get2_pos < get2_req.len() && s_g2.can_send() {
                get2_pos += s_g2.send_slice(&get2_req[get2_pos..]).unwrap();
            }
            while s_g2.can_recv() {
                let _ = s_g2.recv(|b| {
                    get2_rx.extend_from_slice(b);
                    (b.len(), ())
                });
            }
            if !get2_done && !s_g2.may_recv() && !s_g2.can_recv() {
                s_g2.close();
                get2_done = true;
            }
            if up_done && get2_done {
                break;
            }
        }
        assert!(up_done && get2_done);
        assert!(core::str::from_utf8(&up_rx).unwrap().starts_with("HTTP/1.1 200 OK"));
        assert_eq!(store.read_all("/books/War and Peace.epub").unwrap(), epub_bytes);
        assert_eq!(
            core::str::from_utf8(&get2_rx)
                .unwrap()
                .split_once("\r\n\r\n")
                .unwrap()
                .1,
            VVDROP_WEB_HTML
        );

        // Close preconnects before Phase B
        for c in &conns {
            sta_sockets.get_mut::<tcp::Socket<'_>>(c.h).abort();
        }
        sta_sockets.get_mut::<tcp::Socket<'_>>(h_up).abort();
        sta_sockets.get_mut::<tcp::Socket<'_>>(h_get2).abort();

        // 5. Phase B: Lossy radio (max_burst=1, drop every 4th AP TX) must still complete 8KB GET / without 2.5s RST
        {
            let mut q = lan.borrow_mut();
            q.ap_max_burst = Some(1);
            q.drop_every_nth_ap_tx = 4;
        }
        let h_lossy = make_cli_tcp(&mut sta_sockets);
        sta_sockets
            .get_mut::<tcp::Socket<'_>>(h_lossy)
            .connect(sta_iface.context(), remote, 50_200)
            .unwrap();
        let mut lossy_pos = 0usize;
        let mut lossy_rx = Vec::new();
        let mut lossy_done = false;
        for _ in 0..400 {
            step(
                &mut now_ms,
                20,
                &mut engine,
                &mut ap_dev,
                &mut state,
                &mut dhcp,
                &mut store,
                &mut sta_iface,
                &mut sta_dev,
                &mut sta_sockets,
            );
            let s = sta_sockets.get_mut::<tcp::Socket<'_>>(h_lossy);
            if lossy_pos < get2_req.len() && s.can_send() {
                lossy_pos += s.send_slice(&get2_req[lossy_pos..]).unwrap();
            }
            while s.can_recv() {
                let _ = s.recv(|b| {
                    lossy_rx.extend_from_slice(b);
                    (b.len(), ())
                });
            }
            if !s.may_recv() && !s.can_recv() {
                s.close();
                lossy_done = true;
                break;
            }
        }
        assert!(lossy_done, "lossy GET / did not complete cleanly");
        let lossy_str = core::str::from_utf8(&lossy_rx).unwrap();
        assert!(lossy_str.starts_with("HTTP/1.1 200 OK"));
        assert_eq!(lossy_str.split_once("\r\n\r\n").unwrap().1, VVDROP_WEB_HTML);
    }
}
