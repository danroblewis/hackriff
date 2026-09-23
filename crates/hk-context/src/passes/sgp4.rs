//! SGP4, near-earth branch: the propagator TLEs are defined against (Hoots & Roehrich, Spacetrack
//! Report #3, as revised by Vallado, Crawford, Hujsak & Kelso, "Revisiting Spacetrack Report #3",
//! AIAA 2006-6753). WGS-72 constants, as the element sets are fitted with them.
//!
//! **Scope:** orbits with a period under 225 minutes (every LEO weather, amateur and cubesat
//! downlink a handheld receiver tracks through a pass). The deep-space branch (SDP4: GEO,
//! Molniya, GNSS) is not implemented, and such element sets are refused with
//! [`Sgp4Error::DeepSpace`] rather than propagated wrongly — a geostationary satellite does not
//! "pass" anyway. Verified against the published test vectors of the paper (`tests` below).

use super::tle::Tle;

/// WGS-72 gravitational parameter, km³/s².
const MU: f64 = 398_600.8;
/// WGS-72 equatorial radius, km.
pub const RADIUS_EARTH_KM: f64 = 6378.135;
const J2: f64 = 0.001_082_616;
const J3: f64 = -0.000_002_538_81;
const J4: f64 = -0.000_001_655_97;
const TWO_PI: f64 = std::f64::consts::TAU;
const X2O3: f64 = 2.0 / 3.0;

/// Why an element set cannot be propagated.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
pub enum Sgp4Error {
    /// Period ≥ 225 min: needs SDP4, which is not implemented.
    #[error("deep-space orbit (period {period_min:.1} min ≥ 225): SDP4 is not implemented")]
    DeepSpace {
        /// Orbital period, minutes.
        period_min: f64,
    },
    /// Mean elements left the valid domain (eccentricity ≥ 1 or < 0, or a negative semi-latus
    /// rectum) — the element set is far outside its fit span.
    #[error("elements diverged at {tsince_min:.1} min from epoch")]
    Diverged {
        /// Minutes from epoch.
        tsince_min: f64,
    },
    /// The satellite is below the Earth's surface: it has decayed.
    #[error("satellite decayed at {tsince_min:.1} min from epoch")]
    Decayed {
        /// Minutes from epoch.
        tsince_min: f64,
    },
}

/// A position and velocity in the TEME frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StateTeme {
    /// Position, km.
    pub r_km: [f64; 3],
    /// Velocity, km/s.
    pub v_km_s: [f64; 3],
}

/// An initialised SGP4 propagator for one element set.
#[derive(Clone, Debug)]
pub struct Sgp4 {
    xke: f64,
    isimp: bool,
    ecco: f64,
    inclo: f64,
    nodeo: f64,
    argpo: f64,
    mo: f64,
    bstar: f64,
    no: f64,
    eta: f64,
    cc1: f64,
    cc4: f64,
    cc5: f64,
    d2: f64,
    d3: f64,
    d4: f64,
    delmo: f64,
    sinmao: f64,
    mdot: f64,
    argpdot: f64,
    nodedot: f64,
    nodecf: f64,
    omgcof: f64,
    xmcof: f64,
    t2cof: f64,
    t3cof: f64,
    t4cof: f64,
    t5cof: f64,
    xlcof: f64,
    aycof: f64,
    con41: f64,
    x1mth2: f64,
    x7thm1: f64,
}

impl Sgp4 {
    /// Initialises from mean elements (`sgp4init`).
    pub fn new(tle: &Tle) -> Result<Self, Sgp4Error> {
        let xke = 60.0 / (RADIUS_EARTH_KM.powi(3) / MU).sqrt();
        let j3oj2 = J3 / J2;
        let ecco = tle.eccentricity;
        let inclo = tle.inclination_deg.to_radians();
        let nodeo = tle.raan_deg.to_radians();
        let argpo = tle.arg_perigee_deg.to_radians();
        let mo = tle.mean_anomaly_deg.to_radians();
        let no_kozai = tle.mean_motion_rev_day * TWO_PI / 1440.0;
        let bstar = tle.bstar;

        // initl: un-Kozai the mean motion.
        let ss = 78.0 / RADIUS_EARTH_KM + 1.0;
        let qzms2t = ((120.0 - 78.0) / RADIUS_EARTH_KM).powi(4);
        let ak = (xke / no_kozai).powf(X2O3);
        let eccsq = ecco * ecco;
        let omeosq = 1.0 - eccsq;
        let rteosq = omeosq.sqrt();
        let cosio = inclo.cos();
        let cosio2 = cosio * cosio;
        let d1 = 0.75 * J2 * (3.0 * cosio2 - 1.0) / (rteosq * omeosq);
        let mut del = d1 / (ak * ak);
        let adel = ak * (1.0 - del * del - del * (1.0 / 3.0 + 134.0 * del * del / 81.0));
        del = d1 / (adel * adel);
        let no = no_kozai / (1.0 + del);
        let ao = (xke / no).powf(X2O3);
        let sinio = inclo.sin();
        let po = ao * omeosq;
        let con42 = 1.0 - 5.0 * cosio2;
        let con41 = -con42 - cosio2 - cosio2;
        let posq = po * po;
        let rp = ao * (1.0 - ecco);

        let period_min = TWO_PI / no;
        if period_min >= 225.0 {
            return Err(Sgp4Error::DeepSpace { period_min });
        }

        // sgp4init, near-earth.
        let isimp = rp < 220.0 / RADIUS_EARTH_KM + 1.0;
        let mut sfour = ss;
        let mut qzms24 = qzms2t;
        let perige = (rp - 1.0) * RADIUS_EARTH_KM;
        if perige < 156.0 {
            sfour = if perige < 98.0 { 20.0 } else { perige - 78.0 };
            qzms24 = ((120.0 - sfour) / RADIUS_EARTH_KM).powi(4);
            sfour = sfour / RADIUS_EARTH_KM + 1.0;
        }
        let pinvsq = 1.0 / posq;
        let tsi = 1.0 / (ao - sfour);
        let eta = ao * ecco * tsi;
        let etasq = eta * eta;
        let eeta = ecco * eta;
        let psisq = (1.0 - etasq).abs();
        let coef = qzms24 * tsi.powi(4);
        let coef1 = coef / psisq.powf(3.5);
        let cc2 = coef1
            * no
            * (ao * (1.0 + 1.5 * etasq + eeta * (4.0 + etasq))
                + 0.375 * J2 * tsi / psisq * con41 * (8.0 + 3.0 * etasq * (8.0 + etasq)));
        let cc1 = bstar * cc2;
        let cc3 = if ecco > 1.0e-4 {
            -2.0 * coef * tsi * j3oj2 * no * sinio / ecco
        } else {
            0.0
        };
        let x1mth2 = 1.0 - cosio2;
        let cc4 = 2.0
            * no
            * coef1
            * ao
            * omeosq
            * (eta * (2.0 + 0.5 * etasq) + ecco * (0.5 + 2.0 * etasq)
                - J2 * tsi / (ao * psisq)
                    * (-3.0 * con41 * (1.0 - 2.0 * eeta + etasq * (1.5 - 0.5 * eeta))
                        + 0.75
                            * x1mth2
                            * (2.0 * etasq - eeta * (1.0 + etasq))
                            * (2.0 * argpo).cos()));
        let cc5 = 2.0 * coef1 * ao * omeosq * (1.0 + 2.75 * (etasq + eeta) + eeta * etasq);
        let cosio4 = cosio2 * cosio2;
        let temp1 = 1.5 * J2 * pinvsq * no;
        let temp2 = 0.5 * temp1 * J2 * pinvsq;
        let temp3 = -0.46875 * J4 * pinvsq * pinvsq * no;
        let mdot = no
            + 0.5 * temp1 * rteosq * con41
            + 0.0625 * temp2 * rteosq * (13.0 - 78.0 * cosio2 + 137.0 * cosio4);
        let argpdot = -0.5 * temp1 * con42
            + 0.0625 * temp2 * (7.0 - 114.0 * cosio2 + 395.0 * cosio4)
            + temp3 * (3.0 - 36.0 * cosio2 + 49.0 * cosio4);
        let xhdot1 = -temp1 * cosio;
        let nodedot = xhdot1
            + (0.5 * temp2 * (4.0 - 19.0 * cosio2) + 2.0 * temp3 * (3.0 - 7.0 * cosio2)) * cosio;
        let omgcof = bstar * cc3 * argpo.cos();
        let xmcof = if ecco > 1.0e-4 {
            -X2O3 * coef * bstar / eeta
        } else {
            0.0
        };
        let nodecf = 3.5 * omeosq * xhdot1 * cc1;
        let t2cof = 1.5 * cc1;
        let denom = if (cosio + 1.0).abs() > 1.5e-12 {
            1.0 + cosio
        } else {
            1.5e-12
        };
        let xlcof = -0.25 * j3oj2 * sinio * (3.0 + 5.0 * cosio) / denom;
        let aycof = -0.5 * j3oj2 * sinio;
        let delmo = (1.0 + eta * mo.cos()).powi(3);
        let sinmao = mo.sin();
        let x7thm1 = 7.0 * cosio2 - 1.0;

        let (mut d2, mut d3, mut d4, mut t3cof, mut t4cof, mut t5cof) =
            (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        if !isimp {
            let cc1sq = cc1 * cc1;
            d2 = 4.0 * ao * tsi * cc1sq;
            let temp = d2 * tsi * cc1 / 3.0;
            d3 = (17.0 * ao + sfour) * temp;
            d4 = 0.5 * temp * ao * tsi * (221.0 * ao + 31.0 * sfour) * cc1;
            t3cof = d2 + 2.0 * cc1sq;
            t4cof = 0.25 * (3.0 * d3 + cc1 * (12.0 * d2 + 10.0 * cc1sq));
            t5cof = 0.2
                * (3.0 * d4 + 12.0 * cc1 * d3 + 6.0 * d2 * d2 + 15.0 * cc1sq * (2.0 * d2 + cc1sq));
        }
        Ok(Self {
            xke,
            isimp,
            ecco,
            inclo,
            nodeo,
            argpo,
            mo,
            bstar,
            no,
            eta,
            cc1,
            cc4,
            cc5,
            d2,
            d3,
            d4,
            delmo,
            sinmao,
            mdot,
            argpdot,
            nodedot,
            nodecf,
            omgcof,
            xmcof,
            t2cof,
            t3cof,
            t4cof,
            t5cof,
            xlcof,
            aycof,
            con41,
            x1mth2,
            x7thm1,
        })
    }

    /// Un-Kozai'd mean motion, rad/min.
    pub fn mean_motion_rad_min(&self) -> f64 {
        self.no
    }

    /// TEME state `tsince` minutes after epoch.
    pub fn propagate(&self, tsince: f64) -> Result<StateTeme, Sgp4Error> {
        let t = tsince;
        let xmdf = self.mo + self.mdot * t;
        let argpdf = self.argpo + self.argpdot * t;
        let nodedf = self.nodeo + self.nodedot * t;
        let mut argpm = argpdf;
        let mut mm = xmdf;
        let t2 = t * t;
        let mut nodem = nodedf + self.nodecf * t2;
        let mut tempa = 1.0 - self.cc1 * t;
        let mut tempe = self.bstar * self.cc4 * t;
        let mut templ = self.t2cof * t2;
        if !self.isimp {
            let delomg = self.omgcof * t;
            let delm = self.xmcof * ((1.0 + self.eta * xmdf.cos()).powi(3) - self.delmo);
            let temp = delomg + delm;
            mm = xmdf + temp;
            argpm = argpdf - temp;
            let t3 = t2 * t;
            let t4 = t3 * t;
            tempa = tempa - self.d2 * t2 - self.d3 * t3 - self.d4 * t4;
            tempe += self.bstar * self.cc5 * (mm.sin() - self.sinmao);
            templ += self.t3cof * t3 + t4 * (self.t4cof + t * self.t5cof);
        }
        let am = (self.xke / self.no).powf(X2O3) * tempa * tempa;
        let nm = self.xke / am.powf(1.5);
        let mut em = self.ecco - tempe;
        if !(-0.001..1.0).contains(&em) {
            return Err(Sgp4Error::Diverged { tsince_min: t });
        }
        if em < 1.0e-6 {
            em = 1.0e-6;
        }
        mm += self.no * templ;
        let mut xlm = mm + argpm + nodem;
        nodem %= TWO_PI;
        argpm %= TWO_PI;
        xlm %= TWO_PI;
        let mp = (xlm - argpm - nodem) % TWO_PI;
        let (sinip, cosip) = self.inclo.sin_cos();

        // Long-period periodics.
        let axnl = em * argpm.cos();
        let temp = 1.0 / (am * (1.0 - em * em));
        let aynl = em * argpm.sin() + temp * self.aycof;
        let xl = mp + argpm + nodem + temp * self.xlcof * axnl;

        // Kepler's equation.
        let u = (xl - nodem) % TWO_PI;
        let mut eo1 = u;
        let (mut sineo1, mut coseo1) = (0.0, 0.0);
        let mut tem5: f64 = 9999.9;
        let mut ktr = 1;
        while tem5.abs() >= 1.0e-12 && ktr <= 10 {
            (sineo1, coseo1) = eo1.sin_cos();
            tem5 = 1.0 - coseo1 * axnl - sineo1 * aynl;
            tem5 = (u - aynl * coseo1 + axnl * sineo1 - eo1) / tem5;
            if tem5.abs() >= 0.95 {
                tem5 = 0.95f64.copysign(tem5);
            }
            eo1 += tem5;
            ktr += 1;
        }

        // Short-period periodics.
        let ecose = axnl * coseo1 + aynl * sineo1;
        let esine = axnl * sineo1 - aynl * coseo1;
        let el2 = axnl * axnl + aynl * aynl;
        let pl = am * (1.0 - el2);
        if pl < 0.0 {
            return Err(Sgp4Error::Diverged { tsince_min: t });
        }
        let rl = am * (1.0 - ecose);
        let rdotl = am.sqrt() * esine / rl;
        let rvdotl = pl.sqrt() / rl;
        let betal = (1.0 - el2).sqrt();
        let temp = esine / (1.0 + betal);
        let sinu = am / rl * (sineo1 - aynl - axnl * temp);
        let cosu = am / rl * (coseo1 - axnl + aynl * temp);
        let mut su = sinu.atan2(cosu);
        let sin2u = (cosu + cosu) * sinu;
        let cos2u = 1.0 - 2.0 * sinu * sinu;
        let temp = 1.0 / pl;
        let temp1 = 0.5 * J2 * temp;
        let temp2 = temp1 * temp;
        let mrt = rl * (1.0 - 1.5 * temp2 * betal * self.con41) + 0.5 * temp1 * self.x1mth2 * cos2u;
        su -= 0.25 * temp2 * self.x7thm1 * sin2u;
        let xnode = nodem + 1.5 * temp2 * cosip * sin2u;
        let xinc = self.inclo + 1.5 * temp2 * cosip * sinip * cos2u;
        let mvt = rdotl - nm * temp1 * self.x1mth2 * sin2u / self.xke;
        let rvdot = rvdotl + nm * temp1 * (self.x1mth2 * cos2u + 1.5 * self.con41) / self.xke;

        // Orientation vectors.
        let (sinsu, cossu) = su.sin_cos();
        let (snod, cnod) = xnode.sin_cos();
        let (sini, cosi) = xinc.sin_cos();
        let xmx = -snod * cosi;
        let xmy = cnod * cosi;
        let ux = xmx * sinsu + cnod * cossu;
        let uy = xmy * sinsu + snod * cossu;
        let uz = sini * sinsu;
        let vx = xmx * cossu - cnod * sinsu;
        let vy = xmy * cossu - snod * sinsu;
        let vz = sini * cossu;
        if mrt < 1.0 {
            return Err(Sgp4Error::Decayed { tsince_min: t });
        }
        let mr = mrt * RADIUS_EARTH_KM;
        let vkmpersec = RADIUS_EARTH_KM * self.xke / 60.0;
        Ok(StateTeme {
            r_km: [mr * ux, mr * uy, mr * uz],
            v_km_s: [
                (mvt * ux + rvdot * vx) * vkmpersec,
                (mvt * uy + rvdot * vy) * vkmpersec,
                (mvt * uz + rvdot * vz) * vkmpersec,
            ],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passes::tle::parse_pair;

    /// Vallado et al. 2006 verification case 00005 (Vanguard 1, e = 0.186, near-earth, drag).
    const L1: &str = "1 00005U 58002B   00179.78495062  .00000023  00000-0  28098-4 0  4753";
    const L2: &str = "2 00005  34.2682 348.7242 1859667 331.7664  19.3264 10.82419157413667";

    #[test]
    fn matches_the_published_sgp4_verification_vectors() {
        let tle = parse_pair(None, L1, L2, 1).unwrap();
        let sat = Sgp4::new(&tle).unwrap();
        // (tsince min, r km, v km/s) from the paper's tcppver.out.
        let cases: [(f64, [f64; 3], [f64; 3]); 3] = [
            (
                0.0,
                [7022.46529266, -1400.08296755, 0.03995155],
                [1.893841015, 6.405893759, 4.534807250],
            ),
            (
                360.0,
                [-7154.03120202, -3783.17682504, -3536.19412294],
                [4.741887409, -4.151817765, -2.093935425],
            ),
            (
                720.0,
                [-7134.59340119, 6531.68641334, 3260.27186483],
                [-4.113793027, -2.911922039, -2.557327851],
            ),
        ];
        for (t, r, v) in cases {
            let s = sat.propagate(t).unwrap();
            for k in 0..3 {
                assert!(
                    (s.r_km[k] - r[k]).abs() < 1e-3,
                    "t {t} r[{k}] {} vs {}",
                    s.r_km[k],
                    r[k]
                );
                assert!(
                    (s.v_km_s[k] - v[k]).abs() < 1e-6,
                    "t {t} v[{k}] {} vs {}",
                    s.v_km_s[k],
                    v[k]
                );
            }
        }
    }

    #[test]
    fn deep_space_element_sets_are_refused_not_propagated() {
        // A geostationary-like set (1.0027 rev/day): SDP4 territory.
        let mut tle = parse_pair(None, L1, L2, 1).unwrap();
        tle.mean_motion_rev_day = 1.0027;
        tle.eccentricity = 0.0002;
        assert!(matches!(Sgp4::new(&tle), Err(Sgp4Error::DeepSpace { .. })));
    }
}
