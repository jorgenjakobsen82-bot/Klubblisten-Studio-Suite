//! Modul-grensesnittet. Alle lydmoduler (kanal, stemmekjede, plugins osv.)
//! følger dette, så kjernen kan rute, teste og styre dem likt.

use crate::kontroll::{Kontroll, Kontrollfeil, Verdi};
use crate::lyd::Blokk;

pub trait Modul: Send {
    /// Typenavn i klarspråk, f.eks. "Kanal" eller "Forsinkelse".
    fn typenavn(&self) -> &'static str;

    /// Alle kontroller modulen har, med type. Brukes av MIDI-learn og presets.
    fn kontroller(&self) -> Vec<Kontroll>;

    fn sett(&mut self, kontroll: &str, verdi: Verdi) -> Result<(), Kontrollfeil>;

    fn hent(&self, kontroll: &str) -> Option<Verdi>;

    /// Behandler én blokk. `inn` og `ut` har samme lengde.
    fn prosesser(&mut self, inn: &Blokk, ut: &mut Blokk);

    /// Forsinkelse modulen legger til, i samples (look-ahead o.l.).
    /// Moduler med forsinkelse slippes aldri inn i lytteveien.
    fn forsinkelse(&self) -> usize {
        0
    }

    /// Sant når modulen står nøytralt og skal gi nøyaktig samme lyd ut som inn
    /// (nulltest). Aktive moduler sammenlignes i stedet mot en lagret fasit.
    fn er_noytral(&self) -> bool;

    /// Nullstiller interne tilstander (filtre, forsinkelseslinjer).
    fn nullstill(&mut self) {}

    /// Lager en kopi, brukt av linjesjekken (skjult parallell kopi av kjeden).
    fn klon(&self) -> Box<dyn Modul>;
}
