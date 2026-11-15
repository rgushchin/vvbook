#![forbid(unsafe_code)]

pub mod x3_twin;

use miniz_oxide::deflate::compress_to_vec;
use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use vvbook_games::chess::ChessMode;
use vvbook_games::pocket_pip::PipTab;
use vvbook_pal::{
    BatteryStatus, Button, DateTime, DeviceSensors, DirEntry, DisplaySink, EntropySource,
    InputEvent, NetworkPortal, PORTRAIT_H, PORTRAIT_W, PortalStatus, PowerControl, ROW_2BPP_BYTES,
    RefreshMode, StagedOta, StorageBackend, StorageError, Tone,
};
use vvbook_security::{
    Blake3Drbg, FOLIO_RELEASE_PUBKEY, HW_TARGET_XTEINK_X3, MAX_ESP32C3_OTA_BYTES, SignedFwHeader,
    StreamingFwVerifier, VvDropAuth, compute_auth_mac, decode_hex_32, encode_hex,
};
use vvbook_userspace::{FolioApp, ReaderOverlay, SleepPosterKind, UiLang};

const VVDROP_WEB_HTML: &str = include_str!("../assets/vvdrop_web.html");

/// Build a valid compressed ZIP archive in memory using `miniz_oxide`, preserving raw entry name bytes.
pub fn build_zip_bytes_raw(files: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cd = Vec::new();
    for (name, payload) in files {
        let lfh_offset = out.len() as u32;
        let comp = compress_to_vec(payload, 6);
        let crc = crc32_ieee(payload);
        out.extend_from_slice(&0x04034B50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&8u16.to_le_bytes()); // Deflate
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&comp);

        cd.extend_from_slice(&0x02014B50u32.to_le_bytes());
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&8u16.to_le_bytes());
        cd.extend_from_slice(&0u32.to_le_bytes());
        cd.extend_from_slice(&crc.to_le_bytes());
        cd.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u32.to_le_bytes());
        cd.extend_from_slice(&lfh_offset.to_le_bytes());
        cd.extend_from_slice(name);
    }
    let cd_offset = out.len() as u32;
    let cd_size = cd.len() as u32;
    out.extend_from_slice(&cd);
    out.extend_from_slice(&0x06054B50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// Build a valid compressed ZIP archive in memory using `miniz_oxide`.
pub fn build_zip_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
    let raw: Vec<(&[u8], &[u8])> = files
        .iter()
        .map(|(n, p)| (n.as_bytes(), *p))
        .collect();
    build_zip_bytes_raw(&raw)
}

fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Encode UTF-8 Russian/English text to genuine Windows-1251 bytes.
pub fn encode_cp1251(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for ch in s.chars() {
        let b = match ch {
            '\u{0000}'..='\u{007F}' => ch as u8,
            '\u{0410}'..='\u{042F}' => 0xC0 + (ch as u32 - 0x0410) as u8,
            '\u{0430}'..='\u{044F}' => 0xE0 + (ch as u32 - 0x0430) as u8,
            'Ё' => 0xA8,
            'ё' => 0xB8,
            '«' => 0xAB,
            '»' => 0xBB,
            '—' => 0x97,
            '–' => 0x96,
            '№' => 0xB9,
            '…' => 0x85,
            '\u{00A0}' => 0xA0,
            _ => b'?',
        };
        out.push(b);
    }
    out
}

pub fn make_pushkin_fb2_zip() -> Vec<u8> {
    let fb2 = r#"<?xml version="1.0" encoding="utf-8"?>
<FictionBook>
  <description>
    <title-info>
      <author><first-name>Александр</first-name><last-name>Пушкин</last-name></author>
      <book-title>Сказка о царе Салтане</book-title>
      <lang>ru</lang>
    </title-info>
  </description>
  <body>
    <section>
      <title><p>Глава I. Три девицы под окном</p></title>
      <epigraph><p>Ветер по морю гуляет и кораблик подгоняет...</p></epigraph>
      <poem><stanza>
        <v>Три девицы под окном</v>
        <v>Пряли поздно вечерком.</v>
        <v>«Кабы я была царица, —</v>
        <v>Говорит одна девица, —</v>
        <v>То на весь крещёный мир</v>
        <v>Приготовила б я пир».</v>
      </stanza></poem>
      <p>В тот же час у ворот старого терема царь Салтан слушал тихую беседу и выбирал себе невесту. Высокий терем светился сквозь сумерки зимнего сада, а над широкой Невой поднималась серебристая луна.</p>
      <p>«Здравствуй, красная девица, — говорит он, — будь царица и роди богатыря мне к исходу сентября». Все три сестры поклонились государю и отправились во дворец готовиться к великому торжеству.</p>
    </section>
    <section>
      <title><p>Глава II. Остров Буян</p></title>
      <poem><stanza>
        <v>Ветер весело шумит,</v>
        <v>Судно весело бежит</v>
        <v>Мимо острова Буяна,</v>
        <v>К царству славного Салтана.</v>
      </stanza></poem>
      <p>Море вздулось бурливо, закипело, подымая волну за волной; но чудесный корабль плыл легко и уверенно, точно по страницам хорошо свёрстанной книги.</p>
    </section>
  </body>
</FictionBook>"#;
    build_zip_bytes(&[("pushkin_saltan.fb2", fb2.as_bytes())])
}

pub fn make_kashtanka_cp1251() -> Vec<u8> {
    let text = "Антон Павлович Чехов\nКаштанка\n\n\
        Глава 1. Дурное поведение\n\n\
        Молодая рыжая собака — помесь таксы с дворняжкой — очень похожа мордой на лисицу, бегала взад и вперёд по тротуару и беспокойно оглядывалась по сторонам. Изредка она останавливалась и, плача, приподнимая то одну озябшую лапу, то другую, старалась дать себе отчёт: как это могло случиться, что она заблудилась?\n\n\
        Она отлично помнила, как она провела день и как в конце концов попала на этот незнакомый тротуар. День начался с того, что её хозяин, столяр Лука Александрыч, надел шапку, взял под мышку какую-то деревянную вещь, завёрнутую в красный платок, и крикнул: «Каштанка, пойдём!»\n\n\
        Услыхав своё имя, помесь таксы с дворняжкой вышла из-под верстака, где она спала на стружках, сладко потянулась и побежала за хозяином. Стоял холодный, ветреный день; с неба падал мелкий, сухой снег, и на улицах горели тусклые фонари.\n";
    encode_cp1251(text)
}

pub fn make_alice_epub() -> Vec<u8> {
    let container = br#"<?xml version="1.0" encoding="utf-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
    let opf = br#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Alice's Adventures in Wonderland</dc:title>
    <dc:creator>Lewis Carroll</dc:creator>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="ch2.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="ch1"/>
    <itemref idref="ch2"/>
  </spine>
</package>"#;
    let ch1 = br#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<body>
  <h1>I. Down the Rabbit-Hole</h1>
  <p>Alice was beginning to get very <em>tired</em> of sitting by her sister on the bank, and of having nothing to do: once or twice she had peeped into the book her sister was reading, but it had no pictures or conversations in it, "and what is the use of a book," thought Alice, "without pictures or conversation?"</p>
  <p>So she was considering in her own mind, whether the pleasure of making a daisy-chain would be worth the trouble of getting up and picking the daisies, when suddenly a White Rabbit with pink eyes ran close by her.</p>
  <p>There was nothing so <strong>very</strong> remarkable in that; nor did Alice think it so very much out of the way to hear the Rabbit say to itself, "Oh dear! Oh dear! I shall be late!" But when the Rabbit actually took a watch out of its waistcoat-pocket, Alice started to her feet and ran across the field after it.</p>
</body>
</html>"#;
    let ch2 = br#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<body>
  <h1>II. The Pool of Tears</h1>
  <p>"Curiouser and curiouser!" cried Alice. "Now I am opening out like the largest telescope that ever was! Good-bye, feet!" Just at this moment her head struck against the roof of the hall: in fact she was now rather more than nine feet high, and she at once took up the little golden key and hurried off to the garden door.</p>
</body>
</html>"#;
    build_zip_bytes(&[
        ("META-INF/container.xml", container),
        ("OEBPS/content.opf", opf),
        ("OEBPS/ch1.xhtml", ch1),
        ("OEBPS/ch2.xhtml", ch2),
    ])
}

pub fn make_bilingual_guide_md() -> String {
    "# VVBook Folio OS · User Guide\n\n\
    Folio OS transforms the Xteink X3 E-Ink reader into a serene, high-craft digital book crafted with Cupertino minimalism and pure bare-metal Rust.\n\n\
    ## Беспроводная передача VVDrop\n\n\
    Откройте экран VVDrop, отсканируйте QR-код с телефона или компьютера и введите 6-значный PIN. Все книги в форматах FB2, FB2.ZIP, EPUB, Markdown, HTML, TXT и PDF мгновенно попадают на карту памяти.\n\n\
    ## Zero-Drain Battery Architecture\n\n\
    A 16-to-32 block SRAM prefetch window powers down the microSD rail (`GPIO 13`) immediately after loading upcoming pages, while high-voltage panel rails sleep after 2.5 seconds.\n"
        .to_string()
}

/// Populate `sample_library/` (or any `/books/` directory) with multi-format EN/RU books.
pub fn seed_sample_library(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join("pushkin_saltan.fb2.zip"), make_pushkin_fb2_zip())?;
    fs::write(dir.join("chekhov_kashtanka_cp1251.txt"), make_kashtanka_cp1251())?;
    fs::write(dir.join("alice_wonderland.epub"), make_alice_epub())?;
    fs::write(
        dir.join("pocket_typography_bilingual.md"),
        make_bilingual_guide_md().as_bytes(),
    )?;
    Ok(())
}

/// Host filesystem-backed `StorageBackend` representing the Xteink X3 microSD card.
pub struct FsStorage {
    root: PathBuf,
}

impl FsStorage {
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn seed_books_if_empty(&self, sample_dir: &Path) -> std::io::Result<()> {
        let books_dir = self.root.join("books");
        fs::create_dir_all(&books_dir)?;
        let empty = fs::read_dir(&books_dir)?.next().is_none();
        if empty {
            seed_sample_library(&books_dir)?;
            let _ = sample_dir;
        }
        Ok(())
    }

    fn resolve(&self, vpath: &str) -> Result<PathBuf, StorageError> {
        let rel = vpath.trim_start_matches('/');
        let mut out = self.root.clone();
        for comp in Path::new(rel).components() {
            match comp {
                Component::Normal(s) => out.push(s),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(StorageError::InvalidPath);
                }
            }
        }
        Ok(out)
    }
}

impl StorageBackend for FsStorage {
    fn exists(&self, path: &str) -> bool {
        self.resolve(path).map(|p| p.exists()).unwrap_or(false)
    }

    fn file_len(&self, path: &str) -> Result<u64, StorageError> {
        let p = self.resolve(path)?;
        fs::metadata(p).map(|m| m.len()).map_err(|_| StorageError::NotFound)
    }

    fn read_all(&self, path: &str) -> Result<Vec<u8>, StorageError> {
        let p = self.resolve(path)?;
        fs::read(p).map_err(|_| StorageError::NotFound)
    }

    fn read_range(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
        use std::io::{Seek, SeekFrom};
        let p = self.resolve(path)?;
        let mut f = fs::File::open(p).map_err(|_| StorageError::NotFound)?;
        f.seek(SeekFrom::Start(offset)).map_err(|_| StorageError::Io)?;
        f.read(buf).map_err(|_| StorageError::Io)
    }

    fn write_all(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError> {
        let p = self.resolve(path)?;
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).map_err(|_| StorageError::Io)?;
        }
        fs::write(p, data).map_err(|_| StorageError::Io)
    }

    fn write_atomic(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError> {
        let p = self.resolve(path)?;
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).map_err(|_| StorageError::Io)?;
        }
        let tmp = p.with_extension("tmp");
        fs::write(&tmp, data).map_err(|_| StorageError::Io)?;
        fs::rename(&tmp, &p).map_err(|_| StorageError::Io)
    }

    fn append(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError> {
        let p = self.resolve(path)?;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .map_err(|_| StorageError::Io)?;
        f.write_all(data).map_err(|_| StorageError::Io)
    }

    fn remove(&mut self, path: &str) -> Result<(), StorageError> {
        let p = self.resolve(path)?;
        if fs::metadata(&p).map(|m| m.is_dir()).unwrap_or(false) {
            return Err(StorageError::InvalidPath);
        }
        fs::remove_file(p).map_err(|_| StorageError::NotFound)
    }

    fn mkdir_all(&mut self, path: &str) -> Result<(), StorageError> {
        let p = self.resolve(path)?;
        fs::create_dir_all(p).map_err(|_| StorageError::Io)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError> {
        let p = self.resolve(path)?;
        let mut out = Vec::new();
        for entry in fs::read_dir(p).map_err(|_| StorageError::NotFound)? {
            let entry = entry.map_err(|_| StorageError::Io)?;
            let meta = entry.metadata().map_err(|_| StorageError::Io)?;
            out.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                size: meta.len(),
                is_dir: meta.is_dir(),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}

/// Warm E-Ink Carta 1200 palette matching Apple-paper hardware warmth.
#[inline]
pub fn tone_to_rgb(t: Tone) -> [u8; 3] {
    match t {
        Tone::Ink => [0x18, 0x18, 0x16],
        Tone::Charcoal => [0x5C, 0x5A, 0x55],
        Tone::Silver => [0xB8, 0xB5, 0xAD],
        Tone::Paper => [0xF6, 0xF4, 0xEE],
    }
}

#[derive(Clone)]
pub struct EmuDisplaySink {
    pub fb: Vec<u8>,
    pub last_mode: RefreshMode,
    pub commits: u32,
}

impl Default for EmuDisplaySink {
    fn default() -> Self {
        Self::new()
    }
}

impl EmuDisplaySink {
    pub fn new() -> Self {
        Self {
            fb: vec![0xFFu8; PORTRAIT_H * ROW_2BPP_BYTES],
            last_mode: RefreshMode::FullGc,
            commits: 0,
        }
    }

    #[inline]
    fn effective_tone(&self, tone: Tone) -> Tone {
        if self.last_mode != RefreshMode::Grayscale4 {
            if tone.is_dark() { Tone::Ink } else { Tone::Paper }
        } else {
            tone
        }
    }

    pub fn to_rgb_bytes(&self) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(PORTRAIT_W * PORTRAIT_H * 3);
        for y in 0..PORTRAIT_H {
            for xb in 0..ROW_2BPP_BYTES {
                let byte = self.fb[y * ROW_2BPP_BYTES + xb];
                for shift in [6u8, 4, 2, 0] {
                    let tone = self.effective_tone(Tone::from_bits((byte >> shift) & 0x03));
                    rgb.extend_from_slice(&tone_to_rgb(tone));
                }
            }
        }
        rgb
    }

    pub fn to_argb_u32(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(PORTRAIT_W * PORTRAIT_H);
        for y in 0..PORTRAIT_H {
            for xb in 0..ROW_2BPP_BYTES {
                let byte = self.fb[y * ROW_2BPP_BYTES + xb];
                for shift in [6u8, 4, 2, 0] {
                    let tone = self.effective_tone(Tone::from_bits((byte >> shift) & 0x03));
                    let [r, g, b] = tone_to_rgb(tone);
                    out.push(((r as u32) << 16) | ((g as u32) << 8) | (b as u32));
                }
            }
        }
        out
    }

    pub fn encode_png(&self) -> Vec<u8> {
        let rgb = self.to_rgb_bytes();
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, PORTRAIT_W as u32, PORTRAIT_H as u32);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().expect("png header");
            writer.write_image_data(&rgb).expect("png data");
        }
        out
    }

    pub fn save_png(&self, path: &Path) -> std::io::Result<()> {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(path, self.encode_png())
    }
}

impl DisplaySink for EmuDisplaySink {
    fn begin_frame(&mut self, _mode: RefreshMode) {}

    fn write_strip_2bpp(&mut self, y0: usize, rows: usize, packed: &[u8]) {
        let start = y0 * ROW_2BPP_BYTES;
        let len = rows * ROW_2BPP_BYTES;
        if start + len <= self.fb.len() {
            self.fb[start..start + len].copy_from_slice(&packed[..len]);
        }
    }

    fn end_frame(&mut self, mode: RefreshMode) {
        self.last_mode = mode;
        self.commits += 1;
    }
}

pub struct HostEntropy(Blake3Drbg);

impl HostEntropy {
    pub fn from_system() -> Self {
        let mut seed = [0u8; 32];
        let mut ok = false;
        if let Ok(mut f) = fs::File::open("/dev/urandom") {
            ok = f.read_exact(&mut seed).is_ok();
        }
        let mut drbg = Blake3Drbg::from_seed(seed);
        if !ok {
            use std::hash::BuildHasher;
            for salt in 0u64..4 {
                let rs = std::collections::hash_map::RandomState::new();
                drbg.reseed(&rs.hash_one(salt).to_le_bytes());
            }
            if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                drbg.reseed(&d.as_nanos().to_le_bytes());
            }
        }
        Self(drbg)
    }

    pub fn from_seed_u64(seed: u64) -> Self {
        Self(Blake3Drbg::from_u64(seed))
    }
}

impl EntropySource for HostEntropy {
    fn fill_bytes(&mut self, out: &mut [u8]) {
        self.0.fill_bytes(out);
    }
}

pub struct EmuSensors {
    pub start: Instant,
    pub battery: BatteryStatus,
    pub clock: DateTime,
}

impl Default for EmuSensors {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            battery: BatteryStatus {
                percent: 88,
                millivolts: 4120,
                current_ma: -24,
                charging: false,
            },
            clock: DateTime {
                year: 2026,
                month: 11,
                day: 12,
                hour: 21,
                minute: 15,
                second: 0,
            },
        }
    }
}

impl DeviceSensors for EmuSensors {
    fn battery(&mut self) -> BatteryStatus {
        self.battery
    }
    fn now(&mut self) -> DateTime {
        self.clock
    }
    fn uptime_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
    fn temperature_c_x10(&mut self) -> i16 {
        235
    }
}

pub struct EmuPower {
    pub deep_sleeps: u32,
    pub imu_flick: bool,
    pub rotated: bool,
    pub hv_standby: bool,
    pub sd_save: bool,
}

impl Default for EmuPower {
    fn default() -> Self {
        Self {
            deep_sleeps: 0,
            imu_flick: false,
            rotated: false,
            hv_standby: false,
            sd_save: true,
        }
    }
}

impl PowerControl for EmuPower {
    fn enter_deep_sleep(&mut self) {
        self.deep_sleeps += 1;
    }
    fn set_imu_flick(&mut self, enabled: bool) {
        self.imu_flick = enabled;
    }
    fn set_rotated_180(&mut self, rotated: bool) {
        self.rotated = rotated;
    }
    fn controller_name(&self) -> &'static str {
        "UC8253-Emu"
    }
    fn set_panel_hv_standby(&mut self, standby: bool) {
        self.hv_standby = standby;
    }
    fn set_sd_power_save(&mut self, power_save: bool) {
        self.sd_save = power_save;
    }
}

struct PortalState {
    active: bool,
    pin: String,
    auth: VvDropAuth,
    rng: Blake3Drbg,
    rx_bytes: u64,
    last_received: Option<String>,
    pending_uploads: VecDeque<(String, Vec<u8>)>,
    ota_pub_key: [u8; 32],
    min_fw_version: u32,
    staged_ota: Option<(u32, Vec<u8>)>,
    staged_meta: Option<StagedOta>,
}

/// Live local TCP HTTP portal backing `VVDrop` wireless drops and Ed25519 `/api/ota` updates.
pub struct LivePortalServer {
    addr: SocketAddr,
    state: Arc<Mutex<PortalState>>,
    stop_flag: Arc<AtomicBool>,
}

impl LivePortalServer {
    pub fn bind(bind_addr: &str, ota_pub_key: [u8; 32], min_fw_version: u32) -> std::io::Result<Self> {
        let listener = TcpListener::bind(bind_addr)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let mut rng = HostEntropy::from_system().0;
        let auth = VvDropAuth::from_fixed_pin(*b"42819305", &mut rng);
        let state = Arc::new(Mutex::new(PortalState {
            active: false,
            pin: "42819305".to_string(),
            auth,
            rng,
            rx_bytes: 0,
            last_received: None,
            pending_uploads: VecDeque::new(),
            ota_pub_key,
            min_fw_version,
            staged_ota: None,
            staged_meta: None,
        }));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let st_bg = Arc::clone(&state);
        let stop_bg = Arc::clone(&stop_flag);
        std::thread::spawn(move || {
            while !stop_bg.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = handle_portal_http(&mut stream, &st_bg);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            addr,
            state,
            stop_flag,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn staged_ota(&self) -> Option<(u32, Vec<u8>)> {
        self.state.lock().unwrap().staged_ota.clone()
    }
}

impl Drop for LivePortalServer {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }
}

impl NetworkPortal for LivePortalServer {
    fn start(&mut self, pin: &str) {
        let mut st = self.state.lock().unwrap();
        st.active = true;
        st.pin = pin.to_string();
        let mut pin_bytes = [b'0'; 8];
        for (i, b) in pin.as_bytes().iter().take(8).enumerate() {
            pin_bytes[i] = *b;
        }
        st.rng.reseed(&pin_bytes);
        let fresh = HostEntropy::from_system();
        let mut extra = [0u8; 16];
        let mut f = fresh;
        f.fill_bytes(&mut extra);
        st.rng.reseed(&extra);
        let mut rng = st.rng.clone();
        st.auth = VvDropAuth::from_fixed_pin(pin_bytes, &mut rng);
        st.rng = rng;
    }

    fn stop(&mut self) {
        let mut st = self.state.lock().unwrap();
        st.active = false;
    }

    fn status(&mut self) -> PortalStatus {
        let st = self.state.lock().unwrap();
        PortalStatus {
            active: st.active,
            ssid: "VVBook-Folio".to_string(),
            url: self.base_url(),
            pin: st.pin.clone(),
            clients_connected: u8::from(st.active),
            rx_bytes: st.rx_bytes,
            total_bytes: st.rx_bytes,
            last_received_file: st.last_received.clone(),
            busy_message: None,
        }
    }

    fn take_pending_ingest(&mut self) -> Option<(String, Vec<u8>)> {
        self.state.lock().unwrap().pending_uploads.pop_front()
    }

    fn take_staged_ota(&mut self) -> Option<StagedOta> {
        self.state.lock().unwrap().staged_meta.take()
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len()
            && let Ok(val) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) {
                out.push(val);
                i += 3;
                continue;
            }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn send_http_resp(stream: &mut TcpStream, code: u16, reason: &str, ctype: &str, body: &[u8]) {
    let hdr = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(hdr.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

type ParsedHttpRequest = (String, String, Vec<(String, String)>, Vec<u8>);

fn read_http_request(stream: &mut TcpStream) -> Option<ParsedHttpRequest> {
    stream.set_nonblocking(false).ok();
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).ok();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let hdr_end;
    loop {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            hdr_end = pos;
            break;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    }
    let hdr_text = String::from_utf8_lossy(&buf[..hdr_end]).into_owned();
    let mut lines = hdr_text.lines();
    let req_line = lines.next()?;
    let mut parts = req_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut headers = Vec::new();
    let mut content_len = 0usize;
    for ln in lines {
        if let Some((k, v)) = ln.split_once(':') {
            let kl = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            if kl == "content-length" {
                content_len = val.parse().unwrap_or(0);
            }
            headers.push((kl, val));
        }
    }
    if content_len > 32 * 1024 * 1024 {
        return None;
    }
    let mut body = buf[hdr_end + 4..].to_vec();
    while body.len() < content_len {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_len);
    Some((method, path, headers, body))
}

fn hdr_val<'a>(headers: &'a [(String, String)], key: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn handle_portal_http(stream: &mut TcpStream, state: &Arc<Mutex<PortalState>>) -> Option<()> {
    let (method, path, headers, body) = read_http_request(stream)?;

    let mut st = state.lock().unwrap();
    if !st.active {
        send_http_resp(stream, 503, "Service Unavailable", "text/plain", b"VVDrop closed on device");
        return Some(());
    }

    match (method.as_str(), path.as_str()) {
        ("POST", "/api/ota") => {
            let Some(tok_hex) = hdr_val(&headers, "x-vvdrop-token") else {
                send_http_resp(stream, 401, "Unauthorized", "text/plain", b"missing token");
                return Some(());
            };
            let Some(tok) = decode_hex_32(tok_hex) else {
                send_http_resp(stream, 401, "Unauthorized", "text/plain", b"bad token");
                return Some(());
            };
            if st.auth.verify_token(&tok).is_err() {
                send_http_resp(stream, 401, "Unauthorized", "text/plain", b"invalid token");
                return Some(());
            }
            let max_ota = SignedFwHeader::SIZE + MAX_ESP32C3_OTA_BYTES as usize;
            if body.len() < SignedFwHeader::SIZE || body.len() > max_ota {
                send_http_resp(stream, 400, "Bad Request", "text/plain", b"invalid ota size");
                return Some(());
            }
            let mut v = match StreamingFwVerifier::start(
                &body[..SignedFwHeader::SIZE],
                &st.ota_pub_key,
                st.min_fw_version,
                HW_TARGET_XTEINK_X3,
            ) {
                Ok(v) => v,
                Err(e) => {
                    let msg = format!("OTA header rejected: {e:?}");
                    send_http_resp(stream, 403, "Forbidden", "text/plain", msg.as_bytes());
                    return Some(());
                }
            };
            let payload = &body[SignedFwHeader::SIZE..];
            if let Err(e) = v.feed(payload) {
                let msg = format!("OTA stream error: {e:?}");
                send_http_resp(stream, 400, "Bad Request", "text/plain", msg.as_bytes());
                return Some(());
            }
            match v.finish() {
                Ok(hdr) => {
                    st.min_fw_version = hdr.fw_version;
                    st.staged_ota = Some((hdr.fw_version, payload.to_vec()));
                    st.staged_meta = Some(StagedOta {
                        fw_version: hdr.fw_version,
                        payload_len: hdr.payload_len,
                        payload_blake3: hdr.payload_blake3,
                    });
                    send_http_resp(stream, 200, "OK", "text/plain", b"OTA verified and staged");
                }
                Err(e) => {
                    let msg = format!("OTA verify failed: {e:?}");
                    send_http_resp(stream, 403, "Forbidden", "text/plain", msg.as_bytes());
                }
            }
        }
        ("GET", "/") | ("GET", "/index.html") => {
            send_http_resp(stream, 200, "OK", "text/html; charset=utf-8", VVDROP_WEB_HTML.as_bytes());
        }
        ("GET", "/favicon.ico") => {
            send_http_resp(stream, 204, "No Content", "image/x-icon", &[]);
        }
        ("GET", "/api/nonce") => {
            let hex = encode_hex(st.auth.server_nonce());
            send_http_resp(stream, 200, "OK", "text/plain", hex.as_bytes());
        }
        ("POST", "/api/auth") => {
            let Some(cn_hex) = hdr_val(&headers, "x-vvdrop-client-nonce") else {
                send_http_resp(stream, 400, "Bad Request", "text/plain", b"missing client nonce");
                return Some(());
            };
            let Some(cn) = decode_hex_32(cn_hex) else {
                send_http_resp(stream, 400, "Bad Request", "text/plain", b"bad client nonce");
                return Some(());
            };
            let mac = if let Some(mac_hex) = hdr_val(&headers, "x-vvdrop-mac") {
                match decode_hex_32(mac_hex) {
                    Some(m) => m,
                    None => {
                        send_http_resp(stream, 400, "Bad Request", "text/plain", b"bad mac hex");
                        return Some(());
                    }
                }
            } else if let Some(pin) = hdr_val(&headers, "x-vvdrop-pin") {
                let clean_pin: Vec<u8> = pin
                    .bytes()
                    .filter(|b| !b.is_ascii_whitespace() && *b != b'-')
                    .collect();
                compute_auth_mac(&clean_pin, st.auth.server_nonce(), &cn)
            } else {
                send_http_resp(stream, 400, "Bad Request", "text/plain", b"missing credentials");
                return Some(());
            };

            let mut rng = st.rng.clone();
            let res = st.auth.verify_and_issue_token(&cn, &mac, &mut rng);
            st.rng = rng;
            match res {
                Ok(tok) => {
                    send_http_resp(stream, 200, "OK", "text/plain", encode_hex(&tok).as_bytes());
                }
                Err(vvbook_security::AuthError::LockedOut) => {
                    send_http_resp(stream, 423, "Locked", "text/plain", b"PIN locked out");
                }
                Err(_) => {
                    send_http_resp(stream, 401, "Unauthorized", "text/plain", b"Invalid PIN/MAC");
                }
            }
        }
        ("POST", "/api/upload") => {
            let Some(tok_hex) = hdr_val(&headers, "x-vvdrop-token") else {
                send_http_resp(stream, 401, "Unauthorized", "text/plain", b"missing token");
                return Some(());
            };
            let Some(tok) = decode_hex_32(tok_hex) else {
                send_http_resp(stream, 401, "Unauthorized", "text/plain", b"bad token");
                return Some(());
            };
            if st.auth.verify_token(&tok).is_err() {
                send_http_resp(stream, 401, "Unauthorized", "text/plain", b"invalid token");
                return Some(());
            }
            let raw_name = hdr_val(&headers, "x-vvdrop-filename").unwrap_or("uploaded_book.txt");
            let dec_name = percent_decode(raw_name);
            let safe_name = dec_name
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or("uploaded_book.txt")
                .replace("..", "_");
            st.rx_bytes += body.len() as u64;
            st.last_received = Some(safe_name.clone());
            st.pending_uploads.push_back((safe_name, body));
            send_http_resp(stream, 200, "OK", "text/plain", b"Uploaded cleanly");
        }
        _ => {
            send_http_resp(stream, 404, "Not Found", "text/plain", b"Not found");
        }
    }
    Some(())
}

/// Deterministic script exporting all 14 curated high-craft PNG screens for visual review.
pub fn export_all_snapshots(out_dir: &Path, sample_dir: &Path) -> Result<Vec<PathBuf>, String> {
    seed_sample_library(sample_dir).map_err(|e| e.to_string())?;
    let temp_sd = out_dir.join("_emu_sd_tmp");
    if temp_sd.exists() {
        let _ = fs::remove_dir_all(&temp_sd);
    }
    let mut storage = FsStorage::new(&temp_sd).map_err(|e| e.to_string())?;
    storage.seed_books_if_empty(sample_dir).map_err(|e| e.to_string())?;

    // Seed 6 additional curated books in the snapshot SD image so 03_library_shelf.png
    // fills all 8 rows down to the chin bar (10 vol. · 1/2).
    let extra_fb2 = |book_title: &str, first: &str, last: &str, lang: &str, body: &str| -> Vec<u8> {
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<FictionBook xmlns="http://www.gribuser.ru/xml/fictionbook/2.0">
  <description><title-info>
    <author><first-name>{first}</first-name><last-name>{last}</last-name></author>
    <book-title>{book_title}</book-title><lang>{lang}</lang>
  </title-info></description>
  <body><section><title><p>I</p></title><p>{body}</p></section></body>
</FictionBook>"#
        )
        .into_bytes()
    };
    let extras: [(&str, Vec<u8>); 6] = [
        (
            "/books/turgenev_first_love.fb2",
            extra_fb2("Первая любовь", "Иван", "Тургенев", "ru", "Мне было тогда лет шестнадцать. Всё кончилось летом на даче у Нескучного сада."),
        ),
        (
            "/books/bunin_antonov_apples.fb2",
            extra_fb2("Антоновские яблоки", "Иван", "Бунин", "ru", "Помню раннее, свежее, тихое утро и запах антоновских яблок на хуторе."),
        ),
        (
            "/books/lermontov_taman.fb2",
            extra_fb2("Тамань", "Михаил", "Лермонтов", "ru", "Тамань — самый скверный городишко из всех приморских городов России."),
        ),
        (
            "/books/montaigne_essays.fb2",
            extra_fb2("Essays: On Solitude", "Michel", "de Montaigne", "en", "We should reserve a back shop all our own, entirely free, wherein to establish true liberty."),
        ),
        (
            "/books/bronte_poems.fb2",
            extra_fb2("No Coward Soul Is Mine", "Emily", "Brontë", "en", "With wide-embracing love Thy Spirit animates eternal years across quiet moorlands."),
        ),
        (
            "/books/seneca_shortness.fb2",
            extra_fb2("On the Shortness of Life", "Lucius", "Seneca", "en", "It is not that we have a short time to live, but that we waste a great deal of it."),
        ),
    ];
    for (p, data) in &extras {
        storage.write_all(p, data).map_err(|e| format!("{e:?}"))?;
    }

    let mut sink = EmuDisplaySink::new();
    let mut sensors = EmuSensors::default();
    let mut power = EmuPower::default();
    let mut ent = HostEntropy::from_seed_u64(0x2026_1112_7777);
    let mut portal = LivePortalServer::bind("127.0.0.1:0", FOLIO_RELEASE_PUBKEY, 1).map_err(|e| e.to_string())?;

    let mut app = FolioApp::new();
    app.boot(&mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);

    // Pick Pushkin as featured active book for Home Desk EN & RU
    if let Some(pos) = app.catalog.iter().position(|c| c.title.contains("Салтан")) {
        app.active_book_idx = pos;
        app.open_active_book(&mut storage, &mut power);
        app.current_page_num = 0;
        app.refresh_home_cover(&mut storage);
    }

    let mut written = Vec::new();
    let mut snap = |name: &str, app_ref: &mut FolioApp, sink_ref: &mut EmuDisplaySink, pwr: &mut EmuPower| -> Result<(), String> {
        app_ref.redraw(sink_ref, RefreshMode::FullGc, pwr);
        let p = out_dir.join(name);
        sink_ref.save_png(&p).map_err(|e| e.to_string())?;
        written.push(p);
        Ok(())
    };

    // 01: Home Desk EN (feature English novel)
    if let Some(pos) = app.catalog.iter().position(|c| c.title.contains("Alice")) {
        app.active_book_idx = pos;
        app.open_active_book(&mut storage, &mut power);
        app.current_page_num = 1;
        app.refresh_home_cover(&mut storage);
    }
    app.lang = UiLang::En;
    app.screen = vvbook_userspace::Screen::Home;
    snap("01_home_desk_en.png", &mut app, &mut sink, &mut power)?;

    // 02: Home Desk RU (feature Pushkin)
    if let Some(pos) = app.catalog.iter().position(|c| c.title.contains("Салтан")) {
        app.active_book_idx = pos;
        app.open_active_book(&mut storage, &mut power);
        app.current_page_num = 0;
        app.refresh_home_cover(&mut storage);
    }
    app.lang = UiLang::Ru;
    snap("02_home_desk_ru.png", &mut app, &mut sink, &mut power)?;

    // 03: Library shelf showing all 4 formats
    app.lang = UiLang::En;
    app.screen = vvbook_userspace::Screen::Library;
    app.lib_cursor = 0;
    snap("03_library_shelf.png", &mut app, &mut sink, &mut power)?;

    // 04: Reader Cyrillic Pushkin
    if let Some(pos) = app.catalog.iter().position(|c| c.title.contains("Салтан")) {
        app.active_book_idx = pos;
    }
    app.lang = UiLang::Ru;
    app.open_active_book(&mut storage, &mut power);
    app.screen = vvbook_userspace::Screen::Reader;
    snap("04_reader_cyrillic_pushkin.png", &mut app, &mut sink, &mut power)?;

    // 05: Reader English Alice
    if let Some(pos) = app.catalog.iter().position(|c| c.title.contains("Alice")) {
        app.active_book_idx = pos;
    }
    app.lang = UiLang::En;
    app.open_active_book(&mut storage, &mut power);
    snap("05_reader_english_alice.png", &mut app, &mut sink, &mut power)?;

    // 06: Reader Aa HUD overlay
    app.handle_event(
        InputEvent::Press(Button::Confirm),
        &mut sink,
        &mut storage,
        &mut sensors,
        &mut power,
        &mut portal,
        &mut ent,
    );
    snap("06_reader_aa_hud.png", &mut app, &mut sink, &mut power)?;

    // 07: VVDrop QR + 6-digit PIN portal
    app.handle_event(InputEvent::Press(Button::Back), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
    app.handle_event(InputEvent::Press(Button::Back), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
    app.home_cursor = 4; // VVDrop
    app.handle_event(InputEvent::Press(Button::Confirm), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
    snap("07_vvdrop_portal.png", &mut app, &mut sink, &mut power)?;
    app.handle_event(InputEvent::Press(Button::Back), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);

    // 08..10: Pocket Pip (Den, Meadow, Stickers)
    app.pip.progress.starberries = 14;
    app.pip.progress.hats_mask = 0b011_1111;
    app.pip.progress.equipped_hat = 2; // Wizard hat!
    app.pip.progress.stickers_mask = 0b0000_0000_0001_1111_1111;
    app.screen = vvbook_userspace::Screen::PocketPip;
    app.pip.tab = PipTab::Den;
    snap("08_pocket_pip_den.png", &mut app, &mut sink, &mut power)?;

    app.pip.tab = PipTab::Meadow;
    snap("09_pocket_pip_meadow.png", &mut app, &mut sink, &mut power)?;

    app.pip.tab = PipTab::Stickers;
    snap("10_pocket_pip_stickers.png", &mut app, &mut sink, &mut power)?;

    // 11: Grandmaster Folio Chess mid-game (2000 ELO Level 4)
    app.screen = vvbook_userspace::Screen::Chess;
    app.chess.start_ai(4);
    app.chess.menu_level = 4;
    app.chess.mode = ChessMode::PlayingAi { level: 4 };
    // Play a classical opening sequence via engine moves so board & history look vibrant
    for _ in 0..4 {
        let pos_before = app.chess.pos;
        if let Some((mv, _)) = app.chess.engine.choose_move(&pos_before, 4, 1) {
            let san = pos_before.format_san(mv, &app.chess.legal_cache);
            let next = pos_before.make_move(mv);
            app.chess.history.push((pos_before, san, mv.uci()));
            app.chess.pos = next;
            app.chess.last_move = Some((mv.from, mv.to));
            app.chess.legal_cache = next.legal_moves();
        }
    }
    snap("11_chess_grandmaster.png", &mut app, &mut sink, &mut power)?;

    // 12..14: All 3 Typographic Standby Screens
    app.screen = vvbook_userspace::Screen::SleepPoster;
    if let Some(pos) = app.catalog.iter().position(|c| c.title.contains("Alice")) {
        app.active_book_idx = pos;
    }
    app.lang = UiLang::En;
    app.sleep_poster = SleepPosterKind::Literary;
    snap("12_sleep_poster_literary.png", &mut app, &mut sink, &mut power)?;
    app.sleep_poster = SleepPosterKind::Cupertino;
    snap("13_sleep_poster_cupertino.png", &mut app, &mut sink, &mut power)?;
    app.sleep_poster = SleepPosterKind::PipStarlight;
    snap("14_sleep_poster_pip.png", &mut app, &mut sink, &mut power)?;

    // 15..16: Settings EN & RU
    app.reader_overlay = ReaderOverlay::None;
    app.screen = vvbook_userspace::Screen::Settings;
    app.lang = UiLang::En;
    app.settings_cursor = 0;
    snap("15_settings_en.png", &mut app, &mut sink, &mut power)?;
    app.lang = UiLang::Ru;
    app.settings_cursor = 3;
    snap("16_settings_ru.png", &mut app, &mut sink, &mut power)?;

    // 17: Reader TOC drawer
    if let Some(pos) = app.catalog.iter().position(|c| c.title.contains("Alice")) {
        app.active_book_idx = pos;
    }
    app.lang = UiLang::En;
    app.open_active_book(&mut storage, &mut power);
    app.screen = vvbook_userspace::Screen::Reader;
    app.reader_overlay = ReaderOverlay::TocDrawer;
    app.toc_cursor = 0;
    snap("17_reader_toc.png", &mut app, &mut sink, &mut power)?;

    // 18: Chess menu
    app.reader_overlay = ReaderOverlay::None;
    app.screen = vvbook_userspace::Screen::Chess;
    app.chess.mode = ChessMode::Menu;
    snap("18_chess_menu.png", &mut app, &mut sink, &mut power)?;

    let _ = fs::remove_dir_all(&temp_sd);
    Ok(written)
}
