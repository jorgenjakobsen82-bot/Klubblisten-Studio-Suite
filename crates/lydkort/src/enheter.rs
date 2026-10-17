//! Oversikt over lydkortene maskinen ser.

use crate::Lydfeil;
use cpal::traits::{DeviceTrait, HostTrait};
use kjerne::SAMPLERATE;

#[derive(Clone, Debug, serde::Serialize)]
pub struct Enhet {
    pub navn: String,
    pub inn_kanaler: usize,
    pub ut_kanaler: usize,
    pub stotter_48k: bool,
    pub standard_inn: bool,
    pub standard_ut: bool,
}

fn kanaler_og_48k<I: Iterator<Item = cpal::SupportedStreamConfigRange>>(it: Option<I>) -> (usize, bool) {
    let mut kanaler = 0;
    let mut k48 = false;
    if let Some(it) = it {
        for r in it {
            kanaler = kanaler.max(r.channels() as usize);
            if r.min_sample_rate().0 <= SAMPLERATE && r.max_sample_rate().0 >= SAMPLERATE {
                k48 = true;
            }
        }
    }
    (kanaler, k48)
}

/// Alle lydkort maskinen ser, med antall inn- og utkanaler.
pub fn liste() -> Result<Vec<Enhet>, Lydfeil> {
    let host = cpal::default_host();
    let std_inn = host.default_input_device().and_then(|d| d.name().ok());
    let std_ut = host.default_output_device().and_then(|d| d.name().ok());
    let mut ut = Vec::new();
    for d in host.devices().map_err(|e| Lydfeil::Lydsystem(e.to_string()))? {
        let Ok(navn) = d.name() else { continue };
        let (inn, k48_inn) = kanaler_og_48k(d.supported_input_configs().ok());
        let (utk, k48_ut) = kanaler_og_48k(d.supported_output_configs().ok());
        ut.push(Enhet {
            standard_inn: std_inn.as_deref() == Some(navn.as_str()),
            standard_ut: std_ut.as_deref() == Some(navn.as_str()),
            navn,
            inn_kanaler: inn,
            ut_kanaler: utk,
            stotter_48k: k48_inn || k48_ut,
        });
    }
    Ok(ut)
}

/// Finner et lydkort på (del av) navnet, uten å bry seg om store/små bokstaver.
/// «spl» finner f.eks. «SPL Phonitor xe».
pub(crate) fn finn(navn: Option<&str>, inngang: bool) -> Result<cpal::Device, Lydfeil> {
    let host = cpal::default_host();
    match navn {
        None => {
            if inngang {
                host.default_input_device().ok_or(Lydfeil::IngenStandardEnhet("inngangs"))
            } else {
                host.default_output_device().ok_or(Lydfeil::IngenStandardEnhet("utgangs"))
            }
        }
        Some(sok) => {
            let sok_l = sok.to_lowercase();
            let enheter: Vec<cpal::Device> = if inngang {
                host.input_devices().map_err(|e| Lydfeil::Lydsystem(e.to_string()))?.collect()
            } else {
                host.output_devices().map_err(|e| Lydfeil::Lydsystem(e.to_string()))?.collect()
            };
            // Eksakt treff først, så delvis.
            let mut delvis = None;
            for d in enheter {
                let n = d.name().unwrap_or_default().to_lowercase();
                if n == sok_l {
                    return Ok(d);
                }
                if delvis.is_none() && n.contains(&sok_l) {
                    delvis = Some(d);
                }
            }
            delvis.ok_or_else(|| Lydfeil::FantIkkeEnhet(sok.to_string()))
        }
    }
}
