//! Lydkort: finner lydkort, kobler motoren til dem og spiller lydfiler.
//!
//! - Inngang og utgang velges hver for seg (f.eks. Focusrite 2i2 inn, SPL ut).
//! - Motoren kjører i utgangens lyd-tråd. Inngangen leveres via en ringbuffer
//!   som også tåler at to lydkort har litt ulik klokke.
//! - Alt internt er 48 kHz / 32-bit flyttall. Lydfiler med annen samplerate
//!   konverteres når de lastes.

pub mod enheter;
pub mod lydfil;
pub mod opptak;
pub mod strom;

pub use enheter::{liste, Enhet};
pub use lydfil::last_lydfil;
pub use opptak::{Buss, Opptak, OpptakInfo};
pub use strom::{Inngangsvalg, Kommando, Lydoppsett, Lydstrom, Lytt, SpillerStatus, Status, Styring};

/// Feil fra lydkort-laget, med norsk forklaring.
#[derive(Debug)]
pub enum Lydfeil {
    FantIkkeEnhet(String),
    IngenStandardEnhet(&'static str),
    Ikke48k(String),
    ForFaaKanaler { enhet: String, har: usize, trenger: usize },
    Lydsystem(String),
    Fil(String),
}

impl std::fmt::Display for Lydfeil {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Lydfeil::FantIkkeEnhet(n) => write!(f, "Fant ikke lydkortet «{n}». Er det koblet til? Kjør «studio enheter» for å se hva som finnes."),
            Lydfeil::IngenStandardEnhet(hva) => write!(f, "Maskinen har ikke noe standard {hva}-lydkort valgt."),
            Lydfeil::Ikke48k(n) => write!(f, "Lydkortet «{n}» kan ikke kjøre på 48 kHz, som suiten bruker."),
            Lydfeil::ForFaaKanaler { enhet, har, trenger } => write!(f, "Lydkortet «{enhet}» har {har} kanaler, men oppsettet trenger {trenger}."),
            Lydfeil::Lydsystem(e) => write!(f, "Lydsystemet svarte med feil: {e}"),
            Lydfeil::Fil(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Lydfeil {}
