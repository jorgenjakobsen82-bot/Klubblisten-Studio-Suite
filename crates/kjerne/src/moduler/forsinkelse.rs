//! Forsinkelse: forsinker lyden et antall samples. Brukes til
//! forsinkelseskompensasjon i sendeveien (så alt kommer likt ut mot vMix),
//! og i tester av at lytteveien holdes fri for forsinkelse.

use crate::kontroll::{Kontroll, Kontrollfeil, Kontrolltype, Verdi};
use crate::lyd::Blokk;
use crate::modul::Modul;

#[derive(Clone)]
pub struct Forsinkelse {
    samples: usize,
    buf_v: Vec<f32>,
    buf_h: Vec<f32>,
    pos: usize,
}

impl Forsinkelse {
    pub fn ny(samples: usize) -> Self {
        let n = samples.max(1);
        Forsinkelse { samples, buf_v: vec![0.0; n], buf_h: vec![0.0; n], pos: 0 }
    }
}

impl Modul for Forsinkelse {
    fn typenavn(&self) -> &'static str {
        "Forsinkelse"
    }

    fn kontroller(&self) -> Vec<Kontroll> {
        vec![Kontroll::ny(
            "samples",
            "Forsinkelse",
            Kontrolltype::Dreieknapp { min: 0.0, maks: 48_000.0, enhet: "samples".into() },
            Verdi::Tall(0.0),
        )]
    }

    fn sett(&mut self, kontroll: &str, verdi: Verdi) -> Result<(), Kontrollfeil> {
        match kontroll {
            "samples" => {
                let s = verdi.som_tall().ok_or_else(|| Kontrollfeil::UgyldigVerdi(kontroll.into()))?;
                *self = Forsinkelse::ny(s.max(0.0) as usize);
                Ok(())
            }
            _ => Err(Kontrollfeil::UkjentKontroll(kontroll.into())),
        }
    }

    fn hent(&self, kontroll: &str) -> Option<Verdi> {
        (kontroll == "samples").then(|| Verdi::Tall(self.samples as f32))
    }

    fn prosesser(&mut self, inn: &Blokk, ut: &mut Blokk) {
        if self.samples == 0 {
            ut.kopier_fra(inn);
            return;
        }
        let n = self.buf_v.len();
        for i in 0..inn.lengde() {
            ut.v[i] = self.buf_v[self.pos];
            ut.h[i] = self.buf_h[self.pos];
            self.buf_v[self.pos] = inn.v[i];
            self.buf_h[self.pos] = inn.h[i];
            self.pos = (self.pos + 1) % n;
        }
    }

    fn forsinkelse(&self) -> usize {
        self.samples
    }

    /// En forsinkelse endrer ikke lyden, bare tidspunktet. Nulltesten
    /// justerer for forsinkelsen, så den regnes som nøytral.
    fn er_noytral(&self) -> bool {
        true
    }

    fn nullstill(&mut self) {
        self.buf_v.iter_mut().for_each(|s| *s = 0.0);
        self.buf_h.iter_mut().for_each(|s| *s = 0.0);
        self.pos = 0;
    }

    fn klon(&self) -> Box<dyn Modul> {
        Box::new(self.clone())
    }
}
