//! Inngangstrinnet først i hver kanalstripe, som på en konsoll:
//! kanalmodus (stereo / mono / venstre / høyre / bytt), fasevending og
//! lavkuttfilter (HPF). Ingen forsinkelse, så det ligger i begge lydveier.
//! Står alt nøytralt, er lyden bit-identisk.

use crate::lyd::{Blokk, Glatter, SAMPLERATE};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Kanalmodus {
    #[default]
    Stereo,
    /// (V + H) / 2 i begge sider.
    Mono,
    /// Bare venstre inngang, i begge sider (f.eks. mikrofon på inngang 1).
    Venstre,
    /// Bare høyre inngang, i begge sider.
    Hoyre,
    /// Venstre og høyre byttet om.
    Bytt,
}

impl Kanalmodus {
    pub fn fra_tekst(t: &str) -> Option<Self> {
        Some(match t {
            "stereo" => Kanalmodus::Stereo,
            "mono" => Kanalmodus::Mono,
            "venstre" => Kanalmodus::Venstre,
            "hoyre" => Kanalmodus::Hoyre,
            "bytt" => Kanalmodus::Bytt,
            _ => return None,
        })
    }

    pub fn tekst(&self) -> &'static str {
        match self {
            Kanalmodus::Stereo => "stereo",
            Kanalmodus::Mono => "mono",
            Kanalmodus::Venstre => "venstre",
            Kanalmodus::Hoyre => "hoyre",
            Kanalmodus::Bytt => "bytt",
        }
    }
}

/// Andreordens Butterworth høypass (12 dB/oktav), to i serie = 24 dB/oktav.
#[derive(Clone, Debug)]
struct Hoypass {
    b: [f64; 3],
    a: [f64; 3],
    z: [[f64; 2]; 4], // [trinn*2 + side][tilstand]
}

impl Hoypass {
    fn ny(hz: f32) -> Self {
        let mut h = Hoypass { b: [0.0; 3], a: [0.0; 3], z: [[0.0; 2]; 4] };
        h.sett(hz);
        h
    }

    fn sett(&mut self, hz: f32) {
        let w0 = 2.0 * std::f64::consts::PI * hz as f64 / SAMPLERATE as f64;
        let q = std::f64::consts::FRAC_1_SQRT_2;
        let alfa = w0.sin() / (2.0 * q);
        let c = w0.cos();
        let a0 = 1.0 + alfa;
        self.b = [(1.0 + c) / 2.0 / a0, -(1.0 + c) / a0, (1.0 + c) / 2.0 / a0];
        self.a = [1.0, -2.0 * c / a0, (1.0 - alfa) / a0];
    }

    #[inline]
    fn steg(&mut self, x: f32, side: usize) -> f32 {
        let mut y = x as f64;
        for trinn in 0..2 {
            let z = &mut self.z[trinn * 2 + side];
            let ut = self.b[0] * y + z[0];
            z[0] = self.b[1] * y - self.a[1] * ut + z[1];
            z[1] = self.b[2] * y - self.a[2] * ut;
            y = ut;
        }
        y as f32
    }

    fn nullstill(&mut self) {
        self.z = [[0.0; 2]; 4];
    }

    /// Setter bittesmå tilstandsverdier til null, så filteret ikke blir tregt i stillhet (denormaler).
    fn rydd(&mut self) {
        for z in self.z.iter_mut().flatten() {
            if z.abs() < 1e-25 {
                *z = 0.0;
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Inngangstrinn {
    pub modus: Kanalmodus,
    pub fase: bool,
    pub hpf_paa: bool,
    pub hpf_hz: f32,
    filter: Hoypass,
    /// 0 = filter av, 1 = på. Glir over 10 ms så det aldri klikker.
    blanding: Glatter,
}

impl Default for Inngangstrinn {
    fn default() -> Self {
        Inngangstrinn { modus: Kanalmodus::Stereo, fase: false, hpf_paa: false, hpf_hz: 80.0, filter: Hoypass::ny(80.0), blanding: Glatter::ny(0.0, 10.0) }
    }
}

impl Inngangstrinn {
    pub fn sett_hpf(&mut self, paa: bool) {
        self.hpf_paa = paa;
        self.blanding.sett_maal(if paa { 1.0 } else { 0.0 });
    }

    pub fn sett_hpf_hz(&mut self, hz: f32) {
        self.hpf_hz = hz.clamp(20.0, 400.0);
        self.filter.sett(self.hpf_hz);
    }

    pub fn er_noytral(&self) -> bool {
        self.modus == Kanalmodus::Stereo && !self.fase && !self.hpf_paa && self.blanding.er_ferdig()
    }

    pub fn nullstill(&mut self) {
        self.filter.nullstill();
        self.blanding.sett_straks(self.blanding.maal());
    }

    pub fn prosesser(&mut self, inn: &Blokk, ut: &mut Blokk) {
        if self.er_noytral() {
            ut.kopier_fra(inn);
            // Hold filteret varmt, så det ikke klikker når det slås på.
            for i in 0..inn.lengde() {
                self.filter.steg(inn.v[i], 0);
                self.filter.steg(inn.h[i], 1);
            }
            self.filter.rydd();
            return;
        }
        let fortegn = if self.fase { -1.0 } else { 1.0 };
        for i in 0..inn.lengde() {
            let (v, h) = match self.modus {
                Kanalmodus::Stereo => (inn.v[i], inn.h[i]),
                Kanalmodus::Mono => {
                    let m = (inn.v[i] + inn.h[i]) * 0.5;
                    (m, m)
                }
                Kanalmodus::Venstre => (inn.v[i], inn.v[i]),
                Kanalmodus::Hoyre => (inn.h[i], inn.h[i]),
                Kanalmodus::Bytt => (inn.h[i], inn.v[i]),
            };
            let (v, h) = (v * fortegn, h * fortegn);
            let fv = self.filter.steg(v, 0);
            let fh = self.filter.steg(h, 1);
            let b = self.blanding.neste();
            ut.v[i] = if b == 0.0 { v } else if b == 1.0 { fv } else { v + (fv - v) * b };
            ut.h[i] = if b == 0.0 { h } else if b == 1.0 { fh } else { h + (fh - h) * b };
        }
        self.filter.rydd();
    }
}

#[cfg(test)]
mod tester {
    use super::*;
    use crate::{maaling, testlyd};

    fn kjor(t: &mut Inngangstrinn, inn: &Blokk) -> Blokk {
        let mut ut = Blokk::stille(inn.lengde());
        t.prosesser(inn, &mut ut);
        ut
    }

    #[test]
    fn noytral_er_bit_identisk() {
        let s = testlyd::rosa_stoy(-20.0, 0.2);
        let mut t = Inngangstrinn::default();
        assert_eq!(kjor(&mut t, &s), s);
    }

    #[test]
    fn hpf_fjerner_rumling_men_ikke_stemme() {
        let mut t = Inngangstrinn::default();
        t.sett_hpf_hz(100.0);
        t.sett_hpf(true);
        let lav = kjor(&mut t, &testlyd::tone(30.0, -12.0, 1.0));
        let mut t2 = Inngangstrinn::default();
        t2.sett_hpf_hz(100.0);
        t2.sett_hpf(true);
        let stemme = kjor(&mut t2, &testlyd::tone(1000.0, -12.0, 1.0));
        let fra = 24_000;
        assert!(maaling::topp_dbfs(&lav.v[fra..]) < -50.0, "30 Hz skal dempes kraftig: {}", maaling::topp_dbfs(&lav.v[fra..]));
        assert!((maaling::topp_dbfs(&stemme.v[fra..]) - -12.0).abs() < 0.1, "1 kHz skal være uendret");
    }

    #[test]
    fn fase_og_modus() {
        let vh = Blokk { v: vec![0.5; 8], h: vec![0.25; 8] };
        let mut t = Inngangstrinn { fase: true, ..Default::default() };
        let ut = kjor(&mut t, &vh);
        assert_eq!((ut.v[0], ut.h[0]), (-0.5, -0.25));
        let mut t = Inngangstrinn { modus: Kanalmodus::Bytt, ..Default::default() };
        let ut = kjor(&mut t, &vh);
        assert_eq!((ut.v[0], ut.h[0]), (0.25, 0.5));
        let mut t = Inngangstrinn { modus: Kanalmodus::Venstre, ..Default::default() };
        let ut = kjor(&mut t, &vh);
        assert_eq!((ut.v[0], ut.h[0]), (0.5, 0.5));
        let mut t = Inngangstrinn { modus: Kanalmodus::Mono, ..Default::default() };
        let ut = kjor(&mut t, &vh);
        assert_eq!((ut.v[0], ut.h[0]), (0.375, 0.375));
    }
}
