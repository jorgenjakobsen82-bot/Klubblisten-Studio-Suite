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
    StartOpptak(u64, crate::opptak::Buss, ringbuf::HeapProd<f32>, Arc<std::sync::atomic::AtomicU64>),
    StoppOpptak(u64),
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub maalere: Option<Nivaamaalere>,
    pub on_air: bool,
    /// Antall ganger inngangen ikke rakk å levere lyd (bør stå på 0).
    pub inngang_tom: u64,
    /// Antall ganger inngangen ble justert for klokkeforskjell mellom lydkortene.
    pub klokkejustering: u64,
    /// Spillernes posisjon i sekunder og om de spiller.
    pub spillere: HashMap<String, (f32, f32, bool)>,
    pub utgang_navn: String,
    pub inngang_navn: Option<String>,
    pub buffer_ms: f32,
}

struct Spiller {
    lyd: Blokk,
    pos: usize,
    spiller: bool,
    lokke: bool,
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

        let inn_strom = match (&oppsett.inngang, oppsett.innganger.is_empty()) {
            (Some(navn), false) => {
                let enhet = finn(Some(navn), true)?;
                let inn_navn = enhet.name().unwrap_or_default();
                let cfg = velg_konfig(&enhet, true, inn_kanaler_trengs, &inn_navn)?;
                let kanaler = cfg.channels() as usize;
                let valg = oppsett.innganger.clone();
                let mut ramme = vec![0.0f32; inn_info];
                let strom_cfg = strom_konfig(&cfg, oppsett.buffer);
                let feil = |e| eprintln!("Feil på inngangen: {e}");
                let mut skriv = move |data: &[f32]| {
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
        };
        let strom_cfg = strom_konfig(&ut_cfg, oppsett.buffer);
        let feil = |e| eprintln!("Feil på utgangen: {e}");
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

        if let Some(s) = &inn_strom {
            s.play().map_err(|e| Lydfeil::Lydsystem(e.to_string()))?;
        }
        ut_strom.play().map_err(|e| Lydfeil::Lydsystem(e.to_string()))?;
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
}

impl Lydtraad {
    fn kommandoer(&mut self) {
        while let Ok(k) = self.rx.try_recv() {
            match k {
                Kommando::Sett(a, v) => {
                    let _ = self.motor.sett(&a, v);
                }
                Kommando::Last(id, lyd) => {
                    self.spillere.insert(id, Spiller { lyd, pos: 0, spiller: false, lokke: false });
                }
                Kommando::Spill(id) => {
                    if let Some(s) = self.spillere.get_mut(&id) {
                        if s.pos >= s.lyd.lengde() {
                            s.pos = 0;
                        }
                        s.spiller = true;
                    }
                }
                Kommando::Stopp(id) => {
                    if let Some(s) = self.spillere.get_mut(&id) {
                        s.spiller = false;
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
            if s.spiller {
                for f in 0..n {
                    if s.pos >= s.lyd.lengde() {
                        if s.lokke && s.lyd.lengde() > 0 {
                            s.pos = 0;
                        } else {
                            s.spiller = false;
                            break;
                        }
                    }
                    b.v[f] = s.lyd.v[s.pos];
                    b.h[f] = s.lyd.h[s.pos];
                    s.pos += 1;
                }
            }
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

        if let Ok(mut st) = self.status.try_lock() {
            st.maalere = Some(self.motor.nivaamaalere());
            st.on_air = self.motor.er_on_air();
            st.inngang_tom = self.tom;
            st.klokkejustering = self.justert;
            st.spillere = self
                .spillere
                .iter()
                .map(|(id, s)| (id.clone(), (s.pos as f32 / SAMPLERATE as f32, s.lyd.lengde() as f32 / SAMPLERATE as f32, s.spiller)))
                .collect();
        }
        // Hendelser leses av appen senere; tøm dem så de ikke hoper seg opp.
        self.motor.hendelser();
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
}
