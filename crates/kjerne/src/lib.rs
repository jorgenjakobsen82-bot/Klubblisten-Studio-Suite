//! Kjernen i Klubblisten Studio Suite.
//!
//! Lydmotoren jobber alltid i 48 kHz og 32-bit flyttall. Alt som kan styres,
//! oppgis i kontroll-registeret, alt som skjer, sendes som hendelser, og alt
//! kan testes med testlyd, målepunkter og nulltest.

pub mod hendelse;
pub mod kontroll;
pub mod linjesjekk;
pub mod lyd;
pub mod maaling;
pub mod modul;
pub mod moduler;
pub mod motor;
pub mod preset;
pub mod testlyd;

pub use lyd::{db_til_lin, lin_til_db, Blokk, BLOKKSTORRELSE, SAMPLERATE};
