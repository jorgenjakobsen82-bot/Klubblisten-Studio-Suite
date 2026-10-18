//! Lydmodulene som følger med i trinn 0–1.

pub mod forsinkelse;
pub mod gain;
pub mod inngang;

pub use forsinkelse::Forsinkelse;
pub use gain::Gain;
pub use inngang::{Inngangstrinn, Kanalmodus};
