//! Gain/trim: enkel nivåjustering i dB. Brukes som inngangstrim og som
//! automatisk nivåretting i linjesjekken.

use crate::kontroll::{Kontroll, Kontrollfeil, Kontrolltype, Verdi};
use crate::lyd::{db_til_lin, Blokk, Glatter};
use crate::modul::Modul;

#[derive(Clone)]
pub struct Gain {
    db: f32,
    gain: Glatter,
}

impl Gain {
    pub fn ny(db: f32) -> Self {
        Gain { db, gain: Glatter::ny(db_til_lin(db), 10.0) }
    }

    pub fn db(&self) -> f32 {
        self.db
    }
}

impl Modul for Gain {
    fn typenavn(&self) -> &'static str {
        "Gain"
    }

    fn kontroller(&self) -> Vec<Kontroll> {
        vec![Kontroll::ny(
            "gain",
            "Gain",
            Kontrolltype::Dreieknapp { min: -24.0, maks: 24.0, enhet: "dB".into() },
            Verdi::Tall(0.0),
        )]
    }

    fn sett(&mut self, kontroll: &str, verdi: Verdi) -> Result<(), Kontrollfeil> {
        match kontroll {
            "gain" => {
                let db = verdi.som_tall().ok_or_else(|| Kontrollfeil::UgyldigVerdi(kontroll.into()))?;
                self.db = db.clamp(-24.0, 24.0);
                self.gain.sett_maal(db_til_lin(self.db));
                Ok(())
            }
            _ => Err(Kontrollfeil::UkjentKontroll(kontroll.into())),
        }
    }

    fn hent(&self, kontroll: &str) -> Option<Verdi> {
        (kontroll == "gain").then(|| Verdi::Tall(self.db))
    }

    fn prosesser(&mut self, inn: &Blokk, ut: &mut Blokk) {
        if self.gain.er_ferdig() && self.gain.maal() == 1.0 {
            ut.kopier_fra(inn);
            return;
        }
        for i in 0..inn.lengde() {
            let g = self.gain.neste();
            ut.v[i] = inn.v[i] * g;
            ut.h[i] = inn.h[i] * g;
        }
    }

    fn er_noytral(&self) -> bool {
        self.db == 0.0
    }

    fn nullstill(&mut self) {
        self.gain.sett_straks(db_til_lin(self.db));
    }

    fn klon(&self) -> Box<dyn Modul> {
        Box::new(self.clone())
    }
}
