//! Målepunkter: nivå, true peak, DC, klipping, forsinkelse, polaritet og nulltest.

use crate::lyd::{lin_til_db, Blokk};

pub fn rms_dbfs(s: &[f32]) -> f32 {
    if s.is_empty() {
        return f32::NEG_INFINITY;
    }
    let sum: f64 = s.iter().map(|x| (*x as f64) * (*x as f64)).sum();
    lin_til_db((sum / s.len() as f64).sqrt() as f32)
}

pub fn topp_dbfs(s: &[f32]) -> f32 {
    lin_til_db(s.iter().fold(0.0f32, |m, x| m.max(x.abs())))
}

/// True peak ved 4× oversampling (ITU-R BS.1770-prinsippet), med vindusbasert sinc.
pub fn true_peak_dbfs(s: &[f32]) -> f32 {
    const OVER: usize = 4;
    const HALV: isize = 16; // filterlengde i hver retning, i opprinnelige samples
    let mut maks = s.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    let n = s.len() as isize;
    for i in 0..n {
        for fase in 1..OVER {
            let t = i as f64 + fase as f64 / OVER as f64;
            let mut sum = 0.0f64;
            for k in (i - HALV + 1)..=(i + HALV) {
                if k < 0 || k >= n {
                    continue;
                }
                let x = t - k as f64;
                let sinc = if x.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
                // Hann-vindu over filterlengden
                let w = 0.5 * (1.0 + (std::f64::consts::PI * x / HALV as f64).cos());
                sum += s[k as usize] as f64 * sinc * w;
            }
            maks = maks.max(sum.abs() as f32);
        }
    }
    lin_til_db(maks)
}

pub fn dc(s: &[f32]) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    (s.iter().map(|x| *x as f64).sum::<f64>() / s.len() as f64) as f32
}

/// Antall samples som er klippet (på eller over full skala).
pub fn klipp(s: &[f32]) -> usize {
    s.iter().filter(|x| x.abs() >= 1.0).count()
}

/// Finner forsinkelse og polaritet ved å sammenligne inn og ut (krysskorrelasjon).
/// Returnerer (forsinkelse i samples, polaritet_ok).
pub fn forsinkelse_og_polaritet(inn: &[f32], ut: &[f32], maks_forsinkelse: usize) -> Option<(usize, bool)> {
    let mut beste: Option<(usize, f64)> = None;
    for lag in 0..=maks_forsinkelse.min(ut.len().saturating_sub(1)) {
        let mut sum = 0.0f64;
        for i in 0..inn.len() {
            if i + lag >= ut.len() {
                break;
            }
            sum += inn[i] as f64 * ut[i + lag] as f64;
        }
        if beste.map_or(true, |(_, b)| sum.abs() > b.abs()) {
            beste = Some((lag, sum));
        }
    }
    beste.filter(|(_, s)| s.abs() > 1e-12).map(|(lag, s)| (lag, s > 0.0))
}

/// Resultat av en nulltest.
#[derive(Clone, Debug, PartialEq)]
pub struct Nulltest {
    /// Største avvik mellom inn og ut etter forsinkelsesjustering, i dBFS.
    /// −∞ betyr bit-identisk.
    pub rest_dbfs: f32,
    pub bit_identisk: bool,
}

impl Nulltest {
    /// Bestått når resten er under −120 dBFS (langt under det som kan høres).
    pub fn bestatt(&self) -> bool {
        self.bit_identisk || self.rest_dbfs < -120.0
    }
}

/// Nulltest: trekker inn fra ut (justert for forsinkelse). Skal bli stillhet.
pub fn nulltest(inn: &Blokk, ut: &Blokk, forsinkelse: usize) -> Nulltest {
    let mut maks = 0.0f32;
    let mut identisk = true;
    let n = inn.lengde().min(ut.lengde().saturating_sub(forsinkelse));
    for i in 0..n {
        for (a, b) in [(inn.v[i], ut.v[i + forsinkelse]), (inn.h[i], ut.h[i + forsinkelse])] {
            if a.to_bits() != b.to_bits() {
                identisk = false;
            }
            maks = maks.max((a - b).abs());
        }
    }
    Nulltest { rest_dbfs: lin_til_db(maks), bit_identisk: identisk }
}

/// Biquad-filter (brukt til K-veiing).
struct Biquad {
    b: [f64; 3],
    a: [f64; 3],
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn steg(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z1;
        self.z1 = self.b[1] * x - self.a[1] * y + self.z2;
        self.z2 = self.b[2] * x - self.a[2] * y;
        y
    }
}

fn k_veid(s: &[f32]) -> Vec<f64> {
    // ITU-R BS.1770: hyllefilter + høypass (RLB), koeffisienter for 48 kHz.
    let mut f1 = Biquad { b: [1.535_124_859_586_97, -2.691_696_189_406_38, 1.198_392_810_852_85], a: [1.0, -1.690_659_293_182_41, 0.732_480_774_215_85], z1: 0.0, z2: 0.0 };
    let mut f2 = Biquad { b: [1.0, -2.0, 1.0], a: [1.0, -1.990_047_454_833_98, 0.990_072_250_366_21], z1: 0.0, z2: 0.0 };
    s.iter().map(|x| f2.steg(f1.steg(*x as f64))).collect()
}

/// Integrert lydnivå i LUFS etter ITU-R BS.1770 / EBU R128
/// (K-veiing, 400 ms blokker med 75 % overlapp, absolutt og relativ gating).
pub fn lufs_integrert(b: &Blokk) -> f32 {
    let v = k_veid(&b.v);
    let h = k_veid(&b.h);
    let blokk = (0.4 * crate::lyd::SAMPLERATE as f64) as usize;
    let steg = blokk / 4;
    if v.len() < blokk {
        return f32::NEG_INFINITY;
    }
    let mut energier = Vec::new();
    let mut start = 0;
    while start + blokk <= v.len() {
        let e: f64 = v[start..start + blokk].iter().chain(&h[start..start + blokk]).map(|x| x * x).sum::<f64>() / blokk as f64;
        energier.push(e);
        start += steg;
    }
    let til_lufs = |e: f64| -0.691 + 10.0 * e.log10();
    let over_abs: Vec<f64> = energier.into_iter().filter(|e| *e > 0.0 && til_lufs(*e) > -70.0).collect();
    if over_abs.is_empty() {
        return f32::NEG_INFINITY;
    }
    let snitt = over_abs.iter().sum::<f64>() / over_abs.len() as f64;
    let relativ = til_lufs(snitt) - 10.0;
    let over_rel: Vec<f64> = over_abs.into_iter().filter(|e| til_lufs(*e) > relativ).collect();
    if over_rel.is_empty() {
        return f32::NEG_INFINITY;
    }
    til_lufs(over_rel.iter().sum::<f64>() / over_rel.len() as f64) as f32
}

/// Samlet måling av én stereoblokk.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Nivaa {
    pub rms_dbfs: f32,
    pub topp_dbfs: f32,
    pub true_peak_dbfs: f32,
    pub dc: f32,
    pub klipp: usize,
}

pub fn maal(b: &Blokk) -> Nivaa {
    let rms = rms_dbfs(&b.v).max(rms_dbfs(&b.h));
    Nivaa {
        rms_dbfs: rms,
        topp_dbfs: topp_dbfs(&b.v).max(topp_dbfs(&b.h)),
        true_peak_dbfs: true_peak_dbfs(&b.v).max(true_peak_dbfs(&b.h)),
        dc: dc(&b.v).abs().max(dc(&b.h).abs()),
        klipp: klipp(&b.v) + klipp(&b.h),
    }
}

#[cfg(test)]
mod tester {
    use super::*;
    use crate::testlyd;

    #[test]
    fn finner_forsinkelse_og_snudd_polaritet() {
        let inn = testlyd::puls(10, 400, -6.0);
        let mut ut = Blokk::stille(400);
        ut.v[10 + 37] = -0.5;
        let (lag, pol_ok) = forsinkelse_og_polaritet(&inn.v, &ut.v, 200).unwrap();
        assert_eq!(lag, 37);
        assert!(!pol_ok);
    }

    #[test]
    fn true_peak_er_hoyere_enn_sample_peak_ved_kvart_samplerate() {
        // 12 kHz-tone med fase som gjør at samplene bommer på toppen
        let n = 4800;
        let s: Vec<f32> = (0..n)
            .map(|i| (0.5 * (2.0 * std::f64::consts::PI * 12_000.0 * i as f64 / 48_000.0 + std::f64::consts::FRAC_PI_4).sin()) as f32)
            .collect();
        let sp = topp_dbfs(&s);
        let tp = true_peak_dbfs(&s);
        assert!(tp > sp + 2.0, "true peak {tp} skulle vært over sample peak {sp}");
        assert!((tp - lin_til_db(0.5)).abs() < 0.3, "true peak {tp}");
    }

    #[test]
    fn lufs_for_referansetone_i_begge_kanaler() {
        // BS.1770: 1 kHz på 0 dBFS i én kanal gir −3,01 LUFS; i begge kanaler 0 LUFS.
        let t = testlyd::tone(1000.0, -18.0, 3.0);
        let l = lufs_integrert(&t);
        assert!((l - -18.0).abs() < 0.15, "lufs var {l}");
    }

    #[test]
    fn nulltest_bit_identisk() {
        let a = testlyd::rosa_stoy(-20.0, 0.1);
        let r = nulltest(&a, &a.clone(), 0);
        assert!(r.bit_identisk && r.bestatt());
    }
}
