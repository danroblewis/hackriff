# 05 — SDR Use-Cases, Explorations & Research Ideas

A brainstorm catalogue of things to do with a software defined radio, gathered from papers, citizen-science networks, open-source projects and conference talks. It is meant to show how much ground the hackriff tool could cover beyond "demodulate FM audio". Each item is one line. Grouping and prioritizing come later.

Compiled 2026-09-13 · **391 items** across 5 themes, after removing overlaps between sections.

**IDs.** Every item has a permanent ID (`SPACE-`, `PROP-`, `AWARE-`, `SIGNAL-`, `RESEARCH-` + number). IDs are never reused or renumbered: new items get the next number and retired items keep their ID. The machine-readable copy is [`use-cases.yaml`](use-cases.yaml); docs/05 and the YAML must stay in sync.

**Legend.** Each item ends with backtick tags, then references:
- **Frequency/band tag.** Items outside HackRF's native 1 MHz–6 GHz are marked. VLF/ELF usually means a soundcard, direct sampling or an upconverter. Ku-band and above means an LNB or downconverter.
- **`RX` or `TX+RX`.** `TX+RX` items need a license or rule authority (amateur, ISM power limits, an experimental license), or use only your own transmitters and devices.
- **Hardware in place of `RX`.** In the Long Tail section, some items list typical receiver hardware instead.

References were link-checked. A few publisher and DOI links block automated checks but resolve in a browser. A handful of items have no single canonical reference.

**Legal framing.** Receive, measure and characterize freely. Decode content only where that's lawful; see [04 §1.3](04-radio-engineering-and-signals-analysis.md). Record and decrypt **your own** traffic for any research purpose. Never circumvent the security of other people's traffic. Security-research items describe published findings, or testing on your own devices and testbeds.

## Contents

1. **Space Weather, the Sun, Radio Astronomy & Natural Radio** — 81 items
2. **Propagation, Atmosphere, Remote Sensing & RF Sensing** — 83 items
3. **Spectrum Situational Awareness, Interference & Anomalies** — 70 items
4. **Signals of Infrastructure, Transport, Satellites & Nature (the Long Tail)** — 79 items
5. **Unknown Signals, Security Research, Lab Engineering & ML** — 78 items

---

## 1. Space Weather, the Sun, Radio Astronomy & Natural Radio

### Solar Activity & Space Weather

- `SPACE-001` **SID flare monitor (SuperSID)** — Track amplitude and phase of distant navy VLF transmitters: the daytime D-region lowers during X-ray flares, so the signal jumps sharply. `16–30 kHz (below 1 MHz; soundcard/direct sampling)` · `RX` · [Stanford SID](http://solar-center.stanford.edu/SID/) [SuperSID](https://solar-center.stanford.edu/SID/sidmonitor/)
- `SPACE-002` **Flare-class estimation from VLF** — Calibrate your SID amplitude jumps against GOES X-ray flux, then classify flares automatically, e.g. with CNN/HMM models. `VLF (below 1 MHz)` · `RX` · [GOES X-ray](https://www.swpc.noaa.gov/products/goes-x-ray-flux) [CNN–HMM SID paper](https://doi.org/10.3390/s26082548)
- `SPACE-003` **e-CALLISTO solar burst spectrograms** — Sweep a wideband receiver to build dynamic spectra of Type I–V solar radio bursts, and join a 24-hour global network. `45–870 MHz` · `RX` · [e-CALLISTO](https://www.e-callisto.org/) [Benz et al. 2009](https://doi.org/10.1007/s11038-008-9267-6)
- `SPACE-004` **CME shock speed from Type II drift** — Measure the slow frequency drift of Type II bursts (plasma emission from a CME-driven shock) to estimate shock speed hours before the CME arrives. `20–300 MHz` · `RX` · [CMEs & Type II bursts](https://doi.org/10.1029/165gm20)
- `SPACE-005` **Cross-reference burst archives** — Match your own spectrogram events against RSTN/CALLISTO archives with SunPy tooling to confirm detections. `25–180 MHz` · `RX` · [RSTN spectral data](https://www.ngdc.noaa.gov/stp/space-weather/solar-data/solar-features/solar-radio/rstn-spectral/) [sunpy radiospectra](https://github.com/sunpy/radiospectra)
- `SPACE-006` **Homebrew F10.7 solar flux** — Measure the Sun's 10.7 cm radio flux with a small dish, a proxy for solar EUV output and a key input to space-weather models, and compare with Penticton. `2800 MHz` · `RX` · [Tapping 2013](https://doi.org/10.1002/swe.20064) [DRAO solar flux](https://www.spaceweather.gc.ca/forecast-prevision/solar-solaire/solarflux/sx-en.php) [SWPC F10.7](https://www.swpc.noaa.gov/phenomena/f107-cm-radio-emissions)
- `SPACE-007` **Solar radio bursts vs GNSS** — Log GPS carrier-to-noise ratio during big solar bursts: the Dec 2006 events measurably degraded GPS/WAAS tracking on the sunlit hemisphere. `1575 / 1227 MHz` · `RX` · [Cerruti et al.](https://doi.org/10.1029/2006sw000254)
- `SPACE-008` **Solar bursts jamming radar/radionavigation** — Study how intense L/S-band solar bursts (e.g. the Nov 2015 event that disrupted Swedish air-traffic radar) show up as broadband noise rises. `1–3 GHz` · `RX` · [Marqué et al. 2018](https://doi.org/10.1051/swsc/2018029)
- `SPACE-009` **Sun-noise antenna calibration** — Use the quiet Sun (flux from daily F10.7 reports) as a known source to measure G/T, beam pattern and pointing error of a dish or Yagi. `144 MHz–6 GHz` · `RX` · [SWPC F10.7](https://www.swpc.noaa.gov/phenomena/f107-cm-radio-emissions)
- `SPACE-010` **Sun-outage radiometry** — Record the twice-yearly moment the Sun passes behind a geostationary TV satellite; a free, precisely timed drift scan through an LNB. `10.7–12.7 GHz (outside; LNB IF 950–2150 MHz in range)` · `RX`
- `SPACE-011` **Magnetar/GRB ionospheric kicks** — Look for VLF perturbations from gamma-ray events: the 2004 SGR 1806-20 giant flare and GRB 221009A both ionized the D-region from light-years away. `VLF (below 1 MHz)` · `RX` · [Inan et al. 2007](https://doi.org/10.1029/2006gl029145) [GRB 221009A SID](https://doi.org/10.3847/2515-5172/ac9d2f)
- `SPACE-012` **Shortwave fadeout detector** — Automatically flag flare-driven HF radio blackouts from sudden noise-floor and beacon-strength drops across bands, and compare with D-RAP. `2–30 MHz` · `RX` · [SWPC R-scale](https://www.swpc.noaa.gov/phenomena/solar-flares-radio-blackouts) [D-RAP](https://www.swpc.noaa.gov/products/d-region-absorption-predictions-d-rap)
- `SPACE-013` **Sudden frequency deviation** — Catch the brief Doppler shift that a flare's EUV burst imposes on HF skywave carriers. `5–20 MHz` · `RX` · [SFD flare radiation](https://doi.org/10.1029/95ja03676)
- `SPACE-014` **Interplanetary scintillation** — Watch compact radio sources twinkle as solar-wind density irregularities cross the line of sight, to infer solar-wind speed. `~327 MHz (needs large aperture)` · `RX` · [ISEE IPS](https://stsw1.isee.nagoya-u.ac.jp/)
- `SPACE-015` **"Space weather now" local dashboard** — Fuse your SDR measurements (SID, riometer, burst spectra, HF Doppler) with Kp, solar wind and TEC feeds, with alerts. `multi-band` · `RX` · [Planetary Kp](https://www.swpc.noaa.gov/products/planetary-k-index) [Real-time solar wind](https://www.swpc.noaa.gov/products/real-time-solar-wind) [SpaceWeatherLive](https://www.spaceweatherlive.com/)

### Ionosphere & HF Propagation Science

- `SPACE-016` **Grape HF Doppler on WWV/CHU** — Record ultra-stable carrier Doppler at 1 Hz resolution to see sunrise/sunset, TIDs, flares and eclipses in ionospheric height changes. `2.5–25 MHz` · `RX` · [HamSCI Grape](https://hamsci.org/grape) [Collins et al. 2023](https://doi.org/10.5194/essd-15-1403-2023)
- `SPACE-017` **Personal Space Weather Station** — Run a GPS-disciplined multi-instrument ground station (HF, magnetometer) that feeds a shared scientific database. `0.1–60 MHz` · `RX` · [HamSCI PSWS](https://hamsci.org/psws) [TangerineSDR](https://tangerinesdr.com/) [ka9q-radio](https://github.com/ka9q/ka9q-radio)
- `SPACE-018` **Eclipse ionospheric experiments** — Measure HF path changes, Doppler and VLF phase shifts as the Moon's shadow switches off photoionization. `VLF–30 MHz` · `RX` · [HamSCI eclipse](https://hamsci.org/eclipse) [VLF eclipse anomaly](https://doi.org/10.1541/jae.30.95)
- `SPACE-019` **Traveling ionospheric disturbance tracking** — Detect medium- and large-scale TIDs, the ionospheric ripples of atmospheric gravity waves, in HF Doppler and skip-distance data. `3–30 MHz` · `RX` · [Frissell et al. 2016](https://doi.org/10.1002/2015ja022168) [HamSCI TID climatology](https://doi.org/10.1002/essoar.10510601.1)
- `SPACE-020` **Volcano/tsunami ionospheric waves** — Look for the global ionospheric pressure-wave signature of events like the 2022 Hunga Tonga eruption in archived HF Doppler recordings. `3–30 MHz` · `RX` · [Tonga TIDs (URSI)](https://doi.org/10.46620/ursigass.2023.3427.wcbv6678)
- `SPACE-021` **Live foF2/MUF cross-reference** — Pull digisonde data and crowd-sourced MUF maps to predict band openings and validate your own propagation logs. `3–30 MHz` · `RX` · [GIRO](https://giro.uml.edu/) [KC2G prop maps](https://prop.kc2g.com/)
- `SPACE-022` **Geomagnetic storm HF effects** — Quantify storm-time absorption, MUF depression and polar path loss from beacon/WSPR/RBN statistics over years. `1.8–30 MHz` · `RX` · [Sept 2017 storm study](https://doi.org/10.1016/j.asr.2018.07.016) [HamSCI solar-cycle climatology](https://doi.org/10.1002/essoar.10510706.1)
- `SPACE-023` **GNSS total electron content** — Derive slant TEC from dual-frequency GNSS pseudorange/phase differences and watch storm-enhanced density and equatorial anomalies. `1176–1575 MHz` · `RX` · [RTKLIB](https://github.com/tomojitakasu/RTKLIB) [IGS data](https://igs.org/) [SWPC TEC](https://www.swpc.noaa.gov/phenomena/total-electron-content)
- `SPACE-024` **Crowd-sourced ionosphere from phones** — Learn how noisy GNSS data from millions of Android phones mapped TEC better than reference networks, and apply the same idea to cheap receivers. `L-band` · `RX` · [Smith et al., Nature 2024](https://www.nature.com/articles/s41586-024-08072-x)
- `SPACE-025` **Faraday rotation of satellite signals** — Measure the polarization rotation of linearly polarized VHF/UHF satellite downlinks as an integral of TEC × magnetic field. `136–450 MHz` · `RX`
- `SPACE-026` **Rocket-launch ionospheric holes** — Look for TEC depletions and HF/GNSS anomalies after large launches, whose exhaust chemically erases F-region plasma. `L-band, HF` · `RX` · [Rocket exhaust effects (EGU25)](https://doi.org/10.5194/egusphere-egu25-3801)
- `SPACE-027` **D-region modelling from VLF paths** — Fit VLF amplitude/phase along known transmitter paths with LWPC-style waveguide models to retrieve D-region height and sharpness. `10–60 kHz (below 1 MHz)` · `RX` · [Gravity-wave D-region LWPC](https://doi.org/10.5194/angeo-2020-68)
- `SPACE-028` **VLF earthquake-precursor tests** — Test controversial claims of subionospheric VLF anomalies before earthquakes, with a rigorous statistical null model. `VLF/LF (below 1 MHz)` · `RX` · [Hayakawa VLF monitoring](https://doi.org/10.1541/jae.22.87)
- `SPACE-029` **Long-delayed echoes** — Hunt for the still-unexplained HF echoes that arrive seconds after a transmission (reported since 1927). `3–30 MHz` · `TX+RX` · [Holm, LDE review](https://doi.org/10.22541/essoar.168500349.90325274/v1) [Nature 1929](https://doi.org/10.1038/124164a0)
- `SPACE-030` **Ionospheric heater signals (HAARP)** — Receive HAARP campaign transmissions, and the ELF/VLF waves and stimulated electromagnetic emissions its modulated heating creates in the ionosphere. `2.7–10 MHz; ELF/VLF` · `RX` · [HAARP](https://haarp.gi.alaska.edu/) [100 days of ELF/VLF](https://doi.org/10.1002/jgra.50558) [SEE from HAARP](https://doi.org/10.1109/ursigass.2011.6051125)

### Aurora & High-Latitude Physics

- `SPACE-031` **Riometer** — Track the 24-hour sidereal curve of cosmic noise; dips below it reveal auroral and polar-cap absorption from precipitating particles. `20–50 MHz` · `RX` · [SGO riometer data](https://www.sgo.fi/Data/Riometer/rioData.php) [PCA vs solar particles](https://doi.org/10.21236/ada039894)
- `SPACE-032` **Polar cap absorption events** — Correlate multi-day HF blackouts on polar paths and riometer absorption with NOAA S-scale solar proton storms. `2–50 MHz` · `RX` · [SWPC radiation storms](https://www.swpc.noaa.gov/phenomena/solar-radiation-storm)
- `SPACE-033` **SuperDARN data exploration** — Pull HF coherent-scatter radar data for global convection maps, and listen to the radars' sounding sequences directly. `8–20 MHz` · `RX` · [VT SuperDARN](https://vt.superdarn.org/) [SuperDARN Canada](https://superdarn.ca/) [SuperDARN history](https://doi.org/10.5194/hgss-12-77-2021)
- `SPACE-034` **VHF radio aurora** — Receive signals scattered off field-aligned auroral irregularities, with their typical buzzy, Doppler-spread tone, during geomagnetic storms. `28–144 MHz` · `RX` · [SWPC geomagnetic storms](https://www.swpc.noaa.gov/phenomena/geomagnetic-storms)
- `SPACE-035` **Auroral roar** — Search for the narrowband natural emission near 2 and 3 times the electron gyrofrequency that the aurora produces at high latitudes. `2.8 / 4.2 MHz` · `RX` · [South Pole auroral roar](https://doi.org/10.1029/2001ja000319)
- `SPACE-036` **Auroral kilometric radiation (archive)** — Explore spacecraft recordings of Earth's most powerful natural radio emission, which the ionosphere blocks from the ground. `50–800 kHz (space only)` · `RX` · [AKR study](https://doi.org/10.5194/egusphere-egu22-4043) [PDS PPI archive](https://pds-ppi.igpp.ucla.edu/)

### Natural VLF/ELF Radio

- `SPACE-037` **Whistlers, chorus & tweeks (INSPIRE)** — Listen to lightning energy dispersed through the magnetosphere (whistlers), dawn chorus and tweeks with a simple E-field receiver far from power lines. `0.3–12 kHz (below 1 MHz; soundcard)` · `RX` · [INSPIRE project](http://theinspireproject.org/)
- `SPACE-038` **Plasmasphere density from whistler dispersion** — Fit whistler traces to infer equatorial electron density and L-shell, as automatic networks do. `1–10 kHz (below 1 MHz)` · `RX` · [AWDA system](https://doi.org/10.1029/2008ja013467) [whistler density profiles](https://doi.org/10.1016/0021-9169(88)90058-x)
- `SPACE-039` **Ground-based chorus studies** — Correlate chorus intensity with diffuse aurora and radiation-belt electron loss during substorms. `0.5–8 kHz (below 1 MHz)` · `RX` · [chorus & diffuse aurora](https://doi.org/10.1029/2009ja014852)
- `SPACE-040` **Tweek D-region reflection height** — Use the cutoff frequency of tweek atmospherics (~1.8 kHz harmonics) to measure the nighttime D-region height. `1.5–10 kHz (below 1 MHz)` · `RX` · [automatic tweek height](https://doi.org/10.1186/bf03352835) [tweek D-region params](https://doi.org/10.1016/j.asr.2020.08.025)
- `SPACE-041` **Research-grade ELF/VLF receiver** — Build an AWESOME-class, GPS-timed, broadband receiver for narrowband transmitter and broadband sferic science. `0.3–47 kHz (below 1 MHz)` · `RX` · [Cohen et al. 2010](https://doi.org/10.1109/tgrs.2009.2028334)
- `SPACE-042` **Schumann resonances** — Measure the Earth–ionosphere cavity modes (7.8, 14, 20 Hz…) with an induction coil and track global thunderstorm activity and ionospheric response. `3–50 Hz (ELF; well below 1 MHz)` · `RX` · [Q-bursts/SR/WWLLN](https://doi.org/10.5194/egusphere-egu2020-13501) [SR & gamma-ray flare](https://doi.org/10.5194/angeo-30-1321-2012)
- `SPACE-043` **Sprite / TLE detection by radio** — Detect the ELF Q-bursts and early/fast VLF perturbations that come with sprites and other transient luminous events. `ELF/VLF (below 1 MHz)` · `RX` · [sprites & VLF](https://doi.org/10.1029/1999rg900006) [early/fast VLF](https://doi.org/10.1029/2006jd007219)
- `SPACE-044` **Blitzortung lightning TOA network** — Join a GPS-timed sferic receiver network that locates lightning worldwide by time-of-arrival. `3–30 kHz (below 1 MHz)` · `RX` · [Blitzortung](https://www.blitzortung.org/) [LightningMaps](https://www.lightningmaps.org/) [WWLLN](https://wwlln.net/)
- `SPACE-045` **VHF lightning mapping** — Build a 3-D map of lightning channel development from impulsive VHF emissions received at several GPS-synced sites. `60–66 MHz` · `RX` · [Rison et al. 1999](https://doi.org/10.1029/1999gl010856)
- `SPACE-046` **Terrestrial gamma-ray flash radio signatures** — Study the VHF emission that comes with lightning-produced gamma-ray flashes. `VHF` · `RX` · [TGF VHF data](https://doi.org/10.7924/r4r49kc2m)
- `SPACE-047` **Power-line harmonic & magnetospheric line radiation** — Look for power-grid harmonics (50/60 Hz multiples) leaking into the magnetosphere and look-alike natural "line radiation". `0.5–6 kHz (below 1 MHz)` · `RX` · [MLR vs PLHR survey](https://doi.org/10.1029/2006ja012134) [PLHR over China](https://doi.org/10.1109/access.2019.2946099)
- `SPACE-048` **Ionospheric Alfvén resonator** — Detect the ULF spectral resonance structure trapped between ionosphere and magnetosphere at night. `0.5–10 Hz (ULF; induction magnetometer)` · `RX` · [IAR observations](https://doi.org/10.62955/0135-3748-2024-196)
- `SPACE-049` **Electrophonic meteors** — Record ELF/VLF during bright fireballs to test the theory that meteors radiate VLF that people "hear" at the same moment they see them. `ELF/VLF (below 1 MHz)` · `RX` · [1999 Leonids ELF/VLF](https://doi.org/10.1007/978-94-017-2071-7_39) [anomalous fireball sounds](https://doi.org/10.1111/j.1945-5100.1964.tb01419.x)
- `SPACE-050` **Natural radio noise floor survey** — Measure atmospheric, galactic and man-made noise vs frequency and time of day at your site and compare with ITU-R P.372 curves. `10 kHz–1 GHz` · `RX` · [ITU-R P.372](https://www.itu.int/rec/R-REC-P.372)

### Meteors

- `SPACE-051` **GRAVES meteor echoes** — Listen for Doppler-shifted reflections of the French GRAVES space-surveillance radar from meteor trails, aircraft and satellites. `143.050 MHz` · `RX` · [RTL-SDR meteor scatter](https://www.rtl-sdr.com/tag/meteor-scatter/)
- `SPACE-052` **BRAMS forward-scatter network & Radio Meteor Zoo** — Record a dedicated beacon's meteor echoes on a multi-station network, and classify spectrogram echoes as a citizen scientist. `49.97 MHz` · `RX` · [BRAMS](https://brams.aeronomie.be/) [Radio Meteor Zoo](https://www.zooniverse.org/projects/zooniverse/radio-meteor-zoo) [BRAMS status 2024](https://doi.org/10.5194/epsc2024-943)
- `SPACE-053` **Shower activity profiles (RMOB)** — Count hourly echoes from distant FM/TV carriers and submit them to build global meteor-shower activity curves. `50–150 MHz` · `RX` · [RMOB](https://www.rmob.org/) [IMO](https://www.imo.net/)
- `SPACE-054` **Mesospheric winds from meteor trails** — Measure the Doppler drift of specular trail echoes to retrieve 80–100 km neutral winds, tides and temperatures. `30–55 MHz` · `TX+RX` · [SKiYMET winds](https://doi.org/10.1029/2006rs003551) [SKiYMET temperatures](https://doi.org/10.5194/angeo-31-1267-2013)
- `SPACE-055` **Meteor head echoes** — Study the plasma surrounding the meteoroid itself (not the trail) for velocities and orbits, the high-power radar way. `50–450 MHz` · `TX+RX` · [head echo characterization](https://doi.org/10.1109/usnc-ursi-nrsm.2014.6928106)
- `SPACE-056` **Radio + video meteor fusion** — Combine forward-scatter echo durations with camera trajectories to estimate trail ionization profiles. `VHF` · `RX` · [video + radio meteors](https://doi.org/10.5194/egusphere-egu2020-5731)

### Radio Astronomy

- `SPACE-057` **Hydrogen line with an SDR** — Detect 21 cm emission from galactic neutral hydrogen with a horn or small dish plus an LNA and filter. `1420.406 MHz` · `RX` · [PICTOR](https://github.com/0xCoto/PICTOR) [Virgo spectrometer](https://github.com/0xCoto/Virgo)
- `SPACE-058` **Milky Way rotation curve** — Use tangent-point H I velocities along the galactic plane to derive the rotation curve and evidence for dark matter. `1420 MHz` · `RX` · [21 cm rotation curve](https://doi.org/10.1088/1742-6596/2381/1/012080) [NARIT 4.5 m](https://doi.org/10.1088/1742-6596/1380/1/012028)
- `SPACE-059` **Map the spiral arms** — Decompose multi-peaked H I profiles into distinct arms and plot a face-on map of the Galaxy. `1420 MHz` · `RX` · [Haystack SRT](https://www.haystack.mit.edu/haystack-public-outreach/srt-the-small-radio-telescope-for-education/) [gr-radio_astro](https://github.com/WVURAIL/gr-radio_astro)
- `SPACE-060` **Measure Earth's orbital velocity** — Watch the same H I cloud's Doppler velocity swing by ±30 km/s over a year as Earth orbits the Sun, a DIY local-standard-of-rest correction. `1420 MHz` · `RX` · [Virgo](https://github.com/0xCoto/Virgo)
- `SPACE-061` **Pulsar detection** — Fold radio data at a known period to detect bright pulsars (e.g. B0329+54) and measure their dispersion delay across the band. `400–1420 MHz` · `RX` · [RTL-SDR pulsar projects](https://www.rtl-sdr.com/tag/pulsar/) [CAMRAS Dwingeloo](https://www.camras.nl/en/)
- `SPACE-062` **Interstellar scintillation** — Watch pulsar intensity vary in time and frequency as the turbulent interstellar medium moves across the line of sight. `300–1500 MHz (needs large dish)` · `RX`
- `SPACE-063` **Radio JOVE Jupiter–Io storms** — Predict and record Io-controlled decametric L- and S-bursts with a dual-dipole and share them with the network. `18–30 MHz` · `RX` · [Radio JOVE](https://radiojove.gsfc.nasa.gov/) [Radio JOVE citizen science](https://doi.org/10.1553/pre8s125) [Io-D source](https://doi.org/10.1553/pre8s77)
- `SPACE-064` **Jupiter S-burst microstructure** — Capture millisecond, rapidly drifting S-bursts at full SDR bandwidth to study electron acceleration in the Io flux tube. `18–30 MHz` · `RX` · [high-res Jupiter spectra](https://doi.org/10.1038/209387a0)
- `SPACE-065` **Cassiopeia A fading** — Monitor the ~1%/yr secular decline of a young supernova remnant's radio flux against stable calibrators like Cygnus A. `150–1420 MHz` · `RX` · [Cas A secular decrease](https://doi.org/10.1134/s1063772906020077)
- `SPACE-066` **Galactic synchrotron sky as calibrator** — Fit your antenna's sidereal drift against a global sky model to absolutely calibrate gain and receiver temperature. `10–400 MHz` · `RX` · [de Oliveira-Costa 2008 GSM](https://doi.org/10.1111/j.1365-2966.2008.13376.x) [pygdsm](https://github.com/telegraphic/pygdsm) [LWA](https://lwa.unm.edu/)
- `SPACE-067` **Cosmic Dawn global 21 cm** — Explore the radiometer calibration challenge behind EDGES' contested 78 MHz absorption claim and REACH's follow-up. `50–200 MHz` · `RX` · [Bowman et al. 2018](https://www.nature.com/articles/nature25792) [REACH](https://www.astro.phy.cam.ac.uk/research/research-projects/reach)
- `SPACE-068` **Drift-scan radio transits** — Log total power as the Sun, Cygnus A, Cas A or the Galactic plane drift through a fixed beam. `20 MHz–1.4 GHz` · `RX` · [Radio-SkyPipe](https://www.radiosky.com/skypipeishere.html) [SARA](https://www.radio-astronomy.org/)
- `SPACE-069` **Fast radio burst hunting** — Search a wide beam for millisecond dispersed bursts, following the small STARE2 system that caught a Galactic magnetar FRB. `1.28–1.53 GHz` · `RX` · [Bochenek et al. 2020](https://www.nature.com/articles/s41586-020-2872-x) [CHIME/FRB](https://www.chime-frb.ca/)
- `SPACE-070` **Star–planet radio interaction** — Explore coherent low-frequency emission from red dwarfs, possibly driven by planets (Jupiter–Io writ large). `100–200 MHz` · `RX` · [Vedantham 2020](https://www.nature.com/articles/s41550-020-1011-9) [Jupiter as exoplanet](https://doi.org/10.1051/0004-6361/201832848)
- `SPACE-071` **Cosmic-ray air-shower radio flashes** — Trigger on nanosecond broadband pulses from particle cascades in the geomagnetic field. `30–80 MHz` · `RX` · [LOFAR cosmic rays](https://doi.org/10.22323/1.236.0033) [Pierre Auger](https://www.auger.org/)
- `SPACE-072` **Satellite RFI in radio astronomy** — Characterize unintended broadband emissions from Starlink-type constellations passing through your beam. `110–188 MHz` · `RX` · [Di Vruno et al. 2023](https://doi.org/10.1051/0004-6361/202346374) [Gen-2 Starlink 2024](https://doi.org/10.1051/0004-6361/202451856)
- `SPACE-073` **SETI narrowband drift search** — Search Hz-wide, Doppler-drifting carriers in high-resolution spectrograms, and test your pipeline with injected synthetic signals. `1–6 GHz (water hole 1.42–1.72 GHz)` · `RX` · [turbo_seti](https://github.com/UCBerkeleySETI/turbo_seti) [hyperseti](https://github.com/UCBerkeleySETI/hyperseti) [setigen](https://github.com/bbrzycki/setigen) [SETI League](https://www.setileague.org/)
- `SPACE-074` **Open-source radio telescope builds** — Use shared hardware/software designs and SDRangel's radio-astronomy plugin for H I, solar and calibration work. `20 MHz–6 GHz` · `RX` · [Open Source Radio Telescopes](https://www.opensourceradiotelescopes.org/) [SDRangel](https://github.com/f4exb/sdrangel)

### Moon, Planets & Deep Space

- `SPACE-075` **Earth–Moon–Earth (EME)** — Bounce signals off the Moon and measure path loss, libration Doppler spread and Faraday rotation with Q65/JT65 modes. `50 MHz–10 GHz` · `TX+RX` · [WSJT-X](https://wsjt.sourceforge.io/)
- `SPACE-076` **Lunar radar mapping** — Delay-Doppler imaging of the lunar surface, with 70 cm radar maps as the reference target for large-station experiments. `430 MHz` · `TX+RX` · [70-cm lunar radar map](https://doi.org/10.1007/bf00054324)
- `SPACE-077` **Voyager 1 carrier detection** — Receive the interstellar spacecraft's weak carrier, as the restored Dwingeloo 25 m dish did, using DSN predicts. `8.4 GHz (outside 6 GHz)` · `RX` · [CAMRAS](https://www.camras.nl/en/) [Voyager](https://science.nasa.gov/mission/voyager/)
- `SPACE-078` **Rediscovering lost space-weather spacecraft** — Survey S-band for "zombie" satellites, as an amateur did when he found NASA's magnetospheric IMAGE mission alive in 2018. `2.2–2.3 GHz` · `RX` · [Scott Tilley's blog](https://skyriddles.wordpress.com/) [NASA IMAGE](https://science.nasa.gov/mission/image/)
- `SPACE-079` **Deep-space tracking with DSN Now** — Use live DSN schedules to know which spacecraft is transmitting, then attempt S/X-band reception and Doppler (planetary missions, lunar missions). `2.2–2.3 GHz; 8.4 GHz (outside)` · `RX` · [DSN Now](https://eyes.nasa.gov/dsn/dsn.html) [AMSAT-DL Bochum](https://amsat-dl.org/)
- `SPACE-080` **Solar-wind ground-truth correlation** — Line up your ionospheric/VLF data with L1 solar-wind and STEREO beacon products to measure lag from CME arrival to local radio effects. `n/a (data fusion)` · `RX` · [SWPC real-time solar wind](https://www.swpc.noaa.gov/products/real-time-solar-wind) [STEREO beacon](https://stereo-ssc.nascom.nasa.gov/beacon/beacon_secchi.shtml)
- `SPACE-081` **Space-weather CubeSat telemetry** — Decode radiation, particle and magnetometer telemetry from science CubeSats via the SatNOGS ground-station network. `145 / 435 MHz` · `RX` · [SatNOGS Network](https://network.satnogs.org/)

## 2. Propagation, Atmosphere, Remote Sensing & RF Sensing

### Crowd-sourced propagation networks (HF)

- `PROP-001` **WSPR spot archive mining** — Query the multi-billion-row public WSPR spot database to build hour-by-hour, band-by-band path reliability and SNR climatologies between any two grid squares. `LF–6 m` · `RX` · [wspr.live](https://wspr.live/) [wspr.rocks](https://wspr.rocks/)
- `PROP-002` **Run a multi-band WSPR/FT8 skimmer** — Decode every WSPR/FT8 slot on all bands at once and keep per-spot SNR, drift and noise-floor time series for your own analysis as well as uploading them. `HF` · `RX` · [wsprdaemon](http://wsprdaemon.org/) [ka9q-radio](https://github.com/ka9q/ka9q-radio) [rtlsdr-wsprd](https://github.com/Guenael/rtlsdr-wsprd)
- `PROP-003` **Own WSPR beacon reach test** — Transmit a milliwatt WSPR beacon and map who hears you, to measure your antenna's real-world performance and the paths open from your site (amateur licence required). `LF–6 m` · `TX+RX` · [WsprryPi](https://github.com/DanAnkers/WsprryPi) [160 m WSPR antenna study](https://doi.org/10.1155/2022/4809313)
- `PROP-004` **PSKReporter live band-opening map** — Stream FT8/FT4 reception reports to watch openings form, move and collapse across continents in near real time, and archive them with your own recordings. `HF/VHF` · `RX` · [PSKReporter](https://pskreporter.info/)
- `PROP-005` **Reverse Beacon Network as an ionosonde** — Infer skip distance and effective MUF per region from CW skimmer spots of known stations, a published sounding technique. `HF` · `RX` · [RBN](https://www.reversebeacon.net/) [Frissell et al. 2014](https://doi.org/10.1002/2014sw001132)
- `PROP-006` **LSTID detection from spot data** — Detect large-scale travelling ionospheric disturbances as periodic oscillations in the skip distance of aggregated amateur spots. `HF` · `RX` · [Frissell et al. 2022](https://doi.org/10.1029/2022gl097879) [HamSCI LSTID code](https://github.com/HamSCI/hamsci_LSTID_detection)
- `PROP-007` **Pico-balloon beacons as moving probes** — Track circumnavigating WSPR/U4B pico balloons whose telemetry maps HF propagation over oceans and poles (flying one requires a licence). `HF` · `TX+RX` · [QRP Labs U4B](https://www.qrp-labs.com/u4b.html) [Traquito](https://traquito.github.io/) [Antarctic balloon propagation](https://doi.org/10.3390/atmos14071118)
- `PROP-008` **Model vs reality scoring** — Automatically score VOACAP predictions and the real-time MUF map against your measured spots and SNRs for a chosen path. `HF` · `RX` · [VOACAP](https://www.voacap.com/) [KC2G MUF map](https://prop.kc2g.com/)
- `PROP-009` **Grey-line enhancement survey** — Log SNR of distant stations along the solar terminator to measure the dawn/dusk boost when D-layer absorption fades before the F layer decays. `160–40 m` · `RX` · [wspr.live](https://wspr.live/)
- `PROP-010` **Long-path vs short-path detection** — Separate long-path arrivals by direction-finding, echo delay (~0.13 s round-the-world) or daylight geometry to learn when the "wrong way round" the planet wins. `HF` · `RX` · [KrakenSDR docs](https://github.com/krakenrf/krakensdr_docs/wiki)
- `PROP-011` **Trans-equatorial and chordal-hop identification** — Flag north–south paths with anomalously high SNR and no intermediate ground hop, tied to the equatorial ionization anomaly crests. `HF–VHF` · `RX` · [Equatorial anomaly and TEP](https://doi.org/10.1029/rs004i009p00797) [Townsville TEP](https://doi.org/10.1071/ph630171)
- `PROP-012` **Remote multi-site reception** — Record the same signal simultaneously on many public KiwiSDR/WebSDR receivers to compare path loss, fading and delay across geography. `LF–HF` · `RX` · [kiwiclient](https://github.com/jks-prv/kiwiclient) [WebSDR.org](https://websdr.org/)

### Ionospheric sounding & HF channel measurement

- `PROP-013` **Receive other people's chirp ionosondes** — Detect and time-align public linear-FM chirp sounders sweeping 2–30 MHz to produce oblique ionograms without transmitting. `2–30 MHz` · `RX` · [chirpsounder2](https://github.com/jvierine/chirpsounder2) [rtl-sdr.com ionosonde articles](https://www.rtl-sdr.com/tag/ionosonde/) [sigidwiki Ionosonde](https://www.sigidwiki.com/wiki/Ionosonde)
- `PROP-014` **Build your own coded ionosonde** — Transmit a pseudorandom-coded or chirp waveform from an SDR and receive the echoes to make vertical-incidence ionograms (licensed/authorized TX only). `2–15 MHz` · `TX+RX` · [jvierine/ionosonde](https://github.com/jvierine/ionosonde) [pynasonde](https://github.com/shibaji7/pynasonde)
- `PROP-015` **Oblique sounding between two of your own stations** — GPSDO-sync two SDRs and sound the path between them to measure group delay, propagation modes and Doppler/delay spread (licensed TX). `HF` · `TX+RX` · [SDR oblique sounding dispersion study](https://doi.org/10.1029/2018rs006636)
- `PROP-016` **NVIS link characterization** — Measure your own near-vertical signals over 0–400 km to find the critical frequency, O/X-mode splitting and best antenna height (licensed TX). `3–10 MHz` · `TX+RX` · [NVIS elevation and antenna height](https://doi.org/10.1109/map.2015.2397071) [NVIS overview](https://doi.org/10.1007/s11235-017-0287-2) [O/X-mode isolation](https://doi.org/10.1109/map.2015.2445633)
- `PROP-017` **AM broadcast Doppler for TIDs** — Track the Doppler of many medium-wave AM carriers at once to image travelling ionospheric disturbances. `530–1700 kHz` · `RX` · [Doppler-shifted AM TIDs (AMT 2025)](https://doi.org/10.5194/amt-18-1909-2025)
- `PROP-018` **HF scattering-function measurement** — Estimate the delay/Doppler spread of a channel from a known received waveform (own sounder or a broadcast pilot) to predict how an HF modem will fare. `HF` · `RX` or `TX+RX` · [SDR sounding of HF links](https://doi.org/10.1029/2018rs006636)

### Space weather & geophysical signatures

- `PROP-019` **VLF/LF phase and reflection height** — Measure VLF/LF phase with a GPS-disciplined receiver to infer D-region reflection-height changes during eclipses and flares. `VLF/LF` · `RX` · [2017 eclipse VLF phase](https://doi.org/10.1029/2019ea000839) [Great American Eclipse VLF/LF](https://doi.org/10.1002/2018gl077351)
- `PROP-020` **Loran-C skywave vs groundwave** — Separate sky-wave from ground-wave pulses of 100 kHz navigation signals to watch day/night lower-ionosphere transitions. `100 kHz` · `RX` · [Loran-C sky waves through the day](https://doi.org/10.3390/electronics13071240)
- `PROP-021` **Earthquake and tsunami ionospheric signatures** — Detect acoustic-gravity waves launched by earthquakes and tsunamis in GNSS TEC time series. `L-band` · `RX` · [Tsunami gravity waves off Hawaii](https://doi.org/10.1029/2010gl044479) [Tohoku ionospheric disturbances](https://doi.org/10.1029/2011ja016761)
- `PROP-022` **Passive radar of E-region irregularities** — Use FM broadcast as the illuminator to receive coherent echoes from auroral E-region plasma. `~100 MHz` · `RX` · [Manastash Ridge Radar](https://doi.org/10.1029/97rs02454) [First passive radar auroral echoes](https://doi.org/10.1029/1999gl900457) [ICEBEAR](https://doi.org/10.1029/2018rs006747)

### VHF/UHF anomalous propagation

- `PROP-023` **Sporadic-E opening detector** — Auto-flag Es when 6 m/2 m FT8 spots or FM/Band-I broadcasts from 1000–2000 km suddenly appear. `28–150 MHz` · `RX` · [Amateur reports as Es indicators](https://doi.org/10.3390/atmos13060906) [DXMaps](https://www.dxmaps.com/) [MMMonVHF Es](https://www.mmmonvhf.de/es.php)
- `PROP-024` **Tropo ducting from AIS/ADS-B range** — Track maximum reception distance of AIS ship and ADS-B aircraft messages as a live duct indicator. `162 / 1090 MHz` · `RX` · [AISTropoAlert](https://github.com/jeff-luszcz/AISTropoAlert) [SDR AIS anomalous propagation](https://doi.org/10.3390/jmse11061170) [Hepburn tropo forecast](https://www.dxinfocentre.com/tropo.html) [graphs1090 range plots](https://github.com/wiedehopf/graphs1090)
- `PROP-025` **Refractivity inversion from AIS power** — Invert duct height and refractivity profiles from how AIS signal strength falls off with distance. `162 MHz` · `RX` · [Refractivity from AIS power](https://doi.org/10.1515/geo-2019-0044) [AIS duct inversion comparison](https://doi.org/10.3390/rs15143577)
- `PROP-026` **Evaporation-duct over-sea links** — Log long over-water UHF/microwave signals to observe evaporation-duct over-the-horizon propagation and how rain disrupts it. `UHF–X` · `RX` · [Rain and evaporation duct, South China Sea](https://doi.org/10.3390/rs14194787)
- `PROP-027` **DAB/DTV ducting interference log** — Identify distant DAB/DVB-T transmitters by their IDs and log arrivals as a long-term tropospheric-ducting dataset. `174–700 MHz` · `RX` · [DAB ducting in the Eastern Mediterranean](https://doi.org/10.3390/electronics11244183) [ITU-R P.1546](https://www.itu.int/rec/R-REC-P.1546)
- `PROP-028` **Meteoroid trajectories from multi-station scatter** — Combine echo timing from several receivers to reconstruct meteoroid paths and speeds. `49.97 MHz` · `RX` · [BRAMS](https://brams.aeronomie.be/) [BRAMS trajectory reconstruction](https://doi.org/10.1029/2023rs007697)
- `PROP-029` **Meteor-burst link experiment** — Exchange short MSK144-style bursts between two licensed stations and measure duty cycle and throughput against time of day and showers. `50/144 MHz` · `TX+RX` · [MMMonVHF meteor scatter](https://www.mmmonvhf.de/ms.php) [HamSCI meteor-scatter](https://github.com/HamSCI/meteor-scatter)
- `PROP-030` **TEP and plasma bubbles** — Correlate evening trans-equatorial VHF openings with equatorial plasma bubbles seen as VHF satellite scintillation. `VHF` · `RX` · [EPB signatures in VHF scintillation](https://doi.org/10.1002/rds.20025)
- `PROP-031` **Propagation beacon logging** — Continuously log signal strength of fixed VHF–microwave beacons to build a multi-year tropo/Es/rain-scatter record. `50 MHz–10 GHz` · `RX` · [MMMonVHF beacons](https://www.mmmonvhf.de/)

### Satellites & GNSS: ionosphere and troposphere

- `PROP-032` **Beacon-satellite TEC** — Receive coherent dual-frequency LEO beacons and compute TEC from differential phase for ionospheric tomography. `150/400 MHz` · `RX` · [GNU Radio beacon TEC receiver](https://doi.org/10.1186/bf03353137) [Satellite ionospheric tomography](https://doi.org/10.1029/2005rs003360)
- `PROP-033` **GNSS scintillation with SDR** — Record raw L-band IF and compute S4 and phase scintillation indices to catch equatorial and polar irregularities. `L1/L2/L5` · `RX` · [SDR GNSS scintillation analysis](https://doi.org/10.1007/s10291-018-0761-7) [Low-cost scintillation monitoring](https://doi.org/10.1016/j.cja.2025.103469) [GNSS-SDR](https://gnss-sdr.org/)
- `PROP-034` **GNSS precipitable water vapour** — Estimate zenith wet delay from GNSS to get column water vapour for storm nowcasting. `L-band` · `RX` · [Low-cost GNSS water vapour](https://doi.org/10.3390/rs10091493) [Heavy rain in northern Italy](https://doi.org/10.1186/s40623-018-0795-7)
- `PROP-035` **Ground-based GNSS radio occultation** — From a mountaintop, track GNSS satellites setting below the horizon to retrieve refractivity profiles, like COSMIC from the ground. `L-band` · `RX` · [Mountain-top RO](https://doi.org/10.3390/rs12244078) [COSMIC](https://www.cosmic.ucar.edu/)
- `PROP-036` **Ku-band beacon scintillation and fade** — Sample a geostationary Ku-band beacon at high rate to measure clear-air tropospheric scintillation and rain fades. `11–12 GHz` · `RX`
- `PROP-037` **Radiosonde profiles for calibration** — Decode weather balloon radiosondes to get independent temperature/humidity profiles for refractivity and ducting predictions. `400–406 MHz` · `RX` · [radiosonde_auto_rx](https://github.com/projecthorus/radiosonde_auto_rx) [SondeHub](https://sondehub.org/)
- `PROP-038` **Winds aloft from Mode-S EHS** — Derive wind and temperature at flight level from aircraft Mode-S replies (BDS 4,4/5,0/6,0), turning an ADS-B receiver into an upper-air weather sensor. `1090 MHz` · `RX` · [pyModeS](https://github.com/junzis/pyModeS) [Mode-S receiver network for meteorology](https://doi.org/10.1175/jtech-d-15-0184.1) [MRAR validation](https://doi.org/10.1029/2012jd018315)

### GNSS reflectometry & surface sensing

- `PROP-039` **GNSS-IR snow depth** — Extract snow height from the interference oscillation in SNR versus elevation angle using one ordinary antenna. `L-band` · `RX` · [gnssrefl](https://github.com/kristinemlarson/gnssrefl) [GNSS-IR software tools](https://doi.org/10.1007/s10291-018-0744-8) [GiRsnow](https://doi.org/10.1007/s10291-021-01096-0)
- `PROP-040` **GNSS-IR soil moisture** — Track SNR phase shifts from ground-reflected GNSS to monitor near-surface soil moisture. `L-band` · `RX` · [GPS/GLONASS SNR soil moisture](https://doi.org/10.1109/jstars.2016.2537847)
- `PROP-041` **GNSS tide gauge** — Measure water level and storm surge from the reflector height seen by a coastal GNSS antenna. `L-band` · `RX` · [Coastal sea level from one GPS](https://doi.org/10.1016/j.asr.2012.04.017) [Storm surges via GNSS-IR](https://doi.org/10.1007/s10291-019-0838-y) [gnss-reflections.org](https://gnss-reflections.org/)
- `PROP-042` **Lake and sea ice thickness** — Use GNSS-IR over frozen water to estimate ice thickness. `L-band` · `RX` · [Lake ice thickness](https://doi.org/10.3390/rs12172721) [Sea ice thickness](https://doi.org/10.1109/lgrs.2022.3198189)
- `PROP-043` **Vegetation water content** — Compare GNSS SNR above and below a canopy to measure vegetation optical depth through the seasons. `L-band` · `RX` · [VOD with GPS signals](https://doi.org/10.5194/bg-20-1789-2023)
- `PROP-044` **Airborne/drone GNSS-R** — Fly an up/down dual-antenna SDR recorder and map land-surface reflectivity. `L-band` · `RX` · [GLORI airborne GNSS-R](https://doi.org/10.3390/s16050732)

### Opportunistic weather & hydrology from links

- `PROP-045` **Microwave backhaul rain gauge** — Turn received-signal-level drops on point-to-point microwave links into path-averaged rain rate using the ITU-R power law. `10–40 GHz` · `RX` · [Messer et al. 2006](https://doi.org/10.1029/2006wr005631) [Country-wide rain maps](https://doi.org/10.1073/pnas.1217961110) [pycomlink](https://github.com/pycomlink/pycomlink) [ITU-R P.838](https://www.itu.int/rec/R-REC-P.838)
- `PROP-046` **Wet-antenna attenuation study** — Separate radome wetting loss from true path rain attenuation using co-located links and rain gauges. `10–40 GHz` · `RX` · [Wet-antenna modeling](https://doi.org/10.1109/lgrs.2012.2236074) [Link length/frequency effects](https://doi.org/10.1016/j.advwatres.2008.03.004)
- `PROP-047` **Satellite TV dish rain gauge** — Log DVB-S2 SNR from a home dish to estimate rain rate along the slant path. `10.7–12.75 GHz` · `RX` · [Real-time rain rate from downlinks](https://doi.org/10.3390/s17081864) [Opportunistic rain-rate survey](https://doi.org/10.3390/s21175872) [TGRS 2024](https://doi.org/10.1109/tgrs.2023.3349100)
- `PROP-048` **LEO terminal performance vs weather** — Correlate Starlink terminal SNR and throughput with local weather to quantify Ku-band rain effects on LEO links. `Ku` · `RX` · [WetLinks dataset](https://doi.org/10.23919/tma62044.2024.10558998)
- `PROP-049` **Fog and humidity from links** — Detect fog and water vapour from subtle attenuation on high-frequency links. `38–80 GHz` · `RX` · [Cellular networks for fog monitoring](https://doi.org/10.1175/bams-d-13-00292.1) [E-band link atmospheric observations](https://doi.org/10.5194/amt-13-6559-2020)
- `PROP-050` **Own 2.4/5.8 GHz rain and foliage link** — Run a logged license-exempt ISM link across seasons to measure leaf-on/leaf-off loss and, at 5.8 GHz, heavy-rain fades (stay within ISM power rules). `2.4/5.8 GHz` · `TX+RX` · [Orchard foliage path loss](https://doi.org/10.1016/j.biosystemseng.2012.08.011) [ITU-R P.833 vegetation](https://www.itu.int/rec/R-REC-P.833)

### Passive radar

- `PROP-051` **FM passive radar for aircraft** — Cross-correlate a reference channel with a surveillance channel to plot range-Doppler maps of aircraft lit by FM broadcast. `88–108 MHz` · `RX` · [passiveRadar](https://github.com/Max-Manning/passiveRadar) [PCL performance prediction](https://doi.org/10.1049/ip-rsn:20045082)
- `PROP-052` **Real-time passive radar with KrakenSDR/blah2** — Run a phase-coherent multichannel receiver with live range-Doppler display and ADS-B truth overlay. `VHF–UHF` · `RX` · [blah2](https://github.com/30hours/blah2) [krakensdr_pr](https://github.com/mfkiwl/krakensdr_pr) [KrakenSDR](https://www.crowdsupply.com/krakenrf/krakensdr) [tar1090](https://github.com/wiedehopf/tar1090)
- `PROP-053` **DVB-T passive radar with synced RTL-SDRs** — Use OFDM digital TV as a wideband illuminator for finer range resolution. `470–700 MHz` · `RX` · [jmfriedt/passive_radar](https://github.com/jmfriedt/passive_radar) [OFDM passive radar processing](https://doi.org/10.1109/jstsp.2009.2038977)
- `PROP-054` **DAB drone detection** — Detect small UAVs using DAB multiplexes as illuminators. `174–240 MHz` · `RX` · [DAB micro-UAV detection](https://doi.org/10.1109/radar.2017.7944357) [FM/DAB/DVB-T multi-illuminator system](https://doi.org/10.1049/iet-rsn.2013.0162)
- `PROP-055` **LTE and 5G NR passive radar** — Use cellular downlinks (e.g. 5G SSB bursts) as illuminators for drone and vehicle detection. `0.7–3.8 GHz` · `RX` · [LTE passive radar drones](https://doi.org/10.1049/joe.2019.0583) [5G SSB passive radar](https://doi.org/10.1109/jstars.2023.3262291)
- `PROP-056` **Starlink/OneWeb passive radar** — Exploit LEO broadband downlinks as illuminators for target detection and even surface imaging. `Ku` · `RX` · [Starlink vs OneWeb for passive radar](https://doi.org/10.1109/radarconf2351548.2023.10149580) [Starlink surface imaging](https://doi.org/10.1109/jstars.2024.3437179)
- `PROP-057` **DVB-S2 passive imaging** — Use geostationary TV satellites as a fixed illuminator for passive SAR/ISAR imaging. `Ku` · `RX` · [Passive radar imaging with DVB-S2](https://doi.org/10.1109/radar.2017.7944264)
- `PROP-058` **GNSS-based passive radar** — Detect aircraft by coherently integrating GNSS signals they reflect. `L-band` · `RX` · [GNSS passive radar aircraft detection](https://doi.org/10.3390/rs10071164)
- `PROP-059` **Passive radar space surveillance** — Detect satellites and debris by their reflections of FM broadcast signals, as demonstrated with the MWA. `~100 MHz` · `RX` · [MWA as passive radar](https://doi.org/10.3390/rs14112571)
- `PROP-060` **Wi-Fi passive radar indoors** — Track people by the Doppler of reflected Wi-Fi access-point transmissions without joining the network. `2.4/5 GHz` · `RX` · [802.11ax multi-antenna passive radar](https://doi.org/10.1109/jsen.2021.3095675) [CSI vs passive Wi-Fi radar](https://doi.org/10.1109/twc.2021.3098526)

### Active SDR radar & sensing with your own transmitter

- `PROP-061` **SDR phased-array FMCW radar** — Use a Pluto plus 10 GHz phased-array board to do FMCW ranging, beam steering and Doppler (within amateur allocation or authorized). `10–10.5 GHz` · `TX+RX` · [CN0566 phaser](https://wiki.analog.com/resources/eval/user-guides/circuits-from-the-lab/cn0566) [PySDR phaser chapter](https://pysdr.org/content/phaser.html) [pyadi-iio](https://github.com/analogdevicesinc/pyadi-iio)
- `PROP-062` **Contactless vital signs radar** — Extract breathing and heartbeat from millimetre chest motion in CW/FMCW radar phase. `2.4–24 GHz` · `TX+RX` · [Healthcare radar survey](https://doi.org/10.1109/comst.2023.3334269)
- `PROP-063` **Through-wall motion tracking** — Track people behind walls from their radio reflections using a wideband transmitter. `2.4–6 GHz` · `TX+RX` · [WiTrack](https://witrack.csail.mit.edu/) [3D tracking via body reflections](https://www.usenix.org/conference/nsdi14/technical-sessions/presentation/adib) [See through walls with WiFi](https://doi.org/10.1145/2486001.2486039)
- `PROP-064` **Drone vs bird micro-Doppler** — Classify targets by the micro-Doppler signatures of rotors and wingbeats. `X/K band` · `TX+RX` · [Drones vs birds micro-Doppler CNN](https://doi.org/10.1049/iet-rsn.2019.0493)

### Wi-Fi & RSS sensing

- `PROP-065` **CSI breathing and heart rate** — Extract respiration from Wi-Fi channel state information phase on commodity chips. `2.4/5 GHz` · `TX+RX` · [PhaseBeat](https://doi.org/10.1109/icdcs.2017.206) [CSI vital-sign monitoring](https://doi.org/10.1145/3377165) [ESP32-CSI-Tool](https://github.com/StevenMHernandez/ESP32-CSI-Tool) [esp-csi](https://github.com/espressif/esp-csi)
- `PROP-066` **Gait and activity recognition** — Identify people and activities from CSI time series. `2.4/5 GHz` · `TX+RX` · [Gait recognition with WiFi](https://doi.org/10.1145/2971648.2971670) [Linux 802.11n CSI Tool](https://github.com/dhalperi/linux-80211n-csitool)
- `PROP-067` **IEEE 802.11bf sensing** — Experiment with the standardized WLAN sensing procedures now built into Wi-Fi. `sub-7 GHz/60 GHz` · `TX+RX` · [802.11bf overview](https://doi.org/10.1109/comst.2024.3408899)
- `PROP-068` **Radio tomographic imaging** — Image people moving inside a mesh of low-power nodes from RSS drops on each link. `2.4 GHz` · `TX+RX` · [Wilson & Patwari 2010](https://doi.org/10.1109/tmc.2009.174) [npatwari/rti](https://github.com/npatwari/rti)
- `PROP-069` **RF crowd counting** — Estimate crowd density from RSS fluctuation statistics across a sensor network. `2.4 GHz` · `TX+RX` · [RF crowd density](https://doi.org/10.1109/jsen.2013.2259692) [Wi-CaL](https://doi.org/10.1109/access.2022.3155812)

### Observing professional radars & remote-sensing transmitters

- `PROP-070` **CODAR ocean current radar** — Receive the HF sweeps of coastal SeaSonde radars and compare with published surface-current maps. `4–45 MHz` · `RX` · [sigidwiki CODAR](https://www.sigidwiki.com/wiki/CODAR) [US HFRNet](https://hfradar.ioos.us/) [Monterey Bay CODAR currents](https://doi.org/10.5670/oceanog.1997.21)
- `PROP-071` **Weather radar pulses + NEXRAD cross-reference** — Receive S/C-band weather radar pulses and align them with archived Level II volume scans. `2.7–5.6 GHz` · `RX` · [NEXRAD on AWS](https://registry.opendata.aws/noaa-nexrad/) [rtl-sdr.com weather radar](https://www.rtl-sdr.com/tag/weather-radar/)
- `PROP-072` **Biology in weather radar data** — Quantify bird migration heights and densities from operational weather radar echoes. `S/C band` · `RX` · [Bird migration flight altitudes](https://doi.org/10.1098/rsif.2010.0116)
- `PROP-073` **Wind profiler reception** — Receive UHF/VHF wind-profiler radars and cross-check against their published wind and RASS temperature products. `449/915 MHz` · `RX` · [NOAA PSL observing sites](https://psl.noaa.gov/data/obs/sites/) [RASS temperature sounding](https://doi.org/10.1109/36.45742)

### Signals of opportunity: positioning & timing

- `PROP-074` **Starlink Doppler positioning** — Estimate position from Doppler of Starlink downlink tones with known ephemerides. `10.7–12.7 GHz` · `RX` · [Starlink downlink tones for positioning](https://doi.org/10.3390/s23063234) [Differential Doppler with Starlink](https://doi.org/10.33012/2023.18669) [LEO PNT survey](https://doi.org/10.1109/access.2022.3194050)
- `PROP-075` **LTE navigation** — Track LTE synchronization and reference signals as pseudoranges for GNSS-denied positioning. `0.7–2.7 GHz` · `RX` · [Vehicular tracking with LTE](https://doi.org/10.1109/tvt.2016.2589463) [srsRAN](https://github.com/srsran/srsRAN_4G)
- `PROP-076` **eLoran/LF timing** — Recover UTC from 100 kHz Loran-type pulses and measure the weather-dependent ground-wave delay. `100 kHz` · `RX` · [eLoran UTC service](https://doi.org/10.1002/navi.197) [eLoran timing with meteorological data](https://doi.org/10.1109/access.2025.3581615)
- `PROP-077` **Time-station propagation delay** — Measure diurnal arrival-time and phase variation of WWVB/WWV against GPS to see path-length changes. `60 kHz/HF` · `RX` · [WWVB (NIST)](https://doi.org/10.6028/jres.119.004) [NIST WWV](https://www.nist.gov/pml/time-and-frequency-division/time-distribution/radio-station-wwv)

### Link budgets, terrain, antennas & ground

- `PROP-078` **LoRa terrain link-budget mapping** — Drive or fly a LoRa node and map RSSI/SNR against terrain profiles to validate propagation models (within ISM rules). `433/868/915 MHz` · `TX+RX` · [TTN Mapper](https://ttnmapper.org/) [LoRa propagation model review](https://doi.org/10.3390/s24123877) [LoRa models in the Amazon](https://doi.org/10.3390/s24051621)
- `PROP-079` **Drone-borne antenna pattern measurement** — Fly a calibrated source around a fixed antenna to measure its in-situ radiation pattern (TX must be authorized). `VHF–UHF` · `TX+RX` · [UAV antenna pattern measurement](https://doi.org/10.1109/ursigass.2014.6929024)
- `PROP-080` **Antenna patterns from satellite passes** — Log SNR of GNSS and weather/amateur satellites against azimuth/elevation to build an empirical gain map of your antenna. `VHF–L-band` · `RX` · [SatNOGS Network](https://network.satnogs.org/) [gpredict](https://github.com/csete/gpredict)
- `PROP-081` **Ground conductivity from groundwave field strength** — Measure MW/LF broadcast field strength against distance and fit ground constants. `150–1700 kHz` · `RX` · [ITU-R P.527 ground electrical characteristics](https://www.itu.int/rec/R-REC-P.527)
- `PROP-082` **Broadcast coverage survey vs ITU model** — Log FM/DTV field strength while driving and compare with ITU-R P.1546 predictions to find terrain and clutter effects. `30–3000 MHz` · `RX` · [ITU-R P.1546](https://www.itu.int/rec/R-REC-P.1546) [Terrestrial TV propagation survey](https://doi.org/10.1109/access.2021.3061034)
- `PROP-083` **Radio refractivity from signal delay** — Infer atmospheric refractivity (ITU-R P.453) from phase/delay variations on a fixed link or radar clutter returns. `UHF–X` · `RX` or `TX+RX` · [ITU-R P.453](https://www.itu.int/rec/R-REC-P.453) [Refractivity from radar sea clutter](https://doi.org/10.1029/2005rs003423)

## 3. Spectrum Situational Awareness, Interference & Anomalies

### GNSS Jamming, Spoofing & Navigation Integrity

- `AWARE-001` **ADS-B NACp jamming heatmap** — Aggregate the navigation-accuracy (NACp/NIC) fields aircraft broadcast to build a daily hex map of where GPS is degraded, the same way gpsjam.org does. `1090 MHz` · `RX` · [ref](https://gpsjam.org/faq) [ref2](https://github.com/wiedehopf/readsb)
- `AWARE-002` **Local GNSS C/N0 and AGC watchdog** — Log per-satellite carrier-to-noise and front-end AGC from a software GNSS receiver so a sudden, uniform drop across all satellites gets flagged as likely jamming rather than blockage. `L1 1575.42 MHz / L5 1176.45 MHz` · `RX` · [ref](https://gnss-sdr.org/)
- `AWARE-003` **Spoofing tell-tales detector** — Flag GNSS solutions with impossible jumps, every satellite at identical power, or clock/time inconsistencies, then cross-check against the conflict-zone spoofing patterns documented in aviation reports. `GNSS L-band` · `RX` · [ref](https://ops.group/blog/gps-spoofing-final-report/) [ref2](https://ad.easa.europa.eu/ad/2022-02R3)
- `AWARE-004` **State-actor spoofing event correlation** — Match local GNSS anomalies against documented patterns such as spoofing near VIP movements and protected facilities. `GNSS L-band` · `RX` · [ref](https://c4ads.org/reports/above-us-only-stars/)
- `AWARE-005` **"Personal privacy device" hunter** — Detect short, moving, wideband L1 swept-tone bursts from in-vehicle jammers passing highways or airports, and log time, strength and bearing. `1575 MHz` · `RX`
- `AWARE-006` **GNSS jamming timeline vs. geopolitical events** — Line up the daily jamming index (Baltic, Black Sea, Eastern Mediterranean) with news and exercise timelines so users can see why their receiver or phone lost its fix. `GNSS L-band` · `RX` · [ref](https://gpsjam.org/faq)
- `AWARE-007` **Maritime "crop circle" AIS/GNSS spoofing detection** — Look for vessels whose AIS tracks trace circles or pile onto one fake location, a known sign of GNSS spoofing near ports. `161.975/162.025 MHz` · `RX` · [ref](https://globalfishingwatch.org/) [ref2](https://www.aishub.net/)

### Aviation & Maritime Integrity

- `AWARE-008` **ADS-B ghost aircraft detection** — Flag "aircraft" whose positions disagree with multilateration timing, signal strength vs. range, or physically possible kinematics. `1090 MHz` · `RX` · [ref](https://opensky-network.org/) [ref2](https://github.com/wiedehopf/readsb)
- `AWARE-009` **ADS-B RSSI-vs-distance plausibility check** — Model the expected received power for each claimed position and flag outliers that are too strong for their claimed range, which suggests a nearby emitter. `1090 MHz` · `RX`
- `AWARE-010` **Mode S/ADS-B channel congestion (FRUIT) monitor** — Measure the rate of 1090 MHz replies and garbled squitters over time, a metric for how saturated the channel is near busy airspace. `1030/1090 MHz` · `RX`
- `AWARE-011` **Radar altimeter band neighbor monitoring** — Survey the edges of 4.2–4.4 GHz and the adjacent C-band 5G carriers near airports to show how the two bands sit next to each other. `3.7–4.4 GHz` · `RX` · [ref](https://www.faa.gov/5g)
- `AWARE-012` **AIS identity anomaly detection** — Detect MMSI collisions, vessels that "teleport", AIS gaps ("going dark") and invalid MMSIs by comparing live decodes against a vessel history baseline. `161.975/162.025 MHz` · `RX` · [ref](https://globalfishingwatch.org/)

### Cellular Integrity & Fake Base Stations

- `AWARE-013` **IMSI-catcher heuristics on a hotspot** — Run EFF's Rayhunter-style analysis of the control plane for downgrade-to-2G and identity-request anomalies. `LTE bands` · `RX` · [ref](https://github.com/EFForg/rayhunter)
- `AWARE-014` **SDR fake-eNodeB hunter** — Survey LTE broadcast messages (MIB/SIB), cell IDs and TACs with an SDR, and flag cells that appear briefly, have odd parameters, or are missing from tower databases. `LTE bands` · `RX` · [ref](https://github.com/EFForg/crocodilehunter)
- `AWARE-015` **City-wide wardriving cell anomaly map** — Mount cellular scanners in vehicles to baseline every base station and spot temporary or out-of-place transmitters, as SeaGlass did. `GSM/UMTS/LTE` · `RX` · [ref](https://seaglass.cs.washington.edu/)
- `AWARE-016` **GSM broadcast channel inventory** — Decode GSM BCCH system information to catalogue remaining 2G cells, their neighbor lists, and suspicious cells that only offer 2G. `900/1800 MHz` · `RX` · [ref](https://github.com/ptrkrysik/gr-gsm)
- `AWARE-017` **Tower inventory cross-reference** — Compare locally decoded cell IDs against crowdsourced tower databases to label known towers and highlight unknown ones. `cellular` · `RX` · [ref](https://opencellid.org/) [ref2](https://www.cellmapper.net/) [ref3](https://beacondb.net/)
- `AWARE-018` **LTE control-channel load as cell congestion index** — Count active RNTIs per subframe from PDCCH decoding to estimate how busy a cell is during events, without touching any content. `LTE bands` · `RX` · [ref](https://github.com/falkenber9/falcon) [ref2](https://github.com/SysSec-KAIST/LTESniffer)
- `AWARE-019` **5G NR cell discovery and load estimation** — Blind-decode NR PDCCH to discover gNB configuration and RNTI activity for 5G deployment mapping. `FR1 n77/n78 etc.` · `RX` · [ref](https://github.com/spritelab/5GSniffer)

### Drones, Trackers & Short-Range Wireless Threats

- `AWARE-020` **ASTM F3411 Remote ID receiver** — Passively list nearby drones and their operator locations from Bluetooth, Wi-Fi NAN and Wi-Fi beacon Remote ID broadcasts. `2.4 GHz` · `RX` · [ref](https://github.com/opendroneid/receiver-android)
- `AWARE-021` **DJI DroneID decoding** — Demodulate DJI's proprietary DroneID frames (OcuSync) to recover drone position, home point and serial number. `2.4/5.8 GHz` · `RX` · [ref](https://github.com/RUB-SysSec/DroneSecurity) [ref2](https://www.ndss-symposium.org/ndss-paper/drone-security-and-the-mysterious-case-of-djis-droneid/)
- `AWARE-022` **ML drone RF classifier** — Train on recorded drone control and video links to detect and classify UAVs that don't broadcast Remote ID. `2.4/5.8 GHz` · `RX` · [ref](https://data.mendeley.com/datasets/f4c2b4n755/1)
- `AWARE-023` **Wi-Fi deauth/disassoc flood detection** — Watch management frames for bursts of deauthentication, the typical sign of a Wi-Fi denial-of-service or handshake-capture attempt. `2.4/5 GHz` · `RX` · [ref](https://github.com/SpacehuhnTech/DeauthDetector) [ref2](https://www.kismetwireless.net/docs/readme/alerts/alerts/)
- `AWARE-024` **Evil-twin / rogue AP alerts** — Flag known SSIDs suddenly advertised from unexpected MACs or with weaker encryption. `2.4/5/6 GHz` · `RX` · [ref](https://www.kismetwireless.net/docs/readme/alerts/alerts/) [ref2](https://github.com/kismetwireless/kismet)
- `AWARE-025` **Following-tracker detection** — Spot a Bluetooth LE tracker (AirTag, Tile, SmartTag) that stays with you across locations over time. `2.4 GHz BLE` · `RX` · [ref](https://github.com/seemoo-lab/AirGuard) [ref2](https://datatracker.ietf.org/doc/draft-detecting-unwanted-location-trackers/)
- `AWARE-026` **BLE advertising storm monitor** — Track BLE advertisement rates and random-address churn to spot spam floods (fake pairing pop-ups) or crowds forming. `2.4 GHz BLE` · `RX` · [ref](https://github.com/greatscottgadgets/ubertooth)
- `AWARE-027` **ISM car-key jammer detection** — Alert when the 315/433/868 MHz noise floor rises in a sustained, wideband way near parked cars, the signature of a remote-key-blocking attack. `315/433.92/868 MHz` · `RX` · [ref](https://github.com/merbanan/rtl_433)
- `AWARE-028` **Wi-Fi + BLE + drone combined sweep** — Use one headless agent to fuse Wi-Fi, Bluetooth and SDR spectrum sweeps into a single site-survey timeline. `2.4/5 GHz` · `RX` · [ref](https://github.com/ghostop14/sparrow-wifi)

### RFI Hunting & Noise-Floor Forensics

- `AWARE-029` **Power-line arcing locator** — Recognize the 120/100 Hz-modulated broadband buzz of arcing utility hardware and walk it down with a directional antenna and rising-frequency tracking. `HF–UHF` · `RX` · [ref](https://www.arrl.org/power-line-noise)
- `AWARE-030` **Switching-supply / LED / inverter RFI signatures** — Build a library of comb-spectrum signatures (solar inverters, EV chargers, LED drivers, VDSL) so a new noise pattern can be matched to its likely source. `LF–VHF` · `RX` · [ref](https://rsgb.org/main/technical/emc/)
- `AWARE-031` **Long-term noise-floor trend logger** — Run rtl_power-style sweeps continuously and render waterfall heatmaps over months so gradual degradation (new neighbor device, failing insulator) shows up. `24 MHz–1.7 GHz` · `RX` · [ref](https://github.com/keenerd/rtl-sdr-misc)
- `AWARE-032` **Lightning sferics explainer** — Correlate sudden HF/LF crashes with a live lightning-strike network so the tool can say "that noise is a storm 300 km away". `VLF–HF` · `RX` · [ref](https://www.blitzortung.org/)
- `AWARE-033` **Radio-quiet-zone style site survey** — Use radio astronomy RFI surveys as a model for documenting and ranking every emitter visible from a sensitive site. `wideband` · `RX` · [ref](https://greenbankobservatory.org/about/national-radio-quiet-zone/)
- `AWARE-034` **Wi-Fi DFS radar event logging** — Log 5 GHz radar pulses (weather or military) that force access points to change channel, which explains "my Wi-Fi randomly dropped". `5250–5725 MHz` · `RX` · [ref](https://en.wikipedia.org/wiki/Dynamic_frequency_selection)
- `AWARE-035` **Smart-meter mesh as noise contributor** — Quantify how much of the 900 MHz ISM band is filled by utility meter bursts and how that changes by time of day. `902–928 MHz` · `RX` · [ref](https://github.com/bemasher/rtlamr)
- `AWARE-036` **Unknown burst reverse-engineering triage** — Capture unexplained ISM bursts, auto-detect their modulation and bit timing, and add them to the local "known emitters" catalogue. `ISM bands` · `RX` · [ref](https://github.com/jopohl/urh) [ref2](https://github.com/merbanan/rtl_433)

### Crowdsourced Monitoring, Occupancy & Anomaly Detection

- `AWARE-037` **Electrosense-style distributed PSD network** — Stream compressed spectrum sweeps from many low-cost nodes to a backend for global occupancy maps and API queries. `20 MHz–6 GHz` · `RX` · [ref](https://github.com/electrosense) [ref2](https://github.com/electrosense/api-examples)
- `AWARE-038` **Standards-based sensor node** — Expose a sensor through the NTIA SCOS (IEEE 802.15.22.3) API so scheduled acquisitions and metadata work with government and academic monitoring efforts. `wideband` · `RX` · [ref](https://github.com/NTIA/scos-sensor)
- `AWARE-039` **Academic spectrum observatory participation** — Contribute measurements to, or reuse data from, NSF SpectrumX and NASCTN federal spectrum-sharing measurement campaigns. `wideband` · `RX` · [ref](https://www.spectrumx.org/) [ref2](https://www.nist.gov/ctl/nasctn)
- `AWARE-040` **Unsupervised spectrum anomaly detector** — Train an autoencoder on "normal" waterfalls and alert on reconstruction error when something new appears. `any` · `RX` · [ref](https://github.com/electrosense/saife) [ref2](https://arxiv.org/abs/1611.00301)
- `AWARE-041` **PSD-based technology classifier** — Label occupied channels by technology (LTE, DVB-T, GSM, FM) straight from power spectral density, so occupancy statistics are broken down by service. `wideband` · `RX` · [ref](https://github.com/electrosense/PSD-technology-classification-framework)
- `AWARE-042` **Duty-cycle and occupancy statistics** — Compute per-channel percentage occupancy, burst length distributions and hour-of-week profiles to publish "how used is this band, really" reports. `any` · `RX`
- `AWARE-043` **Multi-sensor time-coincident anomaly correlation** — Flag events that hit many geographically spread sensors at the same moment (space weather, GNSS jamming, satellite interference), as opposed to local-only events (a neighbor's device). `any` · `RX`
- `AWARE-044` **"Why did my spectrum change?" event feed** — Join local anomalies with external feeds (space weather scales, lightning, radiosonde launches, satellite passes, HAARP campaigns) to automatically attach a likely explanation. `any` · `RX` · [ref](https://www.spaceweather.gov/noaa-scales-explanation) [ref2](https://sondehub.org/)
- `AWARE-045` **CBRS/shared-band incumbent activity sensing** — Watch the 3.5 GHz band for pulsed radar incumbents versus LTE/NR GAA carriers to study how spectrum sharing behaves in practice. `3550–3700 MHz` · `RX` · [ref](https://en.wikipedia.org/wiki/Citizens_Broadband_Radio_Service)
- `AWARE-046` **TV white space availability check** — Measure which UHF TV channels are really empty at a location and compare against database-claimed availability, including wireless-mic use. `470–608 MHz` · `RX` · [ref](https://en.wikipedia.org/wiki/White_spaces_(radio))

### Emitter Identification, DF & Geolocation

- `AWARE-047` **RF fingerprinting of same-model transmitters** — Tell individual radios apart by hardware imperfections (IQ imbalance, CFO, transients) with CNNs to detect cloned or impersonating devices. `any` · `RX` · [ref](https://arxiv.org/abs/1812.01124)
- `AWARE-048` **Receiver-agnostic Wi-Fi fingerprinting** — Test whether transmitter fingerprints still hold across different receivers and channels using a large public Wi-Fi capture dataset. `2.4 GHz` · `RX` · [ref](https://arxiv.org/abs/2112.15363)
- `AWARE-049` **Coherent 5-channel direction finding** — Take bearings on interferers, pirates or stuck transmitters with a phase-coherent array and plot intersecting lines from mobile or fixed stations. `24–1766 MHz` · `RX` · [ref](https://github.com/krakenrf/krakensdr_doa)
- `AWARE-050` **HF TDoA via public GPS-timed receivers** — Geolocate unknown HF/VLF emitters (OTH radar, military, intruders) by multilaterating GPS-timestamped IQ from several KiwiSDRs. `10 kHz–30 MHz` · `RX` · [ref](https://github.com/hcab14/TDoA)
- `AWARE-051` **Oscillator-offset fingerprint for low-cost sensors** — Calibrate RTL-SDR frequency error against LTE sync signals so that measured offsets can be used to tell emitters apart. `LTE bands` · `RX` · [ref](https://github.com/electrosense/pyltess-track)
- `AWARE-052` **Pirate / unlicensed broadcaster hunting** — Flag FM carriers not in the licence database, take bearings, and log them in an FCC-enforcement-style record. `88–108 MHz` · `RX` · [ref](https://www.fcc.gov/enforcement/areas/pirate-radio)
- `AWARE-053` **Allocation lookup for unknown signals** — Cross-reference a detected frequency against national/regional allocation tables so users learn which service is supposed to be there. `any` · `RX`

### HF, Military & Intruder Monitoring

- `AWARE-054` **Amateur-band intruder logging** — Record non-amateur signals (OTH radar, military data modems, broadcast spurs) in ham bands and format reports for the IARU Monitoring System newsletter. `HF amateur bands` · `RX` · [ref](https://www.iaru-r1.org/about-us/committees-and-working-groups/iarums/)
- `AWARE-055` **Over-the-horizon radar signature catalogue** — Identify FMCW/pulsed OTH radar sweeps by repetition rate and bandwidth, and log how often they occur and on which frequencies. `3–30 MHz` · `RX` · [ref](https://en.wikipedia.org/wiki/Over-the-horizon_radar)
- `AWARE-056` **Woodpecker history replay** — Compare modern OTH radar captures to the historic Duga "Russian Woodpecker" 10 Hz tapping signature to teach how wide HF radars disrupt the band. `7–19 MHz` · `RX` · [ref](https://en.wikipedia.org/wiki/Duga_radar)
- `AWARE-057` **HFGCS/EAM activity counter** — Count Emergency Action Message traffic volume (metadata only) on HFGCS frequencies as a public indicator of military readiness posture. `4724/8992/11175 kHz` · `RX` · [ref](https://en.wikipedia.org/wiki/High_Frequency_Global_Communications_System)
- `AWARE-058` **Conflict-zone EW spectrum observation** — Use public remote receivers near conflict areas to document jamming of HF/VHF/GNSS and new military emitters over time. `HF–L-band` · `RX` · [ref](https://gpsjam.org/faq)

### Space Weather, Propagation & Natural Explainers

- `AWARE-059` **Beacon-network propagation baselines** — Use WSPR/FT8 spot databases as a global sensor net to separate "my station broke" from "the band collapsed". `HF/6 m` · `RX` · [ref](https://www.wsprnet.org/) [ref2](https://pskreporter.info/) [ref3](https://hamsci.org/)
- `AWARE-060` **Sporadic-E / tropo "why am I hearing distant stations"** — Explain sudden far-away FM/TV/ADS-B reception using Es season patterns and tropospheric ducting forecasts. `30–450 MHz` · `RX` · [ref](https://en.wikipedia.org/wiki/Sporadic_E_propagation) [ref2](https://www.dxinfocentre.com/tropo.html)
- `AWARE-061` **Time-signal outage explainer** — Monitor 60 kHz WWVB carrier strength and phase to catch station maintenance and day/night propagation shifts behind radio-clock failures. `60 kHz` · `RX` · [ref](https://www.nist.gov/pml/time-and-frequency-division/time-distribution/radio-station-wwvb)
- `AWARE-062` **Radiosonde launch correlation** — Explain regular 400–406 MHz bursts at 00Z/12Z by matching them to tracked weather balloon launches. `400–406 MHz` · `RX` · [ref](https://sondehub.org/)

### Satellite & Space-Segment Interference

- `AWARE-063` **Starlink Ku downlink tracking** — Detect Starlink Ku-band beams with an LNB-fed SDR and use the sync sequences to identify which satellite passed. `10.7–12.7 GHz` · `RX` · [ref](https://arxiv.org/abs/2210.11578)
- `AWARE-064` **Satellite pass vs. noise-floor attribution** — Tag noise-floor bumps at a ground station with the satellite passes that match in time and frequency. `VHF/UHF/L/S` · `RX` · [ref](https://network.satnogs.org/) [ref2](https://celestrak.org/)
- `AWARE-065` **Crowdsourced satellite reception health** — Compare decode success across many ground stations for the same pass to tell satellite faults apart from local interference. `VHF/UHF` · `RX` · [ref](https://network.satnogs.org/)

### Infrastructure Change Tracking & Activity Proxies

- `AWARE-066` **Legacy network sunset tracker** — Record the disappearance of 2G/3G carriers and refarming to LTE/NR, building a timeline of network retirements. `cellular bands` · `RX` · [ref](https://www.fcc.gov/consumers/guides/plan-ahead-phase-out-3g-cellular-networks-and-service) [ref2](https://github.com/ptrkrysik/gr-gsm)
- `AWARE-067` **Public-safety trunking load index (metadata only)** — Count control-channel grants per talkgroup category as a live incident-activity indicator, without recording audio. `VHF/UHF/700/800 MHz` · `RX` · [ref](https://github.com/TrunkRecorder/trunk-recorder)
- `AWARE-068` **Disaster outage correlation** — Line up the loss of cell carriers and broadcast stations during storms with official outage reports. `cellular/broadcast` · `RX` · [ref](https://www.fcc.gov/disaster-information-reporting-system-dirs)
- `AWARE-069` **Meshtastic/LoRa mesh growth map** — Passively count LoRa mesh nodes and packet rates to map community mesh growth and event-driven traffic spikes. `868/915 MHz` · `RX` · [ref](https://meshmap.net/) [ref2](https://meshtastic.org/) [ref3](https://ttnmapper.org/)
- `AWARE-070` **IoT sensor population census** — Count unique ISM device IDs (weather stations, TPMS, meters) per hour as an urban activity index and to catch new beacon storms. `315/433/868/915 MHz` · `RX` · [ref](https://github.com/merbanan/rtl_433)

## 4. Signals of Infrastructure, Transport, Satellites & Nature (the Long Tail)

### Aviation Data Links & Aircraft-as-Sensors

- `SIGNAL-001` **ADS-B / Mode S (baseline)** — The well-known one: aircraft positions, IDs and velocities; the foundation for everything below. `1090 MHz` · `RTL-SDR` · [readsb](https://github.com/wiedehopf/readsb)
- `SIGNAL-002` **UAT 978 FIS-B / TIS-B** — Receive free US ground-uplinked weather radar mosaics, METARs, TFRs and NOTAMs, plus ATC-rebroadcast traffic. `978 MHz` · `RTL-SDR` · [dump978](https://github.com/flightaware/dump978) [Stratux](https://github.com/cyoung/stratux)
- `SIGNAL-003` **VHF ACARS** — Airline operational messages: gate times, fuel, engine reports, weather requests. `129–137 MHz AM` · `RTL-SDR` · [acarsdec](https://github.com/TLeconte/acarsdec)
- `SIGNAL-004` **VDL Mode 2** — Higher-rate successor to ACARS carrying X.25, MIAM file transfers and reassembled ACARS. `136.650–136.975 MHz` · `RTL-SDR` · [dumpvdl2](https://github.com/szpajder/dumpvdl2)
- `SIGNAL-005` **CPDLC / ADS-C inside ACARS** — Read controller–pilot text clearances and oceanic ADS-C position contracts, plus 737 MAX OHMA diagnostics, carried in datalink payloads. `VHF/HF/L-band` · `any datalink RX` · [libacars](https://github.com/szpajder/libacars)
- `SIGNAL-006` **HFDL** — Track aircraft over oceans far beyond line-of-sight; ground-station squitters double as a live propagation map. `2.9–22 MHz` · `HF SDR (Airspy HF+, RSP)` · [dumphfdl](https://github.com/szpajder/dumphfdl)
- `SIGNAL-007` **Inmarsat Aero (satcom ACARS)** — Decode aircraft datalink relayed via geostationary satellites, including long-haul flights nowhere near you. `~1545 MHz L-band` · `RTL-SDR + L-band patch/LNA` · [JAERO](https://github.com/jontio/JAERO)
- `SIGNAL-008` **All-datalink aggregation** — Fuse ACARS/VDL2/HFDL/Inmarsat/Iridium into one searchable message history with airframe enrichment. `multi-band` · `multiple RTL-SDRs` · [ACARS Hub](https://github.com/sdr-enthusiasts/docker-acarshub) [airframes.io](https://airframes.io)
- `SIGNAL-009` **SELCAL** — Detect the dual-tone pairs that ring a specific aircraft's HF radio and map codes to airframes. `HF aero bands` · `HF SDR` · [wiki](https://en.wikipedia.org/wiki/SELCAL)
- `SIGNAL-010` **VOLMET** — Automated HF voice weather (METAR/TAF/SIGMET) on fixed schedules; a good test case for speech-to-text. `HF aero bands` · `HF SDR` · [wiki](https://en.wikipedia.org/wiki/VOLMET)
- `SIGNAL-011` **VOR radial decoding** — Compare the 30 Hz reference and variable phases to compute your own bearing from a VOR and check it against your known location. `108–117.95 MHz` · `RTL-SDR` · [sigidwiki](https://www.sigidwiki.com/wiki/VHF_Omnidirectional_Range_(VOR))
- `SIGNAL-012` **ILS localizer/glideslope DDM** — Measure the 90/150 Hz modulation depth difference to see where you sit relative to an approach centerline. `108.1–111.95 / 329–335 MHz` · `RTL-SDR`
- `SIGNAL-013` **NDB DXing** — Log low-frequency Morse beacons from airports and offshore platforms hundreds of km away at night. `190–1750 kHz` · `HF SDR + LF antenna` · [sigidwiki](https://www.sigidwiki.com/wiki/Non-Directional_Beacon_(NDB))
- `SIGNAL-014` **ELT 121.5 / Cospas-Sarsat 406 beacons** — Watch for distress beacons and decode 406 MHz hex IDs (country, beacon type); test transmissions happen too. `121.5 / 406 MHz` · `RTL-SDR` · [Cospas-Sarsat](https://www.cospas-sarsat.int)

### Maritime, GMDSS & Legacy Navigation

- `SIGNAL-015` **AIS (baseline)** — Ship positions and voyage data; the maritime equivalent of ADS-B. `161.975 / 162.025 MHz` · `RTL-SDR` · [AIS-catcher](https://github.com/jvde-github/AIS-catcher)
- `SIGNAL-016` **NAVTEX / SITOR-B** — Read coastal navigation warnings, gale warnings and SAR notices sent as FEC teletype. `518 / 490 kHz, 4209.5 kHz` · `HF SDR` · [sigidwiki](https://www.sigidwiki.com/wiki/NAVTEX)
- `SIGNAL-017` **Marine DSC** — Decode Digital Selective Calling distress alerts and ship-to-ship calls with MMSI and position fields. `156.525 MHz (Ch 70), 2187.5 kHz` · `RTL-SDR/HF SDR`
- `SIGNAL-018` **HF radiofax** — Receive surface analysis, wave height and ice charts drawn line by line from NOAA and other coastal stations. `HF 2–26 MHz` · `HF SDR` · [NOAA schedules](https://www.weather.gov/marine/radiofax_charts)
- `SIGNAL-019` **Inmarsat-C EGC / SafetyNET** — Pick up maritime safety broadcasts, NAVAREA warnings and ship position reports from geostationary satellites. `1537–1541 MHz` · `RTL-SDR + L-band patch` · [sigidwiki](https://www.sigidwiki.com/wiki/Inmarsat-C_TDM) [inmarsatc](https://github.com/cropinghigh/inmarsatc)
- `SIGNAL-020` **China BPL** — Receive China's Loran-C-format longwave time service and measure pulse timing. `100 kHz` · `LF RX + big antenna` · [wiki](https://en.wikipedia.org/wiki/BPL_(time_service))
- `SIGNAL-021` **DGPS beacons (history)** — Survey the LF band where US Coast Guard DGPS corrections used to run (shut down 2022) and log the stations still active abroad. `285–325 kHz` · `HF SDR` · [wiki](https://en.wikipedia.org/wiki/Differential_GPS)
- `SIGNAL-022` **Alpha / RSDN-20** — Record Russia's VLF hyperbolic navigation chirp triplets, one of the last Cold War navigation systems still transmitting. `11.9–14.9 kHz` · `soundcard + VLF antenna` · [sigidwiki](https://www.sigidwiki.com/wiki/Alpha)

### Satellites: Comms, Weather & Navigation

- `SIGNAL-023` **Iridium bursts & ring alerts** — Demodulate Iridium frames; IRA ring-alert channels expose satellite and beam positions and paging activity (metadata). `1616–1626.5 MHz` · `SDR + L-band patch/LNA` · [gr-iridium](https://github.com/muccc/gr-iridium) [iridium-toolkit](https://github.com/muccc/iridium-toolkit)
- `SIGNAL-024` **Orbcomm** — Decode LEO messaging satellite packets, including their onboard ephemeris, and correct for Doppler. `137–138 MHz` · `RTL-SDR + QFH` · [ORBCOMM-receiver](https://github.com/fbieberly/ORBCOMM-receiver)
- `SIGNAL-025` **GOES HRIT/EMWIN** — Pull full-disk imagery and NWS text products/EMWIN bulletins from a geostationary satellite 24/7. `1694.1 MHz` · `RTL-SDR + grid dish + LNA` · [goestools](https://pietern.github.io/goestools/)
- `SIGNAL-026` **GK-2A LRIT/HRIT** — Receive Korean geostationary weather images across Asia-Pacific. `1692.14 MHz` · `RTL-SDR + dish` · [xrit-rx](https://github.com/sam210723/xrit-rx)
- `SIGNAL-027` **Meteor-M LRPT** — Demodulate QPSK to produce multispectral images from Russian polar weather satellites with a simple antenna. `137.1 / 137.9 MHz` · `RTL-SDR + QFH/V-dipole` · [meteor_demod](https://github.com/dbdexter-dev/meteor_demod)
- `SIGNAL-028` **Metop / FengYun AHRPT & HRPT** — Track polar orbiters with a small dish to get full-resolution imagery. `~1.7 GHz` · `SDR + tracked dish` · [SatDump](https://github.com/SatDump/SatDump)
- `SIGNAL-029` **NOAA POES end-of-life survey** — NOAA-19 was shut down on 2025-08-13, leaving the 137 MHz APT band nearly empty; use old recordings as reference and watch what (if anything) replaces them. `137 MHz` · `RTL-SDR` · [wiki](https://en.wikipedia.org/wiki/NOAA-19)
- `SIGNAL-030` **GPS L1 C/A + SBAS/WAAS raw processing** — Acquire and track GNSS satellites yourself, compute a fix, and decode SBAS integrity and correction messages. `1575.42 MHz` · `SDR + active GNSS antenna` · [GNSS-SDR](https://gnss-sdr.org)
- `SIGNAL-031` **Galileo OSNMA authentication** — Cryptographically verify that Galileo navigation data is genuine (Initial Service live since July 2025). `1575.42 MHz (E1)` · `GNSS RX / SDR` · [GSC OSNMA](https://www.gsc-europa.eu/galileo/services/galileo-open-service-navigation-message-authentication-osnma) [OSNMAlib](https://github.com/Algafix/OSNMA)
- `SIGNAL-032` **GNSS constellation forensics** — Archive raw navigation messages, compare broadcast ephemerides with precise orbits, and flag clock jumps or spoofing. `L-band` · `u-blox / SDR` · [galmon](https://github.com/berthubert/galmon)
- `SIGNAL-033` **SatNOGS ground station** — Schedule automated passes and contribute or look up observations of hundreds of satellites. `VHF/UHF/S` · `RTL-SDR + rotator` · [SatNOGS Network](https://network.satnogs.org)
- `SIGNAL-034` **Cubesat telemetry** — Decode AX.25/CCSDS beacons from amateur and university satellites (battery voltages, temperatures, attitude). `145 / 435 MHz` · `RTL-SDR + Yagi` · [gr-satellites](https://github.com/daniestevez/gr-satellites)
- `SIGNAL-035` **Doppler-based orbit identification** — Fit a Doppler curve to an unknown carrier to identify the object or refine its orbit, then check it against the TLE catalogue. `any` · `any SDR` · [Riddles in the Sky](https://skyriddles.wordpress.com/2023/07/27/understand-doppler-analysis/) [CelesTrak](https://celestrak.org)
- `SIGNAL-036` **ISS SSTV / APRS digipeater** — Receive commemorative SSTV images and packet traffic relayed through the ISS. `145.800 / 145.825 MHz` · `RTL-SDR` · [ARISS](https://www.ariss.org)
- `SIGNAL-037` **QO-100 geostationary amateur transponder** — Watch always-on amateur narrowband and DATV traffic and the beacons on a geostationary satellite. `10489–10499 MHz` · `LNB + SDR` · [BATC wideband monitor](https://eshail.batc.org.uk/wb/)
- `SIGNAL-038` **Nancy Grace Roman Space Telescope** — Decode S-band telemetry from NASA's newest observatory, launched in 2026. `S-band` · `dish + SDR` · [Daniel Estévez](https://destevez.net/2026/09/decoding-the-roman-space-telescope/)
- `SIGNAL-039` **Tianwen-1 / Tianwen-2 deep-space telemetry** — Decode Chinese Mars-orbiter and asteroid-mission frames, including safe-mode low-rate modes. `X-band` · `large dish + SDR` · [Tianwen-2](https://destevez.net/2026/05/decoding-tianwen-2/) [Tianwen-1 safe mode](https://destevez.net/2026/08/tianwen-1-safe-mode-telemetry/)
- `SIGNAL-040` **STEREO-A flyby reception** — Amateurs decoded solar imagery from a NASA heliophysics probe during its 2023 return close to Earth. `X-band` · `dish + SDR` · [Riddles in the Sky](https://skyriddles.wordpress.com/2023/07/03/stereo-a-comes-home/)

### Rail, Road & Fleet Infrastructure

- `SIGNAL-041` **Railroad EOT telemetry** — Decode end-of-train units (brake-pipe pressure, motion, marker light, unit ID), and note the head-of-train commands on the paired channel. `457.9375 / 452.9375 MHz` · `RTL-SDR` · [PyEOT](https://github.com/ereuter/PyEOT)
- `SIGNAL-042` **Railroad ATCS** — Watch the Advanced Train Control System packets that report signal and switch states and track occupancy. `896–901 / 935–940 MHz` · `RTL-SDR`
- `SIGNAL-043` **PTC 220 MHz** — Characterize the Positive Train Control data radio network that most US freight and commuter railroads share. `220–222 MHz` · `RTL-SDR` · [wiki](https://en.wikipedia.org/wiki/Positive_train_control)
- `SIGNAL-044` **MPT1327 trunked fleets** — Decode control-channel FFSK to see talkgroup and channel assignments on taxi, transit and utility networks (metadata). `VHF/UHF` · `RTL-SDR` · [sigidwiki](https://www.sigidwiki.com/wiki/MPT1327)
- `SIGNAL-045` **Highway advisory radio / TIS** — Find the low-power AM stations that broadcast road-work, park and airport information; map them as you travel. `530, 1610–1700 kHz` · `HF SDR` · [wiki](https://en.wikipedia.org/wiki/Highway_advisory_radio)
- `SIGNAL-046` **TPMS** — Decode tire sensor IDs, pressure and temperature; study how passing cars can be re-identified from their tire sensor IDs. `315 / 433.92 MHz` · `RTL-SDR` · [rtl_433](https://github.com/merbanan/rtl_433) [jboone/tpms](https://github.com/jboone/tpms)
- `SIGNAL-047` **Keyless entry fingerprinting** — Characterize key-fob modulation, bit timing and rolling-code structure without replaying anything. `315 / 433.92 MHz` · `RTL-SDR/HackRF` · [rtl_433](https://github.com/merbanan/rtl_433)
- `SIGNAL-048` **Cellular broadcast metadata & calibration** — Scan LTE/GSM cell IDs and use base-station carriers to measure your SDR's frequency error (ppm). `LTE/GSM bands` · `RTL-SDR` · [kalibrate-rtl](https://github.com/steve-m/kalibrate-rtl) [LTE-Cell-Scanner](https://github.com/Evrytania/LTE-Cell-Scanner)

### Utilities, Industrial Telemetry & IoT

- `SIGNAL-049` **ERT smart meters (SCM/IDM)** — Read your own gas/electric meter's consumption and 5-minute interval data to profile household load. `902–928 MHz` · `RTL-SDR` · [rtlamr](https://github.com/bemasher/rtlamr)
- `SIGNAL-050` **Neptune R900 water meters** — Decode water meter readings including the leak and backflow flags. `902–928 MHz` · `RTL-SDR` · [rtlamr](https://github.com/bemasher/rtlamr)
- `SIGNAL-051` **Wireless M-Bus (EU meters)** — Receive heat, water and gas meter telegrams (decrypt only with your own keys) and publish them to MQTT. `868 MHz` · `RTL-SDR` · [rtl-wmbus](https://github.com/xaelsouth/rtl-wmbus) [wmbusmeters](https://github.com/weetmuts/wmbusmeters)
- `SIGNAL-052` **rtl_433 long tail** — Automatically decode 385+ ISM devices: weather stations, pool thermometers, soil sensors, doorbells, energy clamps. `315/345/433/868/915 MHz` · `RTL-SDR` · [rtl_433](https://github.com/merbanan/rtl_433)
- `SIGNAL-053` **LoRa / LoRaWAN** — Demodulate chirp spread spectrum packets and read the LoRaWAN headers (DevAddr, frame counters) to map nearby IoT deployments. `433/868/915 MHz` · `SDR` · [gr-lora_sdr](https://github.com/tapparelj/gr-lora_sdr) [TTN](https://www.thethingsnetwork.org)
- `SIGNAL-054` **Zigbee / Thread (802.15.4)** — Sniff home and building automation frames (PAN IDs, device addresses) into Wireshark. `2.4 GHz` · `HackRF/USRP` · [gr-ieee802-15-4](https://github.com/bastibl/gr-ieee802-15-4)
- `SIGNAL-055` **Z-Wave** — Demodulate G.9959 home-automation frames to inventory home IDs and node types. `908.42 / 868.42 MHz` · `RTL-SDR` · [waving-z](https://github.com/baol/waving-z)
- `SIGNAL-056` **DECT cordless phones & baby monitors (metadata)** — Detect DECT base stations, their RFPI identities and channel usage; flag base stations that transmit unencrypted. `1880–1900 / 1920–1930 MHz` · `HackRF/USRP` · [gr-dect2](https://github.com/pavelyazev/gr-dect2)
- `SIGNAL-057` **ALERT flood-warning gauges** — Real-time rain and river gauges report through radio networks run by local hydrologic agencies; characterize and time-stamp bursts during storms. `VHF` · `RTL-SDR` · [ALERT Users Group](https://www.alertsystems.org)
- `SIGNAL-058` **Meteor-burst telemetry (SNOTEL history)** — The USDA sent mountain snowpack data by bouncing radio off meteor trails for 40+ years (ended 2023); a great story about the band. `30–50 MHz` · `n/a (history)` · [wiki](https://en.wikipedia.org/wiki/Meteor_burst_communications)
- `SIGNAL-059` **Grid frequency (ENF) from mains hum** — Track the tiny 50/60 Hz drift from power lines or audio and match it against grid logs to timestamp or authenticate recordings. `50/60 Hz` · `soundcard / loop` · [wiki](https://en.wikipedia.org/wiki/Electrical_network_frequency_analysis)

### Time, Frequency & Ionospheric Sounding

- `SIGNAL-060` **DCF77** — Decode Germany's longwave BCD time code and measure propagation delay. `77.5 kHz` · `soundcard/SDR + ferrite` · [wiki](https://en.wikipedia.org/wiki/DCF77)
- `SIGNAL-061` **WWVB (and MSF/JJY)** — Decode the phase- and amplitude-modulated US time code; same pipeline for MSF (60 kHz) and JJY (40/60 kHz). `60 kHz` · `LF RX + loop` · [NIST](https://www.nist.gov/pml/time-and-frequency-division/time-distribution/radio-station-wwvb)

### Broadcast Metadata & Public Alerting

- `SIGNAL-062` **RDS/RBDS & TMC** — Extract station names, radio text, clock time and TMC traffic codes as JSON from every FM station you can hear. `87.5–108 MHz` · `RTL-SDR` · [redsea](https://github.com/windytan/redsea)
- `SIGNAL-063` **HD Radio (NRSC-5) side channels** — Pull station logos, album art, weather radar images and traffic maps from HD Radio data services. `FM band` · `RTL-SDR` · [nrsc5](https://github.com/theori-io/nrsc5)
- `SIGNAL-064` **DAB+ slideshow & DLS** — Collect ensemble lists, service labels, dynamic text and MOT slideshow images. `174–240 MHz` · `RTL-SDR` · [welle.io](https://github.com/AlbrechtL/welle.io)
- `SIGNAL-065` **DRM digital shortwave** — Decode digital HF broadcasts including their text messages and Journaline pages. `531 kHz–26.06 MHz` · `HF SDR` · [Dream](https://sourceforge.net/projects/drm/) [sigidwiki](https://www.sigidwiki.com/wiki/Digital_Radio_Mondiale_(DRM))
- `SIGNAL-066` **ATSC 3.0 bootstrap & wake-up bits** — Detect NextGen TV bootstrap symbols and the emergency alert wake-up flag that can turn on a receiver. `470–608 MHz` · `wideband SDR` · [wiki](https://en.wikipedia.org/wiki/ATSC_3.0)
- `SIGNAL-067` **NOAA Weather Radio SAME/EAS** — Decode alert headers (event code, FIPS areas, expiration) and trigger local automations. `162.400–162.550 MHz` · `RTL-SDR` · [dsame](https://github.com/cuppa-joe/dsame) [multimon-ng](https://github.com/EliasOenal/multimon-ng)
- `SIGNAL-068` **FM/TV DX cross-reference** — Match unknown carriers against the FMLIST transmitter database and a tropospheric ducting forecast to explain surprise reception. `VHF/UHF` · `RTL-SDR` · [FMLIST](https://www.fmlist.org) [Hepburn tropo](https://www.dxinfocentre.com/tropo.html)

### HF Utility, Military & Amateur Long Tail

- `SIGNAL-069` **STANAG 4285 modems** — Classify NATO single-tone PSK data signals and measure their baud rate and interleaver modes. `1.89–22.7 MHz` · `HF SDR` · [sigidwiki](https://www.sigidwiki.com/wiki/STANAG_4285)
- `SIGNAL-070` **2G ALE (MIL-STD-188-141)** — Decode automatic link establishment soundings to see which government, military and amateur stations are calling each other. `3–30 MHz` · `HF SDR` · [wiki](https://en.wikipedia.org/wiki/Automatic_link_establishment)
- `SIGNAL-071` **Numbers stations & UVB-76** — Log schedules and formats of spy/diplomatic shortwave stations against the Priyom database, and watch "The Buzzer" for its rare voice messages. `HF, 4625 kHz` · `HF SDR` · [Priyom](https://priyom.org) [UVB-76](https://en.wikipedia.org/wiki/UVB-76)
- `SIGNAL-072` **NCDXF/IARU beacon chain** — Time the 18 world beacons rotating through five bands to see live, per-continent propagation. `14.100–28.200 MHz` · `HF SDR` · [NCDXF](https://www.ncdxf.org/beacon/)
- `SIGNAL-073` **JS8 keyboard mesh** — Decode weak-signal JS8 heartbeats and relay paths to map store-and-forward HF networks. `HF` · `HF SDR` · [JS8Call](https://js8call.com)
- `SIGNAL-074` **Radiosondes (baseline)** — The well-known one: weather balloon PTU and GPS telemetry, with recovery chasing. `400–406 MHz` · `RTL-SDR` · [auto_rx](https://github.com/projecthorus/radiosonde_auto_rx) [SondeHub](https://sondehub.org)

### Nature, Space Weather & Wildlife

- `SIGNAL-075` **Motus wildlife telemetry** — Detect coded nanotags on migrating birds, bats and insects and cross-reference tag IDs with the Motus network. `166.380 MHz (Americas), 150.1 MHz (EU), 434 MHz` · `RTL-SDR + Yagi` · [Motus](https://motus.org)
- `SIGNAL-076` **VHF collar / radio-tracking** — Automatically detect and log pulses from wildlife tracking tags, and direction-find them with multiple antennas. `~150 MHz` · `RTL-SDR` · [pyradiotracking](https://github.com/Nature40/pyradiotracking)
- `SIGNAL-077` **Argos & ICARUS animal tags** — Characterize satellite-uplink tags on animals and buoys (Argos platform transmitter terminals, ICARUS tags). `~401.65 MHz` · `RTL-SDR` · [Argos](https://www.argos-system.org) [ICARUS](https://www.icarus.mpg.de)
- `SIGNAL-078` **Avalanche beacons** — Receive 457 kHz transceiver pulses and experiment with field-strength direction finding. `457 kHz` · `HF SDR + ferrite loop` · [wiki](https://en.wikipedia.org/wiki/Avalanche_transceiver)
- `SIGNAL-079` **Hearing-aid induction loops** — Pick up the audio-frequency magnetic field of telecoil loops in venues with a coil into a soundcard, and map loop coverage. `audio-frequency magnetic` · `coil + soundcard` · [wiki](https://en.wikipedia.org/wiki/Audio_induction_loop)

### Trunked Radio Systems (proposed 2026-09-13; accepted by the user 2026-09-13)

Added during architecture planning: docs/04 §8 identifies trunk following as the most-requested scanner capability, yet the original catalogue had almost none. They were proposed during planning and **accepted by the user on 2026-09-13** (`status: accepted` in `use-cases.yaml`). Wideband capture of a system's span lets one receiver decode the control channel and every simultaneous voice channel (docs/04 §8.2).

- `SIGNAL-080` **P25 Phase 1 trunk following** — Decode the control channel continuously, follow group voice grants onto channelized voice channels, and log every simultaneous call with talkgroup, radio ID, site and timestamps. `150–174 / 450–470 / 700 / 806–869 MHz` · `RX` · [Trunk Recorder](https://github.com/TrunkRecorder/trunk-recorder)
- `SIGNAL-081` **P25 Phase 2 TDMA trunk following** — Follow a system with a Phase 1 FDMA control channel and 2-slot H-DQPSK TDMA voice, demodulating both slots per granted channel. `700 / 800 MHz` · `RX`
- `SIGNAL-082` **DMR Tier III / Capacity Plus trunk following** — Track trunked DMR (ETSI Tier III, Motorola Capacity Plus/Connect Plus) via control or rest-channel signalling with automatic LCN-to-frequency mapping. `VHF / UHF / 800 MHz` · `RX`
- `SIGNAL-083` **Motorola SmartNet/SmartZone (Type II) following** — Decode the 3600 bps control channel and follow analog-FM or P25 voice grants, a common legacy public-safety configuration. `806–869 MHz` · `RX`
- `SIGNAL-084` **NXDN Type-C / EDACS control-channel decode** — Decode NXDN Type-C or EDACS control channels and follow grants, covering the remaining common LMR trunking families. `VHF / UHF` · `RX`
- `SIGNAL-085` **Automatic control-channel discovery** — Find a control channel with no prior research: flag continuous 100%-duty 4FSK/C4FM on the LMR raster, confirm by frame sync, and propose "this looks like P25 Phase 1, follow it?". `VHF / UHF / 700 / 800 MHz` · `RX`
- `SIGNAL-086` **Encryption-aware call logging (metadata only)** — Read P25 ALGID/Key ID and DMR privacy indicators to label and skip encrypted calls, recording only metadata, never decrypting others' traffic. `VHF / UHF / 700 / 800 MHz` · `RX`
- `SIGNAL-087` **Blind end-to-end auto-decode of a local 800 MHz trunked control channel** — The whole chain on one real, strong, local emitter with nothing told to the system: detect the continuously-keyed control channel in a wideband capture, estimate modulation and symbol rate blind, auto-select the demod+decode chain, and recover control messages (NAC, system/site identity, grants) — the decode is what confirms the emitter. BART's above-ground simulcast is the reference instance; the claim is the blind chain, not that one system. Added 2026-09-20 at the user's request as MAUTO's forcing function. `851–854 MHz` · `RX` · [docs/18](19-bart-800mhz-trunked.md) [RadioReference SID 12049](https://www.radioreference.com/db/sid/12049)

## 5. Unknown Signals, Security Research, Lab Engineering & ML

### Blind protocol & signal reverse engineering

- `RESEARCH-001` **Blind ISM device RE with URH** — demodulate an unknown 315/433/868/915 MHz remote, auto-detect modulation, and reconstruct its packet fields. `315/433/868/915 MHz` · `RX` · [URH/WOOT'18](https://github.com/jopohl/urh)
- `RESEARCH-002` **rtl_433 flex decoder** — write a `-X` flex spec to decode a never-before-seen OOK/FSK sensor from its timing parameters. `433/868/915 MHz` · `RX` · [docs](https://triq.org/rtl_433/OPERATION.html)
- `RESEARCH-003` **Bit-level dissection in inspectrum** — use symbol-rate and period cursors on a spectrogram to read out preamble, sync word and payload of a mystery burst. `any` · `RX` · [inspectrum](https://github.com/miek/inspectrum)
- `RESEARCH-004` **Full RF RE workflow with FISSURE** — chain detection, classification, protocol discovery and packet crafting on an unknown emitter. `any` · `TX+RX` · [FISSURE](https://github.com/ainfosec/FISSURE)
- `RESEARCH-005` **Blind signal detection with gr-inspector** — energy-detect and auto-extract concurrent signals, then blind-estimate OFDM subcarrier spacing/CP for an unknown wideband carrier. `any` · `RX` · [gr-inspector](https://github.com/gnuradio/gr-inspector)
- `RESEARCH-006` **Protocol inference with Netzob** — infer message formats and state machines from a corpus of captured frames of a proprietary link. `any` · `RX` · [Netzob](https://github.com/netzob/netzob)
- `RESEARCH-007` **Catalog unknowns against Sig ID Wiki** — match an unidentified waterfall/audio signature against 590+ documented signals (and 400+ still-unidentified). `VLF–UHF` · `RX` · [Sig ID Wiki](https://www.sigidwiki.com/)

### Line coding, checksums, FEC & scrambler identification

- `RESEARCH-008` **Identify line coding** — recognize Manchester vs NRZ vs PWM/PPM from edge-timing statistics in a decoded bitstream. `any` · `RX`
- `RESEARCH-009` **CRC reverse engineering (differential technique)** — recover polynomial/init/xorout from message–CRC pairs even with non-standard parameters. `n/a` · `RX` · [Ewing essay](https://www.csse.canterbury.ac.nz/greg.ewing/essays/CRC-Reverse-Engineering.html) [CRC Beagle](https://github.com/colinoflynn/crcbeagle)
- `RESEARCH-010` **CRC RevEng** — brute-force/search 100+ CRC models to match observed checksums in an unknown frame format. `n/a` · `RX` · [CRC RevEng](https://reveng.sourceforge.io/)
- `RESEARCH-011` **delsum checksum toolbox** — reverse CRC/Fletcher/modsum/polyhash parameters, including over partial byte ranges. `n/a` · `RX` · [delsum](https://github.com/8051Enthusiast/delsum)
- `RESEARCH-012` **Whitening/scrambler identification** — detect a data-whitening LFSR (e.g. PN9) by looking for the fixed sequence XORed across otherwise-random payloads. `any` · `RX`
- `RESEARCH-013` **FEC identification** — spot convolutional/Reed–Solomon/LDPC structure and recover code parameters from soft symbols of your own link. `any` · `RX`

### Satellite & aerospace signal research

- `RESEARCH-014` **Unencrypted GEO satellite links ("Don't Look Up")** — finding that huge volumes of sensitive backhaul/voice traffic ride GEO satellites in the clear, readable with a ~few-hundred-dollar dish+SDR. `Ku/C-band` · `RX` · [UCSD SATCOM](https://satcom.sysnet.ucsd.edu/)
- `RESEARCH-015` **Whispers Among the Stars** — Pavur's demonstration that VSAT/broadband satellite feeds can be eavesdropped with consumer TV equipment. `Ku-band` · `RX` · [Black Hat 2020 PDF](https://i.blackhat.com/USA-20/Wednesday/us-20-Pavur-Whispers-Among-The-Stars-Perpetrating-And-Preventing-Satellite-Eavesdropping-Attacks.pdf)
- `RESEARCH-016` **Iridium transmitter fingerprinting (SatIQ)** — authenticate satellite transmissions via hardware impairments to detect spoofing/replay. `1.6 GHz` · `RX` · [arXiv:2305.06947](https://arxiv.org/abs/2305.06947)
- `RESEARCH-017` **DVB-S2 receiver from scratch** — implement PL sync, LDPC/BCH FEC and recover MPEG-TS from a satellite IQ capture. `Ku-band` · `RX` · [gr-dvbs2rx](https://github.com/igorauad/gr-dvbs2rx)
- `RESEARCH-018` **ADS-B protocol security survey** — understand why the unauthenticated ADS-B aviation protocol is spoofable and what defenses exist. `1090 MHz` · `RX` · [arXiv:1307.3664](https://arxiv.org/abs/1307.3664)

### Cellular security & privacy research (own testbed)

- `RESEARCH-019` **TETRA:BURST** — cryptanalysis of the TETRA radio standard revealing a deliberately weakened TEA1 cipher and other flaws. `380–430 MHz` · `RX` · [Midnight Blue](https://www.midnightblue.nl/research/tetraburst)
- `RESEARCH-020` **Touching the Untouchables (LTEFuzz)** — dynamic fuzzing of the LTE control plane finding dozens of implementation/spec issues. `LTE` · `TX+RX` · [S&P 2019 PDF](https://syssec.kaist.ac.kr/pub/2019/kim_sp_2019.pdf)
- `RESEARCH-021` **aLTEr / Breaking LTE on Layer Two** — AES-CTR without integrity lets an active attacker do DNS redirection; passive website fingerprinting also shown. `LTE` · `TX+RX` · [Radix/aLTEr](https://www.montsecure.com/research/alter-attack/)
- `RESEARCH-022` **ReVoLTE** — keystream reuse across VoLTE calls allowed decrypting a prior encrypted call. `LTE` · `RX` · [ReVoLTE](https://www.montsecure.com/research/revolte-attack/)
- `RESEARCH-023` **IMP4GT** — missing user-plane integrity plus OS reflection enables 4G user/network impersonation. `LTE` · `TX+RX` · [IMP4GT](https://www.montsecure.com/research/imp4gt-attacks/)
- `RESEARCH-024` **5GSniffer PDCCH leakage** — blind-decode 5G control channel to reveal RNTIs and traffic patterns of privacy messengers. `5G NR (FDD)` · `RX` · [5GSniffer / S&P 2023](https://github.com/spritelab/5GSniffer)
- `RESEARCH-025` **LTESniffer control-channel analysis** — passively decode PDCCH/PDSCH/PUSCH to study identity/capability leakage (no decryption). `LTE` · `RX` · [LTESniffer / WiSec'23](https://github.com/SysSec-KAIST/LTESniffer)
- `RESEARCH-026` **Own 5G/LTE testbed** — stand up a private RAN+core to fuzz and instrument your own handsets legally. `LTE/5G NR` · `TX+RX` · [srsRAN](https://github.com/srsran/srsRAN_Project) [OpenAirInterface](https://openairinterface.org/)

### Wireless device security research (own devices)

- `RESEARCH-027` **RollJam-style keyfob capture-and-replay** — jam+record+replay defeating fixed and some rolling codes on your own vehicle remote. `315/433 MHz` · `TX+RX` · [samy.pl DEF CON 23](https://sa.my/defcon2015/)
- `RESEARCH-028` **RollBack** — time-agnostic replay that triggers keyfob resynchronization to reuse old rolling codes. `315/433 MHz` · `TX+RX` · [RollBack (arXiv)](https://arxiv.org/abs/2210.11923)
- `RESEARCH-029` **BLE tracking despite MAC randomization** — identifying/fingerprinting devices using advertising-payload tokens and side channels. `2.4 GHz` · `RX` · [PETS 2019 PDF](https://petsymposium.org/2019/files/papers/issue3/popets-2019-0036.pdf)
- `RESEARCH-030` **Wi-Fi MAC randomization failures** — probe-request fields and control-frame flaws still allow tracking most phones. `2.4/5 GHz` · `RX` · [arXiv:1703.02874](https://arxiv.org/abs/1703.02874)
- `RESEARCH-031` **KNOB (Bluetooth key entropy)** — the spec allows negotiating 1-byte encryption keys, brute-forceable in real time. `2.4 GHz` · `RX` · [KNOB](https://knobattack.com/)
- `RESEARCH-032` **BIAS (Bluetooth impersonation)** — authentication flaws let a device impersonate a previously paired peer. `2.4 GHz` · `TX+RX` · [BIAS](https://francozappa.github.io/about-bias/)
- `RESEARCH-033` **SweynTooth / BrakTooth BLE fuzzing** — link-layer fuzzing crashing/deadlocking many BLE and Bluetooth SoCs. `2.4 GHz` · `TX+RX` · [SweynTooth](https://asset-group.github.io/disclosures/sweyntooth/)
- `RESEARCH-034` **5Ghoul 5G modem fuzzing** — implementation flaws in commercial 5G basebands found via down-link fuzzing on a testbed. `5G NR` · `TX+RX` · [5Ghoul](https://asset-group.github.io/disclosures/5ghoul/)
- `RESEARCH-035` **BLE sniffing with Sniffle** — follow connections across all BT5 PHYs to study your own device's link-layer behavior. `2.4 GHz` · `RX` · [Sniffle](https://github.com/nccgroup/Sniffle)
- `RESEARCH-036` **ZigBee/802.15.4 security (KillerBee)** — capture, replay and analyze your own ZigBee network's frames and keys. `2.4 GHz` · `TX+RX` · [KillerBee](https://github.com/riverloopsec/killerbee)
- `RESEARCH-037` **IoT Goes Nuclear (ZigBee worm)** — Touchlink proximity bypass + firmware-key side channel enabling a smart-bulb chain reaction. `2.4 GHz` · `TX+RX` · [ePrint 2016/1047](https://eprint.iacr.org/2016/1047)
- `RESEARCH-038` **MouseJack** — unencrypted 2.4 GHz HID dongles accept injected keystrokes from up to ~100 m. `2.4 GHz` · `TX+RX` · [Bastille MouseJack](http://bastille.net/research/vulnerabilities/mousejack)

### TEMPEST, EM side channels & air-gap exfiltration

- `RESEARCH-039` **TempestSDR screen reconstruction** — recover a monitor's image from unintended cable emanations. `VHF/UHF` · `RX` · [TempestSDR](https://github.com/martinmarinov/TempestSDR)
- `RESEARCH-040` **Deep-TEMPEST** — deep learning dramatically improves HDMI screen recovery from EM emanations. `VHF/UHF` · `RX` · [arXiv:2407.09717](https://arxiv.org/abs/2407.09717) [gr-tempest](https://github.com/git-artes/gr-tempest)
- `RESEARCH-041` **Soft Tempest foundations** — Kuhn & Anderson on compromising emanations and how fonts/dithering leak or hide data. `VHF/UHF` · `RX` · [Soft Tempest](https://www.cl.cam.ac.uk/~mgk25/ih98-tempest.pdf)
- `RESEARCH-042` **Screaming Channels** — digital crypto activity couples into a chip's own radio and leaks keys tens of meters away. `2.4 GHz` · `RX` · [Screaming Channels](https://eurecom-s3.github.io/screaming_channels/)
- `RESEARCH-043` **Noise-SDR / arbitrary EM modulation** — unprivileged software shapes unintended emissions into real radio waveforms (LoRa/GNSS/PSK). `varies` · `RX` · [Noise-SDR / S&P 2022](https://github.com/eurecom-s3/noise-sdr)
- `RESEARCH-044` **RAMBO** — malware modulates RAM bus emissions to exfiltrate data from an air-gapped PC. `~1–2 GHz` · `RX` · [arXiv:2409.02292](https://arxiv.org/abs/2409.02292)
- `RESEARCH-045` **AirHopper** — video-card FM emanations bridge an air gap to a nearby phone's radio. `88–108 MHz` · `RX` · [arXiv:1411.0237](https://arxiv.org/abs/1411.0237)
- `RESEARCH-046` **PowerHammer** — exfiltration by modulating CPU-driven current onto power lines. `power line` · `RX` · [arXiv:1804.04014](https://arxiv.org/abs/1804.04014)
- `RESEARCH-047` **ECDSA key extraction via EM/power probe** — non-invasive side channel pulls signing keys from phones with a magnetic probe. `LF probe` · `RX` · [CCS 2016 (ePrint 2016/230)](https://eprint.iacr.org/2016/230)
- `RESEARCH-048` **Keyboard acoustic/EM emanations** — recover keystrokes from compromising emanations (classic side-channel line of work). `LF/RF` · `RX`
- `RESEARCH-049` **Side-channel/glitch bench (ChipWhisperer)** — capture power/EM traces to correlate against crypto and extract keys from your own target. `n/a` · `RX` · [ChipWhisperer](https://github.com/newaetech/chipwhisperer)

### RF test & measurement / lab engineering

- `RESEARCH-050` **SDR as spectrum analyzer / power survey** — wideband sweep to build an occupancy map or hunt spurs. `1 MHz–6 GHz` · `RX` · [rtl_power](https://kmkeen.com/rtl-power/)
- `RESEARCH-051` **Two-port VNA measurements** — measure S-parameters, return loss and insertion loss of your own filters/cables. `100 kHz–6 GHz` · `TX+RX` · [LibreVNA](https://github.com/jankae/LibreVNA)
- `RESEARCH-052` **Cable TDR / fault location** — use a VNA's time-domain mode to find length and impedance discontinuities in coax. `n/a` · `TX+RX` · [NanoVNA TDR](https://nanovna.com/)
- `RESEARCH-053` **RF network analysis in Python (scikit-rf)** — post-process Touchstone data: de-embedding, calibration, cascading. `n/a` · `n/a` · [scikit-rf](https://github.com/scikit-rf/scikit-rf)
- `RESEARCH-054` **Noise-figure by Y-factor** — measure amplifier/receiver NF with a calibrated noise source and hot/cold ratio. `RF` · `RX`
- `RESEARCH-055` **Phase-noise measurement** — characterize oscillator close-in phase noise using an SDR and reference. `RF` · `RX`
- `RESEARCH-056` **Oscillator stability / Allan deviation** — quantify a clock's drift vs a GPSDO reference and plot ADEV. `n/a` · `RX` · [allantools](https://github.com/aewallin/allantools)
- `RESEARCH-057` **EMC pre-compliance scanning** — near-field/radiated emission sweeps to catch problems before formal test. `30 MHz–1 GHz` · `RX` · [tinySA](https://tinysa.org/)
- `RESEARCH-058` **Wi-Fi CSI extraction for sensing** — pull per-subcarrier channel state to experiment with RF sensing/localization. `2.4/5 GHz` · `RX` · [nexmon_csi](https://github.com/seemoo-lab/nexmon_csi)

### SDR fuzzing, IDS & physical-layer authentication

- `RESEARCH-059` **Wi-Fi frame fuzzing on real PHY (Owfuzz)** — fuzz management/control/data frames against APs and clients with an SDR MAC. `2.4/5 GHz` · `TX+RX` · [Owfuzz](https://github.com/alipay/Owfuzz)
- `RESEARCH-060` **Full-stack SDR Wi-Fi for experiments (openwifi)** — modifiable FPGA 802.11 stack for injection, CSI and MAC research. `70 MHz–6 GHz` · `TX+RX` · [openwifi](https://github.com/open-sdr/openwifi)
- `RESEARCH-061` **Wireless IDS** — detect rogue APs, evil twins and deauth floods across Wi-Fi/BLE. `2.4/5 GHz` · `RX` · [nzyme](https://www.nzyme.org/) [Kismet](https://www.kismetwireless.net/)
- `RESEARCH-062` **RF fingerprinting / physical-layer auth (ORACLE)** — classify bit-identical radios by hardware impairments (IQ imbalance, CFO). `2.4 GHz` · `RX` · [ORACLE dataset](https://genesys-lab.org/oracle)

### Digital modes, modems & waveform exploration

- `RESEARCH-063` **Build a LoRa-like CSS modem** — study chirp spread spectrum with a full open transceiver (whitening, Hamming FEC, interleaving). `433/868/915 MHz` · `TX+RX` · [gr-lora_sdr](https://github.com/tapparelj/gr-lora_sdr)
- `RESEARCH-064` **Reverse-engineered LoRa PHY decode** — decode LoRa purely in GNU Radio to understand its physical layer. `868/915 MHz` · `RX` · [gr-lora](https://github.com/rpp0/gr-lora)
- `RESEARCH-065` **Roll your own open digital voice mode** — experiment with an end-to-end open codec+modem for HF/VHF. `HF/VHF` · `TX+RX` · [FreeDV](https://freedv.org/) [M17](https://m17project.org/)
- `RESEARCH-066` **OFDM / 5G NR waveform exploration** — generate, impair and analyze OFDM/NR link-level chains. `n/a` · `TX+RX` · [Sionna](https://github.com/NVlabs/sionna)
- `RESEARCH-067` **Foundation model for wireless channels (LWM)** — self-supervised transformer producing reusable channel embeddings. `n/a` · `n/a` · [arXiv:2411.08872](https://arxiv.org/abs/2411.08872)

### ML, datasets & education

- `RESEARCH-068` **TorchSig + Sig53** — PyTorch toolkit and large labeled dataset for RF modulation classification/detection. `n/a` · `n/a` · [TorchSig](https://github.com/TorchDSP/torchsig) [arXiv:2207.09918](https://arxiv.org/abs/2207.09918)
- `RESEARCH-069` **RadioML AMC baseline (and its critiques)** — benchmark CNN modulation recognition, then read why synthetic RadioML datasets mislead. `n/a` · `n/a` · [arXiv:1712.04578](https://arxiv.org/abs/1712.04578) [cyclostationary critique](https://cyclostationary.blog/2017/01/31/machine-learning-and-modulation-recognition-comments-on-convolutional-radio-modulation-recognition-networks-by-t-oshea-j-corgan-and-t-clancy/)
- `RESEARCH-070` **Build labeled RF datasets from your own captures** — record, annotate and package your own IQ so models train on real, not synthetic, signals. `any` · `RX` · [DeepSig datasets](https://www.deepsig.ai/datasets)
- `RESEARCH-071` **HF modulation dataset (Panoradio)** — 18-class labeled HF dataset for signal-type classification under realistic fading. `HF` · `n/a` · [Panoradio HF](https://panoradio-sdr.de/radio-signal-classification-dataset/)
- `RESEARCH-072` **Large Wi-Fi RF-fingerprint dataset (WiSig)** — 10M packets from 174 transmitters for device-ID model benchmarking. `2.4/5 GHz` · `n/a` · [WiSig](https://cores.ee.ucla.edu/downloads/datasets/wisig/)
- `RESEARCH-073` **Open-set / unknown-signal detection** — train models that flag "none of the above" instead of forcing a known label. `any` · `RX`
- `RESEARCH-074` **AI-driven RF challenge (source separation)** — MIT competition tasks: co-channel separation, detection and geolocation on real captures. `any` · `RX` · [RF Challenge](https://rfchallenge.mit.edu/)
- `RESEARCH-075` **Reproducible IQ archives with SigMF** — standardize IQ + metadata so recordings are portable and citable. `n/a` · `n/a` · [SigMF](https://github.com/sigmf/SigMF)
- `RESEARCH-076` **Browser-based IQ exploration** — visualize, annotate and share recordings without local tooling. `n/a` · `RX` · [IQEngine](https://iqengine.org/)
- `RESEARCH-077` **Learn DSP/SDR with live signals (PySDR)** — free Python textbook covering sampling, filtering, constellations and OFDM. `n/a` · `RX` · [PySDR](https://pysdr.org/)
- `RESEARCH-078` **SDR video course with HackRF** — hands-on GNU Radio + HackRF curriculum from DSP basics to TX. `1 MHz–6 GHz` · `TX+RX` · [Great Scott Gadgets](https://greatscottgadgets.com/sdr/)
