//! Linjesjekk: kjøres hver gang en preset åpnes.
//!
//! Testlyden (1 kHz på −18 dBFS) sendes gjennom en skjult kopi av hele kjeden,
//! så ingen hører den og en pågående sending ikke påvirkes. Hver modul måles
//! inn og ut. Nøytrale moduler må bestå nulltest; aktive moduler sammenlignes
//! med lagret fasit. Til slutt sjekkes hver utgang etter master.

use crate::kontroll::Verdi;
use crate::lyd::{Blokk, REFERANSENIVAA_DBFS, SAMPLERATE};
use crate::maaling;
use crate::modul::Modul;
use crate::motor::Motor;
use crate::testlyd;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

/// Toleranse mot fasit, i dB.
pub const FASIT_TOLERANSE_DB: f32 = 0.5;
/// Største trim som rettes automatisk; større avvik meldes som merknad.
pub const AUTO_TRIM_MAKS_DB: f32 = 6.0;
/// Krav til lytteveien: forsinkelse over dette regnes som feil.
pub const LYTTEVEI_MAKS_MS: f32 = 0.0;
/// Største true peak før koding.
pub const MAKS_TRUE_PEAK_DBTP: f32 = -1.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Merknad,
    Feil,
}

#[derive(Clone, Debug, Serialize)]
pub struct Rad {
    pub gruppe: String,
    pub navn: String,
    pub adresse: String,
    pub inn_dbfs: Option<f32>,
    pub ut_dbfs: Option<f32>,
    pub endring_db: Option<f32>,
    pub forsinkelse_ms: Option<f32>,
    pub merknad: String,
    pub status: Status,
}

/// En nivåretting linjesjekken har gjort automatisk (bare rene trim).
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Retting {
    pub adresse: String,
    pub verdi_db: f32,
}

#[derive(Clone, Debug, Serialize)]
pub struct Rapport {
    pub rader: Vec<Rad>,
    pub rettinger: Vec<Retting>,
    pub merknader: usize,
    pub feil: usize,
}

impl Rapport {
    pub fn bestatt(&self) -> bool {
        self.feil == 0
    }

    pub fn oppsummering(&self) -> String {
        match (self.feil, self.merknader) {
            (0, 0) => "Linjesjekk bestått".into(),
            (0, m) => format!("Linjesjekk bestått · {m} merknad{}", if m == 1 { "" } else { "er" }),
            (f, _) => format!("Linjesjekk ikke bestått · {f} feil"),
        }
    }
}

fn ms(samples: usize) -> f32 {
    samples as f32 * 1000.0 / SAMPLERATE as f32
}

fn referansetone() -> Blokk {
    testlyd::tone(1000.0, REFERANSENIVAA_DBFS, 0.5)
}

/// Måler én modul isolert: nivåendring (med testtone) og forsinkelse (med puls).
fn maal_modul(modul: &dyn Modul) -> (f32, f32, usize, maaling::Nulltest) {
    let tone = referansetone();
    let mut m = modul.klon();
    m.nullstill();
    let ut = kjor_modul(m.as_mut(), &tone);
    // Mål på siste del, etter at eventuelle overganger har satt seg.
    let fra = tone.lengde() / 2;
    let inn_db = maaling::topp_dbfs(&tone.v[fra..]);
    let ut_db = maaling::topp_dbfs(&ut.v[fra..]).max(maaling::topp_dbfs(&ut.h[fra..]));

    let puls = testlyd::puls(64, 24_000, -6.0);
    let mut m2 = modul.klon();
    m2.nullstill();
    let ut_puls = kjor_modul(m2.as_mut(), &puls);
    let forsinkelse = maaling::forsinkelse_og_polaritet(&puls.v, &ut_puls.v, 20_000).map(|(l, _)| l).unwrap_or(modul.forsinkelse());

    let mut m3 = modul.klon();
    m3.nullstill();
    let stoy = testlyd::rosa_stoy(-20.0, 0.25);
    let ut_stoy = kjor_modul(m3.as_mut(), &stoy);
    let null = maaling::nulltest(&stoy, &ut_stoy, forsinkelse);
    (inn_db, ut_db, forsinkelse, null)
}

fn kjor_modul(m: &mut dyn Modul, inn: &Blokk) -> Blokk {
    let mut ut = Blokk::stille(inn.lengde());
    let mut start = 0;
    while start < inn.lengde() {
        let len = crate::lyd::BLOKKSTORRELSE.min(inn.lengde() - start);
        let del = inn.utsnitt(start, len);
        let mut del_ut = Blokk::stille(len);
        m.prosesser(&del, &mut del_ut);
        ut.v[start..start + len].copy_from_slice(&del_ut.v);
        ut.h[start..start + len].copy_from_slice(&del_ut.h);
        start += len;
    }
    ut
}

fn vurder_modul(gruppe: &str, navn: String, adresse: String, modul: &dyn Modul, fasit: &BTreeMap<String, f32>) -> Rad {
    let (inn, ut, forsinkelse, null) = maal_modul(modul);
    let endring = ut - inn;
    let (status, merknad) = if modul.er_noytral() {
        if null.bestatt() {
            (Status::Ok, if forsinkelse > 0 { format!("Nulltest bestått. Forsinkelse {:.1} ms kompensert", ms(forsinkelse)) } else { "Nulltest bestått".into() })
        } else {
            (Status::Feil, format!("Nulltest feilet: modulen endrer lyden selv om den står nøytralt (rest {:.1} dBFS)", null.rest_dbfs))
        }
    } else {
        match fasit.get(&adresse) {
            Some(forventet) if (endring - forventet).abs() <= FASIT_TOLERANSE_DB => (Status::Ok, format!("Matcher fasit ({forventet:+.1} dB)")),
            Some(forventet) => (Status::Feil, format!("Avviker fra fasit: forventet {forventet:+.1} dB, målt {endring:+.1} dB")),
            None => (Status::Merknad, "Ingen fasit lagret ennå. Lytt, og lagre som fasit hvis det er riktig".into()),
        }
    };
    Rad {
        gruppe: gruppe.into(),
        navn,
        adresse,
        inn_dbfs: Some(inn),
        ut_dbfs: Some(ut),
        endring_db: Some(endring),
        forsinkelse_ms: Some(ms(forsinkelse)),
        merknad,
        status,
    }
}

/// Er dette en kalibreringstone (sinus)? Topp minus RMS er da ca. 3 dB.
fn er_tone(b: &Blokk) -> bool {
    let crest = maaling::topp_dbfs(&b.v) - maaling::rms_dbfs(&b.v);
    (2.6..=3.4).contains(&crest)
}

/// Kjører linjesjekken. `innganger` er det som faktisk kommer inn på hver kanal
/// akkurat nå (et kort utsnitt); kanaler uten utsnitt sjekkes ikke for signal.
pub fn kjor(motor: &Motor, fasit: &BTreeMap<String, f32>, innganger: &HashMap<String, Blokk>) -> Rapport {
    let mut rader = Vec::new();
    let mut rettinger = Vec::new();

    // 1. Innganger: ekte kilder sjekkes for signal og nivå.
    for k in motor.kanaler() {
        let id = &k.oppsett.id;
        let adresse = format!("kanal/{id}/inngang");
        let Some(inn) = innganger.get(id) else { continue };
        let topp = maaling::topp_dbfs(&inn.v).max(maaling::topp_dbfs(&inn.h));
        let rms = maaling::rms_dbfs(&inn.v).max(maaling::rms_dbfs(&inn.h));
        let klipp = maaling::klipp(&inn.v) + maaling::klipp(&inn.h);
        let mut rad = Rad {
            gruppe: "Innganger".into(),
            navn: k.oppsett.navn.clone(),
            adresse,
            inn_dbfs: Some(topp),
            ut_dbfs: None,
            endring_db: None,
            forsinkelse_ms: None,
            merknad: String::new(),
            status: Status::Ok,
        };
        if rms < -60.0 {
            rad.status = Status::Merknad;
            rad.inn_dbfs = None;
            rad.merknad = "Ingen signal. Er kilden slått på?".into();
        } else if klipp > 0 {
            rad.status = Status::Feil;
            rad.merknad = "Inngangen klipper. Skru ned gain på mikseren".into();
        } else if er_tone(inn) {
            let trim = REFERANSENIVAA_DBFS - topp;
            if trim.abs() < 0.05 {
                rad.merknad = "Kalibreringstone på riktig nivå".into();
                rad.ut_dbfs = Some(topp);
            } else if trim.abs() <= AUTO_TRIM_MAKS_DB {
                let ny = k.trim_db() + trim;
                rettinger.push(Retting { adresse: format!("kanal/{id}/trim"), verdi_db: ny });
                rad.endring_db = Some(trim);
                rad.ut_dbfs = Some(REFERANSENIVAA_DBFS);
                rad.merknad = format!("Trim justert automatisk {trim:+.1} dB");
            } else {
                rad.status = Status::Merknad;
                rad.merknad = format!("Nivået er {:.1} dB fra referansen. Juster gain på mikseren", -trim);
            }
        } else {
            rad.merknad = format!("Signal OK (RMS {rms:.1} dBFS)");
        }
        rader.push(rad);
    }

    // 2. Gjennom kjeden: hver modul i hver kanal, isolert.
    for k in motor.kanaler() {
        let id = &k.oppsett.id;
        if k.trim_db() != 0.0 {
            rader.push(Rad {
                gruppe: "Gjennom kjeden".into(),
                navn: format!("{} · trim", k.oppsett.navn),
                adresse: format!("kanal/{id}/trim"),
                inn_dbfs: Some(REFERANSENIVAA_DBFS),
                ut_dbfs: Some(REFERANSENIVAA_DBFS + k.trim_db()),
                endring_db: Some(k.trim_db()),
                forsinkelse_ms: Some(0.0),
                merknad: "Bevisst nivåjustering".into(),
                status: Status::Ok,
            });
        }
        for (i, m) in k.kjede().iter().enumerate() {
            rader.push(vurder_modul(
                "Gjennom kjeden",
                format!("{} · {}", k.oppsett.navn, m.typenavn()),
                format!("kanal/{id}/modul/{i}"),
                m.as_ref(),
                fasit,
            ));
        }
    }
    for (i, m) in motor.master_kjede().iter().enumerate() {
        rader.push(vurder_modul("Gjennom kjeden", format!("Master · {}", m.typenavn()), format!("master/modul/{i}"), m.as_ref(), fasit));
    }

    // 3. Etter master: skjult kopi med faderne i nøytral stilling.
    rader.extend(sjekk_utganger(motor, fasit));

    let merknader = rader.iter().filter(|r| r.status == Status::Merknad).count();
    let feil = rader.iter().filter(|r| r.status == Status::Feil).count();
    Rapport { rader, rettinger, merknader, feil }
}

fn sjekk_utganger(motor: &Motor, fasit: &BTreeMap<String, f32>) -> Vec<Rad> {
    let mut rader = Vec::new();
    let mut skygge = motor.skyggekopi();
    for k in motor.kanaler() {
        let id = &k.oppsett.id;
        for (kontroll, verdi) in [("fader", Verdi::Tall(-200.0)), ("mute", Verdi::AvPaa(false)), ("solo", Verdi::AvPaa(false)), ("pan", Verdi::Tall(0.0))] {
            let _ = skygge.sett(&format!("kanal/{id}/{kontroll}"), verdi);
        }
    }
    let _ = skygge.sett("master/fader", Verdi::Tall(0.0));
    skygge.nullstill();

    // Referansekanal: første kanal som går til Master (ikke privat).
    let Some(referanse) = motor.kanaler().iter().find(|k| k.oppsett.til_master && !k.oppsett.privat) else {
        return rader;
    };
    let rid = referanse.oppsett.id.clone();
    let _ = skygge.sett(&format!("kanal/{rid}/fader"), Verdi::Tall(0.0));
    let _ = skygge.sett(&format!("kanal/{rid}/trim"), Verdi::Tall(0.0));
    skygge.nullstill();

    // Forventet endring gjennom kanalens og masterens kjede.
    let mut forventet = 0.0f32;
    for (i, m) in referanse.kjede().iter().enumerate() {
        if !m.er_noytral() {
            forventet += fasit.get(&format!("kanal/{rid}/modul/{i}")).copied().unwrap_or(0.0);
        }
    }
    for (i, m) in motor.master_kjede().iter().enumerate() {
        if !m.er_noytral() {
            forventet += fasit.get(&format!("master/modul/{i}")).copied().unwrap_or(0.0);
        }
    }

    let tone = testlyd::tone(1000.0, REFERANSENIVAA_DBFS, 3.0);
    let inn: HashMap<String, Blokk> = [(rid.clone(), tone.clone())].into_iter().collect();
    let ut = skygge.prosesser(&inn);
    let fra = SAMPLERATE as usize; // etter at alt har satt seg
    let program = ut.program.utsnitt(fra, ut.program.lengde() - fra);
    let prog_topp = maaling::topp_dbfs(&program.v).max(maaling::topp_dbfs(&program.h));
    let prog_tp = maaling::true_peak_dbfs(&program.v[..4800]).max(maaling::true_peak_dbfs(&program.h[..4800]));
    let prog_lufs = maaling::lufs_integrert(&program);
    let avvik = prog_topp - (REFERANSENIVAA_DBFS + forventet);
    let (status, merknad) = if prog_tp > MAKS_TRUE_PEAK_DBTP {
        (Status::Feil, format!("True peak {prog_tp:.1} dBTP er over {MAKS_TRUE_PEAK_DBTP:.0} dBTP før koding"))
    } else if avvik.abs() > FASIT_TOLERANSE_DB {
        (Status::Feil, format!("Nivået avviker {avvik:+.1} dB fra forventet"))
    } else {
        (Status::Ok, format!("Testtone {prog_lufs:.1} LUFS, true peak {prog_tp:.1} dBTP"))
    };
    rader.push(Rad {
        gruppe: "Etter master".into(),
        navn: "Program (Icecast, vMix, opptak)".into(),
        adresse: "utgang/program".into(),
        inn_dbfs: Some(REFERANSENIVAA_DBFS),
        ut_dbfs: Some(prog_topp),
        endring_db: Some(prog_topp - REFERANSENIVAA_DBFS),
        forsinkelse_ms: Some(ms(motor.sendevei_forsinkelse())),
        merknad,
        status,
    });

    // DJ-ren: skal være bit-identisk med inngangen.
    if let Some(dj) = motor.kanaler().iter().find(|k| k.oppsett.til_dj_ren) {
        let mut s2 = motor.skyggekopi();
        let stoy = testlyd::rosa_stoy(-20.0, 0.5);
        let inn: HashMap<String, Blokk> = [(dj.oppsett.id.clone(), stoy.clone())].into_iter().collect();
        let ut = s2.prosesser(&inn);
        let null = maaling::nulltest(&stoy, &ut.dj_ren, 0);
        rader.push(Rad {
            gruppe: "Etter master".into(),
            navn: "Opptak · DJ-ren".into(),
            adresse: "utgang/dj_ren".into(),
            inn_dbfs: Some(maaling::topp_dbfs(&stoy.v)),
            ut_dbfs: Some(maaling::topp_dbfs(&ut.dj_ren.v)),
            endring_db: Some(0.0),
            forsinkelse_ms: Some(0.0),
            merknad: if null.bestatt() { "Urørt, nulltest bestått".into() } else { format!("DJ-ren er endret (rest {:.1} dBFS)", null.rest_dbfs) },
            status: if null.bestatt() { Status::Ok } else { Status::Feil },
        });
    }

    // Lytteveien: forsinkelse må være null.
    let mut s3 = motor.skyggekopi();
    for k in motor.kanaler() {
        let _ = s3.sett(&format!("kanal/{}/fader", k.oppsett.id), Verdi::Tall(if k.oppsett.id == rid { 0.0 } else { -200.0 }));
        let _ = s3.sett(&format!("kanal/{}/mute", k.oppsett.id), Verdi::AvPaa(false));
        let _ = s3.sett(&format!("kanal/{}/solo", k.oppsett.id), Verdi::AvPaa(false));
    }
    s3.nullstill();
    let puls = testlyd::puls(64, 24_000, -6.0);
    let inn: HashMap<String, Blokk> = [(rid.clone(), puls.clone())].into_iter().collect();
    let ut = s3.prosesser(&inn);
    let lytt = maaling::forsinkelse_og_polaritet(&puls.v, &ut.hodetelefoner.v, 20_000);
    let (status, merknad, f_ms) = match lytt {
        Some((lag, true)) if ms(lag) <= LYTTEVEI_MAKS_MS => (Status::Ok, "Ingen forsinkelse i lytteveien. Mikrofon i øret via direktelytt".to_string(), ms(lag)),
        Some((lag, true)) => (Status::Feil, format!("Lytteveien er forsinket {:.1} ms", ms(lag)), ms(lag)),
        Some((lag, false)) => (Status::Feil, "Polariteten er snudd i lytteveien".to_string(), ms(lag)),
        None => (Status::Merknad, "Ingen lyd i hodetelefonene fra referansekanalen. Er den rutet til Studio?".to_string(), 0.0),
    };
    rader.push(Rad {
        gruppe: "Etter master".into(),
        navn: "Hodetelefoner · lyttevei".into(),
        adresse: "utgang/hodetelefoner".into(),
        inn_dbfs: None,
        ut_dbfs: None,
        endring_db: None,
        forsinkelse_ms: Some(f_ms),
        merknad,
        status,
    });

    rader
}

/// Lager fasit for aktive moduler: måler hvor mye hver av dem endrer nivået nå.
/// Brukes når man har lyttet og bekreftet at lyden er riktig («Lagre som fasit»).
pub fn lag_fasit(motor: &Motor) -> BTreeMap<String, f32> {
    let mut fasit = BTreeMap::new();
    for k in motor.kanaler() {
        for (i, m) in k.kjede().iter().enumerate() {
            if !m.er_noytral() {
                let (inn, ut, _, _) = maal_modul(m.as_ref());
                fasit.insert(format!("kanal/{}/modul/{i}", k.oppsett.id), ut - inn);
            }
        }
    }
    for (i, m) in motor.master_kjede().iter().enumerate() {
        if !m.er_noytral() {
            let (inn, ut, _, _) = maal_modul(m.as_ref());
            fasit.insert(format!("master/modul/{i}"), ut - inn);
        }
    }
    fasit
}

#[cfg(test)]
mod tester {
    use super::*;
    use crate::moduler::{Forsinkelse, Gain};
    use crate::motor::{KanalOppsett, Kanaltype};

    fn motor() -> Motor {
        let mut m = Motor::ny();
        let mut v10 = KanalOppsett::ny("v10", "DJM-V10", Kanaltype::LinjeStereo);
        v10.til_dj_ren = true;
        m.legg_til_kanal(v10).unwrap();
        m.legg_til_kanal(KanalOppsett::ny("mik1", "Mikrofon 1", Kanaltype::Mikrofon)).unwrap();
        m.legg_til_modul("v10", Box::new(Gain::ny(0.0))).unwrap();
        m
    }

    #[test]
    fn noytral_kjede_bestaar() {
        let m = motor();
        let r = kjor(&m, &BTreeMap::new(), &HashMap::new());
        assert!(r.bestatt(), "{:#?}", r.rader);
        assert_eq!(r.merknader, 0);
    }

    #[test]
    fn aktiv_modul_uten_fasit_gir_merknad_og_med_fasit_ok() {
        let mut m = motor();
        m.legg_til_master_modul(Box::new(Gain::ny(4.0)));
        let r = kjor(&m, &BTreeMap::new(), &HashMap::new());
        assert!(r.rader.iter().any(|r| r.adresse == "master/modul/0" && r.status == Status::Merknad));
        let fasit = lag_fasit(&m);
        assert!((fasit["master/modul/0"] - 4.0).abs() < 0.05);
        let r = kjor(&m, &fasit, &HashMap::new());
        assert!(r.bestatt() && r.merknader == 0, "{:#?}", r.rader);
    }

    #[test]
    fn endret_modul_avviker_fra_fasit() {
        let mut m = motor();
        m.legg_til_master_modul(Box::new(Gain::ny(4.0)));
        let fasit = lag_fasit(&m);
        m.sett("master/modul/0/gain", Verdi::Tall(7.0)).unwrap();
        let r = kjor(&m, &fasit, &HashMap::new());
        assert!(!r.bestatt());
    }

    #[test]
    fn forsinkelse_i_kjeden_kompenseres_og_lytteveien_er_ren() {
        let mut m = motor();
        m.legg_til_master_modul(Box::new(Forsinkelse::ny(864))); // 18 ms
        let r = kjor(&m, &BTreeMap::new(), &HashMap::new());
        assert!(r.bestatt(), "{:#?}", r.rader);
        let lytt = r.rader.iter().find(|r| r.adresse == "utgang/hodetelefoner").unwrap();
        assert_eq!(lytt.forsinkelse_ms, Some(0.0));
    }

    #[test]
    fn inngang_med_tone_rettes_automatisk_og_stille_mikrofon_gir_merknad() {
        let m = motor();
        let inn: HashMap<String, Blokk> =
            [("v10".to_string(), testlyd::tone(1000.0, -18.2, 0.5)), ("mik1".to_string(), testlyd::stillhet(0.5))].into_iter().collect();
        let r = kjor(&m, &BTreeMap::new(), &inn);
        assert_eq!(r.rettinger.len(), 1);
        assert!((r.rettinger[0].verdi_db - 0.2).abs() < 0.02);
        assert!(r.rader.iter().any(|r| r.navn == "Mikrofon 1" && r.status == Status::Merknad));
    }
}
