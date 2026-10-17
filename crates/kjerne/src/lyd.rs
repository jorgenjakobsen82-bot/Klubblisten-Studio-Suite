//! Grunnleggende lydtyper: stereoblokk, samplerate og dB-omregning.

/// Fast intern samplerate (vedtatt: 48 kHz gjennom hele kjeden).
pub const SAMPLERATE: u32 = 48_000;

/// Standard blokkstørrelse i samples (1,33 ms ved 48 kHz).
pub const BLOKKSTORRELSE: usize = 64;

/// Referansenivå for linjesjekk: 1 kHz på −18 dBFS.
pub const REFERANSENIVAA_DBFS: f32 = -18.0;

/// En stereoblokk med lyd (venstre og høyre), 32-bit flyttall.
#[derive(Clone, Debug, PartialEq)]
pub struct Blokk {
    pub v: Vec<f32>,
    pub h: Vec<f32>,
}

impl Blokk {
    pub fn stille(lengde: usize) -> Self {
        Blokk { v: vec![0.0; lengde], h: vec![0.0; lengde] }
    }

    pub fn fra_mono(sampler: &[f32]) -> Self {
        Blokk { v: sampler.to_vec(), h: sampler.to_vec() }
    }

    pub fn lengde(&self) -> usize {
        self.v.len()
    }

    pub fn nullstill(&mut self) {
        self.v.iter_mut().for_each(|s| *s = 0.0);
        self.h.iter_mut().for_each(|s| *s = 0.0);
    }

    /// Legger `annen` (ganget med `gain`) oppå denne blokken.
    pub fn legg_til(&mut self, annen: &Blokk, gain: f32) {
        for (a, b) in self.v.iter_mut().zip(&annen.v) {
            *a += b * gain;
        }
        for (a, b) in self.h.iter_mut().zip(&annen.h) {
            *a += b * gain;
        }
    }

    pub fn kopier_fra(&mut self, annen: &Blokk) {
        self.v.copy_from_slice(&annen.v);
        self.h.copy_from_slice(&annen.h);
    }

    /// Klipper ut en del av blokken.
    pub fn utsnitt(&self, start: usize, lengde: usize) -> Blokk {
        Blokk { v: self.v[start..start + lengde].to_vec(), h: self.h[start..start + lengde].to_vec() }
    }
}

/// Omregning fra dB til lineær forsterkning. 0 dB gir nøyaktig 1,0.
pub fn db_til_lin(db: f32) -> f32 {
    if db <= -150.0 {
        0.0
    } else if db == 0.0 {
        1.0
    } else {
        10f32.powf(db / 20.0)
    }
}

/// Omregning fra lineært nivå til dB. Stillhet gir −∞.
pub fn lin_til_db(lin: f32) -> f32 {
    if lin <= 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * lin.log10()
    }
}

/// Myk overgang for parametere, så endringer aldri klikker.
/// Når målet er nådd, står verdien nøyaktig på målet (viktig for nulltest).
#[derive(Clone, Debug)]
pub struct Glatter {
    naa: f32,
    maal: f32,
    steg_faktor: f32,
}

impl Glatter {
    /// `tid_ms` er hvor lang tid overgangen tar (ca. 99 % av veien).
    pub fn ny(start: f32, tid_ms: f32) -> Self {
        let samples = (tid_ms / 1000.0 * SAMPLERATE as f32).max(1.0);
        // Eksponentiell overgang som når ~99 % etter `samples`.
        let steg_faktor = 1.0 - (0.01f32).powf(1.0 / samples);
        Glatter { naa: start, maal: start, steg_faktor }
    }

    pub fn sett_maal(&mut self, maal: f32) {
        self.maal = maal;
    }

    pub fn sett_straks(&mut self, verdi: f32) {
        self.naa = verdi;
        self.maal = verdi;
    }

    pub fn maal(&self) -> f32 {
        self.maal
    }

    pub fn er_ferdig(&self) -> bool {
        self.naa == self.maal
    }

    #[inline]
    pub fn neste(&mut self) -> f32 {
        if self.naa != self.maal {
            let ny = self.naa + (self.maal - self.naa) * self.steg_faktor;
            // Snapp til målet når resten er under −100 dB, eller når flyttallene
            // ikke lenger rikker seg (ellers kan overgangen stå fast like før målet).
            if ny == self.naa || (ny - self.maal).abs() < 1e-5 {
                self.naa = self.maal;
            } else {
                self.naa = ny;
            }
        }
        self.naa
    }
}

#[cfg(test)]
mod tester {
    use super::*;

    #[test]
    fn null_db_er_noyaktig_en() {
        assert_eq!(db_til_lin(0.0), 1.0);
        assert!((db_til_lin(-6.0) - 0.501_187).abs() < 1e-5);
        assert_eq!(db_til_lin(-200.0), 0.0);
    }

    #[test]
    fn glatter_lander_noyaktig() {
        let mut g = Glatter::ny(0.0, 10.0);
        g.sett_maal(1.0);
        for _ in 0..48_000 {
            g.neste();
        }
        assert!(g.er_ferdig());
        assert_eq!(g.neste(), 1.0);
    }
}
