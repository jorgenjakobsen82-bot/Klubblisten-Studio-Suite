//! Systemsjekk: bygger et demo-oppsett («Live torsdag»), kjører linjesjekken
//! og skriver en norsk rapport. Lager også testlyd-filer i `utdata/` som kan
//! lyttes på hjemme (SPL + Audeze), og en preset-fil som kan åpnes på nytt.
//!
//! Bruk:
//!   systemsjekk                 kjør demo-oppsettet
//!   systemsjekk <preset.json>   kjør linjesjekk på et lagret preset

use kjerne::linjesjekk::{self, Rapport, Status};
use kjerne::moduler::{Forsinkelse, Gain};
use kjerne::motor::{KanalOppsett, Kanaltype, Motor};
use kjerne::preset::StudioPreset;
use kjerne::{testlyd, Blokk};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn demo_motor() -> Motor {
    let mut m = Motor::ny();
    let mut v10 = KanalOppsett::ny("v10", "DJM-V10", Kanaltype::LinjeStereo);
    v10.til_dj_ren = true;
    let kanaler = vec![
        v10,
        KanalOppsett::ny("rode", "RØDECaster", Kanaltype::LinjeStereo),
        KanalOppsett::ny("mik1", "Mikrofon 1", Kanaltype::Mikrofon),
        KanalOppsett::ny("mik2", "Mikrofon 2", Kanaltype::Mikrofon),
        KanalOppsett::ny("spillerA", "Spiller A", Kanaltype::Spiller),
        KanalOppsett::ny("jingler", "Jingler", Kanaltype::Spiller),
        {
            let mut s = KanalOppsett::ny("spotify", "Spotify (privat)", Kanaltype::AppLyd);
            s.privat = true;
            s
        },
    ];
    for k in kanaler {
        m.legg_til_kanal(k).expect("kanal");
    }
    // Midlertidige moduler til de ekte (EQ, kompressor, Voice of God) er på plass:
    // en nøytral gain på DJ-kanalen, en aktiv gain på mikrofonen, og en
    // «look-ahead» på master som sendeveien må kompensere.
    m.legg_til_modul("v10", Box::new(Gain::ny(0.0))).unwrap();
    m.legg_til_modul("mik1", Box::new(Gain::ny(3.0))).unwrap();
    m.legg_til_master_modul(Box::new(Forsinkelse::ny(240)));
    m
}

/// Testsignaler på inngangene, slik de kunne kommet inn i studio.
fn demo_innganger() -> HashMap<String, Blokk> {
    let mut inn = HashMap::new();
    inn.insert("v10".into(), testlyd::tone(1000.0, -18.0, 0.5)); // perfekt kalibrert
    inn.insert("rode".into(), testlyd::tone(1000.0, -21.0, 0.5)); // 3 dB for lavt → auto-trim
    inn.insert("mik1".into(), testlyd::rosa_stoy(-30.0, 0.5)); // stemme-lignende
    inn.insert("mik2".into(), testlyd::stillhet(0.5)); // ingen signal
    inn.insert("spillerA".into(), testlyd::rosa_stoy(-20.0, 0.5));
    inn
}

fn tall(v: Option<f32>, enhet: &str) -> String {
    match v {
        Some(x) if x.is_finite() => format!("{x:+.1} {enhet}"),
        Some(_) => "−∞".into(),
        None => "—".into(),
    }
}

fn skriv_rapport(r: &Rapport) {
    let mut gruppe = String::new();
    for rad in &r.rader {
        if rad.gruppe != gruppe {
            gruppe = rad.gruppe.clone();
            println!("\n  {}", gruppe.to_uppercase());
            println!("  {:<4} {:<28} {:>10} {:>10} {:>10} {:>9}  {}", "", "Punkt", "Inn", "Ut", "Endring", "Forsink.", "Merknad");
        }
        let merke = match rad.status {
            Status::Ok => "OK",
            Status::Merknad => "!",
            Status::Feil => "FEIL",
        };
        let fors = rad.forsinkelse_ms.map(|f| format!("{f:.1} ms")).unwrap_or_else(|| "—".into());
        println!(
            "  {:<4} {:<28} {:>10} {:>10} {:>10} {:>9}  {}",
            merke,
            rad.navn,
            tall(rad.inn_dbfs, "dB"),
            tall(rad.ut_dbfs, "dB"),
            tall(rad.endring_db, "dB"),
            fors,
            rad.merknad
        );
    }
    if !r.rettinger.is_empty() {
        println!("\n  AUTOMATISKE RETTINGER");
        for ret in &r.rettinger {
            println!("  • {} → {:+.1} dB", ret.adresse, ret.verdi_db);
        }
    }
    println!("\n  {}\n", r.oppsummering());
}

fn skriv_lyttefiler(motor: &Motor, mappe: &Path) -> std::io::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(mappe)?;
    let mut filer = Vec::new();
    let lagre = |navn: &str, b: &Blokk, filer: &mut Vec<PathBuf>| {
        let sti = mappe.join(navn);
        testlyd::skriv_wav(&sti, b).map_err(|e| std::io::Error::other(e.to_string()))?;
        filer.push(sti);
        Ok::<(), std::io::Error>(())
    };
    lagre("01-referansetone-1kHz-minus18.wav", &testlyd::tone(1000.0, -18.0, 5.0), &mut filer)?;
    lagre("02-venstre-hoyre.wav", &testlyd::venstre_hoyre(-18.0, 4.0), &mut filer)?;
    lagre("03-sveip-20Hz-20kHz.wav", &testlyd::sveip(20.0, 20_000.0, -18.0, 10.0), &mut filer)?;
    lagre("04-rosa-stoy-minus20.wav", &testlyd::rosa_stoy(-20.0, 10.0), &mut filer)?;

    // Rosa støy gjennom hele kjeden: det lytteren ville hørt (program).
    let mut m = motor.skyggekopi();
    let stoy = testlyd::rosa_stoy(-20.0, 10.0);
    let mut inn = HashMap::new();
    inn.insert("spillerA".to_string(), stoy);
    let ut = m.prosesser(&inn);
    lagre("05-gjennom-kjeden-program.wav", &ut.program, &mut filer)?;
    lagre("06-gjennom-kjeden-hodetelefoner.wav", &ut.hodetelefoner, &mut filer)?;
    Ok(filer)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let utdata = PathBuf::from("utdata");

    let (motor, fasit, navn) = if let Some(sti) = args.get(1) {
        match StudioPreset::last(Path::new(sti)).and_then(|p| Ok((p.til_motor()?, p.fasit.clone(), p.navn.clone()))) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("Kunne ikke åpne presetet: {e}");
                std::process::exit(2);
            }
        }
    } else {
        let m = demo_motor();
        let fasit = linjesjekk::lag_fasit(&m);
        (m, fasit, "Live torsdag".to_string())
    };

    println!("\n  KLUBBLISTEN STUDIO SUITE · SYSTEMSJEKK");
    println!("  Preset: {navn} · 48 kHz · 32-bit flyttall · blokk {} samples", kjerne::BLOKKSTORRELSE);
    println!(
        "  Sendevei forsinkelse: {:.1} ms (kompensert) · Lyttevei: {:.1} ms",
        motor.sendevei_forsinkelse() as f32 * 1000.0 / kjerne::SAMPLERATE as f32,
        motor.lyttevei_forsinkelse() as f32 * 1000.0 / kjerne::SAMPLERATE as f32
    );

    let rapport = linjesjekk::kjor(&motor, &fasit, &demo_innganger());
    skriv_rapport(&rapport);

    std::fs::create_dir_all(&utdata).ok();
    if args.len() < 2 {
        let preset = StudioPreset::fra_motor(&navn, &motor, fasit);
        let sti = utdata.join("live-torsdag.json");
        match preset.lagre(&sti) {
            Ok(()) => println!("  Preset lagret: {}", sti.display()),
            Err(e) => eprintln!("  Kunne ikke lagre preset: {e}"),
        }
    }
    let rapport_sti = utdata.join("linjesjekk-rapport.json");
    if let Ok(json) = serde_json::to_string_pretty(&rapport) {
        let _ = std::fs::write(&rapport_sti, json);
        println!("  Rapport (JSON): {}", rapport_sti.display());
    }
    match skriv_lyttefiler(&motor, &utdata.join("lyttefiler")) {
        Ok(f) => println!("  {} lyttefiler skrevet til {}", f.len(), utdata.join("lyttefiler").display()),
        Err(e) => eprintln!("  Kunne ikke skrive lyttefiler: {e}"),
    }
    std::process::exit(if rapport.bestatt() { 0 } else { 1 });
}
