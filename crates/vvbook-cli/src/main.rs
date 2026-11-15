#![forbid(unsafe_code)]

use std::env;
use std::fs;
use std::process;
use vvbook_cli::{
    convert_bytes_to_vvb, generate_ed25519_keypair, send_file_over_http, send_ota_over_http,
    sign_firmware_bytes, verify_firmware_bytes,
};

fn usage() -> ! {
    eprintln!(
        "vvbook-cli — VVBook Folio OS document & security companion\n\
         Usage:\n\
         \tvvbook-cli convert <input> -o <output.vvb>\n\
         \tvvbook-cli send <input> --url <http://host:port> --pin <8digits>\n\
         \tvvbook-cli send-ota <signed.vvfw> --pin <8digits> [--host 192.168.4.1:80]\n\
         \tvvbook-cli keygen -o <key_prefix>\n\
         \tvvbook-cli sign-fw <fw.bin> -k <key.sec> -v <version> -o <out.vvfw>\n\
         \tvvbook-cli verify-fw <in.vvfw> -p <key.pub> [--min-version N]"
    );
    process::exit(2);
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    let Some(cmd) = args.get(1).map(String::as_str) else {
        usage();
    };
    match cmd {
        "convert" => {
            let input = args.get(2).ok_or("missing <input>")?;
            let out = args
                .windows(2)
                .find(|w| w[0] == "-o")
                .map(|w| w[1].clone())
                .ok_or("missing -o <output.vvb>")?;
            let raw = fs::read(input).map_err(|e| format!("read {input}: {e}"))?;
            let book = convert_bytes_to_vvb(input, &raw)?;
            let vvb = book.to_bytes();
            fs::write(&out, &vvb).map_err(|e| format!("write {out}: {e}"))?;
            println!(
                "Converted '{}' -> '{}' ({} blocks, {} words, {} bytes)",
                book.meta.title,
                out,
                book.blocks.len(),
                book.meta.word_count,
                vvb.len()
            );
        }
        "send" => {
            let input = args.get(2).ok_or("missing <input>")?;
            let url = args
                .windows(2)
                .find(|w| w[0] == "--url")
                .map(|w| w[1].clone())
                .unwrap_or_else(|| "http://127.0.0.1:8383".to_string());
            let pin = args
                .windows(2)
                .find(|w| w[0] == "--pin")
                .map(|w| w[1].clone())
                .ok_or("missing --pin <8digits>")?;
            let raw = fs::read(input).map_err(|e| format!("read {input}: {e}"))?;
            send_file_over_http(&url, &pin, input, &raw)?;
            println!("Delivered '{input}' to {url}");
        }
        "send-ota" => {
            let input = args.get(2).ok_or("missing <signed.vvfw>")?;
            let host = args
                .windows(2)
                .find(|w| w[0] == "--host")
                .map(|w| w[1].clone())
                .unwrap_or_else(|| "192.168.4.1:80".to_string());
            let pin = args
                .windows(2)
                .find(|w| w[0] == "--pin")
                .map(|w| w[1].clone())
                .ok_or("missing --pin <8digits>")?;
            let raw = fs::read(input).map_err(|e| format!("read {input}: {e}"))?;
            let msg = send_ota_over_http(&host, &pin, &raw)?;
            println!("OTA delivered to {host}: {msg}");
        }
        "keygen" => {
            let prefix = args
                .windows(2)
                .find(|w| w[0] == "-o")
                .map(|w| w[1].clone())
                .ok_or("missing -o <key_prefix>")?;
            let (sk, pk) = generate_ed25519_keypair()?;
            let sec_path = format!("{prefix}.sec");
            let pub_path = format!("{prefix}.pub");
            fs::write(&sec_path, sk).map_err(|e| format!("write {sec_path}: {e}"))?;
            fs::write(&pub_path, pk).map_err(|e| format!("write {pub_path}: {e}"))?;
            println!("Generated Ed25519 keypair: {sec_path} (64B), {pub_path} (32B)");
        }
        "sign-fw" => {
            let input = args.get(2).ok_or("missing <fw.bin>")?;
            let key_path = args
                .windows(2)
                .find(|w| w[0] == "-k")
                .map(|w| w[1].clone())
                .ok_or("missing -k <key.sec>")?;
            let ver: u32 = args
                .windows(2)
                .find(|w| w[0] == "-v")
                .and_then(|w| w[1].parse().ok())
                .ok_or("missing -v <version>")?;
            let out = args
                .windows(2)
                .find(|w| w[0] == "-o")
                .map(|w| w[1].clone())
                .ok_or("missing -o <out.vvfw>")?;
            let payload = fs::read(input).map_err(|e| format!("read {input}: {e}"))?;
            let sk_raw = fs::read(&key_path).map_err(|e| format!("read {key_path}: {e}"))?;
            let sk: [u8; 64] = sk_raw
                .try_into()
                .map_err(|_| "secret key must be 64 bytes".to_string())?;
            let signed = sign_firmware_bytes(&payload, &sk, ver)?;
            fs::write(&out, &signed).map_err(|e| format!("write {out}: {e}"))?;
            println!("Signed firmware v{ver} ({} bytes payload) -> {out}", payload.len());
        }
        "verify-fw" => {
            let input = args.get(2).ok_or("missing <in.vvfw>")?;
            let pub_path = args
                .windows(2)
                .find(|w| w[0] == "-p")
                .map(|w| w[1].clone())
                .ok_or("missing -p <key.pub>")?;
            let min_ver: u32 = args
                .windows(2)
                .find(|w| w[0] == "--min-version")
                .and_then(|w| w[1].parse().ok())
                .unwrap_or(1);
            let raw = fs::read(input).map_err(|e| format!("read {input}: {e}"))?;
            let pk_raw = fs::read(&pub_path).map_err(|e| format!("read {pub_path}: {e}"))?;
            let pk: [u8; 32] = pk_raw
                .try_into()
                .map_err(|_| "public key must be 32 bytes".to_string())?;
            let hdr = verify_firmware_bytes(&raw, &pk, min_ver)?;
            println!(
                "Verified OK: v{} target=0x{:04X} payload={} bytes",
                hdr.fw_version, hdr.target_hw, hdr.payload_len
            );
        }
        _ => usage(),
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}
