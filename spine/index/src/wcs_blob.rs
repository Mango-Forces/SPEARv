//! Compact binary WCS for the `exposure.wcs` column, so a wide coverage query
//! does not spend its time parsing text.
//!
//! Layout, little-endian: `b"WCS1"`, `crpix[2]`, `crval[2]`, `cd[2][2]` as f64,
//! `naxis[2]` as i32, then the SIP polynomials a, b, ap, bp. Each polynomial is a
//! presence byte, an order byte and `(order + 1)²` f64 coefficients.

use spearv_core::{Poly, Sip, Wcs};

const MAGIC: &[u8; 4] = b"WCS1";

pub fn encode(wcs: &Wcs) -> Vec<u8> {
    let mut out = Vec::with_capacity(512);
    out.extend_from_slice(MAGIC);
    for value in [
        wcs.crpix[0], wcs.crpix[1], wcs.crval[0], wcs.crval[1],
        wcs.cd[0][0], wcs.cd[0][1], wcs.cd[1][0], wcs.cd[1][1],
    ] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&wcs.naxis[0].to_le_bytes());
    out.extend_from_slice(&wcs.naxis[1].to_le_bytes());
    for poly in [Some(&wcs.sip.a), Some(&wcs.sip.b), wcs.sip.ap.as_ref(), wcs.sip.bp.as_ref()] {
        match poly {
            None => out.push(0),
            Some(poly) => {
                out.push(1);
                out.push(poly.order as u8);
                for value in poly.coefficients() {
                    out.extend_from_slice(&value.to_le_bytes());
                }
            }
        }
    }
    out
}

pub fn decode(bytes: &[u8]) -> Result<Wcs, String> {
    let mut reader = Reader { bytes, at: 0 };
    if reader.take(4)? != MAGIC {
        return Err("WCS blob has no WCS1 header".into());
    }
    let mut f = [0.0; 8];
    for value in &mut f {
        *value = reader.f64()?;
    }
    let naxis = [reader.i32()?, reader.i32()?];
    let a = reader.poly()?.ok_or("WCS blob has no SIP A")?;
    let b = reader.poly()?.ok_or("WCS blob has no SIP B")?;
    let ap = reader.poly()?;
    let bp = reader.poly()?;
    if reader.at != bytes.len() {
        return Err("WCS blob has trailing bytes".into());
    }
    Ok(Wcs {
        crpix: [f[0], f[1]],
        crval: [f[2], f[3]],
        cd: [[f[4], f[5]], [f[6], f[7]]],
        sip: Sip { a, b, ap, bp },
        naxis,
    })
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(n).filter(|end| *end <= self.bytes.len()).ok_or("WCS blob is short")?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    fn f64(&mut self) -> Result<f64, String> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().map_err(|_| "WCS blob is short")?))
    }

    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().map_err(|_| "WCS blob is short")?))
    }

    fn poly(&mut self) -> Result<Option<Poly>, String> {
        if self.take(1)?[0] == 0 {
            return Ok(None);
        }
        let order = self.take(1)?[0] as usize;
        let coeff = (0..(order + 1) * (order + 1)).map(|_| self.f64()).collect::<Result<Vec<_>, _>>()?;
        Poly::from_coefficients(order, coeff).map(Some).ok_or_else(|| "WCS blob polynomial is malformed".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sip_wcs_survives_the_round_trip() {
        let mut a = Poly::new(3);
        a.set(0, 2, 1.25e-6);
        a.set(2, 1, -3.5e-10);
        let mut bp = Poly::new(2);
        bp.set(1, 1, 7.0e-7);
        let wcs = Wcs {
            crpix: [1020.5, 1020.5],
            crval: [232.663579354, -10.7213238583],
            cd: [[0.00166847758952, -0.000443473792442], [-0.000378376587817, -0.00165038605882]],
            sip: Sip { a, b: Poly::new(3), ap: None, bp: Some(bp) },
            naxis: [2040, 2040],
        };
        let back = decode(&encode(&wcs)).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), serde_json::to_string(&wcs).unwrap());
        assert_eq!(back.pixel_to_sky(100.0, 1900.0), wcs.pixel_to_sky(100.0, 1900.0));
    }

    #[test]
    fn a_damaged_blob_is_an_error_not_a_panic() {
        let blob = encode(&crate::testing::detector(1.0, 2.0));
        for cut in [0, 3, 10, blob.len() - 1] {
            assert!(decode(&blob[..cut]).is_err(), "cut at {cut}");
        }
        let mut long = blob.clone();
        long.push(0);
        assert!(decode(&long).is_err());
    }
}
