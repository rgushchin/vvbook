#![forbid(unsafe_code)]

use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, Stream, dictionary};
use std::io::{Read, Write};
use std::net::TcpStream;
use vvbook_cli::{
    convert_bytes_to_vvb, generate_ed25519_keypair, send_file_over_http, sign_firmware_bytes,
};
use vvbook_emu::{
    EmuDisplaySink, EmuPower, EmuSensors, FsStorage, HostEntropy, LivePortalServer,
    make_kashtanka_cp1251, make_pushkin_fb2_zip,
};
use vvbook_pal::{Button, InputEvent, StorageBackend};
use vvbook_typeset::{PageItem, ReaderTypography, layout_page_with};
use vvbook_userspace::{FolioApp, Screen};

fn build_real_pdf_bytes(title: &str, pages_text: &[&str]) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });

    let mut kids = Vec::new();
    for text in pages_text {
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 14.into()]),
                Operation::new("Td", vec![72.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal(*text)]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => content_id,
        });
        kids.push(page_id.into());
    }

    let pages = dictionary! {
        "Type" => "Pages",
        "Kids" => kids,
        "Count" => pages_text.len() as i64,
    };
    doc.objects.insert(pages_id, Object::Dictionary(pages));

    let info_id = doc.add_object(dictionary! {
        "Title" => Object::string_literal(title),
        "Author" => Object::string_literal("Jony Ives"),
    });
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    doc.trailer.set("Info", info_id);

    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    out
}

#[test]
fn e2e_pdf_generation_cli_conversion_and_reader_layout() {
    let pdf = build_real_pdf_bytes(
        "Cupertino Typography Notes",
        &[
            "Minimalism is not the lack of something; it is simply the perfect amount of something.",
            "Second page of typography craft and optical margins.",
        ],
    );

    let book = convert_bytes_to_vvb("cupertino_notes.pdf", &pdf).unwrap();
    assert_eq!(book.meta.source_format, "PDF");
    assert_eq!(book.meta.title, "Cupertino Typography Notes");
    assert_eq!(book.meta.author, "Jony Ives");
    let vvb_bytes = book.to_bytes();

    // Ingest converted .vvb directly into FolioApp on FsStorage
    let tmp = std::env::temp_dir().join("vvbook_e2e_pdf_sd");
    let _ = std::fs::remove_dir_all(&tmp);
    let mut storage = FsStorage::new(&tmp).unwrap();
    storage.mkdir_all("/books").unwrap();
    storage.write_all("/books/cupertino_notes.vvb", &vvb_bytes).unwrap();

    let mut sink = EmuDisplaySink::new();
    let mut sensors = EmuSensors::default();
    let mut power = EmuPower::default();
    let mut portal = LivePortalServer::bind("127.0.0.1:0", [0u8; 32], 1).unwrap();
    let mut ent = HostEntropy(42);
    let mut app = FolioApp::new();
    app.boot(&mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
    assert_eq!(app.catalog.len(), 1);
    assert!(app.open_active_book(&mut storage, &mut power));

    let anchor = app.active_pages[0];
    let page0 = layout_page_with(book.blocks.len(), anchor, &ReaderTypography::default(), |i| {
        book.blocks.get(i).cloned()
    });
    let mut flat = String::new();
    for it in &page0.items {
        match it {
            PageItem::DropCap { glyph, .. } => flat.push(*glyph),
            PageItem::TextLine { spans, .. } => {
                for sp in spans {
                    flat.push_str(&sp.text);
                    flat.push(' ');
                }
            }
            _ => {}
        }
    }
    assert!(
        flat.contains("Minimalism") && flat.contains("amount"),
        "Expected extracted PDF text in reader layout, got: {flat}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn e2e_live_tcp_vvdrop_lockout_and_multiformat_send() {
    let tmp = std::env::temp_dir().join("vvbook_e2e_vvdrop_sd");
    let _ = std::fs::remove_dir_all(&tmp);
    let mut storage = FsStorage::new(&tmp).unwrap();
    let mut sink = EmuDisplaySink::new();
    let mut sensors = EmuSensors::default();
    let mut power = EmuPower::default();
    let mut portal = LivePortalServer::bind("127.0.0.1:0", [0u8; 32], 1).unwrap();
    let mut ent = HostEntropy(1337);

    let mut app = FolioApp::new();
    app.boot(&mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);

    // Open VVDrop via Home ledger
    app.home_cursor = 4; // VVDrop
    app.handle_event(
        InputEvent::Press(Button::Confirm),
        &mut sink,
        &mut storage,
        &mut sensors,
        &mut power,
        &mut portal,
        &mut ent,
    );
    assert_eq!(app.screen, Screen::VvDrop);

    let url = portal.base_url();
    let initial_pin = app.vvdrop_auth.as_ref().unwrap().pin_str().to_string();
    let wrong_pin = if initial_pin == "00000000" { "11111111" } else { "00000000" };

    // 5 failed attempts must lock out VVDrop auth
    for attempt in 0..5 {
        let err = send_file_over_http(&url, wrong_pin, "bad.txt", b"nope").unwrap_err();
        if attempt == 4 {
            assert!(err.contains("423"), "5th wrong attempt should return 423 Locked, got: {err}");
        }
    }

    // Press Confirm on VVDrop to rotate PIN -> resets lockout cleanly!
    app.handle_event(
        InputEvent::Press(Button::Confirm),
        &mut sink,
        &mut storage,
        &mut sensors,
        &mut power,
        &mut portal,
        &mut ent,
    );
    let active_pin = app.vvdrop_auth.as_ref().unwrap().pin_str().to_string();

    // Send CP1251 Russian TXT + Pushkin FB2.ZIP over real loopback TCP
    send_file_over_http(&url, &active_pin, "chekhov_kashtanka.txt", &make_kashtanka_cp1251()).unwrap();
    app.tick(&mut sink, &mut storage, &mut sensors, &mut power, &mut portal);
    send_file_over_http(&url, &active_pin, "pushkin_saltan.fb2.zip", &make_pushkin_fb2_zip()).unwrap();
    app.tick(&mut sink, &mut storage, &mut sensors, &mut power, &mut portal);

    assert_eq!(app.catalog.len(), 2);
    assert!(app.catalog.iter().any(|c| c.title.contains("Салтан")));

    // Exit VVDrop and confirm Wi-Fi shuts down (503 on GET /)
    app.handle_event(
        InputEvent::Press(Button::Back),
        &mut sink,
        &mut storage,
        &mut sensors,
        &mut power,
        &mut portal,
        &mut ent,
    );
    let err = send_file_over_http(&url, &active_pin, "after.txt", b"closed").unwrap_err();
    assert!(err.contains("503"));
    let _ = std::fs::remove_dir_all(&tmp);
}

fn post_ota_http(host_port: &str, body: &[u8]) -> u16 {
    let mut stream = TcpStream::connect(host_port).unwrap();
    let req = format!(
        "POST /api/ota HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).unwrap();
    let text = String::from_utf8_lossy(&resp);
    text.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[test]
fn e2e_ed25519_signed_ota_accept_tamper_and_rollback() {
    let (sk, pk) = generate_ed25519_keypair().unwrap();
    let portal = LivePortalServer::bind("127.0.0.1:0", pk, 2).unwrap();
    let host_port = portal.addr().to_string();

    let payload_v2 = b"ESP32_C3_VVBOOK_FIRMWARE_IMAGE_V2_CLEAN_BYTES";
    let vvfw_v2 = sign_firmware_bytes(payload_v2, &sk, 2).unwrap();

    // Valid v2 accepted -> 200 OK
    assert_eq!(post_ota_http(&host_port, &vvfw_v2), 200);
    let staged = portal.staged_ota().expect("v2 staged");
    assert_eq!(staged.0, 2);
    assert_eq!(staged.1.as_slice(), payload_v2);

    // Flip 1 bit in signed payload -> rejected!
    let mut tampered = vvfw_v2.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    assert!(post_ota_http(&host_port, &tampered) >= 400);

    // Anti-rollback: valid signature for older v1 -> rejected!
    let vvfw_v1 = sign_firmware_bytes(b"OLD_V1_BINARY", &sk, 1).unwrap();
    assert!(post_ota_http(&host_port, &vvfw_v1) >= 400);
}
