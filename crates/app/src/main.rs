//! Klubblisten Studio Suite – app-vinduet.
//!
//! Rust-delen eier lydmotoren; vinduet (ui/index.html) styrer den via kommandoer.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use kjerne::kontroll::Verdi;
use kjerne::motor::{KanalOppsett, Kanaltype, Motor};
use lydkort::{Buss, Inngangsvalg, Kommando, Lydoppsett, Lydstrom, Lytt, Opptak, Styring};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Mutex;
use tauri::State;

/// Lydstrømmen kan ikke flyttes mellom tråder på Mac, så den bor i sin egen
/// tråd. Appen holder bare styringen og en «stopp»-kanal.
struct Kjorende {
    styring: Styring,
    stopp: mpsc::Sender<()>,
    kanaler: Vec<KanalInfo>,
    oppsett: LydValg,
}

#[derive(Default)]
struct AppTilstand {
    lyd: Mutex<Option<Kjorende>>,
    opptak: Mutex<Vec<Opptak>>,
}

#[derive(Clone, Serialize)]
struct KanalInfo {
    id: String,
    navn: String,
    hvem: String,
    #[serde(rename = "type")]
    kanaltype: String,
    privat: bool,
    til_master: bool,
    til_dj_ren: bool,
    til_studio: bool,
    fader: f32,
    spiller: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LydValg {
    utgang: Option<String>,
    inngang: Option<String>,
    /// Hva som går ut på utgangslydkortet: "hodetelefoner", "hoyttalere" eller "program".
    lytt: Option<String>,
}

fn feil<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[tauri::command]
fn enheter() -> Result<Vec<lydkort::Enhet>, String> {
    lydkort::liste().map_err(feil)
}

fn bygg_motor(med_inngang: bool) -> (Motor, Vec<KanalInfo>, Vec<Inngangsvalg>) {
    let mut m = Motor::ny();
    let mut info = Vec::new();
    let mut innganger = Vec::new();
    let mut legg_til = |m: &mut Motor, o: KanalOppsett, hvem: &str, fader: f32, spiller: bool| {
        m.legg_til_kanal(o.clone()).expect("kanal");
        m.sett(&format!("kanal/{}/fader", o.id), Verdi::Tall(fader)).expect("fader");
        info.push(KanalInfo {
            id: o.id.clone(),
            navn: o.navn.clone(),
            hvem: hvem.into(),
            kanaltype: match o.kanaltype {
                Kanaltype::Mikrofon => "MIC",
                Kanaltype::LinjeStereo => "LINJE",
                Kanaltype::Spiller => "SPILLER",
                Kanaltype::AppLyd => "APP",
                Kanaltype::Retur => "RETUR",
            }
            .into(),
            privat: o.privat,
            til_master: o.til_master && !o.privat,
            til_dj_ren: o.til_dj_ren,
            til_studio: o.til_studio,
            fader,
            spiller,
        });
    };
    if med_inngang {
        legg_til(&mut m, KanalOppsett::ny("mik1", "Mikrofon 1", Kanaltype::Mikrofon), "Inngang 1", -200.0, false);
        legg_til(&mut m, KanalOppsett::ny("inn2", "Inngang 2", Kanaltype::Mikrofon), "Inngang 2", -200.0, false);
        innganger.push(Inngangsvalg { kanal_id: "mik1".into(), forste: 1, stereo: false });
        innganger.push(Inngangsvalg { kanal_id: "inn2".into(), forste: 2, stereo: false });
    }
    legg_til(&mut m, KanalOppsett::ny("spillerA", "Spiller A", Kanaltype::Spiller), "Avvikling", 0.0, true);
    legg_til(&mut m, KanalOppsett::ny("jingler", "Jingler", Kanaltype::Spiller), "Lydknapper", 0.0, true);
    (m, info, innganger)
}

#[tauri::command]
fn start_lyd(valg: LydValg, tilstand: State<AppTilstand>) -> Result<Vec<KanalInfo>, String> {
    stopp_lyd(tilstand.clone())?;
    let med_inngang = valg.inngang.as_deref().is_some_and(|s| !s.is_empty());
    let (motor, kanaler, innganger) = bygg_motor(med_inngang);
    let oppsett = Lydoppsett {
        utgang: valg.utgang.clone().filter(|s| !s.is_empty()),
        inngang: if med_inngang { valg.inngang.clone() } else { None },
        innganger,
        lytt: match valg.lytt.as_deref() {
            Some("program") => Lytt::Program,
            Some("hoyttalere") => Lytt::Hoyttalere,
            _ => Lytt::Hodetelefoner,
        },
        buffer: 128,
    };
    let (svar_tx, svar_rx) = mpsc::channel();
    let (stopp_tx, stopp_rx) = mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("lydkort".into())
        .spawn(move || match Lydstrom::start(motor, oppsett) {
            Ok(s) => {
                let _ = svar_tx.send(Ok(s.styring()));
                let _ = stopp_rx.recv(); // lever til appen sier stopp (eller lukkes)
                drop(s);
            }
            Err(e) => {
                let _ = svar_tx.send(Err(e.to_string()));
            }
        })
        .map_err(feil)?;
    let styring = svar_rx.recv().map_err(feil)??;
    *tilstand.lyd.lock().unwrap() = Some(Kjorende { styring, stopp: stopp_tx, kanaler: kanaler.clone(), oppsett: valg });
    Ok(kanaler)
}

#[tauri::command]
fn stopp_lyd(tilstand: State<AppTilstand>) -> Result<(), String> {
    // Låserekkefølge overalt: først `lyd`, så `opptak`.
    let mut lyd = tilstand.lyd.lock().unwrap();
    let mut opptak = tilstand.opptak.lock().unwrap();
    if let Some(k) = lyd.take() {
        for o in opptak.drain(..) {
            let _ = k.styring.stopp_opptak(o);
        }
        let _ = k.stopp.send(());
    }
    Ok(())
}

fn med_styring<T>(tilstand: &State<AppTilstand>, f: impl FnOnce(&Styring) -> Result<T, String>) -> Result<T, String> {
    let lyd = tilstand.lyd.lock().unwrap();
    match lyd.as_ref() {
        Some(k) => f(&k.styring),
        None => Err("Lyden er ikke startet. Velg lydkort og trykk Start.".into()),
    }
}

#[tauri::command]
fn sett(adresse: String, verdi: Verdi, tilstand: State<AppTilstand>) -> Result<(), String> {
    med_styring(&tilstand, |s| {
        s.send(Kommando::Sett(adresse, verdi));
        Ok(())
    })
}

#[derive(Serialize)]
struct LastetFil {
    navn: String,
    sekunder: f32,
}

/// Kjøres utenfor hovedtråden, så vinduet ikke fryser mens en stor fil lastes.
#[tauri::command]
async fn last_fil(kanal: String, sti: String, tilstand: State<'_, AppTilstand>) -> Result<LastetFil, String> {
    let p = PathBuf::from(&sti);
    let lyd = lydkort::last_lydfil(&p).map_err(feil)?;
    let sekunder = lyd.lengde() as f32 / kjerne::SAMPLERATE as f32;
    med_styring(&tilstand, |s| {
        s.send(Kommando::Last(kanal, lyd));
        Ok(())
    })?;
    Ok(LastetFil { navn: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(sti), sekunder })
}

#[tauri::command]
fn testtone(kanal: String, tilstand: State<AppTilstand>) -> Result<LastetFil, String> {
    let mut lyd = kjerne::testlyd::tone(1000.0, -18.0, 3.0);
    for del in [kjerne::testlyd::stillhet(0.5), kjerne::testlyd::venstre_hoyre(-18.0, 4.0)] {
        lyd.v.extend_from_slice(&del.v);
        lyd.h.extend_from_slice(&del.h);
    }
    let sekunder = lyd.lengde() as f32 / kjerne::SAMPLERATE as f32;
    med_styring(&tilstand, |s| {
        s.send(Kommando::Last(kanal, lyd));
        Ok(())
    })?;
    Ok(LastetFil { navn: "Testtone 1 kHz −18 dBFS + venstre/høyre".into(), sekunder })
}

#[tauri::command]
fn spill(kanal: String, spill: bool, lokke: Option<bool>, tilstand: State<AppTilstand>) -> Result<(), String> {
    med_styring(&tilstand, |s| {
        if let Some(l) = lokke {
            s.send(Kommando::Lokke(kanal.clone(), l));
        }
        s.send(if spill { Kommando::Spill(kanal) } else { Kommando::Stopp(kanal) });
        Ok(())
    })
}

#[derive(Serialize)]
struct Tilstandsrapport {
    kjorer: bool,
    utgang: String,
    inngang: Option<String>,
    buffer_ms: f32,
    on_air: bool,
    kanaler: Vec<(String, Option<f32>, Option<f32>)>,
    master: (Option<f32>, Option<f32>),
    spillere: std::collections::HashMap<String, (f32, f32, bool)>,
    inngang_tom: u64,
    opptak: Vec<(String, f64, String)>,
}

fn endelig(x: f32) -> Option<f32> {
    x.is_finite().then_some(x)
}

#[tauri::command]
async fn status(tilstand: State<'_, AppTilstand>) -> Result<Tilstandsrapport, String> {
    Ok(status_naa(&tilstand))
}

fn status_naa(tilstand: &State<'_, AppTilstand>) -> Tilstandsrapport {
    let lyd = tilstand.lyd.lock().unwrap();
    let opptak = tilstand.opptak.lock().unwrap();
    let Some(k) = lyd.as_ref() else {
        return Tilstandsrapport {
            kjorer: false,
            utgang: String::new(),
            inngang: None,
            buffer_ms: 0.0,
            on_air: false,
            kanaler: vec![],
            master: (None, None),
            spillere: Default::default(),
            inngang_tom: 0,
            opptak: vec![],
        };
    };
    let st = k.styring.hent_topp();
    let m = st.topp.or(st.maalere);
    Tilstandsrapport {
        kjorer: true,
        utgang: st.utgang_navn,
        inngang: st.inngang_navn,
        buffer_ms: st.buffer_ms,
        on_air: st.on_air,
        kanaler: m.as_ref().map(|m| m.kanaler.iter().map(|(id, v, h)| (id.clone(), endelig(*v), endelig(*h))).collect()).unwrap_or_default(),
        master: m.as_ref().map(|m| (endelig(m.master.0), endelig(m.master.1))).unwrap_or((None, None)),
        spillere: st.spillere,
        inngang_tom: st.inngang_tom,
        opptak: opptak.iter().map(|o| (o.buss.navn().to_string(), o.sekunder(), o.sti.display().to_string())).collect(),
    }
}

fn opptaksmappe() -> PathBuf {
    dirs::audio_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from(".")).join("Klubblisten opptak")
}

#[tauri::command]
fn start_opptak(tilstand: State<AppTilstand>) -> Result<Vec<String>, String> {
    let lyd = tilstand.lyd.lock().unwrap();
    let mut opptak = tilstand.opptak.lock().unwrap();
    let Some(k) = lyd.as_ref() else { return Err("Start lyden før du starter opptak.".into()) };
    if !opptak.is_empty() {
        return Err("Opptaket går allerede.".into());
    }
    let tid = chrono::Local::now().format("%Y-%m-%d %H%M%S").to_string();
    let mappe = opptaksmappe();
    for buss in [Buss::Program, Buss::DjRen] {
        let sti = mappe.join(format!("{tid} {}.wav", buss.navn()));
        opptak.push(k.styring.start_opptak(buss, sti).map_err(|e| format!("Kunne ikke starte opptak: {e}"))?);
    }
    Ok(opptak.iter().map(|o| o.sti.display().to_string()).collect())
}

#[derive(Serialize)]
struct Opptaksresultat {
    filer: Vec<String>,
    sekunder: f64,
    tapt: u64,
    mappe: String,
}

#[tauri::command]
fn stopp_opptak(tilstand: State<AppTilstand>) -> Result<Opptaksresultat, String> {
    let lyd = tilstand.lyd.lock().unwrap();
    let mut opptak = tilstand.opptak.lock().unwrap();
    let Some(k) = lyd.as_ref() else { return Err("Lyden er ikke startet.".into()) };
    let mut r = Opptaksresultat { filer: vec![], sekunder: 0.0, tapt: 0, mappe: opptaksmappe().display().to_string() };
    for o in opptak.drain(..) {
        let info = k.styring.stopp_opptak(o).map_err(|e| format!("Feil ved lagring av opptak: {e}"))?;
        r.sekunder = r.sekunder.max(info.sekunder);
        r.tapt += info.tapt;
        r.filer.push(info.sti.display().to_string());
    }
    Ok(r)
}

#[tauri::command]
fn aapne_opptaksmappe() -> Result<(), String> {
    let m = opptaksmappe();
    std::fs::create_dir_all(&m).map_err(feil)?;
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let cmd = "xdg-open";
    std::process::Command::new(cmd).arg(&m).spawn().map_err(feil)?;
    Ok(())
}

#[tauri::command]
fn aktivt_oppsett(tilstand: State<AppTilstand>) -> Option<(LydValg, Vec<KanalInfo>)> {
    tilstand.lyd.lock().unwrap().as_ref().map(|k| (k.oppsett.clone(), k.kanaler.clone()))
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppTilstand::default())
        .invoke_handler(tauri::generate_handler![
            enheter,
            start_lyd,
            stopp_lyd,
            sett,
            last_fil,
            testtone,
            spill,
            status,
            start_opptak,
            stopp_opptak,
            aapne_opptaksmappe,
            aktivt_oppsett
        ])
        .run(tauri::generate_context!())
        .expect("Klarte ikke å starte Klubblisten Studio");
}
