use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use vvbook_pal::{DirEntry, StorageBackend, StorageError};

use crate::{DelayUs, HalError, SdControlPins, SpiBus};

const SECTOR_SIZE: usize = 512;
const EOC_MIN: u32 = 0x0FFF_FFF8;
const EOC_MARK: u32 = 0x0FFF_FFFF;

/// Safe 512-byte sector device abstraction (SPI microSD card or synthetic 16 GB block device).
pub trait BlockDevice {
    fn read_sector(&mut self, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> Result<(), HalError>;
    fn write_sector(&mut self, lba: u32, data: &[u8; SECTOR_SIZE]) -> Result<(), HalError>;
    fn sector_count(&self) -> u32;
    fn set_power_save(&mut self, _save: bool) {}
}

/// SPI SD/SDSC/SDHC block driver supporting 2 GB–32 GB+ cards and automatic `GPIO 13`
/// power-gating (`SD_POWER_EN`) with transparent re-initialization on next block access.
pub struct SpiSdhcCard<SPI, PINS, D> {
    spi: SPI,
    pins: PINS,
    delay: D,
    powered: bool,
    initialized: bool,
    sdhc: bool,
    total_sectors: u32,
    pub power_on_cycles: u32,
    pub power_off_cycles: u32,
    pub sectors_read: u64,
    pub sectors_written: u64,
}

impl<SPI, PINS, D> SpiSdhcCard<SPI, PINS, D>
where
    SPI: SpiBus,
    PINS: SdControlPins,
    D: DelayUs,
{
    pub fn new(spi: SPI, mut pins: PINS, delay: D, total_sectors: u32) -> Self {
        pins.set_cs(true);
        Self {
            spi,
            pins,
            delay,
            powered: false,
            initialized: false,
            sdhc: true,
            total_sectors,
            power_on_cycles: 0,
            power_off_cycles: 0,
            sectors_read: 0,
            sectors_written: 0,
        }
    }

    pub fn is_powered(&self) -> bool {
        self.powered
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn is_sdhc(&self) -> bool {
        self.sdhc
    }

    fn xfer_byte(&mut self, b: u8) -> Result<u8, HalError> {
        let mut rx = [0u8; 1];
        self.spi.transfer(&[b], &mut rx)?;
        Ok(rx[0])
    }

    fn send_cmd(&mut self, cmd: u8, arg: u32, crc: u8) -> Result<u8, HalError> {
        self.pins.set_cs(false);
        let _ = self.xfer_byte(0xFF)?;
        let pkt = [
            0x40 | (cmd & 0x3F),
            (arg >> 24) as u8,
            (arg >> 16) as u8,
            (arg >> 8) as u8,
            arg as u8,
            crc,
        ];
        self.spi.write(&pkt)?;

        for _ in 0..16 {
            let r = self.xfer_byte(0xFF)?;
            if (r & 0x80) == 0 {
                return Ok(r);
            }
        }
        self.pins.set_cs(true);
        Err(HalError::Timeout)
    }

    fn deselect(&mut self) -> Result<(), HalError> {
        self.pins.set_cs(true);
        let _ = self.xfer_byte(0xFF)?;
        Ok(())
    }

    /// Initializes or wakes the SDHC card over SPI (`CMD0 -> CMD8 -> ACMD41 -> CMD58`).
    pub fn ensure_initialized(&mut self) -> Result<(), HalError> {
        if self.initialized {
            return Ok(());
        }
        if !self.powered {
            self.pins.set_cs(true);
            self.pins.set_power_en(true);
            self.delay.delay_us(5_000);
            self.powered = true;
            self.power_on_cycles += 1;
        }

        // SD SPI startup requires 100-400 kHz and >= 74 dummy clocks with CS high.
        self.spi.set_frequency_hz(400_000);
        self.pins.set_cs(true);
        let dummy = [0xFFu8; 10];
        self.spi.write(&dummy)?;

        // CMD0: GO_IDLE_STATE -> R1 = 0x01
        let mut idle_ok = false;
        for _ in 0..32 {
            let r1 = self.send_cmd(0, 0, 0x95)?;
            self.deselect()?;
            if r1 == 0x01 {
                idle_ok = true;
                break;
            }
        }
        if !idle_ok {
            return Err(HalError::CardAbsent);
        }

        // CMD8: SEND_IF_COND (2.7-3.6V, check pattern 0xAA)
        let r1 = self.send_cmd(8, 0x0000_01AA, 0x87)?;
        let mut r7 = [0u8; 4];
        for b in &mut r7 {
            *b = self.xfer_byte(0xFF)?;
        }
        self.deselect()?;
        if r1 != 0x01 || r7[2] != 0x01 || r7[3] != 0xAA {
            return Err(HalError::UnsupportedCard);
        }

        // ACMD41: SD_SEND_OP_COND with HCS=1 (bit 30) for SDHC >= 2GB (incl. 16GB)
        let mut ready = false;
        for _ in 0..500 {
            let _ = self.send_cmd(55, 0, 0x65)?;
            self.deselect()?;
            let r = self.send_cmd(41, 0x4000_0000, 0x77)?;
            self.deselect()?;
            if r == 0x00 {
                ready = true;
                break;
            }
            self.delay.delay_us(1_000);
        }
        if !ready {
            return Err(HalError::Timeout);
        }

        // CMD58: READ_OCR -> verify CCS (bit 30)
        let r58 = self.send_cmd(58, 0, 0xFD)?;
        let mut ocr = [0u8; 4];
        for b in &mut ocr {
            *b = self.xfer_byte(0xFF)?;
        }
        self.deselect()?;
        if r58 != 0x00 {
            return Err(HalError::BadResponse);
        }
        self.sdhc = (ocr[0] & 0x40) != 0;

        self.spi.set_frequency_hz(20_000_000);
        self.initialized = true;
        Ok(())
    }

    fn card_addr(&self, lba: u32) -> u32 {
        if self.sdhc { lba } else { lba.wrapping_mul(512) }
    }
}

impl<SPI, PINS, D> BlockDevice for SpiSdhcCard<SPI, PINS, D>
where
    SPI: SpiBus,
    PINS: SdControlPins,
    D: DelayUs,
{
    fn read_sector(&mut self, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> Result<(), HalError> {
        self.ensure_initialized()?;
        let addr = self.card_addr(lba);
        let r1 = self.send_cmd(17, addr, 0xFF)?;
        if r1 != 0x00 {
            let _ = self.deselect();
            return Err(HalError::BadResponse);
        }
        let mut token = 0xFFu8;
        for _ in 0..10_000 {
            token = self.xfer_byte(0xFF)?;
            if token == 0xFE {
                break;
            }
        }
        if token != 0xFE {
            let _ = self.deselect();
            return Err(HalError::Timeout);
        }
        let tx_ff = [0xFFu8; SECTOR_SIZE];
        self.spi.transfer(&tx_ff, buf)?;
        let _crc1 = self.xfer_byte(0xFF)?;
        let _crc2 = self.xfer_byte(0xFF)?;
        self.deselect()?;
        self.sectors_read += 1;
        Ok(())
    }

    fn write_sector(&mut self, lba: u32, data: &[u8; SECTOR_SIZE]) -> Result<(), HalError> {
        self.ensure_initialized()?;
        let addr = self.card_addr(lba);
        let r1 = self.send_cmd(24, addr, 0xFF)?;
        if r1 != 0x00 {
            let _ = self.deselect();
            return Err(HalError::BadResponse);
        }
        let _ = self.xfer_byte(0xFF)?;
        let _ = self.xfer_byte(0xFE)?; // Single-block write token
        self.spi.write(data)?;
        self.spi.write(&[0xFF, 0xFF])?; // Dummy CRC16
        let resp = self.xfer_byte(0xFF)?;
        if (resp & 0x1F) != 0x05 {
            let _ = self.deselect();
            return Err(HalError::WriteFault);
        }
        // Poll DO line until internal flash programming finishes
        let mut done = false;
        for _ in 0..50_000 {
            if self.xfer_byte(0xFF)? != 0x00 {
                done = true;
                break;
            }
        }
        self.deselect()?;
        if !done {
            return Err(HalError::Timeout);
        }
        self.sectors_written += 1;
        Ok(())
    }

    fn sector_count(&self) -> u32 {
        self.total_sectors
    }

    fn set_power_save(&mut self, save: bool) {
        if save {
            if self.powered || self.initialized {
                self.pins.set_cs(true);
                self.pins.set_power_en(false);
                self.powered = false;
                self.initialized = false;
                self.power_off_cycles += 1;
            }
        } else {
            let _ = self.ensure_initialized();
        }
    }
}

/// Formats any `BlockDevice` (e.g., 16 GB SDHC = 31,250,000 sectors) as FAT32 with MBR.
pub fn format_fat32<B: BlockDevice>(
    dev: &mut B,
    total_sectors: u32,
    volume_label: &[u8; 11],
) -> Result<(), StorageError> {
    if total_sectors < 65_536 {
        return Err(StorageError::InvalidPath);
    }
    let part_lba = 2048u32;
    let part_sectors = total_sectors - part_lba;
    let sec_per_clus: u8 = if total_sectors >= 16_000_000 {
        64 // 32 KB clusters for 8 GB - 32 GB+ cards (16 GB native geometry)
    } else if total_sectors >= 2_000_000 {
        16
    } else {
        8
    };
    let rsvd_sec_cnt: u16 = 32;
    let num_fats: u8 = 2;
    let est_clusters = part_sectors / u32::from(sec_per_clus) + 2;
    let fat_sz32 = (est_clusters * 4).div_ceil(512);

    // 1. Write MBR at LBA 0
    let mut mbr = [0u8; 512];
    mbr[446] = 0x00;
    mbr[446 + 4] = 0x0C; // FAT32 LBA
    mbr[446 + 8..446 + 12].copy_from_slice(&part_lba.to_le_bytes());
    mbr[446 + 12..446 + 16].copy_from_slice(&part_sectors.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    dev.write_sector(0, &mbr).map_err(|_| StorageError::Io)?;

    // 2. Write FAT32 VBR at part_lba
    let mut vbr = [0u8; 512];
    vbr[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    vbr[3..11].copy_from_slice(b"VVBOOK  ");
    vbr[11..13].copy_from_slice(&(512u16).to_le_bytes());
    vbr[13] = sec_per_clus;
    vbr[14..16].copy_from_slice(&rsvd_sec_cnt.to_le_bytes());
    vbr[16] = num_fats;
    vbr[21] = 0xF8;
    vbr[32..36].copy_from_slice(&part_sectors.to_le_bytes());
    vbr[36..40].copy_from_slice(&fat_sz32.to_le_bytes());
    vbr[44..48].copy_from_slice(&2u32.to_le_bytes()); // root_clus = 2
    vbr[48..50].copy_from_slice(&1u16.to_le_bytes()); // fsinfo = 1
    vbr[50..52].copy_from_slice(&6u16.to_le_bytes()); // backup boot = 6
    vbr[66] = 0x29;
    vbr[71..82].copy_from_slice(volume_label);
    vbr[82..90].copy_from_slice(b"FAT32   ");
    vbr[510] = 0x55;
    vbr[511] = 0xAA;
    dev.write_sector(part_lba, &vbr).map_err(|_| StorageError::Io)?;

    // 3. FSInfo sector
    let mut fsi = [0u8; 512];
    fsi[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
    fsi[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
    fsi[488..492].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    fsi[492..496].copy_from_slice(&3u32.to_le_bytes());
    fsi[510] = 0x55;
    fsi[511] = 0xAA;
    dev.write_sector(part_lba + 1, &fsi)
        .map_err(|_| StorageError::Io)?;

    // 4. Initialize first sectors of FAT0 and FAT1 (reserved clus 0,1 + root clus 2)
    let fat0_lba = part_lba + u32::from(rsvd_sec_cnt);
    let fat1_lba = fat0_lba + fat_sz32;
    let mut fat0 = [0u8; 512];
    fat0[0..4].copy_from_slice(&0x0FFF_FFF8u32.to_le_bytes());
    fat0[4..8].copy_from_slice(&EOC_MARK.to_le_bytes());
    fat0[8..12].copy_from_slice(&EOC_MARK.to_le_bytes());
    dev.write_sector(fat0_lba, &fat0).map_err(|_| StorageError::Io)?;
    dev.write_sector(fat1_lba, &fat0).map_err(|_| StorageError::Io)?;

    // Zero root directory cluster (cluster 2)
    let data_start_lba = fat0_lba + u32::from(num_fats) * fat_sz32;
    let zero = [0u8; 512];
    for s in 0..u32::from(sec_per_clus) {
        dev.write_sector(data_start_lba + s, &zero)
            .map_err(|_| StorageError::Io)?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Fat32Geo {
    sec_per_clus: u8,
    num_fats: u8,
    fat0_lba: u32,
    fat_sz32: u32,
    data_start_lba: u32,
    root_clus: u32,
    total_clusters: u32,
}

impl Fat32Geo {
    fn cluster_lba(&self, clus: u32) -> Result<u32, StorageError> {
        let idx = clus.checked_sub(2).ok_or(StorageError::Corrupted)?;
        if idx >= self.total_clusters {
            return Err(StorageError::Corrupted);
        }
        let sec_off = idx
            .checked_mul(u32::from(self.sec_per_clus))
            .ok_or(StorageError::Corrupted)?;
        self.data_start_lba
            .checked_add(sec_off)
            .ok_or(StorageError::Corrupted)
    }
}

#[derive(Clone)]
struct DirMatch {
    name: String,
    is_dir: bool,
    first_clus: u32,
    size: u32,
    sfn_lba: u32,
    sfn_off: usize,
    entry_slots: Vec<(u32, usize)>,
}

type CachedSector = (u32, [u8; 512]);

/// Low-RAM FAT32 filesystem over any `BlockDevice`, supporting VFAT UTF-16LE long filenames
/// (Cyrillic + ASCII), subdirectories, and atomic copy-on-write replacement.
pub struct Fat32Volume<B: BlockDevice> {
    dev: RefCell<B>,
    geo: Fat32Geo,
    next_alloc_hint: RefCell<u32>,
    entry_cache: RefCell<[Option<(String, DirMatch)>; 4]>,
    entry_cache_hand: Cell<usize>,
    cluster_cursors: RefCell<[Option<(u32, u32, u32)>; 16]>,
    cursor_hand: Cell<usize>,
    sector_cache: RefCell<[Option<CachedSector>; 4]>,
    sector_cache_hand: Cell<usize>,
}

impl<B: BlockDevice> Fat32Volume<B> {
    fn is_valid_fat32_bpb(sec: &[u8; 512]) -> bool {
        if sec[510] != 0x55 || sec[511] != 0xAA {
            return false;
        }
        let bytes_per_sec = u16::from_le_bytes([sec[11], sec[12]]);
        let sec_per_clus = sec[13];
        let rsvd_sec_cnt = u16::from_le_bytes([sec[14], sec[15]]);
        let num_fats = sec[16];
        let fat_sz32 = u32::from_le_bytes([sec[36], sec[37], sec[38], sec[39]]);
        let root_clus = u32::from_le_bytes([sec[44], sec[45], sec[46], sec[47]]);
        bytes_per_sec == 512
            && sec_per_clus > 0
            && sec_per_clus.is_power_of_two()
            && rsvd_sec_cnt > 0
            && num_fats >= 1
            && fat_sz32 > 0
            && root_clus >= 2
    }

    pub fn mount(mut dev: B) -> Result<Self, StorageError> {
        let mut sec = [0u8; 512];
        dev.read_sector(0, &mut sec).map_err(|_| StorageError::Io)?;
        if sec[510] != 0x55 || sec[511] != 0xAA {
            return Err(StorageError::Corrupted);
        }

        // Check superfloppy LBA 0 first, then scan all 4 MBR partition entries, then LBA 0 BPB fallback.
        let part_lba = if &sec[82..87] == b"FAT32" && Self::is_valid_fat32_bpb(&sec) {
            0
        } else {
            let mut found = None;
            for slot in 0..4usize {
                let off = 446 + slot * 16;
                let p_type = sec[off + 4];
                let lba = u32::from_le_bytes([sec[off + 8], sec[off + 9], sec[off + 10], sec[off + 11]]);
                let sectors = u32::from_le_bytes([sec[off + 12], sec[off + 13], sec[off + 14], sec[off + 15]]);
                if p_type != 0 && lba > 0 && sectors > 0 {
                    let mut vbr = [0u8; 512];
                    if dev.read_sector(lba, &mut vbr).is_ok() && Self::is_valid_fat32_bpb(&vbr) {
                        sec = vbr;
                        found = Some(lba);
                        break;
                    }
                }
            }
            match found {
                Some(lba) => lba,
                None if Self::is_valid_fat32_bpb(&sec) => 0,
                None => return Err(StorageError::Corrupted),
            }
        };

        let sec_per_clus = sec[13];
        let rsvd_sec_cnt = u16::from_le_bytes([sec[14], sec[15]]);
        let num_fats = sec[16];
        let total_sec32 = u32::from_le_bytes([sec[32], sec[33], sec[34], sec[35]]);
        let fat_sz32 = u32::from_le_bytes([sec[36], sec[37], sec[38], sec[39]]);
        let root_clus = u32::from_le_bytes([sec[44], sec[45], sec[46], sec[47]]);

        let rsvd = u32::from(rsvd_sec_cnt);
        let fats_total_secs = u32::from(num_fats)
            .checked_mul(fat_sz32)
            .ok_or(StorageError::Corrupted)?;
        let fat0_lba = part_lba
            .checked_add(rsvd)
            .ok_or(StorageError::Corrupted)?;
        let data_start_lba = fat0_lba
            .checked_add(fats_total_secs)
            .ok_or(StorageError::Corrupted)?;
        let header_secs = rsvd
            .checked_add(fats_total_secs)
            .ok_or(StorageError::Corrupted)?;
        let data_sectors = total_sec32
            .checked_sub(header_secs)
            .ok_or(StorageError::Corrupted)?;
        let max_by_data = u64::from(data_sectors / u32::from(sec_per_clus));
        let max_by_fat = u64::from(fat_sz32)
            .checked_mul(128)
            .and_then(|n| n.checked_sub(2))
            .ok_or(StorageError::Corrupted)?;
        let total_clusters_u64 = max_by_data.min(max_by_fat);
        if total_clusters_u64 == 0 || total_clusters_u64 > u64::from(u32::MAX - 2) {
            return Err(StorageError::Corrupted);
        }
        let total_clusters = total_clusters_u64 as u32;
        if root_clus < 2 || root_clus.saturating_sub(2) >= total_clusters {
            return Err(StorageError::Corrupted);
        }
        u64::from(data_start_lba)
            .checked_add(
                u64::from(total_clusters)
                    .checked_mul(u64::from(sec_per_clus))
                    .ok_or(StorageError::Corrupted)?,
            )
            .filter(|&end| end <= u64::from(u32::MAX))
            .ok_or(StorageError::Corrupted)?;

        let mut next_alloc_hint = 3u32;
        let fsinfo_sec = u16::from_le_bytes([sec[48], sec[49]]);
        if fsinfo_sec != 0
            && fsinfo_sec != 0xFFFF
            && let Some(fsi_lba) = part_lba.checked_add(u32::from(fsinfo_sec))
        {
            let mut fsi = [0u8; 512];
            if dev.read_sector(fsi_lba, &mut fsi).is_ok()
                && fsi[0..4] == *b"RRaA"
                && fsi[484..488] == *b"rrAa"
                && fsi[510..512] == [0x55, 0xAA]
            {
                let nxt_free = u32::from_le_bytes([fsi[492], fsi[493], fsi[494], fsi[495]]);
                if (2..total_clusters.saturating_add(2)).contains(&nxt_free) {
                    next_alloc_hint = nxt_free.max(3);
                }
            }
        }

        Ok(Self {
            dev: RefCell::new(dev),
            geo: Fat32Geo {
                sec_per_clus,
                num_fats,
                fat0_lba,
                fat_sz32,
                data_start_lba,
                root_clus,
                total_clusters,
            },
            next_alloc_hint: RefCell::new(next_alloc_hint),
            entry_cache: RefCell::new([const { None }; 4]),
            entry_cache_hand: Cell::new(0),
            cluster_cursors: RefCell::new([const { None }; 16]),
            cursor_hand: Cell::new(0),
            sector_cache: RefCell::new([const { None }; 4]),
            sector_cache_hand: Cell::new(0),
        })
    }

    pub fn set_power_save(&mut self, save: bool) {
        if save {
            self.invalidate_entry_cache();
            *self.cluster_cursors.borrow_mut() = [const { None }; 16];
            *self.sector_cache.borrow_mut() = [const { None }; 4];
        }
        self.dev.borrow_mut().set_power_save(save);
    }

    fn store_sector_cache(&self, lba: u32, data: &[u8; 512]) {
        let mut slots = self.sector_cache.borrow_mut();
        for slot in slots.iter_mut() {
            if let Some((cached_lba, _)) = *slot
                && cached_lba == lba
            {
                *slot = Some((lba, *data));
                return;
            }
        }
        let idx = self.sector_cache_hand.get() & 3;
        slots[idx] = Some((lba, *data));
        self.sector_cache_hand.set((idx + 1) & 3);
    }

    fn cached_read_sector(
        &self,
        dev: &mut B,
        lba: u32,
        buf: &mut [u8; 512],
    ) -> Result<(), StorageError> {
        {
            let slots = self.sector_cache.borrow();
            for slot in slots.iter().flatten() {
                if slot.0 == lba {
                    *buf = slot.1;
                    return Ok(());
                }
            }
        }
        dev.read_sector(lba, buf).map_err(|_| StorageError::Io)?;
        self.store_sector_cache(lba, buf);
        Ok(())
    }

    fn cached_write_sector(
        &self,
        dev: &mut B,
        lba: u32,
        data: &[u8; 512],
    ) -> Result<(), StorageError> {
        dev.write_sector(lba, data).map_err(|_| StorageError::Io)?;
        self.store_sector_cache(lba, data);
        Ok(())
    }

    fn invalidate_entry_cache(&self) {
        *self.entry_cache.borrow_mut() = [const { None }; 4];
    }

    fn upsert_entry_cache(&self, path: &str, ent: DirMatch) {
        let trimmed = path.trim_matches('/');
        let mut slots = self.entry_cache.borrow_mut();
        for slot in slots.iter_mut() {
            if let Some((cached_path, _)) = slot
                && cached_path == trimmed
            {
                *slot = Some((cached_path.clone(), ent));
                return;
            }
        }
        let idx = self.entry_cache_hand.get() & 3;
        slots[idx] = Some((trimmed.to_string(), ent));
        self.entry_cache_hand.set((idx + 1) & 3);
    }

    fn upsert_cluster_cursor(&self, first_clus: u32, clus_idx: u32, clus: u32) {
        if !(2..EOC_MIN).contains(&clus) {
            return;
        }
        let mut cursors = self.cluster_cursors.borrow_mut();
        for slot in cursors.iter_mut() {
            if let Some((fc, existing_idx, _)) = *slot
                && fc == first_clus
                && (existing_idx >> 7) == (clus_idx >> 7)
            {
                *slot = Some((first_clus, clus_idx, clus));
                return;
            }
        }
        let idx = self.cursor_hand.get() & 15;
        cursors[idx] = Some((first_clus, clus_idx, clus));
        self.cursor_hand.set((idx + 1) & 15);
    }

    fn seek_cluster(
        &self,
        dev: &mut B,
        first_clus: u32,
        target_idx: u32,
    ) -> Result<u32, StorageError> {
        if target_idx == 0 || !(2..EOC_MIN).contains(&first_clus) {
            return Ok(first_clus);
        }
        let (mut cur_idx, mut cur_clus) = (0u32, first_clus);
        {
            let cursors = self.cluster_cursors.borrow();
            for slot in cursors.iter().flatten() {
                let (fc, c_idx, c_clus) = *slot;
                if fc == first_clus
                    && c_idx <= target_idx
                    && c_idx >= cur_idx
                    && (2..EOC_MIN).contains(&c_clus)
                {
                    cur_idx = c_idx;
                    cur_clus = c_clus;
                }
            }
        }
        let limit = self.geo.total_clusters.min(262_144).saturating_add(1);
        let mut hops = 0u32;
        while cur_idx < target_idx && (2..EOC_MIN).contains(&cur_clus) && hops < limit {
            cur_clus = self.read_fat_entry(dev, cur_clus)?;
            cur_idx += 1;
            hops += 1;
            if (cur_idx & 1023) == 0 || cur_idx == (target_idx.saturating_sub(1) & !127) {
                self.upsert_cluster_cursor(first_clus, cur_idx, cur_clus);
            }
        }
        self.upsert_cluster_cursor(first_clus, cur_idx, cur_clus);
        Ok(cur_clus)
    }

    pub fn with_device<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&B) -> R,
    {
        let dev = self.dev.borrow();
        f(&dev)
    }

    fn read_fat_entry(&self, dev: &mut B, clus: u32) -> Result<u32, StorageError> {
        let fat_off = clus * 4;
        let sec_idx = fat_off / 512;
        let byte_off = (fat_off % 512) as usize;
        let mut buf = [0u8; 512];
        self.cached_read_sector(dev, self.geo.fat0_lba + sec_idx, &mut buf)?;
        let val = u32::from_le_bytes([
            buf[byte_off],
            buf[byte_off + 1],
            buf[byte_off + 2],
            buf[byte_off + 3],
        ]) & 0x0FFF_FFFF;
        Ok(val)
    }

    fn flush_fat_sector(
        &self,
        dev: &mut B,
        sec_idx: u32,
        buf: &[u8; 512],
    ) -> Result<(), StorageError> {
        // Write FAT copies in reverse so FAT1 (f = 0) stays warm in sector_cache.
        for f in (0..u32::from(self.geo.num_fats)).rev() {
            self.cached_write_sector(dev, self.geo.fat0_lba + f * self.geo.fat_sz32 + sec_idx, buf)?;
        }
        Ok(())
    }

    fn write_fat_entry(&self, dev: &mut B, clus: u32, val: u32) -> Result<(), StorageError> {
        let fat_off = clus * 4;
        let sec_idx = fat_off / 512;
        let byte_off = (fat_off % 512) as usize;
        let mut buf = [0u8; 512];
        self.cached_read_sector(dev, self.geo.fat0_lba + sec_idx, &mut buf)?;
        let existing = u32::from_le_bytes([
            buf[byte_off],
            buf[byte_off + 1],
            buf[byte_off + 2],
            buf[byte_off + 3],
        ]);
        let next_val = (existing & 0xF000_0000) | (val & 0x0FFF_FFFF);
        buf[byte_off..byte_off + 4].copy_from_slice(&next_val.to_le_bytes());
        self.flush_fat_sector(dev, sec_idx, &buf)
    }

    fn alloc_cluster(&self, dev: &mut B) -> Result<u32, StorageError> {
        let mut hint = self.next_alloc_hint.borrow_mut();
        let max_c = self.geo.total_clusters + 2;
        let start = (*hint).clamp(3, max_c.saturating_sub(1));
        for pass in 0..2 {
            let (lo, hi) = if pass == 0 {
                (start, max_c)
            } else {
                (3, start)
            };
            let mut c = lo;
            while c < hi {
                let sec_idx = (c * 4) / 512;
                let mut buf = [0u8; 512];
                self.cached_read_sector(dev, self.geo.fat0_lba + sec_idx, &mut buf)?;
                let base_c = sec_idx * 128;
                let first_in_sec = (c - base_c) as usize;
                for slot in first_in_sec..128 {
                    let cand = base_c + slot as u32;
                    if cand >= hi {
                        break;
                    }
                    let off = slot * 4;
                    let v = u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
                        & 0x0FFF_FFFF;
                    if v == 0 {
                        self.write_fat_entry(dev, cand, EOC_MARK)?;
                        *hint = cand + 1;
                        return Ok(cand);
                    }
                }
                c = base_c + 128;
            }
        }
        Err(StorageError::OutOfSpace)
    }

    fn load_fat_sector_for<'a>(
        &self,
        dev: &mut B,
        cur_sec: &'a mut Option<(u32, [u8; 512])>,
        sec_idx: u32,
    ) -> Result<&'a mut [u8; 512], StorageError> {
        let need_swap = match cur_sec {
            Some((idx, _)) => *idx != sec_idx,
            None => true,
        };
        if need_swap {
            if let Some((old_idx, old_buf)) = cur_sec.take() {
                self.flush_fat_sector(dev, old_idx, &old_buf)?;
            }
            let mut buf = [0u8; 512];
            self.cached_read_sector(dev, self.geo.fat0_lba + sec_idx, &mut buf)?;
            *cur_sec = Some((sec_idx, buf));
        }
        Ok(&mut cur_sec.as_mut().unwrap().1)
    }

    fn free_chain(&self, dev: &mut B, mut clus: u32) -> Result<(), StorageError> {
        *self.cluster_cursors.borrow_mut() = [const { None }; 16];
        let mut cur_sec: Option<(u32, [u8; 512])> = None;
        let mut min_freed: Option<u32> = None;
        let limit = self.geo.total_clusters.min(262_144).saturating_add(1);
        let mut hops = 0u32;
        while (2..EOC_MIN).contains(&clus) && hops < limit {
            hops += 1;
            let fat_off = clus * 4;
            let sec_idx = fat_off / 512;
            let byte_off = (fat_off % 512) as usize;
            let buf = self.load_fat_sector_for(dev, &mut cur_sec, sec_idx)?;
            let existing = u32::from_le_bytes([
                buf[byte_off],
                buf[byte_off + 1],
                buf[byte_off + 2],
                buf[byte_off + 3],
            ]);
            let next = existing & 0x0FFF_FFFF;
            let cleared = existing & 0xF000_0000;
            buf[byte_off..byte_off + 4].copy_from_slice(&cleared.to_le_bytes());
            min_freed = Some(min_freed.map_or(clus, |m| m.min(clus)));
            clus = next;
        }
        if let Some((idx, buf)) = cur_sec {
            self.flush_fat_sector(dev, idx, &buf)?;
        }
        if let Some(m) = min_freed {
            let mut hint = self.next_alloc_hint.borrow_mut();
            *hint = (*hint).min(m).max(3);
        }
        Ok(())
    }

    fn write_chain(&self, dev: &mut B, data: &[u8]) -> Result<u32, StorageError> {
        Ok(self.write_chain_ends(dev, data)?.0)
    }

    fn write_chain_ends(&self, dev: &mut B, data: &[u8]) -> Result<(u32, u32), StorageError> {
        if data.is_empty() {
            return Ok((0, 0));
        }
        let clus_bytes = u32::from(self.geo.sec_per_clus) as usize * 512;
        let needed = data.len().div_ceil(clus_bytes);
        let mut clusters = Vec::with_capacity(needed);
        {
            let mut hint = self.next_alloc_hint.borrow_mut();
            let max_c = self.geo.total_clusters + 2;
            let start = (*hint).clamp(3, max_c.saturating_sub(1));
            let mut scan_buf = [0u8; 512];
            'scan: for pass in 0..2 {
                let (lo, hi) = if pass == 0 {
                    (start, max_c)
                } else {
                    (3, start)
                };
                let mut c = lo;
                while c < hi {
                    let sec_idx = (c * 4) / 512;
                    self.cached_read_sector(dev, self.geo.fat0_lba + sec_idx, &mut scan_buf)?;
                    let base_c = sec_idx * 128;
                    let first_in_sec = (c - base_c) as usize;
                    for slot in first_in_sec..128 {
                        let cand = base_c + slot as u32;
                        if cand >= hi {
                            break;
                        }
                        let off = slot * 4;
                        let v = u32::from_le_bytes([
                            scan_buf[off],
                            scan_buf[off + 1],
                            scan_buf[off + 2],
                            scan_buf[off + 3],
                        ]) & 0x0FFF_FFFF;
                        if v == 0 {
                            clusters.push(cand);
                            if clusters.len() == needed {
                                *hint = cand + 1;
                                break 'scan;
                            }
                        }
                    }
                    c = base_c + 128;
                }
            }
            if clusters.len() < needed {
                return Err(StorageError::OutOfSpace);
            }
        }

        let mut cur_sec: Option<(u32, [u8; 512])> = None;
        for (i, &clus) in clusters.iter().enumerate() {
            let link = if i + 1 < clusters.len() {
                clusters[i + 1]
            } else {
                EOC_MARK
            };
            let fat_off = clus * 4;
            let sec_idx = fat_off / 512;
            let byte_off = (fat_off % 512) as usize;
            let buf = self.load_fat_sector_for(dev, &mut cur_sec, sec_idx)?;
            let existing = u32::from_le_bytes([
                buf[byte_off],
                buf[byte_off + 1],
                buf[byte_off + 2],
                buf[byte_off + 3],
            ]);
            let next_val = (existing & 0xF000_0000) | (link & 0x0FFF_FFFF);
            buf[byte_off..byte_off + 4].copy_from_slice(&next_val.to_le_bytes());
        }
        if let Some((idx, buf)) = cur_sec {
            self.flush_fat_sector(dev, idx, &buf)?;
        }

        let mut sec_buf = [0u8; 512];
        let mut offset = 0usize;
        for &c in &clusters {
            let base_lba = self.geo.cluster_lba(c)?;
            for s in 0..u32::from(self.geo.sec_per_clus) {
                if offset >= data.len() {
                    break;
                }
                let take = (data.len() - offset).min(512);
                sec_buf.fill(0);
                sec_buf[..take].copy_from_slice(&data[offset..offset + take]);
                self.cached_write_sector(dev, base_lba + s, &sec_buf)?;
                offset += take;
            }
        }
        Ok((clusters[0], *clusters.last().unwrap_or(&clusters[0])))
    }

    fn scan_dir_cluster<F>(
        &self,
        dev: &mut B,
        start_clus: u32,
        want_slots: bool,
        mut visit: F,
    ) -> Result<(), StorageError>
    where
        F: FnMut(DirMatch) -> bool,
    {
        let mut lfn_parts: Vec<(u8, [u16; 13])> = Vec::new();
        let mut lfn_slots: Vec<(u32, usize)> = Vec::new();
        let mut lfn_csum: Option<u8> = None;
        let mut lfn_expected_ord: u8 = 0;

        let mut clus = start_clus;
        let mut sec = [0u8; 512];
        let mut hops = 0usize;
        while (2..EOC_MIN).contains(&clus) && hops < 256 {
            hops += 1;
            let base_lba = self.geo.cluster_lba(clus)?;
            for s in 0..u32::from(self.geo.sec_per_clus) {
                let lba = base_lba + s;
                self.cached_read_sector(dev, lba, &mut sec)?;
                for i in 0..16 {
                    let off = i * 32;
                    let ent = &sec[off..off + 32];
                    if ent[0] == 0x00 {
                        return Ok(());
                    }
                    if ent[0] == 0xE5 {
                        lfn_parts.clear();
                        lfn_slots.clear();
                        lfn_csum = None;
                        lfn_expected_ord = 0;
                        continue;
                    }
                    let attr = ent[11];
                    if attr == 0x0F {
                        let ord = ent[0] & 0x1F;
                        let valid_zero_clus = ent[26] == 0 && ent[27] == 0;
                        if (ent[0] & 0x40) != 0 {
                            lfn_parts.clear();
                            lfn_slots.clear();
                            if valid_zero_clus && (1..=20).contains(&ord) {
                                lfn_csum = Some(ent[13]);
                                lfn_expected_ord = ord - 1;
                            } else {
                                lfn_csum = None;
                                lfn_expected_ord = 0;
                                continue;
                            }
                        } else if lfn_csum != Some(ent[13])
                            || !valid_zero_clus
                            || lfn_expected_ord == 0
                            || ord != lfn_expected_ord
                        {
                            lfn_parts.clear();
                            lfn_slots.clear();
                            lfn_csum = None;
                            lfn_expected_ord = 0;
                            continue;
                        } else {
                            lfn_expected_ord -= 1;
                        }
                        let chars = decode_lfn_utf16_slice(ent);
                        lfn_parts.push((ord, chars));
                        if want_slots {
                            lfn_slots.push((lba, off));
                        }
                        continue;
                    }
                    if (attr & 0x08) != 0 {
                        lfn_parts.clear();
                        lfn_slots.clear();
                        lfn_csum = None;
                        lfn_expected_ord = 0;
                        continue; // Volume label
                    }
                    let mut sfn = [0u8; 11];
                    sfn.copy_from_slice(&ent[0..11]);
                    let lfn_valid = !lfn_parts.is_empty()
                        && lfn_expected_ord == 0
                        && lfn_csum == Some(sfn_checksum(&sfn));
                    let name = if lfn_valid {
                        lfn_parts.sort_by_key(|(ord, _)| *ord);
                        let mut u16s = Vec::with_capacity(lfn_parts.len() * 13);
                        for (_, part) in &lfn_parts {
                            for &cu in part {
                                if cu == 0x0000 || cu == 0xFFFF {
                                    break;
                                }
                                u16s.push(cu);
                            }
                        }
                        String::from_utf16_lossy(&u16s)
                    } else {
                        lfn_slots.clear();
                        decode_sfn(&sfn, ent[12])
                    };

                    let hi = u16::from_le_bytes([ent[20], ent[21]]) as u32;
                    let lo = u16::from_le_bytes([ent[26], ent[27]]) as u32;
                    let first_clus = (hi << 16) | lo;
                    let size = u32::from_le_bytes([ent[28], ent[29], ent[30], ent[31]]);
                    let is_dir = (attr & 0x10) != 0;

                    let entry_slots = if want_slots {
                        lfn_slots.push((lba, off));
                        core::mem::take(&mut lfn_slots)
                    } else {
                        Vec::new()
                    };
                    lfn_parts.clear();
                    lfn_csum = None;
                    lfn_expected_ord = 0;

                    let bad_name = name.is_empty()
                        || name == "."
                        || name == ".."
                        || name.chars().any(|c| {
                            c == '\u{FFFD}' || c == '/' || c == '\\' || c < ' ' || c == '\x7F'
                        });
                    let bad_cluster = !is_dir && size > 0 && first_clus < 2;
                    if !bad_name && !bad_cluster {
                        let keep_going = visit(DirMatch {
                            name,
                            is_dir,
                            first_clus,
                            size,
                            sfn_lba: lba,
                            sfn_off: off,
                            entry_slots,
                        });
                        if !keep_going {
                            return Ok(());
                        }
                    }
                }
            }
            clus = self.read_fat_entry(dev, clus)?;
        }
        Ok(())
    }

    fn sfn_exists_in_dir(
        &self,
        dev: &mut B,
        start_clus: u32,
        target_sfn: &[u8; 11],
    ) -> Result<bool, StorageError> {
        let mut clus = start_clus;
        let mut sec = [0u8; 512];
        let mut hops = 0usize;
        while (2..EOC_MIN).contains(&clus) && hops < 256 {
            hops += 1;
            let base_lba = self.geo.cluster_lba(clus)?;
            for s in 0..u32::from(self.geo.sec_per_clus) {
                let lba = base_lba + s;
                self.cached_read_sector(dev, lba, &mut sec)?;
                for i in 0..16 {
                    let off = i * 32;
                    let ent = &sec[off..off + 32];
                    if ent[0] == 0x00 {
                        return Ok(false);
                    }
                    if ent[0] == 0xE5 || ent[11] == 0x0F || (ent[11] & 0x08) != 0 {
                        continue;
                    }
                    if &ent[0..11] == target_sfn {
                        return Ok(true);
                    }
                }
            }
            clus = self.read_fat_entry(dev, clus)?;
        }
        Ok(false)
    }

    fn find_in_dir_cluster(
        &self,
        dev: &mut B,
        dir_clus: u32,
        target: &str,
        want_slots: bool,
    ) -> Result<Option<DirMatch>, StorageError> {
        let mut found = None;
        self.scan_dir_cluster(dev, dir_clus, want_slots, |ent| {
            if names_eq(&ent.name, target) {
                found = Some(ent);
                false
            } else {
                true
            }
        })?;
        Ok(found)
    }

    fn resolve_parent<'a>(
        &self,
        dev: &mut B,
        path: &'a str,
    ) -> Result<(u32, &'a str), StorageError> {
        let trimmed = path.trim_matches('/');
        if trimmed.is_empty() {
            return Err(StorageError::InvalidPath);
        }
        let mut parts: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
        let leaf = parts.pop().ok_or(StorageError::InvalidPath)?;
        let mut dir_clus = self.geo.root_clus;
        for part in parts {
            let found = self
                .find_in_dir_cluster(dev, dir_clus, part, false)?
                .filter(|e| e.is_dir)
                .ok_or(StorageError::NotFound)?;
            dir_clus = if found.first_clus < 2 {
                self.geo.root_clus
            } else {
                found.first_clus
            };
        }
        Ok((dir_clus, leaf))
    }

    fn resolve_entry_opts(
        &self,
        dev: &mut B,
        path: &str,
        want_slots: bool,
    ) -> Result<DirMatch, StorageError> {
        let trimmed = path.trim_matches('/');
        if !want_slots {
            let slots = self.entry_cache.borrow();
            for slot in slots.iter().flatten() {
                if slot.0 == trimmed {
                    return Ok(slot.1.clone());
                }
            }
        }
        let (parent_clus, leaf) = self.resolve_parent(dev, path)?;
        let found = self
            .find_in_dir_cluster(dev, parent_clus, leaf, want_slots)?
            .ok_or(StorageError::NotFound)?;
        if !want_slots {
            self.upsert_entry_cache(trimmed, found.clone());
        }
        Ok(found)
    }

    fn resolve_entry(&self, dev: &mut B, path: &str) -> Result<DirMatch, StorageError> {
        self.resolve_entry_opts(dev, path, false)
    }

    fn create_dir_entry(
        &self,
        dev: &mut B,
        dir_clus: u32,
        name: &str,
        is_dir: bool,
        first_clus: u32,
        size: u32,
    ) -> Result<(), StorageError> {
        let mut sfn = make_unique_sfn(name, 0);
        for attempt in 0..=255u8 {
            let cand = make_unique_sfn(name, attempt);
            if !self.sfn_exists_in_dir(dev, dir_clus, &cand)? {
                sfn = cand;
                break;
            }
        }
        let csum = sfn_checksum(&sfn);
        let utf16: Vec<u16> = name.encode_utf16().collect();
        let lfn_count = utf16.len().div_ceil(13).max(1);
        let need_slots = lfn_count + 1;

        let slots = self.alloc_dir_slots(dev, dir_clus, need_slots)?;

        // Write LFN entries from highest ordinal down to 1, then SFN entry
        for (idx, &(slot_lba, slot_off)) in slots.iter().enumerate().take(lfn_count) {
            let ord = (lfn_count - idx) as u8;
            let ord_byte = if idx == 0 { ord | 0x40 } else { ord };
            let start_u16 = (ord as usize - 1) * 13;
            let mut chunk = [0xFFFFu16; 13];
            for (k, slot) in chunk.iter_mut().enumerate() {
                let pos = start_u16 + k;
                if pos < utf16.len() {
                    *slot = utf16[pos];
                } else if pos == utf16.len() {
                    *slot = 0x0000;
                }
            }
            let ent = encode_lfn_entry(ord_byte, csum, &chunk);
            self.write_dir_slot(dev, slot_lba, slot_off, &ent)?;
        }

        let mut sfn_ent = [0u8; 32];
        sfn_ent[0..11].copy_from_slice(&sfn);
        sfn_ent[11] = if is_dir { 0x10 } else { 0x20 };
        sfn_ent[20..22].copy_from_slice(&((first_clus >> 16) as u16).to_le_bytes());
        sfn_ent[26..28].copy_from_slice(&(first_clus as u16).to_le_bytes());
        sfn_ent[28..32].copy_from_slice(&size.to_le_bytes());
        let last = *slots.last().unwrap();
        self.write_dir_slot(dev, last.0, last.1, &sfn_ent)?;
        Ok(())
    }

    fn alloc_dir_slots(
        &self,
        dev: &mut B,
        dir_clus: u32,
        need: usize,
    ) -> Result<Vec<(u32, usize)>, StorageError> {
        let mut free_run: Vec<(u32, usize)> = Vec::new();
        let mut last_clus = dir_clus;
        let mut clus = dir_clus;
        let mut sec = [0u8; 512];
        let mut hops = 0u32;
        let mut seen_eod = false;

        while (2..EOC_MIN).contains(&clus) && hops < 512 {
            hops += 1;
            last_clus = clus;
            let base_lba = self.geo.cluster_lba(clus)?;
            for s in 0..u32::from(self.geo.sec_per_clus) {
                let lba = base_lba + s;
                self.cached_read_sector(dev, lba, &mut sec)?;
                for i in 0..16 {
                    let off = i * 32;
                    if seen_eod {
                        // Every slot logically at-or-past 0x00 EOD is unallocated even if dirty bytes exist.
                        free_run.push((lba, off));
                    } else {
                        let first = sec[off];
                        if first == 0x00 {
                            seen_eod = true;
                            free_run.push((lba, off));
                        } else if first == 0xE5 {
                            free_run.push((lba, off));
                        } else {
                            free_run.clear();
                        }
                    }

                    if free_run.len() == need {
                        if seen_eod {
                            // Preserve the FAT32 0x00 EOD sentinel right after the newly consumed tail.
                            if i + 1 < 16 {
                                let next_off = (i + 1) * 32;
                                if sec[next_off] != 0x00 {
                                    sec[next_off] = 0x00;
                                    self.cached_write_sector(dev, lba, &sec)?;
                                }
                            } else {
                                let next_target = if s + 1 < u32::from(self.geo.sec_per_clus) {
                                    Some(base_lba + s + 1)
                                } else {
                                    let nc = self.read_fat_entry(dev, clus)?;
                                    if (2..EOC_MIN).contains(&nc) {
                                        Some(self.geo.cluster_lba(nc)?)
                                    } else {
                                        None
                                    }
                                };
                                if let Some(next_lba) = next_target {
                                    self.cached_read_sector(dev, next_lba, &mut sec)?;
                                    if sec[0] != 0x00 {
                                        sec[0] = 0x00;
                                        self.cached_write_sector(dev, next_lba, &sec)?;
                                    }
                                }
                            }
                        }
                        return Ok(free_run);
                    }
                }
            }
            clus = self.read_fat_entry(dev, clus)?;
        }
        if hops >= 512 {
            return Err(StorageError::Corrupted);
        }

        // Extend directory with a fresh zeroed cluster (remaining trailing slots are already 0x00).
        let new_c = self.alloc_cluster(dev)?;
        self.write_fat_entry(dev, last_clus, new_c)?;
        self.write_fat_entry(dev, new_c, EOC_MARK)?;
        let zero = [0u8; 512];
        let base_lba = self.geo.cluster_lba(new_c)?;
        for s in 0..u32::from(self.geo.sec_per_clus) {
            let lba = base_lba + s;
            self.cached_write_sector(dev, lba, &zero)?;
            for i in 0..16 {
                if free_run.len() < need {
                    free_run.push((lba, i * 32));
                }
            }
        }
        Ok(free_run)
    }

    fn write_dir_slot(
        &self,
        dev: &mut B,
        lba: u32,
        off: usize,
        entry: &[u8; 32],
    ) -> Result<(), StorageError> {
        let mut sec = [0u8; 512];
        self.cached_read_sector(dev, lba, &mut sec)?;
        sec[off..off + 32].copy_from_slice(entry);
        self.cached_write_sector(dev, lba, &sec)?;
        Ok(())
    }

    fn update_sfn_cluster_and_size(
        &self,
        dev: &mut B,
        sfn_lba: u32,
        sfn_off: usize,
        first_clus: u32,
        size: u32,
    ) -> Result<(), StorageError> {
        let mut sec = [0u8; 512];
        self.cached_read_sector(dev, sfn_lba, &mut sec)?;
        sec[sfn_off + 20..sfn_off + 22]
            .copy_from_slice(&((first_clus >> 16) as u16).to_le_bytes());
        sec[sfn_off + 26..sfn_off + 28].copy_from_slice(&(first_clus as u16).to_le_bytes());
        sec[sfn_off + 28..sfn_off + 32].copy_from_slice(&size.to_le_bytes());
        self.cached_write_sector(dev, sfn_lba, &sec)?;
        Ok(())
    }

    fn read_file_range_internal(
        &self,
        dev: &mut B,
        first_clus: u32,
        file_size: u32,
        offset: u64,
        out: &mut [u8],
    ) -> Result<usize, StorageError> {
        let size_u64 = u64::from(file_size);
        if offset >= size_u64 || out.is_empty() {
            return Ok(0);
        }
        let to_read = (size_u64 - offset).min(out.len() as u64) as usize;
        let clus_bytes = u32::from(self.geo.sec_per_clus) as usize * 512;
        let mut cur_idx = (offset as usize / clus_bytes) as u32;
        let mut pos = (offset as usize) % clus_bytes;
        let mut clus = self.seek_cluster(dev, first_clus, cur_idx)?;

        let mut copied = 0usize;
        let mut sec_buf = [0u8; 512];
        while copied < to_read && (2..EOC_MIN).contains(&clus) {
            let base_lba = self.geo.cluster_lba(clus)?;
            let mut sec_idx = pos / 512;
            let mut in_sec = pos % 512;
            while sec_idx < usize::from(self.geo.sec_per_clus) && copied < to_read {
                self.cached_read_sector(dev, base_lba + sec_idx as u32, &mut sec_buf)?;
                let avail = 512 - in_sec;
                let take = avail.min(to_read - copied);
                out[copied..copied + take].copy_from_slice(&sec_buf[in_sec..in_sec + take]);
                copied += take;
                sec_idx += 1;
                in_sec = 0;
            }
            pos = 0;
            if copied < to_read {
                clus = self.read_fat_entry(dev, clus)?;
                cur_idx += 1;
                self.upsert_cluster_cursor(first_clus, cur_idx, clus);
            }
        }
        Ok(copied)
    }
}

impl<B: BlockDevice> StorageBackend for Fat32Volume<B> {
    fn exists(&self, path: &str) -> bool {
        let trimmed = path.trim_matches('/');
        if trimmed.is_empty() {
            return true;
        }
        let mut dev = self.dev.borrow_mut();
        self.resolve_entry(&mut dev, path).is_ok()
    }

    fn file_len(&self, path: &str) -> Result<u64, StorageError> {
        let mut dev = self.dev.borrow_mut();
        let ent = self.resolve_entry(&mut dev, path)?;
        if ent.is_dir {
            return Err(StorageError::InvalidPath);
        }
        Ok(u64::from(ent.size))
    }

    fn read_all(&self, path: &str) -> Result<Vec<u8>, StorageError> {
        let mut dev = self.dev.borrow_mut();
        let ent = self.resolve_entry(&mut dev, path)?;
        if ent.is_dir {
            return Err(StorageError::InvalidPath);
        }
        let mut buf = vec![0u8; ent.size as usize];
        let n = self.read_file_range_internal(&mut dev, ent.first_clus, ent.size, 0, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    fn read_range(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
        let mut dev = self.dev.borrow_mut();
        let ent = self.resolve_entry(&mut dev, path)?;
        if ent.is_dir {
            return Err(StorageError::InvalidPath);
        }
        self.read_file_range_internal(&mut dev, ent.first_clus, ent.size, offset, buf)
    }

    fn write_all(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError> {
        self.write_atomic(path, data)
    }

    fn write_atomic(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError> {
        self.invalidate_entry_cache();
        let mut dev = self.dev.borrow_mut();
        let (parent_clus, leaf) = self.resolve_parent(&mut dev, path)?;
        let existing = self.find_in_dir_cluster(&mut dev, parent_clus, leaf, false)?;

        // Copy-on-write on FAT32: allocate & flush new cluster chain BEFORE touching the directory entry.
        let new_clus = self.write_chain(&mut dev, data)?;
        let new_size = data.len() as u32;

        if let Some(old) = existing {
            if old.is_dir {
                let _ = self.free_chain(&mut dev, new_clus);
                return Err(StorageError::InvalidPath);
            }
            self.update_sfn_cluster_and_size(&mut dev, old.sfn_lba, old.sfn_off, new_clus, new_size)?;
            if old.first_clus >= 2 {
                let _ = self.free_chain(&mut dev, old.first_clus);
            }
        } else {
            self.create_dir_entry(&mut dev, parent_clus, leaf, false, new_clus, new_size)?;
        }
        Ok(())
    }

    fn append(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError> {
        if data.is_empty() {
            return if self.exists(path) {
                Ok(())
            } else {
                self.write_atomic(path, &[])
            };
        }
        let ent = {
            let mut dev = self.dev.borrow_mut();
            match self.resolve_entry(&mut dev, path) {
                Ok(e) => e,
                Err(StorageError::NotFound) => {
                    drop(dev);
                    return self.write_atomic(path, data);
                }
                Err(e) => return Err(e),
            }
        };
        if ent.is_dir {
            return Err(StorageError::InvalidPath);
        }

        let mut dev = self.dev.borrow_mut();
        let clus_bytes = usize::from(self.geo.sec_per_clus) * 512;
        if ent.size == 0 || ent.first_clus < 2 {
            let (new_clus, end_clus) = self.write_chain_ends(&mut dev, data)?;
            let new_size = u32::try_from(data.len()).map_err(|_| StorageError::OutOfSpace)?;
            self.update_sfn_cluster_and_size(&mut dev, ent.sfn_lba, ent.sfn_off, new_clus, new_size)?;
            if ent.first_clus >= 2 {
                let _ = self.free_chain(&mut dev, ent.first_clus);
            }
            if new_size > 0 {
                let tail_idx = ((new_size as usize - 1) / clus_bytes) as u32;
                self.upsert_cluster_cursor(new_clus, tail_idx, end_clus);
            }
            self.upsert_entry_cache(
                path,
                DirMatch {
                    first_clus: new_clus,
                    size: new_size,
                    ..ent
                },
            );
            return Ok(());
        }

        let new_size = ent
            .size
            .checked_add(u32::try_from(data.len()).map_err(|_| StorageError::OutOfSpace)?)
            .ok_or(StorageError::OutOfSpace)?;

        let last_idx = ((ent.size as usize - 1) / clus_bytes) as u32;
        let mut last_clus = self.seek_cluster(&mut dev, ent.first_clus, last_idx)?;
        // Advance if any trailing cluster exists beyond expected last_idx
        for _ in 0..8 {
            let next = self.read_fat_entry(&mut dev, last_clus)?;
            if !(2..EOC_MIN).contains(&next) {
                break;
            }
            last_clus = next;
        }

        let tail = (ent.size as usize) % clus_bytes;
        let mut offset = 0usize;

        if tail != 0 {
            let base_lba = self.geo.cluster_lba(last_clus)?;
            let mut sec_idx = tail / 512;
            let mut in_sec = tail % 512;
            let mut sec_buf = [0u8; 512];
            while sec_idx < usize::from(self.geo.sec_per_clus) && offset < data.len() {
                let lba = base_lba + sec_idx as u32;
                if in_sec != 0 {
                    self.cached_read_sector(&mut dev, lba, &mut sec_buf)?;
                } else {
                    sec_buf.fill(0);
                }
                let avail = 512 - in_sec;
                let take = avail.min(data.len() - offset);
                sec_buf[in_sec..in_sec + take].copy_from_slice(&data[offset..offset + take]);
                self.cached_write_sector(&mut dev, lba, &sec_buf)?;
                offset += take;
                sec_idx += 1;
                in_sec = 0;
            }
        }

        let mut final_clus = last_clus;
        if offset < data.len() {
            let (extra_first, extra_last) = self.write_chain_ends(&mut dev, &data[offset..])?;
            self.write_fat_entry(&mut dev, last_clus, extra_first)?;
            final_clus = extra_last;
        }

        let new_last_idx = ((new_size as usize - 1) / clus_bytes) as u32;
        self.upsert_cluster_cursor(ent.first_clus, new_last_idx, final_clus);

        self.update_sfn_cluster_and_size(&mut dev, ent.sfn_lba, ent.sfn_off, ent.first_clus, new_size)?;
        self.upsert_entry_cache(
            path,
            DirMatch {
                size: new_size,
                ..ent
            },
        );
        Ok(())
    }

    fn remove(&mut self, path: &str) -> Result<(), StorageError> {
        let mut dev = self.dev.borrow_mut();
        let ent = self.resolve_entry_opts(&mut dev, path, true)?;
        if ent.is_dir {
            return Err(StorageError::InvalidPath);
        }
        self.invalidate_entry_cache();
        let mut sec = [0u8; 512];
        for &(lba, off) in &ent.entry_slots {
            self.cached_read_sector(&mut dev, lba, &mut sec)?;
            sec[off] = 0xE5;
            self.cached_write_sector(&mut dev, lba, &sec)?;
        }
        if ent.first_clus >= 2 {
            self.free_chain(&mut dev, ent.first_clus)?;
        }
        Ok(())
    }

    fn mkdir_all(&mut self, path: &str) -> Result<(), StorageError> {
        self.invalidate_entry_cache();
        let mut dev = self.dev.borrow_mut();
        let mut cur_clus = self.geo.root_clus;
        for part in path.split('/').filter(|s| !s.is_empty()) {
            if let Some(found) = self.find_in_dir_cluster(&mut dev, cur_clus, part, false)? {
                if !found.is_dir {
                    return Err(StorageError::AlreadyExists);
                }
                cur_clus = found.first_clus;
            } else {
                let new_c = self.alloc_cluster(&mut dev)?;
                self.write_fat_entry(&mut dev, new_c, EOC_MARK)?;
                let mut zero = [0u8; 512];
                let base_lba = self.geo.cluster_lba(new_c)?;
                for s in 0..u32::from(self.geo.sec_per_clus) {
                    if s == 0 {
                        // '.' and '..' entries in new directory cluster
                        let mut sec0 = [0u8; 512];
                        sec0[0..11].copy_from_slice(b".          ");
                        sec0[11] = 0x10;
                        sec0[20..22].copy_from_slice(&((new_c >> 16) as u16).to_le_bytes());
                        sec0[26..28].copy_from_slice(&(new_c as u16).to_le_bytes());

                        let parent_c = if cur_clus == self.geo.root_clus {
                            0
                        } else {
                            cur_clus
                        };
                        sec0[32..43].copy_from_slice(b"..         ");
                        sec0[32 + 11] = 0x10;
                        sec0[32 + 20..32 + 22]
                            .copy_from_slice(&((parent_c >> 16) as u16).to_le_bytes());
                        sec0[32 + 26..32 + 28].copy_from_slice(&(parent_c as u16).to_le_bytes());
                        self.cached_write_sector(&mut dev, base_lba, &sec0)?;
                    } else {
                        zero.fill(0);
                        self.cached_write_sector(&mut dev, base_lba + s, &zero)?;
                    }
                }
                self.create_dir_entry(&mut dev, cur_clus, part, true, new_c, 0)?;
                cur_clus = new_c;
            }
        }
        Ok(())
    }

    fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError> {
        let mut dev = self.dev.borrow_mut();
        let trimmed = path.trim_matches('/');
        let target_clus = if trimmed.is_empty() {
            self.geo.root_clus
        } else {
            let ent = self.resolve_entry(&mut dev, path)?;
            if !ent.is_dir {
                return Err(StorageError::InvalidPath);
            }
            if ent.first_clus < 2 {
                self.geo.root_clus
            } else {
                ent.first_clus
            }
        };
        let mut out = Vec::new();
        self.scan_dir_cluster(&mut dev, target_clus, false, |e| {
            out.push(DirEntry {
                name: e.name,
                is_dir: e.is_dir,
                size: u64::from(e.size),
            });
            out.len() < 384
        })?;
        Ok(out)
    }
}

fn names_eq(a: &str, b: &str) -> bool {
    a.chars().flat_map(char::to_lowercase).eq(b.chars().flat_map(char::to_lowercase))
}

fn decode_lfn_utf16_slice(ent: &[u8]) -> [u16; 13] {
    let mut out = [0xFFFFu16; 13];
    let offsets = [
        1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30,
    ];
    for (idx, &o) in offsets.iter().enumerate() {
        out[idx] = u16::from_le_bytes([ent[o], ent[o + 1]]);
    }
    out
}

fn encode_lfn_entry(ord: u8, csum: u8, chars: &[u16; 13]) -> [u8; 32] {
    let mut ent = [0xFFu8; 32];
    ent[0] = ord;
    ent[11] = 0x0F;
    ent[12] = 0x00;
    ent[13] = csum;
    ent[26] = 0x00;
    ent[27] = 0x00;
    let offsets = [
        1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30,
    ];
    for (idx, &o) in offsets.iter().enumerate() {
        let bytes = chars[idx].to_le_bytes();
        ent[o] = bytes[0];
        ent[o + 1] = bytes[1];
    }
    ent
}

fn decode_sfn(sfn: &[u8; 11], nt_res: u8) -> String {
    let lower_base = (nt_res & 0x08) != 0;
    let lower_ext = (nt_res & 0x10) != 0;
    let base: String = sfn[0..8]
        .iter()
        .map(|&b| {
            let c = b as char;
            if lower_base { c.to_ascii_lowercase() } else { c }
        })
        .collect::<String>()
        .trim_end()
        .to_string();
    let ext: String = sfn[8..11]
        .iter()
        .map(|&b| {
            let c = b as char;
            if lower_ext { c.to_ascii_lowercase() } else { c }
        })
        .collect::<String>()
        .trim_end()
        .to_string();
    if ext.is_empty() {
        base
    } else {
        let mut s = base;
        s.push('.');
        s.push_str(&ext);
        s
    }
}

fn sfn_checksum(sfn: &[u8; 11]) -> u8 {
    let mut sum: u8 = 0;
    for &b in sfn {
        sum = sum.rotate_right(1).wrapping_add(b);
    }
    sum
}

fn make_unique_sfn(name: &str, attempt: u8) -> [u8; 11] {
    let mut sfn = [b' '; 11];
    let (stem_raw, ext_raw) = match name.rsplit_once('.') {
        Some((s, e)) => (s, e),
        None => (name, ""),
    };
    let clean_stem: Vec<u8> = stem_raw
        .bytes()
        .filter(|b| b.is_ascii_alphanumeric())
        .map(|b| b.to_ascii_uppercase())
        .collect();
    let mut hash: u32 = 0x811C_9DC5;
    for b in name.as_bytes() {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash = hash.wrapping_add(u32::from(attempt).wrapping_mul(0x9E37_79B9));
    // 5-char stem + ~XX deterministic tag avoids SFN clashes cleanly
    for (i, dst) in sfn.iter_mut().enumerate().take(5) {
        *dst = *clean_stem.get(i).unwrap_or(&b'V');
    }
    sfn[5] = b'~';
    let hex = b"0123456789ABCDEF";
    sfn[6] = hex[((hash >> 4) & 0x0F) as usize];
    sfn[7] = hex[(hash & 0x0F) as usize];

    let clean_ext: Vec<u8> = ext_raw
        .bytes()
        .filter(|b| b.is_ascii_alphanumeric())
        .map(|b| b.to_ascii_uppercase())
        .take(3)
        .collect();
    for (i, &b) in clean_ext.iter().enumerate() {
        sfn[8 + i] = b;
    }
    sfn
}
