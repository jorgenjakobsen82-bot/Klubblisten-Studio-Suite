//! Laster lydfiler (WAV i første omgang) og gjør dem om til 48 kHz stereo.

use crate::Lydfeil;
use kjerne::{Blokk, SAMPLERATE};
use rubato::{FftFixedIn, Resampler};
use std::path::Path;

/// Leser en WAV-fil og konverterer til 48 kHz om nødvendig (høy kvalitet, FFT-basert).
pub fn last_lydfil(sti: &Path) -> Result<Blokk, Lydfeil> {
    let ext = sti.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if ext != "wav" {
        return Err(Lydfeil::Fil(format!(
            "Foreløpig kan suiten bare spille WAV-filer («{}» er .{ext}). MP3, FLAC og AIFF kommer.",
            sti.display()
        )));
    }
    let r = hound::WavReader::open(sti).map_err(|e| Lydfeil::Fil(format!("Kunne ikke åpne «{}»: {e}", sti.display())))?;
    let sr = r.spec().sample_rate;
    drop(r);
    let blokk = kjerne::testlyd::les_wav(sti).map_err(|e| Lydfeil::Fil(format!("Kunne ikke lese «{}»: {e}", sti.display())))?;
    if sr == SAMPLERATE {
        Ok(blokk)
    } else {
        konverter(&blokk, sr).map_err(Lydfeil::Fil)
    }
}

/// Samplerate-konvertering av en hel blokk.
pub fn konverter(inn: &Blokk, fra_hz: u32) -> Result<Blokk, String> {
    const BIT: usize = 1024;
    let mut r = FftFixedIn::<f32>::new(fra_hz as usize, SAMPLERATE as usize, BIT, 2, 2).map_err(|e| e.to_string())?;
    let forsinkelse = r.output_delay();
    let forventet = (inn.lengde() as f64 * SAMPLERATE as f64 / fra_hz as f64).round() as usize;
    let mut v = Vec::with_capacity(forventet + BIT * 2);
    let mut h = Vec::with_capacity(forventet + BIT * 2);
    let mut pos = 0;
    // Mat inn hele fila pluss nok stillhet til å tømme filteret.
    while v.len() < forventet + forsinkelse {
        let mut del_v = vec![0.0f32; BIT];
        let mut del_h = vec![0.0f32; BIT];
        let n = BIT.min(inn.lengde().saturating_sub(pos));
        del_v[..n].copy_from_slice(&inn.v[pos..pos + n]);
        del_h[..n].copy_from_slice(&inn.h[pos..pos + n]);
        pos += n;
        let ut = r.process(&[del_v, del_h], None).map_err(|e| e.to_string())?;
        v.extend_from_slice(&ut[0]);
        h.extend_from_slice(&ut[1]);
    }
    Ok(Blokk { v: v[forsinkelse..forsinkelse + forventet].to_vec(), h: h[forsinkelse..forsinkelse + forventet].to_vec() })
}

#[cfg(test)]
mod tester {
    use super::*;
    use kjerne::maaling;

    #[test]
    fn konvertering_fra_44_1_beholder_frekvens_og_niva() {
        // 1 kHz på −18 dBFS laget i 44,1 kHz
        let n = 44_100;
        let s: Vec<f32> = (0..n)
            .map(|i| (kjerne::db_til_lin(-18.0) as f64 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 44_100.0).sin()) as f32)
            .collect();
        let ut = konverter(&Blokk::fra_mono(&s), 44_100).unwrap();
        assert_eq!(ut.lengde(), 48_000);
        let midt = &ut.v[4800..43_200];
        assert!((maaling::topp_dbfs(midt) - -18.0).abs() < 0.1, "nivå {}", maaling::topp_dbfs(midt));
        // Fasit: en ekte 1 kHz-tone i 48 kHz, fasejustert
        let fasit = kjerne::testlyd::tone(1000.0, -18.0, 1.0);
        let r = maaling::nulltest(&Blokk::fra_mono(midt), &fasit.utsnitt(4800, midt.len()), 0);
        assert!(r.rest_dbfs < -60.0, "rest {}", r.rest_dbfs);
    }
}
