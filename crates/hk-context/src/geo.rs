//! Geometry for correlation: the device site and great-circle distance from it to an
//! [`hk_model::Geo`] extent (spherical Earth, R = 6371.0088 km; good to ~0.5 %).

use hk_model::Geo;
use serde::{Deserialize, Serialize};

/// Mean Earth radius, km (IUGG).
pub const EARTH_RADIUS_KM: f64 = 6371.0088;

/// The device position (config or C06 fix).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Site {
    /// Latitude, degrees.
    pub lat_deg: f64,
    /// Longitude, degrees.
    pub lon_deg: f64,
    /// Altitude, m (informational; unused by the distance rules).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt_m: Option<f64>,
}

impl Site {
    /// A site without altitude.
    pub const fn new(lat_deg: f64, lon_deg: f64) -> Self {
        Self {
            lat_deg,
            lon_deg,
            alt_m: None,
        }
    }

    /// Valid coordinates: finite, |lat| ≤ 90, |lon| ≤ 180.
    pub fn is_valid(&self) -> bool {
        self.lat_deg.is_finite()
            && self.lon_deg.is_finite()
            && self.lat_deg.abs() <= 90.0
            && self.lon_deg.abs() <= 180.0
    }
}

/// Haversine distance, km.
pub fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * a.sqrt().min(1.0).asin()
}

/// Signed longitude difference `lon − reference` wrapped into (−180, 180].
fn wrap_lon(delta: f64) -> f64 {
    let d = (delta + 180.0).rem_euclid(360.0) - 180.0;
    if d == -180.0 { 180.0 } else { d }
}

/// Distance from `site` to a latitude/longitude box, km (0 inside). A box with
/// `west_deg > east_deg` crosses the antimeridian.
///
/// The nearest point is taken with latitude clamped into the box and longitude clamped to the
/// nearer edge; on a sphere this slightly overestimates distances to far boxes at high latitude
/// but is exact for containment (0) and accurate for the tens-of-km radii the rules use.
pub fn distance_to_bbox_km(site: &Site, south: f64, west: f64, north: f64, east: f64) -> f64 {
    let width = (east - west).rem_euclid(360.0);
    let offset = (site.lon_deg - west).rem_euclid(360.0);
    let inside_lon = offset <= width;
    let lat = site.lat_deg.clamp(south, north);
    let lon = if inside_lon {
        site.lon_deg
    } else if wrap_lon(site.lon_deg - west).abs() <= wrap_lon(site.lon_deg - east).abs() {
        west
    } else {
        east
    };
    if inside_lon && lat == site.lat_deg {
        return 0.0;
    }
    haversine_km(site.lat_deg, site.lon_deg, lat, lon)
}

/// Distance from `site` to an event extent, km: 0 inside, `None` when the extent is not a place
/// on the ground (an orbit pass needs the pass rule, not a distance).
pub fn distance_km(site: &Site, geo: &Geo) -> Option<f64> {
    match *geo {
        Geo::Global => Some(0.0),
        Geo::Point { lat_deg, lon_deg } => {
            Some(haversine_km(site.lat_deg, site.lon_deg, lat_deg, lon_deg))
        }
        Geo::Circle {
            lat_deg,
            lon_deg,
            radius_km,
        } => {
            Some((haversine_km(site.lat_deg, site.lon_deg, lat_deg, lon_deg) - radius_km).max(0.0))
        }
        Geo::BoundingBox {
            south_deg,
            west_deg,
            north_deg,
            east_deg,
        } => Some(distance_to_bbox_km(
            site, south_deg, west_deg, north_deg, east_deg,
        )),
        Geo::OrbitPass { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haversine_known_distance() {
        // One degree of latitude ≈ 111.195 km on the mean sphere.
        assert!((haversine_km(0.0, 0.0, 1.0, 0.0) - 111.195).abs() < 0.01);
        assert_eq!(haversine_km(52.2, 0.12, 52.2, 0.12), 0.0);
    }

    #[test]
    fn bbox_containment_edges_and_antimeridian() {
        let site = Site::new(52.2, 0.12);
        assert_eq!(distance_to_bbox_km(&site, 52.0, 0.1, 52.5, 0.8), 0.0);
        // 0.02° of longitude west of the box at 52.2° N ≈ 1.36 km.
        let d = distance_to_bbox_km(&site, 52.0, 0.14, 52.5, 0.8);
        assert!((d - 1.365).abs() < 0.01, "{d}");
        // Antimeridian box W 179.5 → E −179.9 contains 179.8 and −179.95.
        for lon in [179.8, -179.95] {
            assert_eq!(
                distance_to_bbox_km(&Site::new(65.0, lon), 64.9, 179.5, 65.4, -179.9),
                0.0
            );
        }
        let outside = distance_to_bbox_km(&Site::new(65.0, 0.0), 64.9, 179.5, 65.4, -179.9);
        assert!(outside > 3000.0);
        assert_eq!(distance_km(&site, &Geo::Global), Some(0.0));
        assert_eq!(
            distance_km(
                &site,
                &Geo::OrbitPass {
                    norad_id: 1,
                    max_elevation_deg: 40.0
                }
            ),
            None
        );
        let c = Geo::Circle {
            lat_deg: 52.2,
            lon_deg: 0.12,
            radius_km: 5.0,
        };
        assert_eq!(distance_km(&Site::new(52.21, 0.12), &c), Some(0.0));
    }
}
