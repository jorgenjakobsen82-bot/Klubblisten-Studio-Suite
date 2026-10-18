//! Hendelser: modulene snakker sammen ved å sende hendelser via kjernen,
//! uten å kjenne hverandre. F.eks. lytter ON AIR-lampe, klokke og
//! monitorkutt alle på «mikrofon åpen».

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "hendelse", rename_all = "snake_case")]
pub enum Hendelse {
    MikrofonAapen { kanal: String },
    MikrofonLukket { kanal: String },
    /// Minst én mikrofon er åpen (ON AIR).
    OnAir { paa: bool },
    PresetLastet { navn: String },
    Klipping { sted: String },
    KontrollEndret { adresse: String },
    /// Kanalen ble slått på / faderen skjøvet opp (starter spilleren hvis fader-start er på).
    FaderStart { kanal: String },
    /// Kanalen ble slått av / faderen tatt ned.
    FaderStopp { kanal: String },
}

/// Enkel hendelsesbuss. Hendelser samles og hentes ut av den som lytter.
#[derive(Default, Debug)]
pub struct Hendelsesbuss {
    ko: Vec<Hendelse>,
}

impl Hendelsesbuss {
    pub fn send(&mut self, h: Hendelse) {
        self.ko.push(h);
    }

    /// Henter og tømmer alle hendelser siden sist.
    pub fn hent_alle(&mut self) -> Vec<Hendelse> {
        std::mem::take(&mut self.ko)
    }

    pub fn er_tom(&self) -> bool {
        self.ko.is_empty()
    }
}
