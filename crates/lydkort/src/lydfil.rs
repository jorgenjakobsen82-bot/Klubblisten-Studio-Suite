//! Laster lydfiler og gjør dem klare for spillerne:
//! dekoder (WAV, AIFF, MP3, AAC/M4A, FLAC, OGG), konverterer til 48 kHz stereo,
//! leser artist og tittel, og analyserer fila (bølgeform, loudness og autocue).

use crate::Lydfeil;
use kjerne::{Blokk, SAMPLERATE};
use rubato::{FftFixedIn, Resampler};
use serde::Serialize;
use std::path::Path;

/// Bølgeformen har én verdi per 10 ms.
pub const BOLGE_PER_SEK: usize = 100;

/// Autocue: hvor lyden faktisk starter og slutter, og hvor neste låt bør starte.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Autocue {
    /// Første lyd over −45 dBFS (sekunder).
    pub start: f32,
    /// Siste lyd over −45 dBFS.
    pub slutt: f32,
    /// Der låta faller merkbart av mot slutten – naturlig sted å starte neste.
    pub miks_ut: f32,
}

#[derive(Clone, Debug, Serialize)]
pub struct Lydinfo {
    pub filnavn: String,
    pub tittel: Option<String>,
    pub artist: Option<String>,
    /// F.eks. «MP3 · 44,1 kHz · stereo».
    pub format: String,
    pub sekunder: f32,
    /// Integrert loudness (EBU R128 / BS.1770).
    pub lufs: Option<f32>,
    pub autocue: Autocue,
    /// Toppnivå per 10 ms, 0–255 (kvadratrot-skala, ser naturlig ut).
    pub bolge: Vec<u8>,
}

pub struct Lydfil {
    pub lyd: Blokk,
    pub info: Lydinfo,
}

/// Leser en lydfil og gir bare lyden (48 kHz stereo).
pub fn last_lydfil(sti: &Path) -> Result<Blokk, Lydfeil> {
    Ok(last_med_info(sti)?.lyd)
}

/// Leser og analyserer en lydfil.
pub fn last_med_info(sti: &Path) -> Result<Lydfil, Lydfeil> {
    let (mut blokk, sr, mut tittel, mut artist, codec, kanaler) = dekod(sti)?;
    // AIFF: dekoderen kan lese for langt når fila har ekstra blokker etter lyden
    // (f.eks. tagger). Antall rammer i COMM-blokken er fasit.
    if let Some(n) = aiff_rammer(sti) {
        if blokk.lengde() > n {
            blokk.v.truncate(n);
            blokk.h.truncate(n);
        }
    }
    // AIFF: taggene ligger i en ID3-blokk som dekoderen ikke leser – les den selv.
    if tittel.is_none() || artist.is_none() {
        if let Some((t, a)) = aiff_id3(sti) {
            tittel = tittel.or(t);
            artist = artist.or(a);
        }
    }
    // Siste utvei: «Artist - Tittel.mp3» i filnavnet.
    if tittel.is_none() {
        if let Some(stamme) = sti.file_stem().and_then(|s| s.to_str()) {
            match stamme.split_once(" - ") {
                Some((a, t)) => {
                    artist = artist.or(Some(a.trim().to_string()));
                    tittel = Some(t.trim().to_string());
                }
                None => tittel = Some(stamme.to_string()),
            }
        }
    }
    let lyd = if sr == SAMPLERATE { blokk } else { konverter(&blokk, sr).map_err(Lydfeil::Fil)? };
    let sekunder = lyd.lengde() as f32 / SAMPLERATE as f32;
    let lufs = {
        let l = kjerne::maaling::lufs_integrert(&lyd);
        l.is_finite().then_some(l)
    };
    let info = Lydinfo {
        filnavn: sti.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        tittel,
        artist,
        format: format!(
            "{codec} · {} kHz · {}",
            (sr as f32 / 1000.0).to_string().replace('.', ","),
            match kanaler {
                1 => "mono".to_string(),
                2 => "stereo".to_string(),
                n => format!("{n} kanaler"),
            }
        ),
        sekunder,
        lufs,
        autocue: autocue(&lyd),
        bolge: bolgeform(&lyd),
    };
    Ok(Lydfil { lyd, info })
}

type Dekodet = (Blokk, u32, Option<String>, Option<String>, String, usize);

fn dekod(sti: &Path) -> Result<Dekodet, Lydfeil> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
    use symphonia::core::errors::Error;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::{MetadataOptions, StandardTagKey};
    use symphonia::core::probe::Hint;

    let vis = sti.display();
    let fil = std::fs::File::open(sti).map_err(|e| Lydfeil::Fil(format!("Kunne ikke åpne «{vis}»: {e}")))?;
    let mss = MediaSourceStream::new(Box::new(fil), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = sti.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let fo = FormatOptions { enable_gapless: true, ..Default::default() };
    let mut probed = symphonia::default::get_probe()
        .format(&hint, mss, &fo, &MetadataOptions::default())
        .map_err(|_| Lydfeil::Fil(format!("«{vis}» er ikke en lydfil suiten kjenner igjen. Den kan spille WAV, AIFF, MP3, AAC/M4A, FLAC og OGG.")))?;

    let mut tittel = None;
    let mut artist = None;
    // WAV-tagger slutter ofte med nulltegn; vask bort det og tomme felt.
    let vask = |v: &symphonia::core::meta::Value| {
        let t = v.to_string().trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string();
        (!t.is_empty()).then_some(t)
    };
    let mut les_tagger = |tags: &[symphonia::core::meta::Tag]| {
        for t in tags {
            match t.std_key {
                Some(StandardTagKey::TrackTitle) if tittel.is_none() => tittel = vask(&t.value),
                Some(StandardTagKey::Artist) if artist.is_none() => artist = vask(&t.value),
                _ => {}
            }
        }
    };
    if let Some(m) = probed.metadata.get() {
        if let Some(r) = m.current() {
            les_tagger(r.tags());
        }
    }
    let mut format = probed.format;
    if let Some(r) = format.metadata().current() {
        les_tagger(r.tags());
    }

    let spor = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| Lydfeil::Fil(format!("«{vis}» har ikke noe lydspor.")))?;
    let spor_id = spor.id;
    let sr = spor.codec_params.sample_rate.ok_or_else(|| Lydfeil::Fil(format!("«{vis}» mangler samplerate.")))?;
    let codec = symphonia::default::get_codecs()
        .get_codec(spor.codec_params.codec)
        .map(|c| c.short_name.to_uppercase())
        .unwrap_or_else(|| "Ukjent".into());
    let codec = match codec.as_str() {
        c if c.starts_with("PCM") => sti.extension().and_then(|e| e.to_str()).unwrap_or("PCM").to_uppercase(),
        _ => codec,
    };
    let mut dekoder = symphonia::default::get_codecs()
        .make(&spor.codec_params, &DecoderOptions::default())
        .map_err(|e| Lydfeil::Fil(format!("Kan ikke dekode «{vis}»: {e}")))?;

    let mut v = Vec::new();
    let mut h = Vec::new();
    let mut kanaler = 0usize;
    let mut buf: Option<SampleBuffer<f32>> = None;
    loop {
        let pakke = match format.next_packet() {
            Ok(p) => p,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(Error::ResetRequired) => break,
            Err(e) => return Err(Lydfeil::Fil(format!("Feil midt i «{vis}»: {e}"))),
        };
        if pakke.track_id() != spor_id {
            continue;
        }
        match dekoder.decode(&pakke) {
            Ok(lyd) => {
                let spec = *lyd.spec();
                kanaler = spec.channels.count();
                let b = buf.get_or_insert_with(|| SampleBuffer::new(lyd.capacity() as u64, spec));
                if b.capacity() < lyd.capacity() * kanaler {
                    *b = SampleBuffer::new(lyd.capacity() as u64, spec);
                }
                b.copy_interleaved_ref(lyd);
                for ramme in b.samples().chunks(kanaler.max(1)) {
                    v.push(ramme[0]);
                    h.push(if kanaler > 1 { ramme[1] } else { ramme[0] });
                }
            }
            // En skadet pakke hoppes over, som i alle profesjonelle spillere.
            Err(Error::DecodeError(_)) => continue,
            Err(e) => return Err(Lydfeil::Fil(format!("Kan ikke dekode «{vis}»: {e}"))),
        }
    }
    if v.is_empty() {
        return Err(Lydfeil::Fil(format!("«{vis}» inneholder ingen lyd.")));
    }
    Ok((Blokk { v, h }, sr, tittel, artist, codec, kanaler))
}

/// Går gjennom blokkene i en AIFF-fil.
fn aiff_blokker(sti: &Path) -> Option<Vec<([u8; 4], Vec<u8>)>> {
    let ext = sti.extension()?.to_str()?.to_lowercase();
    if ext != "aif" && ext != "aiff" && ext != "aifc" {
        return None;
    }
    let data = std::fs::read(sti).ok()?;
    if data.len() < 12 || &data[0..4] != b"FORM" {
        return None;
    }
    let mut ut = Vec::new();
    let mut pos = 12;
    while pos + 8 <= data.len() {
        let id: [u8; 4] = data[pos..pos + 4].try_into().ok()?;
        let len = u32::from_be_bytes(data[pos + 4..pos + 8].try_into().ok()?) as usize;
        let slutt = (pos + 8 + len).min(data.len());
        // Lyddataene trengs ikke her; bare de små blokkene.
        let innhold = if &id == b"SSND" { Vec::new() } else { data[pos + 8..slutt].to_vec() };
        ut.push((id, innhold));
        pos += 8 + len + (len & 1);
    }
    Some(ut)
}

/// Antall lydrammer ifølge AIFF-filens COMM-blokk.
fn aiff_rammer(sti: &Path) -> Option<usize> {
    let (_, comm) = aiff_blokker(sti)?.into_iter().find(|(id, _)| id == b"COMM")?;
    Some(u32::from_be_bytes(comm.get(2..6)?.try_into().ok()?) as usize)
}

/// Leser tittel og artist fra ID3-blokken i en AIFF-fil.
fn aiff_id3(sti: &Path) -> Option<(Option<String>, Option<String>)> {
    let (_, id3) = aiff_blokker(sti)?.into_iter().find(|(id, _)| id == b"ID3 " || id == b"id3 ")?;
    Some(les_id3v2(&id3))
}

/// Minimal ID3v2.3/2.4-leser: henter TIT2 (tittel) og TPE1 (artist).
fn les_id3v2(d: &[u8]) -> (Option<String>, Option<String>) {
    let mut tittel = None;
    let mut artist = None;
    if d.len() < 10 || &d[0..3] != b"ID3" {
        return (None, None);
    }
    let versjon = d[3];
    let synksafe = |b: &[u8]| ((b[0] as usize) << 21) | ((b[1] as usize) << 14) | ((b[2] as usize) << 7) | b[3] as usize;
    let slutt = (10 + synksafe(&d[6..10])).min(d.len());
    let mut p = 10;
    while p + 10 <= slutt {
        let id = &d[p..p + 4];
        if id[0] == 0 {
            break;
        }
        let len = if versjon >= 4 { synksafe(&d[p + 4..p + 8]) } else { u32::from_be_bytes([d[p + 4], d[p + 5], d[p + 6], d[p + 7]]) as usize };
        let Some(ramme) = d.get(p + 10..p + 10 + len) else { break };
        if (id == b"TIT2" || id == b"TPE1") && !ramme.is_empty() {
            let tekst = match ramme[0] {
                0 => ramme[1..].iter().map(|&b| b as char).collect::<String>(),
                1 | 2 => {
                    let mut b = &ramme[1..];
                    let mut le = ramme[0] == 1;
                    if b.len() >= 2 && (b[0], b[1]) == (0xFF, 0xFE) {
                        le = true;
                        b = &b[2..];
                    } else if b.len() >= 2 && (b[0], b[1]) == (0xFE, 0xFF) {
                        le = false;
                        b = &b[2..];
                    }
                    let enheter: Vec<u16> = b.chunks_exact(2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }).collect();
                    String::from_utf16_lossy(&enheter)
                }
                _ => String::from_utf8_lossy(&ramme[1..]).into_owned(),
            };
            let tekst = tekst.trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string();
            if !tekst.is_empty() {
                if id == b"TIT2" {
                    tittel = Some(tekst);
                } else {
                    artist = Some(tekst);
                }
            }
        }
        p += 10 + len;
    }
    (tittel, artist)
}

/// Bølgeform til visning: toppnivå per 10 ms.
pub fn bolgeform(b: &Blokk) -> Vec<u8> {
    let bit = SAMPLERATE as usize / BOLGE_PER_SEK;
    (0..b.lengde().div_ceil(bit))
        .map(|i| {
            let a = i * bit;
            let e = (a + bit).min(b.lengde());
            let t = b.v[a..e].iter().chain(&b.h[a..e]).fold(0.0f32, |m, x| m.max(x.abs()));
            (t.min(1.0).sqrt() * 255.0).round() as u8
        })
        .collect()
}

/// Finner start, slutt og miks-ut automatisk.
pub fn autocue(b: &Blokk) -> Autocue {
    let sr = SAMPLERATE as f32;
    let terskel = kjerne::db_til_lin(-45.0);
    let over = |i: usize| b.v[i].abs() > terskel || b.h[i].abs() > terskel;
    let n = b.lengde();
    let start = (0..n).find(|&i| over(i)).unwrap_or(0);
    let slutt = (0..n).rev().find(|&i| over(i)).map(|i| i + 1).unwrap_or(n);
    // Miks ut: RMS i 100 ms-vinduer; referansen er medianen av låta.
    let vindu = SAMPLERATE as usize / 10;
    let rms: Vec<f32> = (start / vindu..slutt / vindu)
        .map(|w| {
            let a = w * vindu;
            let s: f32 = (a..(a + vindu).min(n)).map(|i| b.v[i] * b.v[i] + b.h[i] * b.h[i]).sum();
            kjerne::lin_til_db((s / (2 * vindu) as f32).sqrt())
        })
        .collect();
    let miks_ut = if rms.len() > 50 {
        let mut sortert: Vec<f32> = rms.iter().copied().filter(|x| x.is_finite()).collect();
        sortert.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = sortert.get(sortert.len() / 2).copied().unwrap_or(-20.0);
        let grense = median - 12.0;
        // Siste vindu som fortsatt er «i låta»; se bare på de siste 40 sekundene.
        let fra = rms.len().saturating_sub(400);
        match (fra..rms.len()).rev().find(|&w| rms[w] > grense) {
            Some(w) => ((start / vindu + w + 1) * vindu).min(slutt),
            None => slutt,
        }
    } else {
        slutt
    };
    Autocue { start: start as f32 / sr, slutt: slutt as f32 / sr, miks_ut: miks_ut as f32 / sr }
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

    /// Testfilene: 0,5 s stillhet, 1,5 s 1 kHz-tone (−33,1 dBFS), 0,5 s stillhet, 44,1 kHz,
    /// med tittel «Testlåt» og artist «Klubblisten».
    #[test]
    fn leser_alle_formater_med_tagger_og_autocue() {
        let mappe = Path::new(env!("CARGO_MANIFEST_DIR")).join("testfiler");
        for (ext, kodek) in [("wav", "WAV"), ("aiff", "AIFF"), ("flac", "FLAC"), ("mp3", "MP3"), ("m4a", "AAC"), ("ogg", "VORBIS")] {
            let f = last_med_info(&mappe.join(format!("test.{ext}"))).unwrap_or_else(|e| panic!("{ext}: {e}"));
            let i = &f.info;
            assert!(i.format.starts_with(kodek), "{ext}: format {}", i.format);
            assert!(i.format.contains("44,1 kHz"), "{ext}: {}", i.format);
            assert!((i.sekunder - 2.5).abs() < 0.08, "{ext}: lengde {}", i.sekunder);
            assert_eq!(i.tittel.as_deref(), Some("Testlåt"), "{ext}: tittel");
            assert_eq!(i.artist.as_deref(), Some("Klubblisten"), "{ext}: artist");
            // Autocue finner tonen (kodeker med forsinkelse får litt slingringsmonn).
            assert!((i.autocue.start - 0.5).abs() < 0.06, "{ext}: start {}", i.autocue.start);
            assert!((i.autocue.slutt - 2.0).abs() < 0.06, "{ext}: slutt {}", i.autocue.slutt);
            let midt = &f.lyd.v[48_000..72_000];
            assert!((maaling::topp_dbfs(midt) - -33.1).abs() < 0.5, "{ext}: nivå {}", maaling::topp_dbfs(midt));
            assert_eq!(i.bolge.len(), (i.sekunder * 100.0).ceil() as usize);
            assert!(i.lufs.is_some());
        }
    }

    #[test]
    fn miks_ut_finner_der_laata_faller_av() {
        // 30 s «låt» på −14 dBFS, så 5 s fade-hale på −40 dBFS.
        let mut lyd = kjerne::testlyd::rosa_stoy(-14.0, 30.0);
        let hale = kjerne::testlyd::rosa_stoy(-40.0, 5.0);
        lyd.v.extend_from_slice(&hale.v);
        lyd.h.extend_from_slice(&hale.h);
        let a = autocue(&lyd);
        assert!((a.miks_ut - 30.0).abs() < 0.2, "miks ut {}", a.miks_ut);
        assert!(a.slutt > 34.9);
    }

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
