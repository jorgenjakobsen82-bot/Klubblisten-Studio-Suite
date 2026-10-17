//! «studio» – prøveprogram for lydkortene, før app-vinduet er på plass.
//!
//!   studio enheter                         viser alle lydkort
//!   studio testtone  [--ut SPL]            1 kHz referanse, så venstre/høyre-test
//!   studio spill FIL.wav [--ut SPL] [--lokke]
//!   studio mikrofon --inn Focusrite [--ut SPL] [--musikk FIL.wav]
//!
//! Mens lyden går: Enter = stopp. I mikrofon-modus: «m» + Enter = mute/av-mute.

use kjerne::kontroll::Verdi;
use kjerne::motor::{KanalOppsett, Kanaltype, Motor};
use lydkort::{Inngangsvalg, Kommando, Lydoppsett, Lydstrom, Lytt, Styring};
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn arg(args: &[String], navn: &str) -> Option<String> {
    args.iter().position(|a| a == navn).and_then(|i| args.get(i + 1).cloned())
}

fn har(args: &[String], navn: &str) -> bool {
    args.iter().any(|a| a == navn)
}

fn feil(e: impl std::fmt::Display) -> ! {
    eprintln!("\n  {e}\n");
    std::process::exit(1);
}

fn hjelp() {
    println!(
        "
  KLUBBLISTEN STUDIO SUITE · lydkort-test

  studio enheter
      Viser alle lydkort maskinen ser.

  studio testtone [--ut NAVN]
      Spiller 1 kHz på −18 dBFS i 3 sekunder, så «venstre» og «høyre» hver for seg.

  studio spill FIL.wav [--ut NAVN] [--lokke]
      Spiller en lydfil gjennom mikseren (kanal «Spiller A»).

  studio mikrofon --inn NAVN [--ut NAVN] [--musikk FIL.wav]
      Mikrofon på inngang 1 rett i hodetelefonene, uten forsinkelse.

  NAVN kan være en del av navnet, f.eks. «spl» eller «focusrite».
  Uten --ut brukes maskinens standard utgang.
"
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("enheter") => enheter(),
        Some("testtone") => testtone(&args),
        Some("spill") => spill(&args),
        Some("mikrofon") => mikrofon(&args),
        _ => hjelp(),
    }
}

fn enheter() {
    let liste = lydkort::liste().unwrap_or_else(|e| feil(e));
    println!("\n  LYDKORT\n");
    println!("  {:<42} {:>5} {:>5}  {}", "Navn", "Inn", "Ut", "");
    for e in liste {
        let mut merk = Vec::new();
        if e.standard_inn {
            merk.push("standard inn");
        }
        if e.standard_ut {
            merk.push("standard ut");
        }
        if !e.stotter_48k {
            merk.push("STØTTER IKKE 48 kHz");
        }
        println!("  {:<42} {:>5} {:>5}  {}", e.navn, e.inn_kanaler, e.ut_kanaler, merk.join(" · "));
    }
    println!();
}

fn motor_med_spiller() -> Motor {
    let mut m = Motor::ny();
    m.legg_til_kanal(KanalOppsett::ny("spillerA", "Spiller A", Kanaltype::Spiller)).unwrap();
    m
}

fn start(motor: Motor, oppsett: Lydoppsett) -> Lydstrom {
    let s = Lydstrom::start(motor, oppsett).unwrap_or_else(|e| feil(e));
    let st = s.status();
    println!("\n  Utgang: {}  ·  48 kHz  ·  buffer {:.1} ms", st.utgang_navn, st.buffer_ms);
    if let Some(inn) = &st.inngang_navn {
        println!("  Inngang: {inn}");
    }
    s
}

/// Tegner nivåmålere i terminalen til `stopp` settes.
fn maalere(s: &Styring, stopp: &AtomicBool, vis: &[&str]) {
    let mut holdt: Vec<f32> = vec![-90.0; vis.len() + 1];
    while !stopp.load(Ordering::Relaxed) {
        let st = s.status();
        let mut linje = String::from("\r  ");
        if let Some(m) = &st.maalere {
            let mut verdier: Vec<(String, f32)> = vis
                .iter()
                .map(|id| {
                    let db = m.kanaler.iter().find(|k| k.0 == *id).map(|k| k.1.max(k.2)).unwrap_or(f32::NEG_INFINITY);
                    (id.to_string(), db)
                })
                .collect();
            verdier.push(("Master".into(), m.master.0.max(m.master.1)));
            for (i, (navn, db)) in verdier.iter().enumerate() {
                let db = if db.is_finite() { *db } else { -90.0 };
                holdt[i] = if db > holdt[i] { db } else { (holdt[i] - 1.5).max(db).max(-90.0) };
                let fylt = (((holdt[i] + 60.0) / 60.0).clamp(0.0, 1.0) * 20.0) as usize;
                let tekst = if holdt[i] <= -89.0 { "  −∞ ".to_string() } else { format!("{:5.1}", holdt[i]) };
                linje.push_str(&format!("{navn} [{}{}] {tekst}  ", "█".repeat(fylt), " ".repeat(20 - fylt)));
            }
        }
        for (id, (pos, len, _)) in &st.spillere {
            linje.push_str(&format!("{id} {:.0}/{:.0} s  ", pos, len));
        }
        if st.on_air {
            linje.push_str("● ON AIR  ");
        }
        if st.inngang_tom > 0 {
            linje.push_str(&format!("hakk {}  ", st.inngang_tom));
        }
        print!("{linje}   ");
        let _ = std::io::stdout().flush();
        std::thread::sleep(Duration::from_millis(80));
    }
    println!();
}

fn vent_paa_enter(s: &Lydstrom, mikrofon: Option<&str>, vis: &[&str]) {
    let stopp = Arc::new(AtomicBool::new(false));
    let stopp2 = stopp.clone();
    let mik = mikrofon.map(String::from);
    let (tx, rx) = std::sync::mpsc::channel::<Option<Kommando>>();
    std::thread::spawn(move || {
        let mut muted = false;
        for linje in std::io::stdin().lock().lines() {
            let l = linje.unwrap_or_default();
            match (l.trim(), &mik) {
                ("m", Some(id)) => {
                    muted = !muted;
                    let _ = tx.send(Some(Kommando::Sett(format!("kanal/{id}/mute"), Verdi::AvPaa(muted))));
                }
                _ => {
                    stopp2.store(true, Ordering::Relaxed);
                    let _ = tx.send(None);
                    break;
                }
            }
        }
    });
    println!("  Trykk Enter for å stoppe.{}\n", if mikrofon.is_some() { " Skriv m + Enter for å mute mikrofonen." } else { "" });
    let styring = s.styring();
    std::thread::scope(|sc| {
        sc.spawn(|| maalere(&styring, &stopp, vis));
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Some(k)) => s.send(k),
                Ok(None) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Stopp av seg selv når spilleren er ferdig (og ikke looper).
                    let st = s.status();
                    if mikrofon.is_none() && !st.spillere.is_empty() && st.spillere.values().all(|(_, _, spiller)| !spiller) {
                        stopp.store(true, Ordering::Relaxed);
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

fn oppsett_ut(args: &[String]) -> Lydoppsett {
    Lydoppsett { utgang: arg(args, "--ut"), ..Default::default() }
}

fn testtone(args: &[String]) {
    let s = start(motor_med_spiller(), oppsett_ut(args));
    let mut lyd = kjerne::testlyd::tone(1000.0, -18.0, 3.0);
    let pause = kjerne::testlyd::stillhet(0.5);
    let vh = kjerne::testlyd::venstre_hoyre(-18.0, 4.0);
    for del in [&pause, &vh] {
        lyd.v.extend_from_slice(&del.v);
        lyd.h.extend_from_slice(&del.h);
    }
    println!("  1 kHz på −18 dBFS i 3 sekunder, deretter venstre og høyre hver for seg.");
    s.send(Kommando::Last("spillerA".into(), lyd));
    s.send(Kommando::Spill("spillerA".into()));
    std::thread::sleep(Duration::from_millis(200));
    vent_paa_enter(&s, None, &["spillerA"]);
}

fn spill(args: &[String]) {
    let Some(fil) = args.get(1).filter(|a| !a.starts_with("--")) else {
        feil("Skriv hvilken fil som skal spilles, f.eks.: studio spill sang.wav");
    };
    print!("  Laster {fil} …");
    let _ = std::io::stdout().flush();
    let lyd = lydkort::last_lydfil(Path::new(fil)).unwrap_or_else(|e| feil(e));
    println!(" {:.0} sekunder", lyd.lengde() as f32 / kjerne::SAMPLERATE as f32);
    let s = start(motor_med_spiller(), oppsett_ut(args));
    s.send(Kommando::Last("spillerA".into(), lyd));
    s.send(Kommando::Lokke("spillerA".into(), har(args, "--lokke")));
    s.send(Kommando::Spill("spillerA".into()));
    std::thread::sleep(Duration::from_millis(200));
    vent_paa_enter(&s, None, &["spillerA"]);
}

fn mikrofon(args: &[String]) {
    let Some(inn) = arg(args, "--inn") else {
        feil("Skriv hvilket lydkort mikrofonen står i, f.eks.: studio mikrofon --inn focusrite --ut spl");
    };
    let mut m = Motor::ny();
    m.legg_til_kanal(KanalOppsett::ny("mik1", "Mikrofon 1", Kanaltype::Mikrofon)).unwrap();
    m.sett("kanal/mik1/fader", Verdi::Tall(0.0)).unwrap();
    let musikk = arg(args, "--musikk");
    if musikk.is_some() {
        m.legg_til_kanal(KanalOppsett::ny("spillerA", "Spiller A", Kanaltype::Spiller)).unwrap();
        m.sett("kanal/spillerA/fader", Verdi::Tall(-10.0)).unwrap();
    }
    let oppsett = Lydoppsett {
        utgang: arg(args, "--ut"),
        inngang: Some(inn),
        innganger: vec![Inngangsvalg { kanal_id: "mik1".into(), forste: 1, stereo: false }],
        lytt: Lytt::Hodetelefoner,
        ..Default::default()
    };
    let s = start(m, oppsett);
    if let Some(fil) = musikk {
        let lyd = lydkort::last_lydfil(Path::new(&fil)).unwrap_or_else(|e| feil(e));
        s.send(Kommando::Last("spillerA".into(), lyd));
        s.send(Kommando::Lokke("spillerA".into(), true));
        s.send(Kommando::Spill("spillerA".into()));
    }
    println!("  Mikrofon på inngang 1 går rett i hodetelefonene. Pass på volumet!");
    vent_paa_enter(&s, Some("mik1"), &["mik1", "spillerA"]);
}
