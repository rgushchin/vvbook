#![forbid(unsafe_code)]

use ed25519_compact::{KeyPair, SecretKey, Seed};
use std::io::{Read, Write};
use std::net::TcpStream;
use vvbook_format::{VvbBook, ingest_document};
use vvbook_security::{
    HW_TARGET_XTEINK_X3, SignedFwHeader, StreamingFwVerifier, VVFW_FORMAT_VERSION,
    compute_auth_mac, decode_hex_32, encode_hex,
};

fn read_urandom(buf: &mut [u8]) -> Result<(), String> {
    let mut f = std::fs::File::open("/dev/urandom").map_err(|e| format!("open /dev/urandom: {e}"))?;
    f.read_exact(buf).map_err(|e| format!("read /dev/urandom: {e}"))
}

fn pdf_string_to_utf8(obj: &lopdf::Object) -> Option<String> {
    let bytes = match obj {
        lopdf::Object::String(b, _) => b,
        _ => return None,
    };
    if bytes.starts_with(&[0xFE, 0xFF]) && bytes.len() >= 2 {
        let u16s: Vec<u16> = bytes[2..]
            .as_chunks::<2>().0.iter()
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&u16s).ok()
    } else if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8(bytes[3..].to_vec()).ok()
    } else if let Ok(s) = std::str::from_utf8(bytes) {
        Some(s.to_string())
    } else {
        Some(bytes.iter().map(|&b| b as char).collect())
    }
}

fn extract_pdf_info(doc: &lopdf::Document) -> (Option<String>, Option<String>) {
    let Ok(info_ref) = doc.trailer.get(b"Info").and_then(|o| o.as_reference()) else {
        return (None, None);
    };
    let Ok(info_dict) = doc.get_dictionary(info_ref) else {
        return (None, None);
    };
    let title = info_dict.get(b"Title").ok().and_then(pdf_string_to_utf8).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let author = info_dict.get(b"Author").ok().and_then(pdf_string_to_utf8).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    (title, author)
}

/// Convert `.fb2`, `.fb2.zip`, `.epub`, `.md`, `.html`, `.txt`, `.vvb`, OR `.pdf` bytes into `VvbBook`.
pub fn convert_bytes_to_vvb(filename: &str, raw: &[u8]) -> Result<VvbBook, String> {
    if filename.to_ascii_lowercase().ends_with(".pdf") {
        let doc = lopdf::Document::load_mem(raw).map_err(|e| format!("PDF parse failed: {e}"))?;
        let (pdf_title, pdf_author) = extract_pdf_info(&doc);
        let fallback_title = filename
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(filename)
            .strip_suffix(".pdf")
            .or_else(|| filename.strip_suffix(".PDF"))
            .unwrap_or(filename)
            .to_string();
        let title = pdf_title.unwrap_or(fallback_title);

        let pages = doc.get_pages();
        if pages.is_empty() {
            return Err("PDF contains 0 pages".to_string());
        }
        let mut md = format!("# {title}\n\n");
        let mut extracted_any = false;
        for &page_num in pages.keys() {
            let text = doc.extract_text(&[page_num]).unwrap_or_default();
            let cleaned = text.replace("\r\n", "\n").replace('\r', "\n");
            let trimmed = cleaned.trim();
            if !trimmed.is_empty() {
                extracted_any = true;
                if pages.len() > 1 {
                    md.push_str(&format!("## Page {page_num}\n\n"));
                }
                for para in trimmed.split("\n\n") {
                    let line = para.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !line.is_empty() {
                        md.push_str(&line);
                        md.push_str("\n\n");
                    }
                }
            }
        }
        if !extracted_any {
            md.push_str("(No extractable text layer found in PDF.)\n");
        }

        let mut book = ingest_document("converted.md", md.as_bytes())
            .map_err(|e| format!("VVB build error: {e:?}"))?;
        book.meta.source_format = "PDF".to_string();
        book.meta.title = title;
        if let Some(a) = pdf_author {
            book.meta.author = a;
        }
        Ok(book)
    } else {
        ingest_document(filename, raw).map_err(|e| format!("Ingest error: {e:?}"))
    }
}

pub fn generate_ed25519_keypair() -> Result<([u8; 64], [u8; 32]), String> {
    let mut seed_bytes = [0u8; 32];
    read_urandom(&mut seed_bytes)?;
    let kp = KeyPair::from_seed(Seed::new(seed_bytes));
    let mut sk = [0u8; 64];
    sk.copy_from_slice(kp.sk.as_ref());
    let mut pk = [0u8; 32];
    pk.copy_from_slice(kp.pk.as_ref());
    Ok((sk, pk))
}

pub fn sign_firmware_bytes(
    payload: &[u8],
    secret_key_bytes: &[u8; 64],
    fw_version: u32,
) -> Result<Vec<u8>, String> {
    let sk = SecretKey::from_slice(secret_key_bytes).map_err(|e| format!("bad secret key: {e}"))?;
    let digest = *blake3::hash(payload).as_bytes();
    let mut hdr = SignedFwHeader {
        format_ver: VVFW_FORMAT_VERSION,
        fw_version,
        target_hw: HW_TARGET_XTEINK_X3,
        reserved: 0,
        payload_len: payload.len() as u32,
        payload_blake3: digest,
        ed25519_sig: [0u8; 64],
    };
    let msg = SignedFwHeader::canonical_body(&hdr);
    let sig = sk.sign(msg, None);
    hdr.ed25519_sig.copy_from_slice(sig.as_ref());

    let mut out = Vec::with_capacity(SignedFwHeader::SIZE + payload.len());
    out.extend_from_slice(&hdr.to_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

pub fn verify_firmware_bytes(
    vvfw: &[u8],
    pub_key_bytes: &[u8; 32],
    min_fw_version: u32,
) -> Result<SignedFwHeader, String> {
    if vvfw.len() < SignedFwHeader::SIZE {
        return Err("file smaller than SignedFwHeader".to_string());
    }
    let mut verifier = StreamingFwVerifier::start(
        &vvfw[..SignedFwHeader::SIZE],
        pub_key_bytes,
        min_fw_version,
        HW_TARGET_XTEINK_X3,
    )
    .map_err(|e| format!("header reject: {e:?}"))?;
    for chunk in vvfw[SignedFwHeader::SIZE..].chunks(4096) {
        verifier.feed(chunk).map_err(|e| format!("stream error: {e:?}"))?;
    }
    verifier.finish().map_err(|e| format!("verify failed: {e:?}"))
}

fn parse_http_host_port(url: &str) -> Result<(String, &'static str), String> {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let (host_port, _) = rest.split_once('/').unwrap_or((rest, ""));
    let host_port = host_port.trim();
    if host_port.is_empty() {
        return Err("Empty HTTP host".to_string());
    }
    if host_port.contains(':') {
        Ok((host_port.to_string(), "/"))
    } else {
        Ok((format!("{host_port}:80"), "/"))
    }
}

fn http_request(
    host_port: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<(u16, Vec<u8>), String> {
    let mut stream = TcpStream::connect(host_port).map_err(|e| format!("connect {host_port}: {e}"))?;
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).map_err(|e| format!("write hdr: {e}"))?;
    if !body.is_empty() {
        stream.write_all(body).map_err(|e| format!("write body: {e}"))?;
    }
    stream.flush().map_err(|e| format!("flush: {e}"))?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| format!("read resp: {e}"))?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "malformed HTTP response".to_string())?;
    let hdr_str = std::str::from_utf8(&raw[..split]).map_err(|_| "non-utf8 headers".to_string())?;
    let status_line = hdr_str.lines().next().unwrap_or("");
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("bad status line: {status_line}"))?;
    Ok((code, raw[split + 4..].to_vec()))
}

/// Authenticate to a live VVDrop server with HMAC-BLAKE3 challenge-response and upload a document.
/// Automatically converts `.pdf` inputs to `.vvb` before sending.
fn authenticate_vvdrop_session(host_port: &str, pin: &str) -> Result<String, String> {
    let (nonce_code, nonce_body) = http_request(host_port, "GET", "/api/nonce", &[], &[])?;
    if nonce_code != 200 {
        return Err(format!("GET /api/nonce failed with HTTP {nonce_code}"));
    }
    let nonce_str = std::str::from_utf8(&nonce_body).unwrap_or("").trim();
    let server_nonce =
        decode_hex_32(nonce_str).ok_or_else(|| "invalid server nonce hex".to_string())?;

    let mut client_nonce = [0u8; 32];
    read_urandom(&mut client_nonce)?;
    let clean_pin: String = pin
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    let mac = compute_auth_mac(clean_pin.as_bytes(), &server_nonce, &client_nonce);

    let cn_hex = encode_hex(&client_nonce);
    let mac_hex = encode_hex(&mac);
    let (auth_code, auth_body) = http_request(
        host_port,
        "POST",
        "/api/auth",
        &[("X-VVDrop-Client-Nonce", &cn_hex), ("X-VVDrop-Mac", &mac_hex)],
        &[],
    )?;
    if auth_code != 200 {
        return Err(format!(
            "Authentication rejected (HTTP {auth_code}): {}",
            String::from_utf8_lossy(&auth_body)
        ));
    }
    Ok(std::str::from_utf8(&auth_body).unwrap_or("").trim().to_string())
}

pub fn send_file_over_http(
    base_url: &str,
    pin: &str,
    filename: &str,
    raw_bytes: &[u8],
) -> Result<(), String> {
    let (host_port, _) = parse_http_host_port(base_url)?;
    let token_hex = authenticate_vvdrop_session(&host_port, pin)?;

    let (upload_name, upload_bytes): (String, Vec<u8>) =
        if filename.to_ascii_lowercase().ends_with(".pdf") {
            let book = convert_bytes_to_vvb(filename, raw_bytes)?;
            let base = filename
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or("document")
                .trim_end_matches(".pdf")
                .trim_end_matches(".PDF");
            (format!("{base}.vvb"), book.to_bytes())
        } else {
            let clean = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
            (clean.to_string(), raw_bytes.to_vec())
        };

    let (up_code, up_body) = http_request(
        &host_port,
        "POST",
        "/api/upload",
        &[
            ("X-VVDrop-Token", &token_hex),
            ("X-VVDrop-Filename", &upload_name),
        ],
        &upload_bytes,
    )?;
    if up_code != 200 {
        return Err(format!(
            "Upload rejected (HTTP {up_code}): {}",
            String::from_utf8_lossy(&up_body)
        ));
    }
    Ok(())
}

/// Authenticate with a VVDrop portal via PIN challenge-response and upload a signed `.vvfw` OTA package.
pub fn send_ota_over_http(
    host_port_or_url: &str,
    pin: &str,
    signed_fw: &[u8],
) -> Result<String, String> {
    let host_port = if host_port_or_url.contains("://") {
        parse_http_host_port(host_port_or_url)?.0
    } else {
        host_port_or_url.to_string()
    };
    let token_hex = authenticate_vvdrop_session(&host_port, pin)?;
    let (ota_code, ota_body) = http_request(
        &host_port,
        "POST",
        "/api/ota",
        &[("X-VVDrop-Token", &token_hex)],
        signed_fw,
    )?;
    let msg = String::from_utf8_lossy(&ota_body).trim().to_string();
    if ota_code != 200 {
        return Err(format!("OTA rejected (HTTP {ota_code}): {msg}"));
    }
    Ok(msg)
}
