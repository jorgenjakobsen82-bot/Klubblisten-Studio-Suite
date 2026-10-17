//! Kontroll-registeret: alt som kan styres i suiten, med type.
//!
//! Hver modul oppgir sine kontroller her. MIDI-learn, presets og
//! fjernstyring bruker registeret, så nye moduler blir styrbare automatisk.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Hva slags kontroll det er. Avgjør hvilken fysisk kontroll som passer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Kontrolltype {
    /// Fader i dB, f.eks. −∞ (min) til +10 dB.
    Fader { min_db: f32, max_db: f32 },
    /// Dreieknapp med område, f.eks. pan −1..1 eller 0..100 %.
    Dreieknapp { min: f32, maks: f32, enhet: String },
    /// Knapp av/på (trykk én gang = på, igjen = av).
    AvPaa,
    /// Knapp som er på så lenge den holdes (f.eks. Talk, hosteknapp).
    Hold,
    /// Knapp som utløser noe én gang (f.eks. spill jingle).
    Utloser,
    /// Valgliste.
    Valg { valg: Vec<String> },
}

/// En verdi for en kontroll.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Verdi {
    Tall(f32),
    AvPaa(bool),
    Valg(usize),
}

impl Verdi {
    pub fn som_tall(&self) -> Option<f32> {
        match self {
            Verdi::Tall(t) => Some(*t),
            Verdi::Valg(v) => Some(*v as f32),
            Verdi::AvPaa(b) => Some(if *b { 1.0 } else { 0.0 }),
        }
    }

    pub fn som_bool(&self) -> Option<bool> {
        match self {
            Verdi::AvPaa(b) => Some(*b),
            Verdi::Tall(t) => Some(*t >= 0.5),
            Verdi::Valg(v) => Some(*v > 0),
        }
    }
}

/// Beskrivelse av én kontroll i en modul.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Kontroll {
    /// Kort id innenfor modulen, f.eks. "fader".
    pub id: String,
    /// Navn i klarspråk, f.eks. "Fader".
    pub navn: String,
    pub kontrolltype: Kontrolltype,
    pub standard: Verdi,
}

impl Kontroll {
    pub fn ny(id: &str, navn: &str, kontrolltype: Kontrolltype, standard: Verdi) -> Self {
        Kontroll { id: id.into(), navn: navn.into(), kontrolltype, standard }
    }
}

/// En kontroll med full adresse i suiten, f.eks. "kanal/mik1/fader".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegistrertKontroll {
    pub adresse: String,
    pub modulnavn: String,
    pub kontroll: Kontroll,
}

/// Feil ved styring av en kontroll.
#[derive(Clone, Debug, PartialEq)]
pub enum Kontrollfeil {
    UkjentKontroll(String),
    UgyldigVerdi(String),
    Sperret(String),
}

impl fmt::Display for Kontrollfeil {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Kontrollfeil::UkjentKontroll(k) => write!(f, "Fant ingen kontroll som heter «{k}»"),
            Kontrollfeil::UgyldigVerdi(k) => write!(f, "Ugyldig verdi for «{k}»"),
            Kontrollfeil::Sperret(grunn) => write!(f, "Ikke tillatt: {grunn}"),
        }
    }
}

impl std::error::Error for Kontrollfeil {}
