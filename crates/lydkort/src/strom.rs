//! Lydstrømmen: motoren koblet til ekte lydkort.
//!
//! Utgangens lyd-tråd driver alt: den henter inngangslyd fra ringbufferen,
//! lyd fra spillerne, kjører motoren og skriver valgt buss til lydkortet.
//! Styring (fader, spill, stopp) sendes inn som kommandoer, og status
//! (nivåmålere, ON AIR, avbrudd) leses ut uten å forstyrre lyden.

use crate::enheter::finn;
use crate::Lydfeil;
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use kjerne::kontroll::Verdi;
use kjerne::motor::{Motor, Nivaamaalere};
use kjerne::{Blokk, SAMPLERATE};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::HeapRb;
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// Hvilken buss som går ut på utgangslydkortet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lytt {
    /// Det programlederen hører (lyttevei, null forsinkelse, cue når solo er på).
    Hodetelefoner,
    /// Studiohøyttalerne (kuttes når mikrofonen er åpen).
    Hoyttalere,
    /// Ferdig program, slik lytterne hører det.
    Program,
}

/// Hvilke kanaler på inngangslydkortet som går til hvilken mikserkanal.
#[derive(Clone, Debug)]
pub struct Inngangsvalg {
    pub kanal_id: String,
    /// Første kanal på lydkortet, talt fra 1 (Focusrite 2i2: 1 = mik, 2 = instrument/linje).
    pub forste: usize,
    pub stereo: bool,
}

#[derive(Clone, Debug)]
pub struct Lydoppsett {
    /// Del av navnet på utgangslydkortet. `None` = maskinens standard.
    pub utgang: Option<String>,
    /// Del av navnet på inngangslydkortet. `None` = ingen inngang.
    pub inngang: Option<String>,
    pub innganger: Vec<Inngangsvalg>,
    pub lytt: Lytt,
    /// Ønsket bufferstørrelse i samples (mindre = lavere forsinkelse).
    pub buffer: u32,
}

impl Default for Lydoppsett {
    fn default() -> Self {
        Lydoppsett { utgang: None, inngang: None, innganger: Vec::new(), lytt: Lytt::Hodetelefoner, buffer: 128 }
    }
}

pub enum Kommando {
    Sett(String, Verdi),
    /// Legg en lydfil i spilleren på en kanal.
    Last(String, Blokk),
    Spill(String),
    Stopp(String),
    /// Spill i løkke.
    Lokke(String, bool),
    /// Start- og stoppunkt (cue) i sekunder.
    Markorer(String, f32, f32),
    /// Hopp til et sted i fila (sekunder).
    Hopp(String, f32),
    StartOpptak(u64, crate::opptak::Buss, ringbuf::HeapProd<f32>, Arc<std::sync::atomic::AtomicU64>),
    StoppOpptak(u64),
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub maalere: Option<Nivaamaalere>,
    /// Høyeste nivå siden sist `hent_topp` ble kalt (til målere i app-vinduet).
    pub topp: Option<Nivaamaalere>,
    pub on_air: bool,
    /// Antall ganger inngangen ikke rakk å levere lyd (bør stå på 0).
    pub inngang_tom: u64,
    /// Antall ganger inngangen ble justert for klokkeforskjell mellom lydkortene.
    pub klokkejustering: u64,
    /// Spillernes posisjon, lengde, cue-punkter og om de spiller.
    pub spillere: HashMap<String, SpillerStatus>,
    pub utgang_navn: String,
    pub inngang_navn: Option<String>,
    pub buffer_ms: f32,
    /// Hvor mange ganger utgangen har bedt om lyd (skal øke hele tiden).
    pub runder_ut: u64,
    /// Hvor mange ganger inngangen har levert lyd (skal øke hele tiden).
    pub runder_inn: u64,
    /// Siste feilmelding fra lydsystemet.
    pub feil: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct SpillerStatus {
    pub pos: f32,
    pub lengde: f32,
    pub spiller: bool,
    pub start: f32,
    pub slutt: f32,
    pub lokke: bool,
}

/// Lengden på de små inn/ut-rampene og kryssfadene (5 ms) som hindrer klikk ved stopp, hopp og løkke.
const RAMPE: f32 = 1.0 / (0.005 * SAMPLERATE as f32);

pub(crate) struct Spiller {
    lyd: Blokk,
    pos: usize,
    spiller: bool,
    lokke: bool,
    /// Cue: der avspillingen starter og stopper.
    start: usize,
    slutt: usize,
    /// Klikkfri rampe: nåværende forsterkning og om den er på vei ned.
    rampe: f32,
    stopper: bool,
    /// Kryssfade ved hopp og løkke: den gamle posisjonen fades ut mens den nye fades inn.
    xf_pos: Option<usize>,
    xf: f32,
    /// Forsterkningen på forrige sample (til å avslutte pent).
    sist_g: f32,
}

impl Spiller {
    fn ny(lyd: Blokk) -> Self {
        let slutt = lyd.lengde();
        Spiller { lyd, pos: 0, spiller: false, lokke: false, start: 0, slutt, rampe: 1.0, stopper: false, xf_pos: None, xf: 0.0, sist_g: 1.0 }
    }

    fn spill(&mut self) {
        if self.spiller {
            // Spiller allerede: et nytt trykk avbryter bare en stopp som er på vei.
            self.stopper = false;
            return;
        }
        if self.pos >= self.slutt.min(self.lyd.lengde()) {
            self.pos = self.start;
        }
        // Fra startpunktet: full styrke med en gang (tight start). Midt i: myk inn.
        self.rampe = if self.pos == self.start { 1.0 } else { 0.0 };
        self.stopper = false;
        self.xf_pos = None;
        self.spiller = true;
    }

    fn stopp(&mut self) {
        if self.spiller {
            self.stopper = true;
        }
    }

    fn stopp_naa(&mut self) {
        self.spiller = false;
        self.stopper = false;
        self.rampe = 1.0;
        self.xf_pos = None;
        self.sist_g = 1.0;
    }

    fn hopp(&mut self, til: usize) {
        let til = til.min(self.lyd.lengde());
        if self.spiller && !self.stopper {
            self.xf_pos = Some(self.pos);
            self.xf = 0.0;
        }
        self.pos = til;
    }

    fn sett_markorer(&mut self, start: usize, slutt: usize) {
        let n = self.lyd.lengde();
        let ny_start = start.min(n);
        let ny_slutt = slutt.clamp(ny_start, n);
        // Står spilleren, og startpunktet flyttes (eller spillehodet havner utenfor), gå til start.
        if !self.spiller && (ny_start != self.start || self.pos < ny_start || self.pos > ny_slutt) {
            self.pos = ny_start;
        }
        self.start = ny_start;
        self.slutt = ny_slutt;
    }

    #[inline]
    fn sample(&self, i: usize) -> (f32, f32) {
        if i < self.lyd.lengde() {
            (self.lyd.v[i], self.lyd.h[i])
        } else {
            (0.0, 0.0)
        }
    }

    fn fyll(&mut self, b: &mut Blokk) {
        if !self.spiller {
            return;
        }
        let len = self.lyd.lengde();
        let slutt = self.slutt.min(len);
        let kan_lokke = self.lokke && slutt > self.start;
        for f in 0..b.lengde() {
            if self.pos >= slutt && !self.stopper {
                if kan_lokke {
                    self.hopp(self.start);
                } else if self.sist_g < 0.01 || self.pos >= len {
                    // Naturlig slutt: lyden er allerede rampet ned.
                    self.stopp_naa();
                    return;
                } else {
                    // Sluttpunktet ble flyttet bak spillehodet: fade ut i stedet for å kutte.
                    self.rampe = self.sist_g;
                    self.stopper = true;
                }
            }
            if self.stopper {
                self.rampe -= RAMPE;
                if self.rampe <= 0.0 {
                    self.stopp_naa();
                    return;
                }
            } else if self.rampe < 1.0 {
                self.rampe = (self.rampe + RAMPE).min(1.0);
            }
            let mut g = self.rampe;
            if !kan_lokke && !self.stopper && self.pos < slutt && slutt < len {
                g = g.min((slutt - self.pos) as f32 * RAMPE);
            }
            let (mut v, mut h) = self.sample(self.pos);
            if let Some(xp) = self.xf_pos {
                let (gv, gh) = self.sample(xp);
                let t = self.xf;
                v = v * t + gv * (1.0 - t);
                h = h * t + gh * (1.0 - t);
                self.xf += RAMPE;
                self.xf_pos = if self.xf >= 1.0 { None } else { Some(xp + 1) };
            }
            b.v[f] = v * g;
            b.h[f] = h * g;
            self.sist_g = g;
            self.pos += 1;
        }
    }

    fn status(&self) -> SpillerStatus {
        let sr = SAMPLERATE as f32;
        SpillerStatus {
            pos: self.pos.min(self.lyd.lengde()) as f32 / sr,
            lengde: self.lyd.lengde() as f32 / sr,
            spiller: self.spiller,
            start: self.start as f32 / sr,
            slutt: self.slutt as f32 / sr,
            lokke: self.lokke,
        }
    }
}

/// Slår av «denormale» tall i prosessoren for lyd-tråden. Bittesmå tall i stillhet
/// kan ellers gjøre filtre og målere opptil ti ganger tregere.
#[inline]
fn denormaler_av() {
    #[cfg(target_arch = "x86_64")]
    #[allow(deprecated)]
    // SAFETY: setter bare FTZ/DAZ-bitene i MXCSR for denne tråden.
    unsafe {
        use std::arch::x86_64::{_mm_getcsr, _mm_setcsr};
        _mm_setcsr(_mm_getcsr() | 0x8040);
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: setter bare FZ-biten (24) i FPCR for denne tråden.
    unsafe {
        let mut fpcr: u64;
        std::arch::asm!("mrs {}, fpcr", out(reg) fpcr);
        fpcr |= 1 << 24;
        std::arch::asm!("msr fpcr, {}", in(reg) fpcr);
    }
}

/// En kjørende lydstrøm. Lyden stopper når denne slippes.
pub struct Lydstrom {
    _ut: cpal::Stream,
    _inn: Option<cpal::Stream>,
    kommandoer: Sender<Kommando>,
    status: Arc<Mutex<Status>>,
}

/// Styring av en lydstrøm som kan sendes til andre tråder (f.eks. app-vinduet).
#[derive(Clone)]
pub struct Styring {
    kommandoer: Sender<Kommando>,
    status: Arc<Mutex<Status>>,
}

impl Styring {
    pub fn send(&self, k: Kommando) {
        let _ = self.kommandoer.send(k);
    }

    /// Status, med målere som viser høyeste nivå siden forrige kall.
    pub fn hent_topp(&self) -> Status {
        match self.status.lock() {
            Ok(mut s) => {
                let ut = s.clone();
                s.topp = None;
                ut
            }
            Err(_) => Status::default(),
        }
    }

    /// Starter opptak av en buss til en WAV-fil.
    pub fn start_opptak(&self, buss: crate::opptak::Buss, sti: std::path::PathBuf) -> std::io::Result<crate::opptak::Opptak> {
        use std::sync::atomic::AtomicU64;
        static NESTE: AtomicU64 = AtomicU64::new(1);
        let id = NESTE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // 10 sekunder buffer: disken kan henge en god stund uten at noe går tapt.
        let (prod, cons) = HeapRb::<f32>::new(SAMPLERATE as usize * 2 * 10).split();
        let o = crate::opptak::Opptak::start(id, buss, sti, cons)?;
        self.send(Kommando::StartOpptak(id, buss, prod, o.tapt.clone()));
        Ok(o)
    }

    /// Stopper opptaket og lukker fila.
    pub fn stopp_opptak(&self, opptak: crate::opptak::Opptak) -> std::io::Result<crate::opptak::OpptakInfo> {
        self.send(Kommando::StoppOpptak(opptak.id));
        opptak.avslutt()
    }

    pub fn status(&self) -> Status {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

impl Lydstrom {
    pub fn send(&self, k: Kommando) {
        let _ = self.kommandoer.send(k);
    }

    pub fn status(&self) -> Status {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn start_opptak(&self, buss: crate::opptak::Buss, sti: std::path::PathBuf) -> std::io::Result<crate::opptak::Opptak> {
        self.styring().start_opptak(buss, sti)
    }

    pub fn stopp_opptak(&self, opptak: crate::opptak::Opptak) -> std::io::Result<crate::opptak::OpptakInfo> {
        self.styring().stopp_opptak(opptak)
    }

    pub fn styring(&self) -> Styring {
        Styring { kommandoer: self.kommandoer.clone(), status: self.status.clone() }
    }

    pub fn start(motor: Motor, oppsett: Lydoppsett) -> Result<Self, Lydfeil> {
        let ut_enhet = finn(oppsett.utgang.as_deref(), false)?;
        let ut_navn = ut_enhet.name().unwrap_or_default();
        let ut_cfg = velg_konfig(&ut_enhet, false, 2, &ut_navn)?;
        let ut_kanaler = ut_cfg.channels() as usize;

        // Inngang (valgfritt)
        let inn_kanaler_trengs: usize = oppsett.innganger.iter().map(|v| v.forste + if v.stereo { 1 } else { 0 }).max().unwrap_or(0);
        let rb_frames = 8192;
        let maal_fyll = (oppsett.buffer as usize * 3).max(512);
        let (mut prod, cons, inn_info) = {
            let antall = oppsett.innganger.len().max(1) * 2;
            let rb = HeapRb::<f32>::new(rb_frames * antall);
            let (p, c) = rb.split();
            (p, c, antall)
        };
        let status = Arc::new(Mutex::new(Status {
            utgang_navn: ut_navn.clone(),
            buffer_ms: oppsett.buffer as f32 * 1000.0 / SAMPLERATE as f32,
            ..Default::default()
        }));

        let inn_runder = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let inn_strom = match (&oppsett.inngang, oppsett.innganger.is_empty()) {
            (Some(navn), false) => {
                let enhet = finn(Some(navn), true)?;
                let inn_navn = enhet.name().unwrap_or_default();
                let cfg = velg_konfig(&enhet, true, inn_kanaler_trengs, &inn_navn)?;
                let kanaler = cfg.channels() as usize;
                let valg = oppsett.innganger.clone();
                let mut ramme = vec![0.0f32; inn_info];
                let strom_cfg = strom_konfig(&cfg, oppsett.buffer);
                let st_feil = status.clone();
                let feil = move |e: cpal::StreamError| {
                    if let Ok(mut s) = st_feil.lock() {
                        s.feil = Some(format!("Inngangen: {e}"));
                    }
                };
                let inn_teller = inn_runder.clone();
                let mut skriv = move |data: &[f32]| {
                    inn_teller.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    for frame in data.chunks(kanaler) {
                        for (i, v) in valg.iter().enumerate() {
                            let a = frame.get(v.forste - 1).copied().unwrap_or(0.0);
                            let b = if v.stereo { frame.get(v.forste).copied().unwrap_or(0.0) } else { a };
                            ramme[i * 2] = a;
                            ramme[i * 2 + 1] = b;
                        }
                        // Full buffer: dropp heller enn å blokkere lyd-tråden.
                        prod.push_slice(&ramme);
                    }
                };
                let s = match cfg.sample_format() {
                    cpal::SampleFormat::F32 => enhet.build_input_stream(&strom_cfg, move |d: &[f32], _| skriv(d), feil, None),
                    cpal::SampleFormat::I16 => {
                        let mut buf = Vec::new();
                        enhet.build_input_stream(&strom_cfg, move |d: &[i16], _| { buf.clear(); buf.extend(d.iter().map(|s| s.to_sample::<f32>())); skriv(&buf) }, feil, None)
                    }
                    cpal::SampleFormat::I32 => {
                        let mut buf = Vec::new();
                        enhet.build_input_stream(&strom_cfg, move |d: &[i32], _| { buf.clear(); buf.extend(d.iter().map(|s| s.to_sample::<f32>())); skriv(&buf) }, feil, None)
                    }
                    annet => return Err(Lydfeil::Lydsystem(format!("Ukjent lydformat på inngangen: {annet:?}"))),
                }
                .map_err(|e| Lydfeil::Lydsystem(e.to_string()))?;
                if let Ok(mut st) = status.lock() {
                    st.inngang_navn = Some(inn_navn);
                }
                Some(s)
            }
            _ => None,
        };

        let (tx, rx) = channel();
        let (soppel_tx, soppel_rx) = channel::<Spiller>();
        let _ = std::thread::Builder::new().name("lydkort-rydding".into()).spawn(move || for gammel in soppel_rx { drop(gammel) });
        let mut lyd = Lydtraad {
            motor,
            rx,
            cons,
            inn_ider: oppsett.innganger.iter().map(|v| v.kanal_id.clone()).collect(),
            inn_bredde: inn_info,
            maal_fyll,
            spillere: HashMap::new(),
            status: status.clone(),
            lytt: oppsett.lytt,
            ut_kanaler,
            har_inngang: inn_strom.is_some(),
            fylt_opp: false,
            tom: 0,
            justert: 0,
            mellom: Vec::new(),
            opptak: Vec::new(),
            flett: Vec::new(),
            runder_ut: 0,
            runder_inn: inn_runder.clone(),
            soppel: soppel_tx,
            siden_status: 0,
        };
        let strom_cfg = strom_konfig(&ut_cfg, oppsett.buffer);
        let st_feil = status.clone();
        let feil = move |e: cpal::StreamError| {
            if let Ok(mut s) = st_feil.lock() {
                s.feil = Some(format!("Utgangen: {e}"));
            }
        };
        let ut_strom = match ut_cfg.sample_format() {
            cpal::SampleFormat::F32 => bygg_utgang::<f32>(&ut_enhet, &strom_cfg, lyd, feil),
            cpal::SampleFormat::I16 => bygg_utgang::<i16>(&ut_enhet, &strom_cfg, lyd, feil),
            cpal::SampleFormat::I32 => bygg_utgang::<i32>(&ut_enhet, &strom_cfg, lyd, feil),
            annet => {
                lyd.motor.nullstill();
                return Err(Lydfeil::Lydsystem(format!("Ukjent lydformat på utgangen: {annet:?}")));
            }
        }
        .map_err(|e| Lydfeil::Lydsystem(e.to_string()))?;

        // Utgangen først: den driver motoren, og skal gå selv om inngangen sperres.
        ut_strom.play().map_err(|e| Lydfeil::Lydsystem(e.to_string()))?;
        if let Some(s) = &inn_strom {
            if let Err(e) = s.play() {
                if let Ok(mut st) = status.lock() {
                    st.feil = Some(format!("Inngangen kunne ikke starte: {e}"));
                }
            }
        }
        Ok(Lydstrom { _ut: ut_strom, _inn: inn_strom, kommandoer: tx, status })
    }
}

fn velg_konfig(enhet: &cpal::Device, inngang: bool, min_kanaler: usize, navn: &str) -> Result<cpal::SupportedStreamConfig, Lydfeil> {
    let alle: Vec<cpal::SupportedStreamConfigRange> = if inngang {
        enhet.supported_input_configs().map_err(|e| Lydfeil::Lydsystem(e.to_string()))?.collect()
    } else {
        enhet.supported_output_configs().map_err(|e| Lydfeil::Lydsystem(e.to_string()))?.collect()
    };
    let med_48k: Vec<_> = alle
        .into_iter()
        .filter(|r| r.min_sample_rate().0 <= SAMPLERATE && r.max_sample_rate().0 >= SAMPLERATE)
        .collect();
    if med_48k.is_empty() {
        return Err(Lydfeil::Ikke48k(navn.into()));
    }
    let maks = med_48k.iter().map(|r| r.channels() as usize).max().unwrap_or(0);
    if maks < min_kanaler {
        return Err(Lydfeil::ForFaaKanaler { enhet: navn.into(), har: maks, trenger: min_kanaler });
    }
    // Foretrekk 32-bit flyttall, og færrest mulig kanaler som dekker behovet.
    let rangering = |f: cpal::SampleFormat| match f {
        cpal::SampleFormat::F32 => 0,
        cpal::SampleFormat::I32 => 1,
        cpal::SampleFormat::I16 => 2,
        _ => 9,
    };
    let beste = med_48k
        .into_iter()
        .filter(|r| r.channels() as usize >= min_kanaler.max(1))
        .min_by_key(|r| (rangering(r.sample_format()), r.channels()))
        .ok_or_else(|| Lydfeil::ForFaaKanaler { enhet: navn.into(), har: maks, trenger: min_kanaler })?;
    Ok(beste.with_sample_rate(cpal::SampleRate(SAMPLERATE)))
}

fn strom_konfig(cfg: &cpal::SupportedStreamConfig, buffer: u32) -> cpal::StreamConfig {
    let mut c = cfg.config();
    if let cpal::SupportedBufferSize::Range { min, max } = cfg.buffer_size() {
        c.buffer_size = cpal::BufferSize::Fixed(buffer.clamp(*min, *max));
    }
    c
}

struct Lydtraad {
    motor: Motor,
    rx: Receiver<Kommando>,
    cons: ringbuf::HeapCons<f32>,
    inn_ider: Vec<String>,
    inn_bredde: usize,
    maal_fyll: usize,
    spillere: HashMap<String, Spiller>,
    status: Arc<Mutex<Status>>,
    lytt: Lytt,
    ut_kanaler: usize,
    har_inngang: bool,
    fylt_opp: bool,
    tom: u64,
    justert: u64,
    mellom: Vec<f32>,
    opptak: Vec<(u64, crate::opptak::Buss, ringbuf::HeapProd<f32>, Arc<std::sync::atomic::AtomicU64>)>,
    flett: Vec<f32>,
    runder_ut: u64,
    runder_inn: Arc<std::sync::atomic::AtomicU64>,
    /// Gamle spillere sendes hit og frigjøres utenfor lyd-tråden.
    soppel: Sender<Spiller>,
    /// Samples siden status sist ble oppdatert (oppdateres ca. 50 ganger i sekundet).
    siden_status: usize,
}

impl Lydtraad {
    fn kommandoer(&mut self) {
        while let Ok(k) = self.rx.try_recv() {
            match k {
                Kommando::Sett(a, v) => {
                    let _ = self.motor.sett(&a, v);
                }
                Kommando::Last(id, lyd) => {
                    let lokke = self.spillere.get(&id).map(|s| s.lokke).unwrap_or(false);
                    let mut s = Spiller::ny(lyd);
                    s.lokke = lokke;
                    if let Some(gammel) = self.spillere.insert(id, s) {
                        // Å frigjøre en stor fil kan ta flere millisekunder: gjøres i en annen tråd.
                        let _ = self.soppel.send(gammel);
                    }
                }
                Kommando::Spill(id) => {
                    if let Some(s) = self.spillere.get_mut(&id) {
                        s.spill();
                    }
                }
                Kommando::Stopp(id) => {
                    if let Some(s) = self.spillere.get_mut(&id) {
                        s.stopp();
                    }
                }
                Kommando::Markorer(id, start, slutt) => {
                    if let Some(s) = self.spillere.get_mut(&id) {
                        let sr = SAMPLERATE as f32;
                        s.sett_markorer((start.max(0.0) * sr) as usize, (slutt.max(0.0) * sr) as usize);
                    }
                }
                Kommando::Hopp(id, sek) => {
                    if let Some(s) = self.spillere.get_mut(&id) {
                        s.hopp((sek.max(0.0) * SAMPLERATE as f32) as usize);
                    }
                }
                Kommando::Lokke(id, p) => {
                    if let Some(s) = self.spillere.get_mut(&id) {
                        s.lokke = p;
                    }
                }
                Kommando::StartOpptak(id, buss, prod, tapt) => self.opptak.push((id, buss, prod, tapt)),
                Kommando::StoppOpptak(id) => self.opptak.retain(|o| o.0 != id),
            }
        }
    }

    /// Lager én runde med lyd (`n` samples) og returnerer valgt buss.
    fn lag(&mut self, n: usize) -> Blokk {
        self.runder_ut += 1;
        self.kommandoer();
        let mut inn: HashMap<String, Blokk> = HashMap::new();

        // Inngangen: vent til bufferen er fylt opp før vi begynner å lese,
        // og juster forsiktig hvis lydkortene går i litt ulikt tempo.
        if self.har_inngang {
            let bredde = self.inn_bredde;
            let fyll = self.cons.occupied_len() / bredde;
            if !self.fylt_opp && fyll >= self.maal_fyll {
                self.fylt_opp = true;
            }
            if self.fylt_opp {
                if fyll > self.maal_fyll * 2 {
                    // Inngangen går litt fortere enn utgangen: hopp over ett sample
                    // om gangen (uhørbart), til bufferen er tilbake på normalt nivå.
                    let hopp = bredde;
                    self.mellom.resize(hopp, 0.0);
                    self.cons.pop_slice(&mut self.mellom);
                    self.justert += 1;
                }
                self.mellom.resize(n * bredde, 0.0);
                let fikk = self.cons.pop_slice(&mut self.mellom) / bredde;
                if fikk < n {
                    // Inngangen går tregere eller hakket: fyll med stillhet og bygg opp igjen.
                    self.mellom[fikk * bredde..].iter_mut().for_each(|s| *s = 0.0);
                    self.tom += 1;
                    self.fylt_opp = false;
                }
                for (i, id) in self.inn_ider.iter().enumerate() {
                    let mut b = Blokk::stille(n);
                    for f in 0..n {
                        b.v[f] = self.mellom[f * bredde + i * 2];
                        b.h[f] = self.mellom[f * bredde + i * 2 + 1];
                    }
                    inn.insert(id.clone(), b);
                }
            } else {
                for id in &self.inn_ider {
                    inn.insert(id.clone(), Blokk::stille(n));
                }
            }
        }

        // Spillerne
        for (id, s) in self.spillere.iter_mut() {
            let mut b = Blokk::stille(n);
            s.fyll(&mut b);
            inn.insert(id.clone(), b);
        }

        if inn.is_empty() {
            // Motoren må alltid gå, ellers stopper målere, hendelser og glatting.
            inn.insert(String::new(), Blokk::stille(n));
        }
        let ut = self.motor.prosesser(&inn);
        for (_, buss, prod, tapt) in self.opptak.iter_mut() {
            let kilde = match buss {
                crate::opptak::Buss::Program => &ut.program,
                crate::opptak::Buss::DjRen => &ut.dj_ren,
            };
            self.flett.clear();
            for f in 0..kilde.lengde() {
                self.flett.push(kilde.v[f]);
                self.flett.push(kilde.h[f]);
            }
            let skrevet = prod.push_slice(&self.flett);
            if skrevet < self.flett.len() {
                tapt.fetch_add(((self.flett.len() - skrevet) / 2) as u64, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let mut valgt = match self.lytt {
            Lytt::Hodetelefoner => ut.hodetelefoner,
            Lytt::Hoyttalere => ut.hoyttalere,
            Lytt::Program => ut.program,
        };
        if valgt.lengde() < n {
            valgt.v.resize(n, 0.0);
            valgt.h.resize(n, 0.0);
        }

        self.siden_status += n;
        let status_naa = self.siden_status >= SAMPLERATE as usize / 50;
        if let Some(mut st) = status_naa.then(|| self.status.try_lock().ok()).flatten() {
            self.siden_status = 0;
            let n = self.motor.nivaamaalere();
            self.motor.nullstill_maaletopp();
            st.topp = Some(match st.topp.take() {
                Some(mut t) => {
                    t.slaa_sammen(&n);
                    t
                }
                None => n.clone(),
            });
            st.maalere = Some(n);
            st.on_air = self.motor.er_on_air();
            st.runder_ut = self.runder_ut;
            st.runder_inn = self.runder_inn.load(std::sync::atomic::Ordering::Relaxed);
            st.inngang_tom = self.tom;
            st.klokkejustering = self.justert;
            st.spillere = self
                .spillere
                .iter()
                .map(|(id, s)| (id.clone(), s.status()))
                .collect();
        }
        // Fader-start: ON-knappen eller faderen starter og stopper spilleren på kanalen.
        for h in self.motor.hendelser() {
            match h {
                kjerne::hendelse::Hendelse::FaderStart { kanal } => {
                    if let Some(s) = self.spillere.get_mut(&kanal) {
                        if !s.spiller {
                            s.spill();
                        }
                    }
                }
                kjerne::hendelse::Hendelse::FaderStopp { kanal } => {
                    if let Some(s) = self.spillere.get_mut(&kanal) {
                        s.stopp();
                    }
                }
                _ => {}
            }
        }
        valgt
    }
}

fn bygg_utgang<T>(
    enhet: &cpal::Device,
    cfg: &cpal::StreamConfig,
    mut lyd: Lydtraad,
    feil: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + FromSample<f32>,
{
    enhet.build_output_stream(
        cfg,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            denormaler_av();
            let k = lyd.ut_kanaler;
            let n = data.len() / k;
            let b = lyd.lag(n);
            for (f, ramme) in data.chunks_mut(k).enumerate() {
                for (c, s) in ramme.iter_mut().enumerate() {
                    // Kanal 1 = venstre, 2 = høyre, resten stille.
                    let x = match c {
                        0 => b.v[f],
                        1 => b.h[f],
                        _ => 0.0,
                    };
                    *s = T::from_sample(x.clamp(-1.0, 1.0));
                }
            }
        },
        feil,
        None,
    )
}

#[cfg(test)]
mod tester {
    use super::*;
    use kjerne::motor::{KanalOppsett, Kanaltype};

    fn traad(med_inngang: bool) -> (Lydtraad, ringbuf::HeapProd<f32>, Sender<Kommando>) {
        let mut m = Motor::ny();
        m.legg_til_kanal(KanalOppsett::ny("mik1", "Mikrofon 1", Kanaltype::Mikrofon)).unwrap();
        m.sett("kanal/mik1/fader", Verdi::Tall(0.0)).unwrap();
        m.sett("kanal/mik1/paa", Verdi::AvPaa(true)).unwrap();
        m.legg_til_kanal(KanalOppsett::ny("spillerA", "Spiller A", Kanaltype::Spiller)).unwrap();
        let (p, c) = HeapRb::<f32>::new(8192 * 2).split();
        let (tx, rx) = channel();
        let t = Lydtraad {
            motor: m,
            rx,
            cons: c,
            inn_ider: vec!["mik1".into()],
            inn_bredde: 2,
            maal_fyll: 4096,
            spillere: HashMap::new(),
            status: Arc::new(Mutex::new(Status::default())),
            lytt: Lytt::Hodetelefoner,
            ut_kanaler: 2,
            har_inngang: med_inngang,
            fylt_opp: false,
            tom: 0,
            justert: 0,
            mellom: Vec::new(),
            opptak: Vec::new(),
            flett: Vec::new(),
            runder_ut: 0,
            runder_inn: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            soppel: channel().0,
            siden_status: usize::MAX / 2,
        };
        (t, p, tx)
    }

    #[test]
    fn mikrofon_kommer_gjennom_uendret() {
        let (mut t, mut p, _tx) = traad(true);
        // La faderen gli helt opp først (den glir mykt fra −∞ til 0 dB).
        for _ in 0..50 {
            t.lag(128);
        }
        let tone = kjerne::testlyd::tone(1000.0, -18.0, 0.1);
        let ramme: Vec<f32> = tone.v.iter().flat_map(|s| [*s, *s]).collect();
        p.push_slice(&ramme);
        let mut ut = Vec::new();
        for _ in 0..40 {
            ut.extend(t.lag(128).v);
        }
        // Ingen hakk før inngangen er brukt opp, og lyden er nøyaktig den som kom inn.
        assert_eq!(t.tom, 1, "bare ett tomt kall, når inngangen er brukt opp");
        let start = ut.iter().position(|s| *s != 0.0).unwrap() - 1; // første sample i tonen er 0
        assert_eq!(&ut[start..start + 4600], &tone.v[..4600]);
    }

    #[test]
    fn spiller_spiller_og_stopper() {
        let (mut t, _p, tx) = traad(false);
        tx.send(Kommando::Last("spillerA".into(), kjerne::testlyd::tone(440.0, -12.0, 0.01))).unwrap();
        tx.send(Kommando::Spill("spillerA".into())).unwrap();
        let mut ut = Vec::new();
        for _ in 0..10 {
            ut.extend(t.lag(64).v);
        }
        assert!(kjerne::maaling::topp_dbfs(&ut) > -12.5);
        assert!(!t.spillere["spillerA"].spiller, "skal stoppe av seg selv");
        let st = t.status.lock().unwrap();
        assert!(st.maalere.is_some());
    }

    #[test]
    fn opptak_av_program_er_bit_identisk_med_det_som_spilles() {
        let (mut t, _p, tx) = traad(false);
        t.motor.sett("kanal/mik1/fader", Verdi::Tall(-200.0)).unwrap();
        let lyd = kjerne::testlyd::rosa_stoy(-20.0, 0.2);
        tx.send(Kommando::Last("spillerA".into(), lyd.clone())).unwrap();
        tx.send(Kommando::Spill("spillerA".into())).unwrap();
        let sti = std::env::temp_dir().join("studio-suite-opptakstest").join("program.wav");
        let (prod, cons) = HeapRb::<f32>::new(48_000 * 2).split();
        let o = crate::opptak::Opptak::start(7, crate::opptak::Buss::Program, sti.clone(), cons).unwrap();
        tx.send(Kommando::StartOpptak(7, crate::opptak::Buss::Program, prod, o.tapt.clone())).unwrap();
        for _ in 0..100 {
            t.lag(128); // 12 800 samples > 9 600 i lyden
        }
        tx.send(Kommando::StoppOpptak(7)).unwrap();
        t.lag(128);
        let info = o.avslutt().unwrap();
        assert_eq!(info.tapt, 0);
        let lest = kjerne::testlyd::les_wav(&sti).unwrap();
        assert_eq!(lest.utsnitt(0, lyd.lengde()), lyd, "opptaket skal være nøyaktig det som ble spilt");
    }

    #[test]
    fn cue_punkter_og_hopp() {
        let (mut t, _p, tx) = traad(false);
        // 1 sekund lyd der hver sample har verdien av sin egen posisjon (lett å sjekke).
        let n = 48_000;
        let lyd = Blokk { v: (0..n).map(|i| i as f32 / n as f32).collect(), h: vec![0.5; n] };
        tx.send(Kommando::Last("spillerA".into(), lyd)).unwrap();
        tx.send(Kommando::Markorer("spillerA".into(), 0.25, 0.5)).unwrap();
        tx.send(Kommando::Spill("spillerA".into())).unwrap();
        t.lag(64);
        // Starter nøyaktig på startpunktet, med full styrke.
        let st = t.spillere["spillerA"].status();
        assert!((st.start - 0.25).abs() < 1e-4 && st.spiller);
        let mut ut = Vec::new();
        for _ in 0..400 {
            ut.extend(t.lag(64).h);
        }
        let spilt = ut.iter().filter(|x| **x != 0.0).count();
        assert!((spilt as i64 - 12_000 + 64).abs() < 300, "spilte {spilt} samples, skal være ca. 0,25 s");
        assert!(!t.spillere["spillerA"].spiller, "stopper ved stoppunktet");
        // Hopp
        tx.send(Kommando::Hopp("spillerA".into(), 0.4)).unwrap();
        t.lag(64);
        assert!((t.spillere["spillerA"].status().pos - 0.4).abs() < 0.01);
    }

    #[test]
    fn stopp_er_klikkfri() {
        let (mut t, _p, tx) = traad(false);
        tx.send(Kommando::Last("spillerA".into(), Blokk { v: vec![0.5; 48_000], h: vec![0.5; 48_000] })).unwrap();
        tx.send(Kommando::Spill("spillerA".into())).unwrap();
        for _ in 0..20 {
            t.lag(64);
        }
        tx.send(Kommando::Stopp("spillerA".into())).unwrap();
        let mut ut = Vec::new();
        for _ in 0..10 {
            ut.extend(t.lag(64).v);
        }
        // Ingen sprang større enn rampen tillater (ett klikk ville vært et hopp på 0,5).
        let maks_sprang = ut.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(maks_sprang < 0.01, "sprang {maks_sprang}");
        assert!(!t.spillere["spillerA"].spiller);
    }

    #[test]
    fn fader_start_starter_og_stopper_spilleren() {
        let (mut t, _p, tx) = traad(false);
        tx.send(Kommando::Last("spillerA".into(), Blokk { v: vec![0.5; 48_000], h: vec![0.5; 48_000] })).unwrap();
        tx.send(Kommando::Sett("kanal/spillerA/paa".into(), Verdi::AvPaa(false))).unwrap();
        t.lag(64);
        assert!(!t.spillere["spillerA"].spiller);
        tx.send(Kommando::Sett("kanal/spillerA/paa".into(), Verdi::AvPaa(true))).unwrap();
        t.lag(64);
        assert!(t.spillere["spillerA"].spiller, "ON skal starte spilleren");
        tx.send(Kommando::Sett("kanal/spillerA/fader".into(), Verdi::Tall(-200.0))).unwrap();
        for _ in 0..10 {
            t.lag(64);
        }
        assert!(!t.spillere["spillerA"].spiller, "fader ned skal stoppe");
    }

    /// Største sprang mellom to samples – et klikk gir et stort sprang.
    fn maks_sprang(x: &[f32]) -> f32 {
        x.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max)
    }

    fn jevn_lyd() -> Blokk {
        // Lav sinus: naturlige sprang er små (< 0,01), et klikk ville vært stort.
        kjerne::testlyd::tone(50.0, -6.0, 3.0)
    }

    #[test]
    fn sluttpunkt_bak_spillehodet_fader_ut_i_stedet_for_aa_kutte() {
        let (mut t, _p, tx) = traad(false);
        tx.send(Kommando::Last("spillerA".into(), jevn_lyd())).unwrap();
        tx.send(Kommando::Spill("spillerA".into())).unwrap();
        let mut ut = Vec::new();
        for _ in 0..200 {
            ut.extend(t.lag(64).v);
        }
        // Sett slutt 50 ms bak der spillehodet er nå (som når du trykker «Slutt» med forsinket visning).
        tx.send(Kommando::Markorer("spillerA".into(), 0.0, 0.2)).unwrap();
        for _ in 0..20 {
            ut.extend(t.lag(64).v);
        }
        assert!(maks_sprang(&ut) < 0.02, "sprang {}", maks_sprang(&ut));
        assert!(!t.spillere["spillerA"].spiller);
    }

    #[test]
    fn loekke_og_nytt_spilltrykk_gir_ingen_dupp() {
        let (mut t, _p, tx) = traad(false);
        tx.send(Kommando::Last("spillerA".into(), jevn_lyd())).unwrap();
        tx.send(Kommando::Spill("spillerA".into())).unwrap();
        let mut ut = Vec::new();
        for _ in 0..50 {
            ut.extend(t.lag(64).v);
        }
        tx.send(Kommando::Lokke("spillerA".into(), true)).unwrap();
        tx.send(Kommando::Spill("spillerA".into())).unwrap();
        for _ in 0..50 {
            ut.extend(t.lag(64).v);
        }
        assert!(maks_sprang(&ut) < 0.02, "sprang {}", maks_sprang(&ut));
    }

    #[test]
    fn hopp_og_loekke_kryssfader() {
        let (mut t, _p, tx) = traad(false);
        tx.send(Kommando::Last("spillerA".into(), jevn_lyd())).unwrap();
        tx.send(Kommando::Lokke("spillerA".into(), true)).unwrap();
        tx.send(Kommando::Markorer("spillerA".into(), 0.0, 0.5)).unwrap();
        tx.send(Kommando::Spill("spillerA".into())).unwrap();
        let mut ut = Vec::new();
        for i in 0..1000 {
            if i == 100 {
                tx.send(Kommando::Hopp("spillerA".into(), 0.123)).unwrap();
            }
            ut.extend(t.lag(64).v);
        }
        // 64 000 samples med en løkke på 0,5 s = flere løkker og ett hopp, uten klikk.
        assert!(maks_sprang(&ut) < 0.03, "sprang {}", maks_sprang(&ut));
        assert!(t.spillere["spillerA"].spiller);
    }

    #[test]
    fn markorer_flytter_spillehodet_naar_spilleren_staar() {
        let (mut t, _p, tx) = traad(false);
        tx.send(Kommando::Last("spillerA".into(), jevn_lyd())).unwrap();
        tx.send(Kommando::Markorer("spillerA".into(), 0.4, 2.0)).unwrap();
        t.lag(64);
        assert!((t.spillere["spillerA"].status().pos - 0.4).abs() < 1e-3);
        // Hopp mens den står, og sett en markør som ikke endrer start: posisjonen beholdes.
        tx.send(Kommando::Hopp("spillerA".into(), 1.0)).unwrap();
        tx.send(Kommando::Markorer("spillerA".into(), 0.4, 2.5)).unwrap();
        t.lag(64);
        assert!((t.spillere["spillerA"].status().pos - 1.0).abs() < 1e-3);
    }
}
