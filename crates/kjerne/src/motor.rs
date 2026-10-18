//! Lydmotoren: kanaler, busser og de to lydveiene.
//!
//! * **Sendeveien** (Master → Icecast, vMix, opptak) tillater moduler med
//!   forsinkelse (look-ahead, plugins). Alle kanaler forsinkes likt, så lyd og
//!   bilde holder synk.
//! * **Lytteveien** (hodetelefoner og høyttalere) slipper aldri inn moduler med
//!   forsinkelse. Den har null intern forsinkelse.
//!
//! Solo er cue/forhåndslytt: den endrer bare hodetelefonene, aldri det som
//! sendes ut. Mikrofoner som åpnes, kutter høyttalerne (monitorkutt) og tenner ON AIR.

use crate::hendelse::{Hendelse, Hendelsesbuss};
use crate::kontroll::{Kontroll, Kontrollfeil, Kontrolltype, RegistrertKontroll, Verdi};
use crate::lyd::{db_til_lin, Blokk, Glatter, BLOKKSTORRELSE, SAMPLERATE};
use crate::modul::Modul;
use crate::moduler::{Forsinkelse, Gain, Inngangstrinn, Kanalmodus};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kanaltype {
    /// Mikrofon: får stemmekjeden, åpner ON AIR og monitorkutt.
    Mikrofon,
    /// Linje stereo, f.eks. DJM-V10 og RØDECaster.
    LinjeStereo,
    /// Avviklingsspiller eller jingler.
    Spiller,
    /// Lyd fra et program på PC-en, f.eks. Spotify eller nettleser.
    AppLyd,
    /// Lyd tilbake fra en plugin eller et annet program.
    Retur,
}

impl Kanaltype {
    pub fn navn(&self) -> &'static str {
        match self {
            Kanaltype::Mikrofon => "MIC",
            Kanaltype::LinjeStereo => "LINJE",
            Kanaltype::Spiller => "SPILLER",
            Kanaltype::AppLyd => "APP",
            Kanaltype::Retur => "RETUR",
        }
    }
}

/// Oppsett for en ny kanal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KanalOppsett {
    pub id: String,
    pub navn: String,
    pub kanaltype: Kanaltype,
    /// Privat-lås: kanalen kan aldri sendes til Master (Icecast, vMix, opptak).
    #[serde(default)]
    pub privat: bool,
    #[serde(default = "sant")]
    pub til_master: bool,
    #[serde(default = "sant")]
    pub til_studio: bool,
    /// DJ-ren: uprosessert og uavhengig av fader, til opptak av DJ-settet.
    #[serde(default)]
    pub til_dj_ren: bool,
}

fn sant() -> bool {
    true
}

impl KanalOppsett {
    pub fn ny(id: &str, navn: &str, kanaltype: Kanaltype) -> Self {
        KanalOppsett { id: id.into(), navn: navn.into(), kanaltype, privat: false, til_master: true, til_studio: true, til_dj_ren: false }
    }
}

pub struct Kanal {
    pub oppsett: KanalOppsett,
    trim: Gain,
    trim_lytt: Gain,
    kjede: Vec<Box<dyn Modul>>,
    /// Lytteveiens kopi av kjeden: moduler med forsinkelse er tatt ut (None).
    lytt_kjede: Vec<Option<Box<dyn Modul>>>,
    kompensasjon: Forsinkelse,
    fader_db: f32,
    fader: Glatter,
    mute: bool,
    solo: bool,
    pan: f32,
    var_aapen: bool,
    /// Inngangstrinn (modus, fase, lavkutt) – ett per lydvei, så filtrene har egen tilstand.
    inn_send: Inngangstrinn,
    inn_lytt: Inngangstrinn,
    /// ON/OFF-knappen, som på en radiokonsoll.
    paa: bool,
    /// Start/stopp spilleren når kanalen slås på/av eller faderen skyves opp/ned.
    faderstart: bool,
    var_oppe: bool,
    /// Hvor lenge kanalen har stått på (samples) – til ON-tidtakeren.
    paa_samples: u64,
    maal: Maalertilstand,
}

/// Målertilstand per kanal og for master.
#[derive(Clone, Debug, Default)]
struct Maalertilstand {
    /// Toppnivå siste periode (lineært), etter fader.
    topp: [f32; 2],
    /// Glidende kvadrert snitt (RMS, 300 ms), etter fader.
    rms2: [f32; 2],
    /// Toppnivå før fader (etter trim) – til å stille inngangsnivået.
    inn_topp: f32,
    /// Antall samples som har klippet siden start (vises til den nullstilles).
    klipp: u32,
}

impl Maalertilstand {
    /// Tidskonstant for RMS-måleren (300 ms, som en VU-måler).
    fn rms_faktor() -> f32 {
        1.0 - (-1.0 / (0.3 * SAMPLERATE as f32)).exp()
    }

    fn maaling(&self, id: &str, paa_sek: f32, aapen: bool, paa: bool) -> KanalMaaling {
        use crate::lyd::lin_til_db;
        KanalMaaling {
            id: id.to_string(),
            topp: [lin_til_db(self.topp[0]), lin_til_db(self.topp[1])],
            rms: [lin_til_db(self.rms2[0].sqrt()), lin_til_db(self.rms2[1].sqrt())],
            inn_topp: lin_til_db(self.inn_topp),
            klipp: self.klipp,
            paa_sek,
            aapen,
            paa,
        }
    }
}

impl Kanal {
    fn ny(oppsett: KanalOppsett) -> Self {
        let mut oppsett = oppsett;
        if oppsett.privat {
            oppsett.til_master = false;
        }
        // Som på en radiokonsoll: faderen står på 0 dB, og mikrofoner starter AV.
        let start_db = 0.0;
        let paa = oppsett.kanaltype != Kanaltype::Mikrofon;
        Kanal {
            oppsett,
            trim: Gain::ny(0.0),
            trim_lytt: Gain::ny(0.0),
            kjede: Vec::new(),
            lytt_kjede: Vec::new(),
            kompensasjon: Forsinkelse::ny(0),
            fader_db: start_db,
            fader: Glatter::ny(if paa { db_til_lin(start_db) } else { 0.0 }, 10.0),
            mute: false,
            solo: false,
            pan: 0.0,
            var_aapen: false,
            inn_send: Inngangstrinn::default(),
            inn_lytt: Inngangstrinn::default(),
            paa,
            faderstart: true,
            var_oppe: paa,
            paa_samples: 0,
            maal: Maalertilstand::default(),
        }
    }

    /// Kanalen gir lyd: slått på, ikke mutet og faderen over −60 dB.
    fn er_oppe(&self) -> bool {
        self.paa && !self.mute && self.fader_db > -60.0
    }

    fn kjede_forsinkelse(&self) -> usize {
        self.kjede.iter().map(|m| m.forsinkelse()).sum()
    }

    fn bygg_lytt_kjede(&mut self) {
        self.lytt_kjede = self.kjede.iter().map(|m| (m.forsinkelse() == 0).then(|| m.klon())).collect();
    }

    /// En mikrofon er åpen når den er slått på, ikke mutet og faderen er oppe.
    pub fn er_aapen(&self) -> bool {
        self.oppsett.kanaltype == Kanaltype::Mikrofon && self.paa && !self.mute && self.fader_db > -100.0
    }

    pub fn kjede(&self) -> &[Box<dyn Modul>] {
        &self.kjede
    }

    pub fn trim_db(&self) -> f32 {
        self.trim.db()
    }

    fn panorering(&self) -> (f32, f32) {
        // Balanse der midtstilling gir nøyaktig 1,0 i begge kanaler (nulltest).
        let p = self.pan.clamp(-1.0, 1.0);
        let v = if p > 0.0 { 1.0 - p } else { 1.0 };
        let h = if p < 0.0 { 1.0 + p } else { 1.0 };
        (v, h)
    }

    fn kanalkontroller() -> Vec<Kontroll> {
        vec![
            Kontroll::ny("fader", "Fader", Kontrolltype::Fader { min_db: f32::NEG_INFINITY, max_db: 10.0 }, Verdi::Tall(0.0)),
            Kontroll::ny("trim", "Inngangstrim", Kontrolltype::Dreieknapp { min: -24.0, maks: 24.0, enhet: "dB".into() }, Verdi::Tall(0.0)),
            Kontroll::ny("mute", "Mute", Kontrolltype::AvPaa, Verdi::AvPaa(false)),
            Kontroll::ny("solo", "Solo (cue)", Kontrolltype::AvPaa, Verdi::AvPaa(false)),
            Kontroll::ny("pan", "Panorering", Kontrolltype::Dreieknapp { min: -1.0, maks: 1.0, enhet: "".into() }, Verdi::Tall(0.0)),
            Kontroll::ny("til_master", "Til Master", Kontrolltype::AvPaa, Verdi::AvPaa(true)),
            Kontroll::ny("til_studio", "Til Studio", Kontrolltype::AvPaa, Verdi::AvPaa(true)),
            Kontroll::ny("til_dj_ren", "Til DJ-ren", Kontrolltype::AvPaa, Verdi::AvPaa(false)),
            Kontroll::ny("paa", "ON/OFF", Kontrolltype::AvPaa, Verdi::AvPaa(true)),
            Kontroll::ny("faderstart", "Fader-start", Kontrolltype::AvPaa, Verdi::AvPaa(true)),
            Kontroll::ny("hpf", "Lavkutt", Kontrolltype::AvPaa, Verdi::AvPaa(false)),
            Kontroll::ny("hpf_hz", "Lavkutt-frekvens", Kontrolltype::Dreieknapp { min: 20.0, maks: 400.0, enhet: "Hz".into() }, Verdi::Tall(80.0)),
            Kontroll::ny("fase", "Fasevending", Kontrolltype::AvPaa, Verdi::AvPaa(false)),
            Kontroll::ny(
                "modus",
                "Kanalmodus",
                Kontrolltype::Valg { valg: ["Stereo", "Mono", "Venstre", "Høyre", "Bytt V/H"].iter().map(|s| s.to_string()).collect() },
                Verdi::Valg(0),
            ),
            Kontroll::ny("nullstill_klipp", "Nullstill klippvarsel", Kontrolltype::Utloser, Verdi::AvPaa(false)),
        ]
    }

    fn sett(&mut self, kontroll: &str, verdi: Verdi) -> Result<(), Kontrollfeil> {
        let feil = || Kontrollfeil::UgyldigVerdi(kontroll.to_string());
        match kontroll {
            "fader" => {
                let db = verdi.som_tall().ok_or_else(feil)?;
                self.fader_db = if db.is_nan() { f32::NEG_INFINITY } else { db.min(10.0) };
            }
            "trim" => {
                self.trim.sett("gain", verdi.clone())?;
                self.trim_lytt.sett("gain", verdi)?;
            }
            "mute" => self.mute = verdi.som_bool().ok_or_else(feil)?,
            "solo" => self.solo = verdi.som_bool().ok_or_else(feil)?,
            "pan" => self.pan = verdi.som_tall().ok_or_else(feil)?.clamp(-1.0, 1.0),
            "til_master" => {
                let paa = verdi.som_bool().ok_or_else(feil)?;
                if paa && self.oppsett.privat {
                    return Err(Kontrollfeil::Sperret(format!(
                        "«{}» har privat-lås og kan ikke sendes til Master, Icecast, vMix eller opptak",
                        self.oppsett.navn
                    )));
                }
                self.oppsett.til_master = paa;
            }
            "til_studio" => self.oppsett.til_studio = verdi.som_bool().ok_or_else(feil)?,
            "til_dj_ren" => self.oppsett.til_dj_ren = verdi.som_bool().ok_or_else(feil)?,
            "paa" => {
                self.paa = verdi.som_bool().ok_or_else(feil)?;
                if !self.paa {
                    self.paa_samples = 0;
                }
            }
            "faderstart" => self.faderstart = verdi.som_bool().ok_or_else(feil)?,
            "hpf" => {
                let p = verdi.som_bool().ok_or_else(feil)?;
                self.inn_send.sett_hpf(p);
                self.inn_lytt.sett_hpf(p);
            }
            "hpf_hz" => {
                let hz = verdi.som_tall().ok_or_else(feil)?;
                self.inn_send.sett_hpf_hz(hz);
                self.inn_lytt.sett_hpf_hz(hz);
            }
            "fase" => {
                let p = verdi.som_bool().ok_or_else(feil)?;
                self.inn_send.fase = p;
                self.inn_lytt.fase = p;
            }
            "modus" => {
                let m = match verdi.som_tall().ok_or_else(feil)? as usize {
                    0 => Kanalmodus::Stereo,
                    1 => Kanalmodus::Mono,
                    2 => Kanalmodus::Venstre,
                    3 => Kanalmodus::Hoyre,
                    4 => Kanalmodus::Bytt,
                    _ => return Err(feil()),
                };
                self.inn_send.modus = m;
                self.inn_lytt.modus = m;
            }
            "nullstill_klipp" => self.maal.klipp = 0,
            _ => return Err(Kontrollfeil::UkjentKontroll(kontroll.to_string())),
        }
        let maal = if self.mute || !self.paa { 0.0 } else { db_til_lin(self.fader_db) };
        self.fader.sett_maal(maal);
        Ok(())
    }

    fn modus_indeks(&self) -> usize {
        match self.inn_send.modus {
            Kanalmodus::Stereo => 0,
            Kanalmodus::Mono => 1,
            Kanalmodus::Venstre => 2,
            Kanalmodus::Hoyre => 3,
            Kanalmodus::Bytt => 4,
        }
    }

    fn hent(&self, kontroll: &str) -> Option<Verdi> {
        Some(match kontroll {
            "fader" => Verdi::Tall(self.fader_db),
            "trim" => Verdi::Tall(self.trim.db()),
            "mute" => Verdi::AvPaa(self.mute),
            "solo" => Verdi::AvPaa(self.solo),
            "pan" => Verdi::Tall(self.pan),
            "til_master" => Verdi::AvPaa(self.oppsett.til_master),
            "til_studio" => Verdi::AvPaa(self.oppsett.til_studio),
            "til_dj_ren" => Verdi::AvPaa(self.oppsett.til_dj_ren),
            "paa" => Verdi::AvPaa(self.paa),
            "faderstart" => Verdi::AvPaa(self.faderstart),
            "hpf" => Verdi::AvPaa(self.inn_send.hpf_paa),
            "hpf_hz" => Verdi::Tall(self.inn_send.hpf_hz),
            "fase" => Verdi::AvPaa(self.inn_send.fase),
            "modus" => Verdi::Valg(self.modus_indeks()),
            _ => return None,
        })
    }
}

/// Motorens utganger for én prosessert periode.
#[derive(Clone, Debug)]
pub struct Utganger {
    /// Ferdig miks etter master-kjeden: til Icecast, vMix og opptak.
    pub program: Blokk,
    /// Uprosessert DJ-lyd til opptak av settet.
    pub dj_ren: Blokk,
    /// Hodetelefoner (lytteveien, null forsinkelse). Viser cue når solo er på.
    pub hodetelefoner: Blokk,
    /// Studiohøyttalere (lytteveien), med monitorkutt når mikrofon er åpen.
    pub hoyttalere: Blokk,
}

impl Utganger {
    fn stille(n: usize) -> Self {
        Utganger { program: Blokk::stille(n), dj_ren: Blokk::stille(n), hodetelefoner: Blokk::stille(n), hoyttalere: Blokk::stille(n) }
    }
}

/// Målinger for én kanal (eller master). Alle nivåer i dBFS; −∞ = stillhet.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct KanalMaaling {
    pub id: String,
    /// Toppnivå (venstre, høyre) etter fader.
    pub topp: [f32; 2],
    /// RMS/VU (300 ms) etter fader.
    pub rms: [f32; 2],
    /// Toppnivå før fader, til å stille trim/gain.
    pub inn_topp: f32,
    /// Antall klippede samples siden sist nullstilt.
    pub klipp: u32,
    /// Sekunder siden kanalen ble slått på.
    pub paa_sek: f32,
    /// Mikrofonen er åpen (ON AIR).
    pub aapen: bool,
    pub paa: bool,
}

impl KanalMaaling {
    /// Slår sammen to målinger: høyeste nivå vinner (til målere som leses sjeldnere enn lyden går).
    pub fn slaa_sammen(&mut self, ny: &KanalMaaling) {
        for i in 0..2 {
            self.topp[i] = self.topp[i].max(ny.topp[i]);
            self.rms[i] = self.rms[i].max(ny.rms[i]);
        }
        self.inn_topp = self.inn_topp.max(ny.inn_topp);
        self.klipp = ny.klipp;
        self.paa_sek = ny.paa_sek;
        self.aapen = ny.aapen;
        self.paa = ny.paa;
    }
}

/// Nivåmålernes verdier etter siste prosesserte blokk.
#[derive(Clone, Debug, Serialize)]
pub struct Nivaamaalere {
    pub kanaler: Vec<KanalMaaling>,
    pub master: KanalMaaling,
}

impl Nivaamaalere {
    pub fn slaa_sammen(&mut self, ny: &Nivaamaalere) {
        if self.kanaler.len() != ny.kanaler.len() || self.kanaler.iter().zip(&ny.kanaler).any(|(a, b)| a.id != b.id) {
            *self = ny.clone();
            return;
        }
        for (a, b) in self.kanaler.iter_mut().zip(&ny.kanaler) {
            a.slaa_sammen(b);
        }
        self.master.slaa_sammen(&ny.master);
    }
}

/// Innstillinger for monitorkutt.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Monitor {
    pub hodetelefoner_db: f32,
    pub hoyttalere_db: f32,
    /// Hvor mye høyttalerne dempes når en mikrofon er åpen. −∞ = helt av.
    pub kutt_db: f32,
    /// Hvor lenge høyttalerne venter før de kommer tilbake (mellom ord).
    pub holdetid_ms: f32,
}

impl Default for Monitor {
    fn default() -> Self {
        // −200 dB = helt av (JSON tåler ikke −∞).
        Monitor { hodetelefoner_db: 0.0, hoyttalere_db: 0.0, kutt_db: -200.0, holdetid_ms: 400.0 }
    }
}

pub struct Motor {
    kanaler: Vec<Kanal>,
    master_kjede: Vec<Box<dyn Modul>>,
    master_db: f32,
    master_fader: Glatter,
    pub monitor: Monitor,
    monitor_gain: Glatter,
    hold_igjen: usize,
    var_on_air: bool,
    hendelser: Hendelsesbuss,
    master_maal: Maalertilstand,
}

impl Default for Motor {
    fn default() -> Self {
        Self::ny()
    }
}

impl Motor {
    pub fn ny() -> Self {
        Motor {
            kanaler: Vec::new(),
            master_kjede: Vec::new(),
            master_db: 0.0,
            master_fader: Glatter::ny(1.0, 10.0),
            monitor: Monitor::default(),
            monitor_gain: Glatter::ny(1.0, 20.0),
            hold_igjen: 0,
            var_on_air: false,
            hendelser: Hendelsesbuss::default(),
            master_maal: Maalertilstand::default(),
        }
    }

    pub fn legg_til_kanal(&mut self, oppsett: KanalOppsett) -> Result<(), Kontrollfeil> {
        if self.kanaler.iter().any(|k| k.oppsett.id == oppsett.id) {
            return Err(Kontrollfeil::UgyldigVerdi(format!("kanal-id «{}» finnes allerede", oppsett.id)));
        }
        self.kanaler.push(Kanal::ny(oppsett));
        self.oppdater_kompensasjon();
        Ok(())
    }

    pub fn fjern_kanal(&mut self, id: &str) {
        self.kanaler.retain(|k| k.oppsett.id != id);
        self.oppdater_kompensasjon();
    }

    pub fn kanaler(&self) -> &[Kanal] {
        &self.kanaler
    }

    pub fn kanal(&self, id: &str) -> Option<&Kanal> {
        self.kanaler.iter().find(|k| k.oppsett.id == id)
    }

    pub fn master_kjede(&self) -> &[Box<dyn Modul>] {
        &self.master_kjede
    }

    /// Legger en modul i kanalens kjede (etter trimmen, før faderen).
    pub fn legg_til_modul(&mut self, kanal: &str, modul: Box<dyn Modul>) -> Result<(), Kontrollfeil> {
        let k = self.kanaler.iter_mut().find(|k| k.oppsett.id == kanal).ok_or_else(|| Kontrollfeil::UkjentKontroll(format!("kanal/{kanal}")))?;
        k.kjede.push(modul);
        k.bygg_lytt_kjede();
        self.oppdater_kompensasjon();
        Ok(())
    }

    /// Legger en modul i master-kjeden (bare sendeveien, f.eks. Stereo Tool).
    pub fn legg_til_master_modul(&mut self, modul: Box<dyn Modul>) {
        self.master_kjede.push(modul);
    }

    /// Total forsinkelse i sendeveien (kanal + master), i samples.
    pub fn sendevei_forsinkelse(&self) -> usize {
        let kanal = self.kanaler.iter().map(|k| k.kjede_forsinkelse()).max().unwrap_or(0);
        kanal + self.master_kjede.iter().map(|m| m.forsinkelse()).sum::<usize>()
    }

    /// Lytteveien har per konstruksjon ingen intern forsinkelse.
    pub fn lyttevei_forsinkelse(&self) -> usize {
        0
    }

    fn oppdater_kompensasjon(&mut self) {
        let maks = self.kanaler.iter().map(|k| k.kjede_forsinkelse()).max().unwrap_or(0);
        for k in &mut self.kanaler {
            let trengs = maks - k.kjede_forsinkelse();
            if k.kompensasjon.forsinkelse() != trengs {
                k.kompensasjon = Forsinkelse::ny(trengs);
            }
        }
    }

    /// Alle kontroller i motoren, med full adresse (brukes av MIDI-learn og presets).
    pub fn register(&self) -> Vec<RegistrertKontroll> {
        let mut r = Vec::new();
        for k in &self.kanaler {
            for kontroll in Kanal::kanalkontroller() {
                r.push(RegistrertKontroll { adresse: format!("kanal/{}/{}", k.oppsett.id, kontroll.id), modulnavn: k.oppsett.navn.clone(), kontroll });
            }
            for (i, m) in k.kjede.iter().enumerate() {
                for kontroll in m.kontroller() {
                    r.push(RegistrertKontroll {
                        adresse: format!("kanal/{}/modul/{}/{}", k.oppsett.id, i, kontroll.id),
                        modulnavn: format!("{} · {}", k.oppsett.navn, m.typenavn()),
                        kontroll,
                    });
                }
            }
        }
        r.push(RegistrertKontroll {
            adresse: "master/fader".into(),
            modulnavn: "Master".into(),
            kontroll: Kontroll::ny("fader", "Master-fader", Kontrolltype::Fader { min_db: f32::NEG_INFINITY, max_db: 10.0 }, Verdi::Tall(0.0)),
        });
        for (i, m) in self.master_kjede.iter().enumerate() {
            for kontroll in m.kontroller() {
                r.push(RegistrertKontroll { adresse: format!("master/modul/{}/{}", i, kontroll.id), modulnavn: format!("Master · {}", m.typenavn()), kontroll });
            }
        }
        for (id, navn) in [("hodetelefoner", "Hodetelefoner"), ("hoyttalere", "Høyttalere"), ("kutt_db", "Monitorkutt")] {
            r.push(RegistrertKontroll {
                adresse: format!("monitor/{id}"),
                modulnavn: "Monitor".into(),
                kontroll: Kontroll::ny(id, navn, Kontrolltype::Fader { min_db: f32::NEG_INFINITY, max_db: 10.0 }, Verdi::Tall(0.0)),
            });
        }
        r
    }

    /// Setter en kontroll via adresse, f.eks. "kanal/mik1/fader".
    pub fn sett(&mut self, adresse: &str, verdi: Verdi) -> Result<(), Kontrollfeil> {
        let deler: Vec<&str> = adresse.split('/').collect();
        let ukjent = || Kontrollfeil::UkjentKontroll(adresse.to_string());
        let res = match deler.as_slice() {
            ["kanal", id, "modul", n, kontroll] => {
                let k = self.kanaler.iter_mut().find(|k| k.oppsett.id == *id).ok_or_else(ukjent)?;
                let n: usize = n.parse().map_err(|_| ukjent())?;
                let m = k.kjede.get_mut(n).ok_or_else(ukjent)?;
                m.sett(kontroll, verdi.clone())?;
                if let Some(Some(l)) = k.lytt_kjede.get_mut(n) {
                    l.sett(kontroll, verdi)?;
                }
                Ok(())
            }
            ["kanal", id, kontroll] => {
                let k = self.kanaler.iter_mut().find(|k| k.oppsett.id == *id).ok_or_else(ukjent)?;
                let r = k.sett(kontroll, verdi);
                let oppe = k.er_oppe();
                if oppe != k.var_oppe {
                    k.var_oppe = oppe;
                    if k.faderstart {
                        let kanal = k.oppsett.id.clone();
                        self.hendelser.send(if oppe { Hendelse::FaderStart { kanal } } else { Hendelse::FaderStopp { kanal } });
                    }
                }
                r
            }
            ["master", "nullstill_klipp"] => {
                self.nullstill_master_klipp();
                Ok(())
            }
            ["master", "fader"] => {
                self.master_db = verdi.som_tall().ok_or_else(ukjent)?.min(10.0);
                self.master_fader.sett_maal(db_til_lin(self.master_db));
                Ok(())
            }
            ["master", "modul", n, kontroll] => {
                let n: usize = n.parse().map_err(|_| ukjent())?;
                self.master_kjede.get_mut(n).ok_or_else(ukjent)?.sett(kontroll, verdi)
            }
            ["monitor", "hodetelefoner"] => {
                self.monitor.hodetelefoner_db = verdi.som_tall().ok_or_else(ukjent)?;
                Ok(())
            }
            ["monitor", "hoyttalere"] => {
                self.monitor.hoyttalere_db = verdi.som_tall().ok_or_else(ukjent)?;
                Ok(())
            }
            ["monitor", "kutt_db"] => {
                self.monitor.kutt_db = verdi.som_tall().ok_or_else(ukjent)?;
                Ok(())
            }
            _ => Err(ukjent()),
        };
        if res.is_ok() {
            self.hendelser.send(Hendelse::KontrollEndret { adresse: adresse.to_string() });
        }
        res
    }

    pub fn hent(&self, adresse: &str) -> Option<Verdi> {
        let deler: Vec<&str> = adresse.split('/').collect();
        match deler.as_slice() {
            ["kanal", id, "modul", n, kontroll] => self.kanal(id)?.kjede.get(n.parse::<usize>().ok()?)?.hent(kontroll),
            ["kanal", id, kontroll] => self.kanal(id)?.hent(kontroll),
            ["master", "fader"] => Some(Verdi::Tall(self.master_db)),
            ["master", "modul", n, kontroll] => self.master_kjede.get(n.parse::<usize>().ok()?)?.hent(kontroll),
            ["monitor", "hodetelefoner"] => Some(Verdi::Tall(self.monitor.hodetelefoner_db)),
            ["monitor", "hoyttalere"] => Some(Verdi::Tall(self.monitor.hoyttalere_db)),
            ["monitor", "kutt_db"] => Some(Verdi::Tall(self.monitor.kutt_db)),
            _ => None,
        }
    }

    /// Nivåmålere per kanal og for master.
    pub fn nivaamaalere(&self) -> Nivaamaalere {
        Nivaamaalere {
            kanaler: self
                .kanaler
                .iter()
                .map(|k| k.maal.maaling(&k.oppsett.id, k.paa_samples as f32 / SAMPLERATE as f32, k.er_aapen(), k.paa))
                .collect(),
            master: self.master_maal.maaling("master", 0.0, false, true),
        }
    }

    /// Nullstiller klippvarselet på master.
    pub fn nullstill_master_klipp(&mut self) {
        self.master_maal.klipp = 0;
    }

    pub fn hendelser(&mut self) -> Vec<Hendelse> {
        self.hendelser.hent_alle()
    }

    pub fn send_hendelse(&mut self, h: Hendelse) {
        self.hendelser.send(h);
    }

    pub fn er_on_air(&self) -> bool {
        self.kanaler.iter().any(|k| k.er_aapen())
    }

    /// Prosesserer lyd. `innganger` har én blokk per kanal-id (alle like lange).
    /// Kanaler uten inngang får stillhet.
    pub fn prosesser(&mut self, innganger: &HashMap<String, Blokk>) -> Utganger {
        let n = innganger.values().map(|b| b.lengde()).max().unwrap_or(0);
        let mut ut = Utganger::stille(n);
        let mut start = 0;
        while start < n {
            let len = BLOKKSTORRELSE.min(n - start);
            let biter: HashMap<&str, Blokk> = innganger.iter().map(|(id, b)| (id.as_str(), b.utsnitt(start, len))).collect();
            let del = self.prosesser_blokk(&biter, len);
            for (maal, kilde) in [
                (&mut ut.program, &del.program),
                (&mut ut.dj_ren, &del.dj_ren),
                (&mut ut.hodetelefoner, &del.hodetelefoner),
                (&mut ut.hoyttalere, &del.hoyttalere),
            ] {
                maal.v[start..start + len].copy_from_slice(&kilde.v);
                maal.h[start..start + len].copy_from_slice(&kilde.h);
            }
            start += len;
        }
        ut
    }

    fn prosesser_blokk(&mut self, innganger: &HashMap<&str, Blokk>, len: usize) -> Utganger {
        let mut master = Blokk::stille(len);
        let mut studio = Blokk::stille(len);
        let mut dj_ren = Blokk::stille(len);
        let mut cue = Blokk::stille(len);
        let noen_solo = self.kanaler.iter().any(|k| k.solo);
        let stille = Blokk::stille(len);

        let mut a = Blokk::stille(len);
        let mut b = Blokk::stille(len);
        let mut l = Blokk::stille(len);
        let mut l2 = Blokk::stille(len);
        let mut fader_buf = vec![0.0f32; len];

        for k in &mut self.kanaler {
            let inn = innganger.get(k.oppsett.id.as_str()).unwrap_or(&stille);

            // DJ-ren: uprosessert, før trim, kjede og fader.
            if k.oppsett.til_dj_ren {
                dj_ren.legg_til(inn, 1.0);
            }

            // Sendeveien: inngangstrinn → trim → hele kjeden → forsinkelseskompensasjon.
            k.inn_send.prosesser(inn, &mut b);
            k.trim.prosesser(&b, &mut a);
            for m in &mut k.kjede {
                m.prosesser(&a, &mut b);
                std::mem::swap(&mut a, &mut b);
            }
            k.kompensasjon.prosesser(&a, &mut b);
            std::mem::swap(&mut a, &mut b);

            // Lytteveien: inngangstrinn → trim → kjeden uten moduler med forsinkelse.
            k.inn_lytt.prosesser(inn, &mut l2);
            k.trim_lytt.prosesser(&l2, &mut l);
            for m in k.lytt_kjede.iter_mut().flatten() {
                m.prosesser(&l, &mut l2);
                std::mem::swap(&mut l, &mut l2);
            }

            // Cue (solo) er forhåndslytt før fader.
            if k.solo {
                cue.legg_til(&l, 1.0);
            }

            for g in fader_buf.iter_mut() {
                *g = k.fader.neste();
            }
            let (pv, ph) = k.panorering();

            let til_master = k.oppsett.til_master && !k.oppsett.privat;
            let mut topp = [0.0f32; 2];
            let rf = Maalertilstand::rms_faktor();
            for i in 0..len {
                let g = fader_buf[i];
                let (sv, sh) = (l.v[i] * g * pv, l.h[i] * g * ph);
                topp[0] = topp[0].max(sv.abs());
                topp[1] = topp[1].max(sh.abs());
                k.maal.rms2[0] += (sv * sv - k.maal.rms2[0]) * rf;
                k.maal.rms2[1] += (sh * sh - k.maal.rms2[1]) * rf;
                let inn_abs = l.v[i].abs().max(l.h[i].abs());
                k.maal.inn_topp = k.maal.inn_topp.max(inn_abs);
                if inn_abs >= 0.999 {
                    k.maal.klipp = k.maal.klipp.saturating_add(1);
                }
                if til_master {
                    master.v[i] += a.v[i] * g * pv;
                    master.h[i] += a.h[i] * g * ph;
                }
                if k.oppsett.til_studio {
                    studio.v[i] += l.v[i] * g * pv;
                    studio.h[i] += l.h[i] * g * ph;
                }
            }

            k.maal.topp = topp;
            if k.paa && !k.mute {
                k.paa_samples += len as u64;
            }

            let aapen = k.er_aapen();
            if aapen != k.var_aapen {
                k.var_aapen = aapen;
                let kanal = k.oppsett.id.clone();
                self.hendelser.send(if aapen { Hendelse::MikrofonAapen { kanal } } else { Hendelse::MikrofonLukket { kanal } });
            }
        }

        // Master-kjeden (bare sendeveien).
        let mut p = master;
        let mut p2 = Blokk::stille(len);
        for m in &mut self.master_kjede {
            m.prosesser(&p, &mut p2);
            std::mem::swap(&mut p, &mut p2);
        }
        for i in 0..len {
            let g = self.master_fader.neste();
            p.v[i] *= g;
            p.h[i] *= g;
        }

        // Monitorkutt med holdetid og myk overgang.
        let on_air = self.kanaler.iter().any(|k| k.er_aapen());
        if on_air != self.var_on_air {
            self.var_on_air = on_air;
            self.hendelser.send(Hendelse::OnAir { paa: on_air });
        }
        if on_air {
            self.hold_igjen = (self.monitor.holdetid_ms / 1000.0 * SAMPLERATE as f32) as usize;
            self.monitor_gain.sett_maal(db_til_lin(self.monitor.kutt_db));
        } else if self.hold_igjen > 0 {
            self.hold_igjen = self.hold_igjen.saturating_sub(len);
        } else {
            self.monitor_gain.sett_maal(1.0);
        }

        let hodetelefon_kilde = if noen_solo { &cue } else { &studio };
        let gh = db_til_lin(self.monitor.hodetelefoner_db);
        let gs = db_til_lin(self.monitor.hoyttalere_db);
        let mut hodetelefoner = Blokk::stille(len);
        let mut hoyttalere = Blokk::stille(len);
        for i in 0..len {
            hodetelefoner.v[i] = hodetelefon_kilde.v[i] * gh;
            hodetelefoner.h[i] = hodetelefon_kilde.h[i] * gh;
            let kutt = self.monitor_gain.neste();
            hoyttalere.v[i] = studio.v[i] * gs * kutt;
            hoyttalere.h[i] = studio.h[i] * gs * kutt;
        }

        let rf = Maalertilstand::rms_faktor();
        let mut mt = [0.0f32; 2];
        let mut mk = 0u32;
        for i in 0..len {
            let (v, h) = (p.v[i], p.h[i]);
            mt[0] = mt[0].max(v.abs());
            mt[1] = mt[1].max(h.abs());
            self.master_maal.rms2[0] += (v * v - self.master_maal.rms2[0]) * rf;
            self.master_maal.rms2[1] += (h * h - self.master_maal.rms2[1]) * rf;
            if v.abs() >= 0.999 || h.abs() >= 0.999 {
                mk += 1;
            }
        }
        self.master_maal.topp = mt;
        self.master_maal.inn_topp = mt[0].max(mt[1]);
        if mk > 0 {
            self.master_maal.klipp = self.master_maal.klipp.saturating_add(mk);
            self.hendelser.send(Hendelse::Klipping { sted: "Master".into() });
        }

        Utganger { program: p, dj_ren, hodetelefoner, hoyttalere }
    }

    /// Skjult kopi av hele motoren, til linjesjekk som ikke påvirker sendingen.
    pub fn skyggekopi(&self) -> Motor {
        let kanaler = self
            .kanaler
            .iter()
            .map(|k| Kanal {
                oppsett: k.oppsett.clone(),
                trim: k.trim.clone(),
                trim_lytt: k.trim_lytt.clone(),
                kjede: k.kjede.iter().map(|m| m.klon()).collect(),
                lytt_kjede: k.lytt_kjede.iter().map(|m| m.as_ref().map(|m| m.klon())).collect(),
                kompensasjon: k.kompensasjon.clone(),
                fader_db: k.fader_db,
                fader: k.fader.clone(),
                mute: k.mute,
                solo: k.solo,
                pan: k.pan,
                var_aapen: k.var_aapen,
                inn_send: k.inn_send.clone(),
                inn_lytt: k.inn_lytt.clone(),
                paa: k.paa,
                faderstart: k.faderstart,
                var_oppe: k.var_oppe,
                paa_samples: 0,
                maal: Maalertilstand::default(),
            })
            .collect();
        let mut m = Motor {
            kanaler,
            master_kjede: self.master_kjede.iter().map(|m| m.klon()).collect(),
            master_db: self.master_db,
            master_fader: self.master_fader.clone(),
            monitor: self.monitor.clone(),
            monitor_gain: Glatter::ny(1.0, 20.0),
            hold_igjen: 0,
            var_on_air: false,
            hendelser: Hendelsesbuss::default(),
            master_maal: Maalertilstand::default(),
        };
        m.nullstill();
        m
    }

    /// Nullstiller alle filtre og forsinkelseslinjer.
    pub fn nullstill(&mut self) {
        for k in &mut self.kanaler {
            k.trim.nullstill();
            k.trim_lytt.nullstill();
            k.inn_send.nullstill();
            k.inn_lytt.nullstill();
            k.kjede.iter_mut().for_each(|m| m.nullstill());
            k.lytt_kjede.iter_mut().flatten().for_each(|m| m.nullstill());
            k.kompensasjon.nullstill();
            k.fader.sett_straks(k.fader.maal());
        }
        self.master_kjede.iter_mut().for_each(|m| m.nullstill());
        self.master_fader.sett_straks(self.master_fader.maal());
    }
}

#[cfg(test)]
mod tester {
    use super::*;
    use crate::maaling;
    use crate::testlyd;

    fn innganger(par: &[(&str, Blokk)]) -> HashMap<String, Blokk> {
        par.iter().map(|(id, b)| (id.to_string(), b.clone())).collect()
    }

    #[test]
    fn noytral_kanal_gir_bit_identisk_program() {
        let mut m = Motor::ny();
        m.legg_til_kanal(KanalOppsett::ny("v10", "DJM-V10", Kanaltype::LinjeStereo)).unwrap();
        let stoy = testlyd::rosa_stoy(-20.0, 0.5);
        let ut = m.prosesser(&innganger(&[("v10", stoy.clone())]));
        let r = maaling::nulltest(&stoy, &ut.program, 0);
        assert!(r.bit_identisk, "rest {} dBFS", r.rest_dbfs);
    }

    #[test]
    fn privat_kanal_kan_aldri_naa_master() {
        let mut m = Motor::ny();
        let mut o = KanalOppsett::ny("spot", "Spotify", Kanaltype::AppLyd);
        o.privat = true;
        m.legg_til_kanal(o).unwrap();
        assert!(matches!(m.sett("kanal/spot/til_master", Verdi::AvPaa(true)), Err(Kontrollfeil::Sperret(_))));
        let ut = m.prosesser(&innganger(&[("spot", testlyd::tone(1000.0, -18.0, 0.2))]));
        assert_eq!(maaling::topp_dbfs(&ut.program.v), f32::NEG_INFINITY);
        assert!(maaling::topp_dbfs(&ut.hodetelefoner.v) > -19.0, "privat skal høres i studio");
    }

    #[test]
    fn lytteveien_har_ingen_forsinkelse_selv_med_forsinkelse_i_kjeden() {
        let mut m = Motor::ny();
        m.legg_til_kanal(KanalOppsett::ny("a", "A", Kanaltype::LinjeStereo)).unwrap();
        m.legg_til_modul("a", Box::new(Forsinkelse::ny(480))).unwrap(); // 10 ms look-ahead
        let p = testlyd::puls(100, 4000, -6.0);
        let ut = m.prosesser(&innganger(&[("a", p.clone())]));
        let (lytt, _) = maaling::forsinkelse_og_polaritet(&p.v, &ut.hodetelefoner.v, 1000).unwrap();
        let (send, _) = maaling::forsinkelse_og_polaritet(&p.v, &ut.program.v, 1000).unwrap();
        assert_eq!(lytt, 0, "lytteveien skal ha null forsinkelse");
        assert_eq!(send, 480);
    }

    #[test]
    fn sendeveien_kompenserer_forsinkelse_mellom_kanaler() {
        let mut m = Motor::ny();
        m.legg_til_kanal(KanalOppsett::ny("a", "A", Kanaltype::LinjeStereo)).unwrap();
        m.legg_til_kanal(KanalOppsett::ny("b", "B", Kanaltype::LinjeStereo)).unwrap();
        m.legg_til_modul("a", Box::new(Forsinkelse::ny(256))).unwrap();
        let p = testlyd::puls(100, 2000, -6.0);
        // Bare kanal B får puls; den skal komme ut forsinket like mye som A.
        let ut = m.prosesser(&innganger(&[("b", p.clone())]));
        let (lag, _) = maaling::forsinkelse_og_polaritet(&p.v, &ut.program.v, 1000).unwrap();
        assert_eq!(lag, 256);
    }

    #[test]
    fn mikrofon_kutter_hoyttalere_men_ikke_hodetelefoner() {
        let mut m = Motor::ny();
        m.legg_til_kanal(KanalOppsett::ny("musikk", "Musikk", Kanaltype::Spiller)).unwrap();
        m.legg_til_kanal(KanalOppsett::ny("mik1", "Mikrofon 1", Kanaltype::Mikrofon)).unwrap();
        let tone = testlyd::tone(1000.0, -18.0, 0.5);
        let inn = innganger(&[("musikk", tone.clone())]);
        let ut = m.prosesser(&inn);
        assert!(maaling::rms_dbfs(&ut.hoyttalere.v[12_000..]) > -22.0);
        assert!(!m.er_on_air(), "mikrofonen starter AV, selv med faderen på 0 dB");

        m.sett("kanal/mik1/paa", Verdi::AvPaa(true)).unwrap();
        let ut = m.prosesser(&inn);
        assert!(m.er_on_air());
        let h = m.hendelser();
        assert!(h.contains(&Hendelse::OnAir { paa: true }));
        assert!(h.contains(&Hendelse::MikrofonAapen { kanal: "mik1".into() }));
        assert!(maaling::rms_dbfs(&ut.hoyttalere.v[12_000..]) < -100.0, "høyttalerne skal være kuttet");
        assert!(maaling::rms_dbfs(&ut.hodetelefoner.v[12_000..]) > -22.0, "hodetelefonene skal ikke kuttes");

        // Holdetid: like etter at mikrofonen lukkes er høyttalerne fortsatt kuttet.
        m.sett("kanal/mik1/paa", Verdi::AvPaa(false)).unwrap();
        let kort = innganger(&[("musikk", testlyd::tone(1000.0, -18.0, 0.1))]);
        let ut = m.prosesser(&kort);
        assert!(maaling::rms_dbfs(&ut.hoyttalere.v) < -60.0, "holdetid skal holde kuttet");
        let ut = m.prosesser(&inn);
        assert!(maaling::rms_dbfs(&ut.hoyttalere.v[18_000..]) > -22.0, "høyttalerne skal komme tilbake");
    }

    #[test]
    fn solo_endrer_bare_hodetelefonene() {
        let mut m = Motor::ny();
        m.legg_til_kanal(KanalOppsett::ny("a", "A", Kanaltype::LinjeStereo)).unwrap();
        m.legg_til_kanal(KanalOppsett::ny("b", "B", Kanaltype::LinjeStereo)).unwrap();
        m.sett("kanal/b/solo", Verdi::AvPaa(true)).unwrap();
        let ta = testlyd::tone(1000.0, -18.0, 0.2);
        let ut = m.prosesser(&innganger(&[("a", ta.clone())]));
        assert!(maaling::topp_dbfs(&ut.program.v) > -18.1, "solo skal ikke påvirke sendingen");
        assert_eq!(maaling::topp_dbfs(&ut.hodetelefoner.v), f32::NEG_INFINITY, "hodetelefoner viser bare cue");
    }

    #[test]
    fn dj_ren_er_uprosessert_og_uavhengig_av_fader() {
        let mut m = Motor::ny();
        let mut o = KanalOppsett::ny("v10", "DJM-V10", Kanaltype::LinjeStereo);
        o.til_dj_ren = true;
        m.legg_til_kanal(o).unwrap();
        m.sett("kanal/v10/fader", Verdi::Tall(-12.0)).unwrap();
        m.sett("kanal/v10/trim", Verdi::Tall(3.0)).unwrap();
        let stoy = testlyd::rosa_stoy(-20.0, 0.3);
        let ut = m.prosesser(&innganger(&[("v10", stoy.clone())]));
        assert!(maaling::nulltest(&stoy, &ut.dj_ren, 0).bit_identisk);
    }

    #[test]
    fn registeret_har_alle_kanalkontroller() {
        let mut m = Motor::ny();
        m.legg_til_kanal(KanalOppsett::ny("mik1", "Mikrofon 1", Kanaltype::Mikrofon)).unwrap();
        m.legg_til_modul("mik1", Box::new(Gain::ny(0.0))).unwrap();
        let r = m.register();
        for a in ["kanal/mik1/fader", "kanal/mik1/mute", "kanal/mik1/solo", "kanal/mik1/modul/0/gain", "master/fader", "monitor/kutt_db"] {
            assert!(r.iter().any(|k| k.adresse == a), "mangler {a}");
        }
        m.sett("kanal/mik1/modul/0/gain", Verdi::Tall(-3.0)).unwrap();
        assert_eq!(m.hent("kanal/mik1/modul/0/gain"), Some(Verdi::Tall(-3.0)));
    }

    #[test]
    fn nivaamaalere_viser_kanal_og_master() {
        let mut m = Motor::ny();
        m.legg_til_kanal(KanalOppsett::ny("a", "A", Kanaltype::LinjeStereo)).unwrap();
        m.legg_til_kanal(KanalOppsett::ny("b", "B", Kanaltype::LinjeStereo)).unwrap();
        m.sett("kanal/a/fader", Verdi::Tall(-6.0)).unwrap();
        for _ in 0..20 {
            m.prosesser(&innganger(&[("a", testlyd::tone(1000.0, -18.0, 0.1))]));
        }
        let n = m.nivaamaalere();
        let a = n.kanaler.iter().find(|k| k.id == "a").unwrap();
        let b = n.kanaler.iter().find(|k| k.id == "b").unwrap();
        assert!((a.topp[0] - -24.0).abs() < 0.2, "kanal a viste {}", a.topp[0]);
        // Sinus: RMS ligger 3 dB under toppen.
        assert!((a.rms[0] - -27.0).abs() < 0.3, "RMS viste {}", a.rms[0]);
        assert!((a.inn_topp - -18.0).abs() < 0.2, "før fader viste {}", a.inn_topp);
        assert_eq!(b.topp[0], f32::NEG_INFINITY);
        assert!((n.master.topp[0] - -24.0).abs() < 0.2, "master viste {}", n.master.topp[0]);
        assert_eq!(n.master.klipp, 0);
    }

    #[test]
    fn on_knapp_gir_fader_start_og_tidtaker() {
        let mut m = Motor::ny();
        m.legg_til_kanal(KanalOppsett::ny("a", "Spiller A", Kanaltype::Spiller)).unwrap();
        m.hendelser();
        m.sett("kanal/a/paa", Verdi::AvPaa(false)).unwrap();
        assert!(m.hendelser().contains(&Hendelse::FaderStopp { kanal: "a".into() }));
        m.sett("kanal/a/paa", Verdi::AvPaa(true)).unwrap();
        assert!(m.hendelser().contains(&Hendelse::FaderStart { kanal: "a".into() }));
        // Fader ned og opp gir også stopp/start.
        m.sett("kanal/a/fader", Verdi::Tall(-200.0)).unwrap();
        assert!(m.hendelser().contains(&Hendelse::FaderStopp { kanal: "a".into() }));
        m.sett("kanal/a/fader", Verdi::Tall(-3.0)).unwrap();
        assert!(m.hendelser().contains(&Hendelse::FaderStart { kanal: "a".into() }));
        // Små fader-bevegelser oppe gir ingen nye hendelser.
        m.sett("kanal/a/fader", Verdi::Tall(-2.0)).unwrap();
        assert!(!m.hendelser().iter().any(|h| matches!(h, Hendelse::FaderStart { .. } | Hendelse::FaderStopp { .. })));
        // Fader-start av: ingen hendelser.
        m.sett("kanal/a/faderstart", Verdi::AvPaa(false)).unwrap();
        m.sett("kanal/a/paa", Verdi::AvPaa(false)).unwrap();
        assert!(!m.hendelser().iter().any(|h| matches!(h, Hendelse::FaderStopp { .. })));
        // Tidtaker
        m.sett("kanal/a/paa", Verdi::AvPaa(true)).unwrap();
        m.prosesser(&innganger(&[("a", testlyd::stillhet(1.5))]));
        let t = m.nivaamaalere().kanaler[0].paa_sek;
        assert!((t - 1.5).abs() < 0.01, "tidtaker {t}");
        m.sett("kanal/a/paa", Verdi::AvPaa(false)).unwrap();
        assert_eq!(m.nivaamaalere().kanaler[0].paa_sek, 0.0);
    }

    #[test]
    fn av_kanal_gir_ingen_lyd_men_cue_virker() {
        let mut m = Motor::ny();
        m.legg_til_kanal(KanalOppsett::ny("a", "A", Kanaltype::LinjeStereo)).unwrap();
        m.sett("kanal/a/paa", Verdi::AvPaa(false)).unwrap();
        m.sett("kanal/a/solo", Verdi::AvPaa(true)).unwrap();
        let ut = m.prosesser(&innganger(&[("a", testlyd::tone(1000.0, -18.0, 0.3))]));
        assert!(maaling::topp_dbfs(&ut.program.v[4800..]) < -100.0, "av = ikke på lufta");
        assert!(maaling::topp_dbfs(&ut.hodetelefoner.v[4800..]) > -18.5, "men du kan forhåndslytte");
    }

    #[test]
    fn klippvarsel_blir_staaende_til_det_nullstilles() {
        let mut m = Motor::ny();
        m.legg_til_kanal(KanalOppsett::ny("a", "A", Kanaltype::LinjeStereo)).unwrap();
        m.prosesser(&innganger(&[("a", testlyd::tone(1000.0, 0.0, 0.1))]));
        m.prosesser(&innganger(&[("a", testlyd::stillhet(0.5))]));
        let n = m.nivaamaalere();
        assert!(n.kanaler[0].klipp > 0);
        assert!(n.master.klipp > 0);
        m.sett("kanal/a/nullstill_klipp", Verdi::AvPaa(true)).unwrap();
        m.sett("master/nullstill_klipp", Verdi::AvPaa(true)).unwrap();
        let n = m.nivaamaalere();
        assert_eq!((n.kanaler[0].klipp, n.master.klipp), (0, 0));
    }

    #[test]
    fn inngangstrinn_ligger_i_begge_lydveier() {
        let mut m = Motor::ny();
        m.legg_til_kanal(KanalOppsett::ny("a", "A", Kanaltype::LinjeStereo)).unwrap();
        m.sett("kanal/a/fase", Verdi::AvPaa(true)).unwrap();
        m.sett("kanal/a/modus", Verdi::Tall(2.0)).unwrap(); // bare venstre
        let inn = Blokk { v: vec![0.5; 256], h: vec![0.1; 256] };
        let ut = m.prosesser(&innganger(&[("a", inn)]));
        assert_eq!((ut.program.v[100], ut.program.h[100]), (-0.5, -0.5));
        assert_eq!((ut.hodetelefoner.v[100], ut.hodetelefoner.h[100]), (-0.5, -0.5));
        assert_eq!(m.hent("kanal/a/modus"), Some(Verdi::Valg(2)));
    }
}
