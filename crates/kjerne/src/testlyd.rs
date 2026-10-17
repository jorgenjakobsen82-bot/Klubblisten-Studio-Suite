//! Testsignaler som sendes gjennom kjeden: tone, sveip, rosa støy, puls,
//! venstre/høyre-ID og stillhet. Alt er deterministisk, så testene gir
//! samme resultat hver gang.

use crate::lyd::{db_til_lin, Blokk, SAMPLERATE};
use std::f64::consts::PI;
use std::path::Path;

fn antall(sekunder: f32) -> usize {
    (sekunder * SAMPLERATE as f32).round() as usize
}

/// Sinustone, f.eks. 1 kHz på −18 dBFS (referansen).
/// Nivået er toppnivå (peak), som er vanlig for kalibreringstoner.
pub fn tone(frekvens: f32, dbfs: f32, sekunder: f32) -> Blokk {
    let a = db_til_lin(dbfs) as f64;
    let n = antall(sekunder);
    let s: Vec<f32> = (0..n)
        .map(|i| (a * (2.0 * PI * frekvens as f64 * i as f64 / SAMPLERATE as f64).sin()) as f32)
        .collect();
    Blokk::fra_mono(&s)
}

/// Logaritmisk frekvenssveip fra `f0` til `f1`.
pub fn sveip(f0: f32, f1: f32, dbfs: f32, sekunder: f32) -> Blokk {
    let a = db_til_lin(dbfs) as f64;
    let n = antall(sekunder);
    let t_tot = sekunder as f64;
    let k = (f1 as f64 / f0 as f64).ln();
    let s: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f64 / SAMPLERATE as f64;
            let fase = 2.0 * PI * f0 as f64 * t_tot / k * ((t / t_tot * k).exp() - 1.0);
            (a * fase.sin()) as f32
        })
        .collect();
    Blokk::fra_mono(&s)
}

/// Rosa støy (lik energi per oktav). Nivået er omtrent RMS.
pub fn rosa_stoy(dbfs: f32, sekunder: f32) -> Blokk {
    let n = antall(sekunder);
    let mut tilstand: u32 = 0x1234_5678;
    let mut tilfeldig = move || {
        // xorshift32, deterministisk
        tilstand ^= tilstand << 13;
        tilstand ^= tilstand >> 17;
        tilstand ^= tilstand << 5;
        (tilstand as f32 / u32::MAX as f32) * 2.0 - 1.0
    };
    // Paul Kellets filter for rosa støy
    let (mut b0, mut b1, mut b2, mut b3, mut b4, mut b5, mut b6) = (0.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    let mut s = Vec::with_capacity(n);
    for _ in 0..n {
        let w = tilfeldig();
        b0 = 0.99886 * b0 + w * 0.0555179;
        b1 = 0.99332 * b1 + w * 0.0750759;
        b2 = 0.96900 * b2 + w * 0.1538520;
        b3 = 0.86650 * b3 + w * 0.3104856;
        b4 = 0.55000 * b4 + w * 0.5329522;
        b5 = -0.7616 * b5 - w * 0.0168980;
        s.push(b0 + b1 + b2 + b3 + b4 + b5 + b6 + w * 0.5362);
        b6 = w * 0.115926;
    }
    // Skaler til ønsket RMS
    let rms = (s.iter().map(|x| x * x).sum::<f32>() / n.max(1) as f32).sqrt();
    let maal = db_til_lin(dbfs);
    let k = if rms > 0.0 { maal / rms } else { 0.0 };
    s.iter_mut().for_each(|x| *x *= k);
    Blokk::fra_mono(&s)
}

/// Én positiv puls i stillhet. Brukes til å måle forsinkelse og polaritet.
pub fn puls(posisjon: usize, lengde: usize, dbfs: f32) -> Blokk {
    let mut b = Blokk::stille(lengde);
    let a = db_til_lin(dbfs);
    if posisjon < lengde {
        b.v[posisjon] = a;
        b.h[posisjon] = a;
    }
    b
}

/// Venstre/høyre-ID: tone bare i venstre første halvdel, bare i høyre andre halvdel.
pub fn venstre_hoyre(dbfs: f32, sekunder: f32) -> Blokk {
    let t = tone(1000.0, dbfs, sekunder);
    let n = t.lengde();
    let halv = n / 2;
    let mut b = Blokk::stille(n);
    b.v[..halv].copy_from_slice(&t.v[..halv]);
    b.h[halv..].copy_from_slice(&t.h[halv..]);
    b
}

pub fn stillhet(sekunder: f32) -> Blokk {
    Blokk::stille(antall(sekunder))
}

/// Skriver en blokk til WAV (32-bit flyttall, 48 kHz), f.eks. for lyttetest hjemme.
pub fn skriv_wav(sti: &Path, blokk: &Blokk) -> Result<(), hound::Error> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SAMPLERATE,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(sti, spec)?;
    for i in 0..blokk.lengde() {
        w.write_sample(blokk.v[i])?;
        w.write_sample(blokk.h[i])?;
    }
    w.finalize()
}

/// Leser en WAV-fil (stereo eller mono) til en blokk.
pub fn les_wav(sti: &Path) -> Result<Blokk, hound::Error> {
    let mut r = hound::WavReader::open(sti)?;
    let spec = r.spec();
    let kanaler = spec.channels as usize;
    let sampler: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let maks = (1i64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>().map(|s| s.map(|v| v as f32 / maks)).collect::<Result<_, _>>()?
        }
    };
    let n = sampler.len() / kanaler.max(1);
    let mut b = Blokk::stille(n);
    for i in 0..n {
        b.v[i] = sampler[i * kanaler];
        b.h[i] = sampler[i * kanaler + if kanaler > 1 { 1 } else { 0 }];
    }
    Ok(b)
}

#[cfg(test)]
mod tester {
    use super::*;
    use crate::maaling;

    #[test]
    fn referansetone_har_riktig_niva() {
        let t = tone(1000.0, -18.0, 1.0);
        let topp = maaling::topp_dbfs(&t.v);
        assert!((topp - -18.0).abs() < 0.01, "topp var {topp}");
        // RMS av sinus er 3,01 dB under toppen
        let rms = maaling::rms_dbfs(&t.v);
        assert!((rms - -21.01).abs() < 0.05, "rms var {rms}");
    }

    #[test]
    fn rosa_stoy_er_deterministisk() {
        assert_eq!(rosa_stoy(-20.0, 0.1), rosa_stoy(-20.0, 0.1));
    }
}
