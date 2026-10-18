//! Studio-presets: hele oppsettet (kanaler, ruting, kontrollverdier, monitor og
//! fasit for linjesjekken) lagret som en fil som kan flyttes mellom PC og Mac.

use crate::kontroll::{Kontrollfeil, Verdi};
use crate::motor::{KanalOppsett, Monitor, Motor};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const FORMATVERSJON: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct KanalPreset {
    #[serde(flatten)]
    pub oppsett: KanalOppsett,
    /// Kanalens kontrollverdier, f.eks. "fader", "trim", "mute", "pan".
    pub kontroller: BTreeMap<String, Verdi>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StudioPreset {
    pub navn: String,
    pub formatversjon: u32,
    pub kanaler: Vec<KanalPreset>,
    pub master_db: f32,
    pub monitor: Monitor,
    /// Godkjent linjesjekk: forventet nivåendring (dB) per modul-adresse.
    #[serde(default)]
    pub fasit: BTreeMap<String, f32>,
}

#[derive(Debug)]
pub enum Presetfeil {
    Fil(std::io::Error),
    Format(serde_json::Error),
    Kontroll(Kontrollfeil),
    NyereVersjon(u32),
}

impl std::fmt::Display for Presetfeil {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Presetfeil::Fil(e) => write!(f, "Kunne ikke lese eller skrive presetfilen: {e}"),
            Presetfeil::Format(e) => write!(f, "Presetfilen er skadet eller har feil format: {e}"),
            Presetfeil::Kontroll(e) => write!(f, "{e}"),
            Presetfeil::NyereVersjon(v) => write!(f, "Presetfilen er laget med en nyere versjon av suiten (format {v}). Oppdater suiten."),
        }
    }
}

impl std::error::Error for Presetfeil {}

const KANALKONTROLLER: [&str; 11] = ["fader", "trim", "mute", "solo", "pan", "paa", "faderstart", "hpf", "hpf_hz", "fase", "modus"];

impl StudioPreset {
    /// Lager en preset av motorens nåværende tilstand.
    pub fn fra_motor(navn: &str, motor: &Motor, fasit: BTreeMap<String, f32>) -> Self {
        let kanaler = motor
            .kanaler()
            .iter()
            .map(|k| {
                let mut kontroller = BTreeMap::new();
                for id in KANALKONTROLLER {
                    if let Some(v) = motor.hent(&format!("kanal/{}/{}", k.oppsett.id, id)) {
                        kontroller.insert(id.to_string(), json_trygg(v));
                    }
                }
                KanalPreset { oppsett: k.oppsett.clone(), kontroller }
            })
            .collect();
        let master_db = motor.hent("master/fader").and_then(|v| v.som_tall()).unwrap_or(0.0);
        StudioPreset { navn: navn.into(), formatversjon: FORMATVERSJON, kanaler, master_db, monitor: motor.monitor.clone(), fasit }
    }

    /// Bygger en motor fra presetet.
    pub fn til_motor(&self) -> Result<Motor, Presetfeil> {
        let mut m = Motor::ny();
        for k in &self.kanaler {
            m.legg_til_kanal(k.oppsett.clone()).map_err(Presetfeil::Kontroll)?;
            for (id, v) in &k.kontroller {
                m.sett(&format!("kanal/{}/{}", k.oppsett.id, id), v.clone()).map_err(Presetfeil::Kontroll)?;
            }
        }
        m.sett("master/fader", Verdi::Tall(self.master_db)).map_err(Presetfeil::Kontroll)?;
        m.monitor = self.monitor.clone();
        m.nullstill();
        m.hendelser(); // presetlasting skal ikke gi «kontroll endret»-støy
        m.send_hendelse(crate::hendelse::Hendelse::PresetLastet { navn: self.navn.clone() });
        Ok(m)
    }

    pub fn lagre(&self, sti: &Path) -> Result<(), Presetfeil> {
        let json = serde_json::to_string_pretty(self).map_err(Presetfeil::Format)?;
        // Skriv til midlertidig fil først, så en strømbrudd aldri ødelegger presetet.
        let tmp = sti.with_extension("tmp");
        std::fs::write(&tmp, json).map_err(Presetfeil::Fil)?;
        std::fs::rename(&tmp, sti).map_err(Presetfeil::Fil)
    }

    pub fn last(sti: &Path) -> Result<Self, Presetfeil> {
        let tekst = std::fs::read_to_string(sti).map_err(Presetfeil::Fil)?;
        let p: StudioPreset = serde_json::from_str(&tekst).map_err(Presetfeil::Format)?;
        if p.formatversjon > FORMATVERSJON {
            return Err(Presetfeil::NyereVersjon(p.formatversjon));
        }
        Ok(p)
    }
}

/// JSON tåler ikke −∞, så helt nede lagres som −200 dB (gir nøyaktig stillhet).
fn json_trygg(v: Verdi) -> Verdi {
    match v {
        Verdi::Tall(t) if !t.is_finite() => Verdi::Tall(if t > 0.0 { 10.0 } else { -200.0 }),
        // Valg lagres som tall, så fila leses likt tilbake.
        Verdi::Valg(n) => Verdi::Tall(n as f32),
        annen => annen,
    }
}

#[cfg(test)]
mod tester {
    use super::*;
    use crate::motor::Kanaltype;

    #[test]
    fn preset_tur_retur_gir_samme_oppsett() {
        let mut m = Motor::ny();
        m.legg_til_kanal(KanalOppsett::ny("mik1", "Mikrofon 1", Kanaltype::Mikrofon)).unwrap();
        let mut spot = KanalOppsett::ny("spot", "Spotify", Kanaltype::AppLyd);
        spot.privat = true;
        m.legg_til_kanal(spot).unwrap();
        m.sett("kanal/mik1/fader", Verdi::Tall(-4.0)).unwrap();
        m.sett("kanal/mik1/trim", Verdi::Tall(4.5)).unwrap();
        m.sett("kanal/spot/pan", Verdi::Tall(-0.25)).unwrap();
        m.sett("kanal/mik1/hpf", Verdi::AvPaa(true)).unwrap();
        m.sett("kanal/mik1/hpf_hz", Verdi::Tall(100.0)).unwrap();
        m.sett("kanal/mik1/modus", Verdi::Tall(2.0)).unwrap();
        m.sett("kanal/mik1/paa", Verdi::AvPaa(true)).unwrap();

        let p = StudioPreset::fra_motor("Live torsdag", &m, BTreeMap::new());
        let dir = std::env::temp_dir().join("studio-suite-presettest");
        std::fs::create_dir_all(&dir).unwrap();
        let sti = dir.join("live-torsdag.json");
        p.lagre(&sti).unwrap();
        let lest = StudioPreset::last(&sti).unwrap();
        assert_eq!(lest, p);

        let m2 = lest.til_motor().unwrap();
        assert_eq!(m2.hent("kanal/mik1/fader"), Some(Verdi::Tall(-4.0)));
        assert_eq!(m2.hent("kanal/mik1/trim"), Some(Verdi::Tall(4.5)));
        assert_eq!(m2.hent("kanal/spot/til_master"), Some(Verdi::AvPaa(false)));
        assert_eq!(m2.hent("kanal/mik1/hpf"), Some(Verdi::AvPaa(true)));
        assert_eq!(m2.hent("kanal/mik1/hpf_hz"), Some(Verdi::Tall(100.0)));
        assert_eq!(m2.hent("kanal/mik1/modus"), Some(Verdi::Valg(2)));
        assert_eq!(m2.hent("kanal/mik1/paa"), Some(Verdi::AvPaa(true)));
    }
}
