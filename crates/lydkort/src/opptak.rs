//! Opptak til fil: 48 kHz / 32-bit flyttall WAV.
//!
//! - Skrives i en egen tråd, så disken aldri kan få lyden til å hakke.
//! - Filhodet oppdateres hvert annet sekund: krasjer maskinen, er alt fram
//!   til da spillbart.
//! - Blir fila over 4 GB (ca. 3 timer stereo), gjøres den automatisk om til
//!   RF64 (EBU-standarden for store WAV-filer) når opptaket avsluttes.

use kjerne::SAMPLERATE;
use ringbuf::traits::{Consumer, Observer};
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const KANALER: u16 = 2;
const BYTES_PER_SAMPLE: u16 = 4;
const RAMME: u64 = (KANALER * BYTES_PER_SAMPLE) as u64;
/// Størrelsen på filhodet (RIFF + JUNK/ds64 + fmt + fact + data-hode).
const HODE: u64 = 12 + (8 + 28) + (8 + 18) + (8 + 4) + 8;

/// Skriver en WAV-fil som kan bli RF64.
pub struct WavSkriver {
    fil: std::io::BufWriter<File>,
    data_bytes: u64,
    grense: u64,
}

impl WavSkriver {
    pub fn ny(sti: &Path) -> std::io::Result<Self> {
        Self::med_grense(sti, u32::MAX as u64 - HODE)
    }

    /// `grense` = hvor mange databytes før fila må bli RF64 (kun mindre i tester).
    pub fn med_grense(sti: &Path, grense: u64) -> std::io::Result<Self> {
        let f = File::create(sti)?;
        let mut w = WavSkriver { fil: std::io::BufWriter::with_capacity(1 << 20, f), data_bytes: 0, grense };
        w.skriv_hode(false)?;
        Ok(w)
    }

    fn skriv_hode(&mut self, rf64: bool) -> std::io::Result<()> {
        let rammer = self.data_bytes / RAMME;
        let riff_storrelse = HODE - 8 + self.data_bytes;
        let f = &mut self.fil;
        f.seek(SeekFrom::Start(0))?;
        if rf64 {
            f.write_all(b"RF64")?;
            f.write_all(&u32::MAX.to_le_bytes())?;
            f.write_all(b"WAVE")?;
            f.write_all(b"ds64")?;
            f.write_all(&28u32.to_le_bytes())?;
            f.write_all(&riff_storrelse.to_le_bytes())?;
            f.write_all(&self.data_bytes.to_le_bytes())?;
            f.write_all(&rammer.to_le_bytes())?;
            f.write_all(&0u32.to_le_bytes())?;
        } else {
            f.write_all(b"RIFF")?;
            f.write_all(&(riff_storrelse as u32).to_le_bytes())?;
            f.write_all(b"WAVE")?;
            f.write_all(b"JUNK")?;
            f.write_all(&28u32.to_le_bytes())?;
            f.write_all(&[0u8; 28])?;
        }
        // fmt: IEEE float, stereo, 48 kHz
        f.write_all(b"fmt ")?;
        f.write_all(&18u32.to_le_bytes())?;
        f.write_all(&3u16.to_le_bytes())?;
        f.write_all(&KANALER.to_le_bytes())?;
        f.write_all(&SAMPLERATE.to_le_bytes())?;
        f.write_all(&(SAMPLERATE * RAMME as u32).to_le_bytes())?;
        f.write_all(&(RAMME as u16).to_le_bytes())?;
        f.write_all(&(BYTES_PER_SAMPLE * 8).to_le_bytes())?;
        f.write_all(&0u16.to_le_bytes())?;
        // fact: antall rammer
        f.write_all(b"fact")?;
        f.write_all(&4u32.to_le_bytes())?;
        f.write_all(&(if rf64 { u32::MAX } else { rammer as u32 }).to_le_bytes())?;
        f.write_all(b"data")?;
        f.write_all(&(if rf64 { u32::MAX } else { self.data_bytes as u32 }).to_le_bytes())?;
        debug_assert_eq!(f.stream_position()?, HODE);
        f.seek(SeekFrom::Start(HODE + self.data_bytes))?;
        Ok(())
    }

    /// Skriver sammenflettede stereo-samples (V, H, V, H …).
    pub fn skriv(&mut self, sampler: &[f32]) -> std::io::Result<()> {
        let mut bytes = Vec::with_capacity(sampler.len() * 4);
        for s in sampler {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        self.fil.write_all(&bytes)?;
        self.data_bytes += bytes.len() as u64;
        Ok(())
    }

    /// Oppdaterer filhodet og skriver alt til disk (krasjsikring).
    pub fn sikre(&mut self) -> std::io::Result<()> {
        let rf64 = self.data_bytes > self.grense;
        self.skriv_hode(rf64)?;
        self.fil.flush()?;
        self.fil.get_ref().sync_data()
    }

    pub fn avslutt(mut self) -> std::io::Result<u64> {
        self.sikre()?;
        Ok(self.data_bytes)
    }

    pub fn sekunder(&self) -> f64 {
        (self.data_bytes / RAMME) as f64 / SAMPLERATE as f64
    }
}

/// Hva som tas opp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Buss {
    /// Ferdig program, slik lytterne hører det.
    Program,
    /// DJ-lyden helt urørt.
    DjRen,
}

impl Buss {
    pub fn navn(&self) -> &'static str {
        match self {
            Buss::Program => "Program",
            Buss::DjRen => "DJ-ren",
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct OpptakInfo {
    pub sti: PathBuf,
    pub sekunder: f64,
    pub bytes: u64,
    /// Rammer som ikke rakk fram til fila (skal være 0).
    pub tapt: u64,
}

/// Et kjørende opptak. Avsluttes med `Styring::stopp_opptak`.
pub struct Opptak {
    pub id: u64,
    pub buss: Buss,
    pub sti: PathBuf,
    stopp: Arc<AtomicBool>,
    pub(crate) rammer: Arc<AtomicU64>,
    pub(crate) tapt: Arc<AtomicU64>,
    traad: Option<JoinHandle<std::io::Result<u64>>>,
}

impl Opptak {
    pub(crate) fn start(id: u64, buss: Buss, sti: PathBuf, mut cons: ringbuf::HeapCons<f32>) -> std::io::Result<Self> {
        if let Some(mappe) = sti.parent() {
            std::fs::create_dir_all(mappe)?;
        }
        let mut skriver = WavSkriver::ny(&sti)?;
        let stopp = Arc::new(AtomicBool::new(false));
        let rammer = Arc::new(AtomicU64::new(0));
        let tapt = Arc::new(AtomicU64::new(0));
        let (stopp2, rammer2) = (stopp.clone(), rammer.clone());
        let traad = std::thread::Builder::new().name(format!("opptak-{}", buss.navn())).spawn(move || {
            let mut buf = vec![0.0f32; 48_000];
            let mut sist_sikret = Instant::now();
            loop {
                let ferdig = stopp2.load(Ordering::Acquire);
                while cons.occupied_len() >= 2 {
                    let n = cons.pop_slice(&mut buf) & !1;
                    if n == 0 {
                        break;
                    }
                    skriver.skriv(&buf[..n])?;
                    rammer2.fetch_add(n as u64 / 2, Ordering::Relaxed);
                }
                if ferdig {
                    return skriver.avslutt();
                }
                if sist_sikret.elapsed() >= Duration::from_secs(2) {
                    skriver.sikre()?;
                    sist_sikret = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })?;
        Ok(Opptak { id, buss, sti, stopp, rammer, tapt, traad: Some(traad) })
    }

    pub fn sekunder(&self) -> f64 {
        self.rammer.load(Ordering::Relaxed) as f64 / SAMPLERATE as f64
    }

    /// Venter til alt er skrevet og lukker fila.
    pub(crate) fn avslutt(mut self) -> std::io::Result<OpptakInfo> {
        // Gi lyd-tråden tid til å slippe bufferen, og skrivetråden tid til å tømme den.
        std::thread::sleep(Duration::from_millis(60));
        self.stopp.store(true, Ordering::Release);
        let bytes = match self.traad.take().map(|t| t.join()) {
            Some(Ok(r)) => r?,
            _ => return Err(std::io::Error::other("Opptakstråden stoppet uventet")),
        };
        Ok(OpptakInfo { sti: self.sti.clone(), sekunder: (bytes / RAMME) as f64 / SAMPLERATE as f64, bytes, tapt: self.tapt.load(Ordering::Relaxed) })
    }
}

impl Drop for Opptak {
    fn drop(&mut self) {
        // Glemt å stoppe? Lukk fila pent likevel.
        self.stopp.store(true, Ordering::Release);
        if let Some(t) = self.traad.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tester {
    use super::*;

    fn tmp(navn: &str) -> PathBuf {
        let d = std::env::temp_dir().join("studio-suite-opptakstest");
        std::fs::create_dir_all(&d).unwrap();
        d.join(navn)
    }

    #[test]
    fn wav_kan_leses_og_er_bit_identisk() {
        let sti = tmp("vanlig.wav");
        let tone = kjerne::testlyd::rosa_stoy(-20.0, 0.5);
        let flett: Vec<f32> = (0..tone.lengde()).flat_map(|i| [tone.v[i], tone.h[i]]).collect();
        let mut w = WavSkriver::ny(&sti).unwrap();
        w.skriv(&flett[..10_000]).unwrap();
        w.sikre().unwrap();
        w.skriv(&flett[10_000..]).unwrap();
        w.avslutt().unwrap();
        let lest = kjerne::testlyd::les_wav(&sti).unwrap();
        assert_eq!(lest, tone);
    }

    #[test]
    fn fil_er_spillbar_midt_i_opptaket() {
        // Etter `sikre` (hvert 2. sekund) skal fila kunne leses selv om den aldri lukkes.
        let sti = tmp("krasj.wav");
        let mut w = WavSkriver::ny(&sti).unwrap();
        w.skriv(&vec![0.25f32; 9600]).unwrap();
        w.sikre().unwrap();
        std::mem::forget(w); // «krasj»
        let lest = kjerne::testlyd::les_wav(&sti).unwrap();
        assert_eq!(lest.lengde(), 4800);
    }

    #[test]
    fn stor_fil_blir_rf64() {
        let sti = tmp("stor.wav");
        let mut w = WavSkriver::med_grense(&sti, 1000).unwrap();
        w.skriv(&vec![0.5f32; 2000]).unwrap();
        w.avslutt().unwrap();
        let b = std::fs::read(&sti).unwrap();
        assert_eq!(&b[0..4], b"RF64");
        assert_eq!(&b[12..16], b"ds64");
        assert_eq!(u64::from_le_bytes(b[28..36].try_into().unwrap()), 8000, "datastørrelse i ds64");
        assert_eq!(b.len() as u64, HODE + 8000);
    }
}
