"""Trunking control-channel scenario (T-267, C23): a continuous C4FM CC hidden among bursty NBFM.

The scene exists to separate two things a frequency-channel-occupancy (FCO) measure cannot:

- a **control channel** — continuous C4FM on the 12.5 kHz LMR raster, carrying a real frame sync
  and CRC-valid blocks;
- a **continuous data emitter** — equally continuous, equally on-raster, equally 4FSK, carrying
  neither.

Both sit at 100 % FCO on the raster, so both are *candidates*. Only the first should ever be
confirmed. That decoy is the point of the fixture: C23's named pitfall is "false CCs: continuous
data emitters pass the FCO test. Require sync plus CRC."

Bursty NBFM neighbours fill the rest of the raster so the CC has to be found among traffic
rather than in an otherwise-empty band.
"""

from __future__ import annotations

import math
from typing import Any

import numpy as np

from hkpy.synth import trunking as tk
from hkpy.synth.scenarios import Ctx, DEFAULT_START_UTC, utc_plus
from hkpy.synth.scene import Scene, complex_noise, db, undb

TRUNK_CC_DEFAULTS: dict[str, Any] = {
    "sample_rate": 500e3,
    # 800 MHz public-safety trunking (docs/04 §4: 851-869 MHz, control channels 100 % duty).
    "center_hz": 851.0125e6,
    "duration_s": 1.0,
    "raster_hz": tk.LMR_RASTER_HZ,
    # Raster channel indices, relative to center_hz.
    "cc_channel": 3,
    "decoy_channel": -5,
    "nbfm_channels": [-2, 7, 11, -9],
    "cc_snr_db": 20.0,
    "decoy_snr_db": 20.0,
    "nbfm_snr_db": 18.0,
    "symbol_rate_bd": tk.C4FM_SYMBOL_RATE_BD,
    "cc_cfo_hz": 0.0,
    "fm_deviation_hz": 2500.0,
    "audio_tone_hz": 1000.0,
    "burst_mean_on_s": 0.08,
    "burst_mean_off_s": 0.12,
    "noise_dbfs": -40.0,
    "calibration_k_db": -70.0,
    "start_utc": DEFAULT_START_UTC,
    # --- TSBK content (T-268). OFF by default, so ``trunk_control_channel`` stays exactly the
    # T-267 scene: the branch below consumes no randomness when it is off, so the recording is
    # byte-for-byte what it was. ``trunk_tsbk_control_channel`` turns it on.
    "tsbk": False,
    # The band plan the control channel announces. 851.00625 MHz base / 6.25 kHz spacing is an
    # 800 MHz public-safety plan; both encode exactly (base is a whole number of 5 Hz steps,
    # spacing a whole number of 125 Hz steps).
    "tsbk_iden": 1,
    "tsbk_base_hz": 851.00625e6,
    "tsbk_spacing_hz": 6250.0,
    "tsbk_tx_offset_hz": -45e6,
    "tsbk_bandwidth_hz": 12500.0,
    # The voice frequency a grant must resolve to. The CHANNEL NUMBER is derived from it, not the
    # other way round, so the truth is a frequency chosen first and the decoder has to arrive at
    # it through the band plan rather than through arithmetic the fixture also did.
    "grant_target_hz": 851.7375e6,
    "grant_talkgroup": 1234,
    "grant_source": 5678,
    # A grant naming an identifier the control channel NEVER announces. Resolving it through the
    # announced identifier would give a perfectly plausible 800 MHz frequency, which is exactly
    # the C23 pitfall; it must come out unmapped instead.
    "unannounced_iden": 7,
    "unannounced_channel": 300,
    # --- The voice channel a grant must be FOLLOWED onto (T-269). Chosen as a frequency first,
    # exactly like ``grant_target_hz``; its channel number is derived from the band plan above, so
    # the decoder still has to arrive at it through the IDEN_UP messages.
    #
    # 851.075 MHz is +62.5 kHz from the tuned centre and so inside the window the radio is holding,
    # while ``grant_target_hz`` at +725 kHz is outside it. One recording therefore carries BOTH
    # C23 cases: a grant that can be followed, and a grant the <=20 MHz span limit refuses.
    "follow_target_hz": 851.075e6,
    "follow_talkgroup": 2468,
    "follow_source": 1357,
    # The traffic on that channel: repeated keyings, each far shorter than the window a hunt
    # buffers, separated by more silence than any silence timeout shorter than a P25 voice frame.
    # on 0.10 + off 0.15 = a 0.25 s period, so ANY 0.5 s window spans two keyings and contains at
    # least one complete one with its closing silence -- which is what lets a follower measure a
    # start AND an end rather than a fragment. At 40 % duty the channel never reaches control-
    # channel candidacy (MIN_CC_FCO is 0.95), so it cannot be mistaken for a second CC.
    "follow_on_s": 0.10,
    "follow_off_s": 0.15,
    "follow_snr_db": 18.0,
    # --- Encryption (T-270). OFF by default, so the T-267/T-268/T-269 fixtures stay byte-identical:
    # the branches guarded by it emit no extra TSBK and consume no randomness when it is off.
    "encryption": False,
    # A granted voice channel whose GRANT carries the service-options encryption bit. Chosen as a
    # frequency first, like every other target here, so its channel number is derived from the band
    # plan rather than the other way round. 851.125 MHz is +112.5 kHz from the tuned centre -- well
    # inside the window the radio holds -- and lands on raster channel 9, clear of the control
    # channel (3), the decoy (-5), the followed channel (5) and every NBFM neighbour.
    "encrypted_target_hz": 851.125e6,
    "encrypted_talkgroup": 3690,
    "encrypted_source": 2580,
    # A granted voice channel announced ONLY by a grant update: late entry, joined with no header.
    # A real grant update carries no service-options octet at all, so nothing ever states this
    # channel's encryption state and it must come back `unknown` -- never `clear`, however ordinary
    # its traffic looks. 851.2 MHz is +187.5 kHz, raster channel 15, also inside the window.
    "late_entry_target_hz": 851.2e6,
    "late_entry_talkgroup": 4812,
    # --- P25 Phase 2 (T-272). OFF by default, so the T-267/T-268/T-269/T-270 fixtures stay
    # byte-identical: the branches guarded by it emit no extra TSBK and consume no randomness when
    # it is off.
    #
    # A Phase 2 system's CONTROL channel is a Phase 1 channel -- the same C4FM, the same TSBKs --
    # so what makes this scene Phase 2 is the band plan it announces: an IDEN_UP_TDMA entry whose
    # channel type names two slots per carrier. Two talkgroups are then granted on ALTERNATING
    # SLOTS of one frequency, which is C23's TDMA slot mix-up pitfall in its exact form: read as
    # FDMA, the two channel numbers become two different (wrong) frequencies and the two
    # talkgroups are attributed to channels that do not exist.
    "phase2": False,
    "p2_iden": 2,
    # Channel type 3 = two slots per carrier.
    "p2_channel_type": 3,
    "p2_base_hz": 851.0e6,
    "p2_spacing_hz": 12500.0,
    # The ONE frequency both talkgroups share. Chosen as a frequency first, like every other target
    # here; its channel number is derived from the TDMA band plan, so a decoder still has to divide
    # by the slot count it read off the air to arrive at it. +162.5 kHz from the tuned centre is
    # raster channel 13 -- inside the window, and clear of the control channel (3), the decoy (-5),
    # the followed channel (5), the encrypted (9) and late-entry (15) channels and every NBFM
    # neighbour.
    "p2_target_hz": 851.175e6,
    "p2_slot0_talkgroup": 7001,
    "p2_slot0_source": 1111,
    "p2_slot1_talkgroup": 7002,
    "p2_slot1_source": 2222,
    # --- P25 Phase 1 voice frames (T-849). OFF by default, so every earlier trunk fixture stays
    # byte-identical: the branches guarded by it emit no extra TSBK and consume no randomness when
    # it is off.
    #
    # Two more granted voice channels whose keyings carry REAL voice frames -- LDU1 link control
    # and LDU2 encryption sync, coded as the decoder reads them -- instead of unframed 4FSK. Both
    # grants carry service options 0, so the control channel says nothing about encryption for
    # either (`unknown`); the ONLY thing separating the clear call from the encrypted one is the
    # ALGID in its own LDU2, which is the whole point: that statement is only reachable by
    # demodulating the granted channel.
    "voice_frames": False,
    # Raster channel 1 (+12.5 kHz) and 14 (+175 kHz): inside the window, clear of the control
    # channel (3), the decoy (-5), the followed channel (5) and every NBFM neighbour, and -- like
    # every other target -- chosen as a frequency first, its channel number derived from the band
    # plan.
    "vf_clear_target_hz": 851.025e6,
    "vf_clear_talkgroup": 5150,
    "vf_clear_source": 1_000_001,
    "vf_encrypted_target_hz": 851.1875e6,
    "vf_encrypted_talkgroup": 5151,
    "vf_encrypted_source": 2_000_002,
    # AES-256 and a key id; the clear call carries ALGID 0x80, key id 0 and an all-zero MI.
    "vf_encrypted_algid": tk.P25_ALGID_AES256,
    "vf_encrypted_key_id": 0x02A5,
    "vf_nac": tk.P25_DEFAULT_NAC,
    # Each keying is `vf_superframes` LDU1+LDU2 pairs (0.36 s each) starting at `vf_first_s`, then
    # `vf_off_s` of silence. The trunk hunt holds the recording's first 0.5 s (it attaches at the
    # ring's oldest sample), so the first keying -- [0.02, 0.38] s -- lies wholly inside it, with a
    # silence after it longer than the follower's 90 ms timeout: a complete LDU1 AND a complete LDU2
    # per channel, and a call with a measured start and end.
    "vf_first_s": 0.02,
    "vf_superframes": 1,
    "vf_off_s": 0.14,
}

#: The same scene with TSBK content switched on (T-268).
TRUNK_TSBK_DEFAULTS: dict[str, Any] = {**TRUNK_CC_DEFAULTS, "tsbk": True}

#: The TSBK scene plus the encrypted and late-entry granted channels (T-270).
TRUNK_ENCRYPTED_DEFAULTS: dict[str, Any] = {**TRUNK_TSBK_DEFAULTS, "encryption": True}

#: The TSBK scene plus a P25 Phase 2 TDMA band plan and two talkgroups on alternating slots of one
#: frequency (T-272).
TRUNK_P25P2_DEFAULTS: dict[str, Any] = {**TRUNK_TSBK_DEFAULTS, "phase2": True}

#: The TSBK scene plus two granted voice channels carrying real P25 Phase 1 voice frames: one clear
#: by its LDU2 ALGID, one encrypted by it, with grants that say nothing either way (T-849).
TRUNK_VOICE_FRAMES_DEFAULTS: dict[str, Any] = {**TRUNK_TSBK_DEFAULTS, "voice_frames": True}


def _tsbk_stream(p: dict[str, Any], n_frames: int) -> tuple[np.ndarray, list[dict[str, Any]], dict[str, Any]]:
    """The control channel's outbound message stream, and the truth describing it.

    The cycle carries, in order: an identifier update, a grant that resolves through it, the same
    identifier update again (a band-plan entry is only trustworthy once it has been repeated), a
    grant update for the same channel, a grant onto a channel that is inside the tuned window
    (T-269), a grant naming an identifier that is never announced, and a message this decoder does
    not read. Repeating the cycle makes every one of those appear several times in any window long
    enough to confirm the channel at all.

    Two of those grants resolve to real frequencies that differ in one way that matters: one lands
    inside the window the radio is holding and one lands outside it. A follower must follow the
    first and refuse -- visibly -- the second.
    """
    iden = int(p["tsbk_iden"])
    base_hz, spacing_hz = float(p["tsbk_base_hz"]), float(p["tsbk_spacing_hz"])

    def channel_of(hz: float, what: str) -> int:
        """The band-plan channel number of a frequency. The FREQUENCY is the truth; this derives
        the number a grant has to carry to name it, never the other way round."""
        steps = (hz - base_hz) / spacing_hz
        if steps != int(steps) or not 0 <= steps <= 0xFFF:
            raise ValueError(f"{what} {hz} is not a channel of this band plan")
        return int(steps)

    target_hz = float(p["grant_target_hz"])
    channel = channel_of(target_hz, "grant_target_hz")
    follow_hz = float(p["follow_target_hz"])
    follow_channel = channel_of(follow_hz, "follow_target_hz")
    if follow_hz == target_hz:
        raise ValueError("the followed channel and the out-of-window channel must differ")
    tg, src = int(p["grant_talkgroup"]), int(p["grant_source"])
    f_tg, f_src = int(p["follow_talkgroup"]), int(p["follow_source"])
    u_iden, u_chan = int(p["unannounced_iden"]), int(p["unannounced_channel"])
    if u_iden == iden:
        raise ValueError("the unannounced identifier must differ from the announced one")

    iden_args = tk.iden_up_args(iden, base_hz, spacing_hz,
                                tx_offset_hz=float(p["tsbk_tx_offset_hz"]),
                                bandwidth_hz=float(p["tsbk_bandwidth_hz"]))
    iden_block = tk.tsbk(tk.TSBK_OP_IDEN_UP, iden_args)
    chan16 = tk.channel_number(iden, channel)
    follow_chan16 = tk.channel_number(iden, follow_channel)
    u_chan16 = tk.channel_number(u_iden, u_chan)
    cycle = [
        ("iden-up", iden_block),
        ("grant", tk.tsbk(tk.TSBK_OP_GRP_VCH_GRANT, tk.grant_args(chan16, tg, source=src))),
        ("iden-up", iden_block),
        ("grant-update", tk.tsbk(tk.TSBK_OP_GRP_VCH_GRANT_UPDATE, tk.grant_args(chan16, tg))),
        ("grant-follow",
         tk.tsbk(tk.TSBK_OP_GRP_VCH_GRANT, tk.grant_args(follow_chan16, f_tg, source=f_src))),
        ("grant-unannounced",
         tk.tsbk(tk.TSBK_OP_GRP_VCH_GRANT, tk.grant_args(u_chan16, tg + 1, source=src))),
        # An opcode this decoder does not read (RFSS status broadcast), so the stream is not made
        # only of messages it happens to understand.
        ("other", tk.tsbk(0x3A, bytes(8))),
    ]

    # --- Encryption (T-270). Two more granted channels, both INSIDE the window so both are
    # followed, differing only in what their announcement was entitled to say.
    enc_truth: dict[str, Any] | None = None
    if bool(p.get("encryption", False)):
        e_hz = float(p["encrypted_target_hz"])
        e_chan = channel_of(e_hz, "encrypted_target_hz")
        e_chan16 = tk.channel_number(iden, e_chan)
        e_tg, e_src = int(p["encrypted_talkgroup"]), int(p["encrypted_source"])
        l_hz = float(p["late_entry_target_hz"])
        l_chan = channel_of(l_hz, "late_entry_target_hz")
        l_chan16 = tk.channel_number(iden, l_chan)
        l_tg = int(p["late_entry_talkgroup"])
        if len({e_hz, l_hz, follow_hz, target_hz}) != 4:
            raise ValueError("each granted voice channel must be a distinct frequency")
        cycle.extend([
            # A plain grant carrying the verified encryption bit: this one must be FLAGGED.
            ("grant-encrypted",
             tk.tsbk(tk.TSBK_OP_GRP_VCH_GRANT,
                     tk.grant_args(e_chan16, e_tg, source=e_src,
                                   service_options=tk.SVC_ENCRYPTED))),
            # A grant UPDATE and nothing else. There is no plain grant for this channel anywhere in
            # the stream, so its header was never seen -- the late-entry case, which must record
            # `unknown`. Its traffic is deliberately indistinguishable from the followed channel's.
            ("grant-late-entry",
             tk.tsbk(tk.TSBK_OP_GRP_VCH_GRANT_UPDATE, tk.grant_args(l_chan16, l_tg))),
        ])
        enc_truth = {
            "encrypted_target_hz": e_hz,
            "encrypted_channel": e_chan,
            "encrypted_channel_16bit": e_chan16,
            "encrypted_talkgroup": e_tg,
            "encrypted_source": e_src,
            "encrypted_service_options": tk.SVC_ENCRYPTED,
            "late_entry_target_hz": l_hz,
            "late_entry_channel": l_chan,
            "late_entry_channel_16bit": l_chan16,
            "late_entry_talkgroup": l_tg,
            "expected": {
                "encrypted_call": "encrypted, by the grant's service-options bit; no audio",
                "late_entry_call": "unknown -- never clear: no grant, so no header, so nothing "
                                   "ever said",
                "clear_calls": "none: saying `clear` needs an ALGID, and no ALGID is carried on a "
                               "control channel",
            },
        }

    # --- P25 Phase 2 (T-272). A TDMA band plan, announced twice like every other identifier so it
    # has to clear the same agreement gate, and two grants naming consecutive channel numbers on
    # it: one frequency, two slots, two talkgroups.
    p2_truth: dict[str, Any] | None = None
    if bool(p.get("phase2", False)):
        p2_iden = int(p["p2_iden"])
        if p2_iden in (iden, u_iden):
            raise ValueError("the TDMA identifier must differ from the FDMA and unannounced ones")
        ctype = int(p["p2_channel_type"])
        slots = tk.TDMA_SLOTS_PER_CHANNEL_TYPE[ctype]
        if slots < 2:
            raise ValueError("a Phase 2 scene needs a channel type with more than one slot")
        p2_base, p2_spacing = float(p["p2_base_hz"]), float(p["p2_spacing_hz"])
        p2_hz = float(p["p2_target_hz"])
        steps = (p2_hz - p2_base) / p2_spacing
        if steps != int(steps) or not 0 <= steps * slots <= 0xFFF:
            raise ValueError(f"p2_target_hz {p2_hz} is not a channel of the TDMA band plan")
        p2_channel = int(steps)
        if p2_hz in {target_hz, follow_hz}:
            raise ValueError("the Phase 2 channel must be a frequency of its own")
        tdma_args = tk.iden_up_tdma_args(p2_iden, ctype, p2_base, p2_spacing)
        tdma_block = tk.tsbk(tk.TSBK_OP_IDEN_UP_TDMA, tdma_args)
        slot_tgs = [int(p["p2_slot0_talkgroup"]), int(p["p2_slot1_talkgroup"])]
        slot_srcs = [int(p["p2_slot0_source"]), int(p["p2_slot1_source"])]
        slot_chan16 = [tk.tdma_channel_number(p2_iden, p2_channel, k, slots) for k in (0, 1)]
        # Inserted near the FRONT of the cycle, not appended to it. A hunt buffers a window and
        # decodes the blocks it holds, so a message late in a long cycle can fall off the end of
        # every window -- and the cycle length and the window period here happen to be
        # commensurate, so "late" means late in *every* pass, not merely some. Both agreements for
        # the TDMA identifier and both slot grants therefore sit in the first five frames, which
        # keeps what this scene tests independent of how much of the cycle a window happens to
        # span. The two agreements are adjacent for the same reason; the gate is that two messages
        # agree bit for bit, not that they are spread out.
        cycle[1:1] = [
            ("iden-up-tdma", tdma_block),
            ("iden-up-tdma", tdma_block),
            ("grant-p2-slot0",
             tk.tsbk(tk.TSBK_OP_GRP_VCH_GRANT,
                     tk.grant_args(slot_chan16[0], slot_tgs[0], source=slot_srcs[0]))),
            ("grant-p2-slot1",
             tk.tsbk(tk.TSBK_OP_GRP_VCH_GRANT,
                     tk.grant_args(slot_chan16[1], slot_tgs[1], source=slot_srcs[1]))),
        ]
        p2_truth = {
            "iden": p2_iden,
            "channel_type": ctype,
            "slots": slots,
            "base_hz": p2_base,
            "spacing_hz": p2_spacing,
            "target_hz": p2_hz,
            "channel": p2_channel,
            "slot_channel_16bit": slot_chan16,
            "slot_talkgroups": slot_tgs,
            "slot_sources": slot_srcs,
            # What an FDMA reading of the SAME two channel numbers would produce: two different
            # frequencies, both wrong, neither of which any row may ever report.
            "wrong_frequencies_if_read_as_fdma_hz": [
                p2_base + p2_spacing * (p2_channel * slots + k) for k in (0, 1)
            ],
            "expected": {
                "protocol": "p25-phase2",
                "calls": "two, on ONE frequency, attributed to slots 0 and 1 with their own "
                         "talkgroups",
                "timing": "both slots key one carrier, so call boundaries are the shared "
                          "envelope's (`tdma-shared-envelope`), not per-slot",
            },
        }

    # --- P25 Phase 1 voice frames (T-849). Two plain grants with service options 0: the control
    # channel states NOTHING about either call's encryption, so the ALGID in each call's own LDU2 is
    # the only place the difference lives.
    vf_truth: dict[str, Any] | None = None
    if bool(p.get("voice_frames", False)):
        chans: list[dict[str, Any]] = []
        for key, algid, key_id in (
            ("clear", tk.P25_ALGID_CLEAR, 0),
            ("encrypted", int(p["vf_encrypted_algid"]), int(p["vf_encrypted_key_id"])),
        ):
            hz = float(p[f"vf_{key}_target_hz"])
            if hz in {target_hz, follow_hz}:
                raise ValueError("each granted voice channel must be a distinct frequency")
            chan = channel_of(hz, f"vf_{key}_target_hz")
            chan16 = tk.channel_number(iden, chan)
            v_tg, v_src = int(p[f"vf_{key}_talkgroup"]), int(p[f"vf_{key}_source"])
            cycle.append((f"grant-voice-frames-{key}",
                          tk.tsbk(tk.TSBK_OP_GRP_VCH_GRANT,
                                  tk.grant_args(chan16, v_tg, source=v_src))))
            chans.append({
                "name": key,
                "target_hz": hz,
                "channel": chan,
                "channel_16bit": chan16,
                "talkgroup": v_tg,
                "source": v_src,
                "grant_service_options": 0,
                "algid": algid,
                "key_id": key_id,
            })
        if chans[0]["target_hz"] == chans[1]["target_hz"]:
            raise ValueError("the clear and encrypted voice-frame channels must differ")
        vf_truth = {
            "nac": int(p["vf_nac"]),
            "channels": chans,
            "expected": {
                "grants": "both unknown: service options 0 states nothing",
                "voice_frames": "each channel's LDU1 carries its own talkgroup and source, and its "
                                "LDU2 its own ALGID (0x80 clear / an algorithm) and key id",
                "other_channels": "no voice frames: their keyings are unframed 4FSK",
            },
        }

    dibits = tk.frames_from_blocks([b for _, b in cycle], n_frames)
    frames = [{"index": i, "kind": cycle[i % len(cycle)][0],
               "block_hex": cycle[i % len(cycle)][1].hex()} for i in range(n_frames)]
    counts = {kind: sum(1 for i in range(n_frames) if cycle[i % len(cycle)][0] == kind)
              for kind, _ in cycle}
    truth = {
        "iden": iden,
        "base_hz": base_hz,
        "spacing_hz": spacing_hz,
        "tx_offset_hz": float(p["tsbk_tx_offset_hz"]),
        "bandwidth_hz": float(p["tsbk_bandwidth_hz"]),
        "grant_channel": channel,
        "grant_channel_16bit": chan16,
        "grant_target_hz": target_hz,
        "grant_talkgroup": tg,
        "grant_source": src,
        "follow_channel": follow_channel,
        "follow_channel_16bit": follow_chan16,
        "follow_target_hz": follow_hz,
        "follow_talkgroup": f_tg,
        "follow_source": f_src,
        "unannounced_iden": u_iden,
        "unannounced_channel": u_chan,
        "unannounced_channel_16bit": u_chan16,
        # What a decoder that fell back to the announced identifier would produce. Nothing may
        # ever report this frequency.
        "wrong_frequency_if_misresolved_hz": base_hz + spacing_hz * u_chan,
        "frames_per_cycle": len(cycle),
        "counts": counts,
        "expected": {
            "protocol": "p25-phase1",
            "iden_admitted": True,
            "grant_maps_to_target": True,
            "unannounced_grant": "unmapped-channel",
        },
        "coding": "none (no trellis, no interleaving, no status symbols; CRC-16/CCITT-FALSE, "
                  "not the augmented CRC-CCITT real P25 uses)",
    }
    if enc_truth is not None:
        truth["encryption"] = enc_truth
    if vf_truth is not None:
        truth["voice_frames"] = vf_truth
    if p2_truth is not None:
        truth["phase2"] = p2_truth
        # A system announcing a TDMA band plan is Phase 2, however Phase 1 its control channel is.
        truth["expected"]["protocol"] = "p25-phase2"
    return dibits, frames, truth


def _n(p: dict[str, Any]) -> int:
    return int(round(float(p["duration_s"]) * float(p["sample_rate"])))


def _power(cap: Any, snr_db: float, bw_hz: float) -> float:
    """Absolute dBFS for a wanted SNR in ``bw_hz`` against the capture's floor density."""
    return float(snr_db) + cap.floor_dbfs_per_hz + db(bw_hz)


def _on_off(rng: np.random.Generator, span_s: float, mean_on: float, mean_off: float
            ) -> list[tuple[float, float]]:
    """Two-state on/off intervals over ``span_s`` (exponential dwell times)."""
    out: list[tuple[float, float]] = []
    t = float(rng.exponential(mean_off))
    while t < span_s:
        on = float(rng.exponential(mean_on))
        end = min(span_s, t + on)
        if end > t:
            out.append((t, end))
        t = end + float(rng.exponential(mean_off))
    return out


def trunk_control_channel(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    p = ctx.params
    fs = float(p["sample_rate"])
    n = _n(p)
    scene = ctx.scene(
        "trunk_control_channel", fs, n,
        "hkpy.synth trunk_control_channel: continuous C4FM control channel, a continuous 4FSK "
        "decoy with no framing, and bursty NBFM on the 12.5 kHz LMR raster",
    )

    # Noise floor over the whole scene.
    cap = scene.add_capture(0, n, float(p["center_hz"]), utc_plus(p["start_utc"], 0.0),
                            calibration_k_db=float(p["calibration_k_db"]))
    cap.floor_dbfs_per_hz = float(p["noise_dbfs"]) - db(fs)
    scene.add_samples(0, complex_noise(scene.rng("noise"), n, float(p["noise_dbfs"])))
    scene.add_floor(0, n, cap.floor_dbfs_per_hz)

    raster = float(p["raster_hz"])
    rate = float(p["symbol_rate_bd"])
    # C4FM occupies a 12.5 kHz channel; Carson on the outer deviation plus the symbol rate.
    c4fm_bw = 2 * (1800.0 + rate / 2)
    t_all = scene.time(0, n)

    def place_c4fm(channel: int, dibits: np.ndarray, snr_db: float, cfo_hz: float) -> tuple[float, float]:
        off = channel * raster + cfo_hz
        if abs(off) + c4fm_bw / 2 > fs / 2:
            raise ValueError(f"channel {channel} does not fit inside the sample rate")
        power = _power(cap, snr_db, c4fm_bw)
        iq = tk.c4fm(dibits, fs, rate)[:n]
        phase0 = float(scene.rng("phase", channel).uniform(0, 2 * math.pi))
        scene.add_samples(0, math.sqrt(undb(power)) * iq
                          * np.exp(1j * (2 * math.pi * off * t_all[: len(iq)] + phase0)))
        return off, power

    # --- The control channel: continuous C4FM, real frame sync, CRC-valid blocks.
    frames_needed = int(math.ceil(n * rate / fs / tk.FRAME_DIBITS)) + 1
    tsbk_truth: dict[str, Any] | None = None
    if bool(p["tsbk"]):
        cc_dibits, cc_frames, tsbk_truth = _tsbk_stream(p, frames_needed)
    else:
        cc_dibits, cc_frames = tk.control_channel_dibits(scene.rng("cc"), frames_needed)
    cc_ch = int(p["cc_channel"])
    cc_off, cc_power = place_c4fm(cc_ch, cc_dibits, float(p["cc_snr_db"]), float(p["cc_cfo_hz"]))
    cc_f = cap.center_hz + cc_off
    scene.annotate(
        0, n, cc_f - c4fm_bw / 2, cc_f + c4fm_bw / 2, "trunk-control-channel",
        scene.emission_truth(
            cap, cc_off, c4fm_bw, cc_power,
            kind="trunk-control-channel", modulation="c4fm", levels=4,
            symbol_rate_bd=rate, duty_cycle=1.0, fco=1.0,
            raster_hz=raster, raster_channel=cc_ch,
            nominal_center_hz=cap.center_hz + cc_ch * raster,
            cfo_hz=float(p["cc_cfo_hz"]),
            is_control_channel=True, confirmable=True,
            frame=tk.CC_FRAME_SPEC,
            n_frames=len(cc_frames),
            frames=cc_frames[:8],
            sync_hex=tk.P25_FRAME_SYNC_HEX,
            carries_tsbk=tsbk_truth is not None,
        ),
    )

    # --- The decoy: continuous, on-raster, 4FSK, and unframed. Must never be confirmed.
    n_dibits = int(math.ceil(n * rate / fs)) + 1
    decoy_dibits = tk.continuous_data_dibits(scene.rng("decoy"), n_dibits)
    dc_ch = int(p["decoy_channel"])
    dc_off, dc_power = place_c4fm(dc_ch, decoy_dibits, float(p["decoy_snr_db"]), 0.0)
    dc_f = cap.center_hz + dc_off
    scene.annotate(
        0, n, dc_f - c4fm_bw / 2, dc_f + c4fm_bw / 2, "continuous-data",
        scene.emission_truth(
            cap, dc_off, c4fm_bw, dc_power,
            kind="continuous-data", modulation="c4fm", levels=4,
            symbol_rate_bd=rate, duty_cycle=1.0, fco=1.0,
            raster_hz=raster, raster_channel=dc_ch,
            nominal_center_hz=cap.center_hz + dc_ch * raster,
            is_control_channel=False, confirmable=False,
            why_not="continuous 4FSK with no frame sync and no CRC-valid block: passes FCO "
                    "candidacy, fails sync+CRC confirmation",
        ),
    )

    # --- Bursty NBFM neighbours.
    dev, tone = float(p["fm_deviation_hz"]), float(p["audio_tone_hz"])
    nbfm_bw = 2 * (dev + tone)
    span_s = n / fs
    n_bursts = 0
    for ch in [int(c) for c in p["nbfm_channels"]]:
        off = ch * raster
        if abs(off) + nbfm_bw / 2 > fs / 2:
            raise ValueError(f"nbfm channel {ch} does not fit inside the sample rate")
        power = _power(cap, float(p["nbfm_snr_db"]), nbfm_bw)
        amp = math.sqrt(undb(power))
        r = scene.rng("nbfm", ch)
        for b0, b1 in _on_off(r, span_s, float(p["burst_mean_on_s"]), float(p["burst_mean_off_s"])):
            i0, i1 = int(round(b0 * fs)), min(n, int(round(b1 * fs)))
            if i1 <= i0:
                continue
            phase0, audio_phase = (float(v) for v in r.uniform(0, 2 * math.pi, 2))
            tt = scene.time(i0, i1 - i0)
            phase = (phase0 + 2 * math.pi * off * tt
                     + (dev / tone) * np.sin(2 * math.pi * tone * (tt - b0) + audio_phase))
            scene.add_samples(i0, amp * np.exp(1j * phase))
            f = cap.center_hz + off
            scene.annotate(
                i0, i1 - i0, f - nbfm_bw / 2, f + nbfm_bw / 2, "nbfm-burst",
                scene.emission_truth(
                    cap, off, nbfm_bw, power, kind="nbfm-burst", modulation="nbfm",
                    deviation_hz=dev, audio_tone_hz=tone,
                    raster_hz=raster, raster_channel=ch,
                    is_control_channel=False, confirmable=False,
                    burst_start_s=b0, burst_duration_s=b1 - b0,
                ),
            )
            n_bursts += 1

    # --- The voice channel a grant is followed onto (T-269), present only when the control
    # channel is actually issuing grants. Bursty by construction: repeated keyings with real
    # silence between them, so a follower has boundaries to **measure** rather than a carrier that
    # is simply always there. At 40 % duty it never reaches control-channel candidacy
    # (MIN_CC_FCO is 0.95), so it cannot be mistaken for a second control channel.
    if tsbk_truth is not None:
        v_power = _power(cap, float(p["follow_snr_db"]), c4fm_bw)
        v_amp = math.sqrt(undb(v_power))
        on_s, off_s = float(p["follow_on_s"]), float(p["follow_off_s"])
        if on_s <= 0 or off_s <= 0:
            raise ValueError("a keying needs a positive on and off time")

        def place_voice(target_hz: float, rng_name: str, talkgroup: int, chan16: int,
                        what: str, *, tdma_slots: list[dict[str, Any]] | None = None
                        ) -> tuple[list[tuple[float, float]], float]:
            """Repeated keyings on one granted voice channel, and the offset it sits at.

            Every granted channel in this scene is placed by this one function, so the encrypted
            and late-entry channels are acoustically **indistinguishable** from the ordinary one:
            same power, same duty, same modulation, same kind of payload. Nothing about the samples
            says which is which -- only what the control channel announced does, which is the whole
            point of the test they serve.
            """
            off_hz = target_hz - cap.center_hz
            if abs(off_hz) + c4fm_bw / 2 > fs / 2:
                raise ValueError(f"{what} does not fit inside the sample rate")
            rr = scene.rng(rng_name)
            out: list[tuple[float, float]] = []
            t_key = 0.0
            while t_key < span_s:
                i0, i1 = int(round(t_key * fs)), min(n, int(round((t_key + on_s) * fs)))
                if i1 > i0:
                    nd = int(math.ceil((i1 - i0) * rate / fs)) + 1
                    iq = tk.c4fm(tk.continuous_data_dibits(rr, nd), fs, rate)[: i1 - i0]
                    phase0 = float(rr.uniform(0, 2 * math.pi))
                    tt = scene.time(i0, len(iq))
                    scene.add_samples(
                        i0, v_amp * iq * np.exp(1j * (2 * math.pi * off_hz * tt + phase0)))
                    b0, b1 = i0 / fs, (i0 + len(iq)) / fs
                    out.append((b0, b1))
                    vf = cap.center_hz + off_hz
                    scene.annotate(
                        i0, len(iq), vf - c4fm_bw / 2, vf + c4fm_bw / 2, "trunk-voice-keying",
                        scene.emission_truth(
                            cap, off_hz, c4fm_bw, v_power,
                            kind="trunk-voice-keying", modulation="c4fm", levels=4,
                            symbol_rate_bd=rate, raster_hz=raster,
                            raster_channel=int(round(off_hz / raster)),
                            is_control_channel=False, confirmable=False,
                            burst_start_s=b0, burst_duration_s=b1 - b0,
                            granted_by_channel_16bit=chan16,
                            talkgroup=talkgroup,
                            # On a TDMA carrier one emission carries BOTH slots: the samples are
                            # one keying, and which slot was talking is not separable from them.
                            **({"tdma_slots": tdma_slots} if tdma_slots else {}),
                        ),
                    )
                t_key += on_s + off_s
            return out, off_hz

        keyings, v_off = place_voice(
            float(tsbk_truth["follow_target_hz"]), "voice",
            int(tsbk_truth["follow_talkgroup"]), int(tsbk_truth["follow_channel_16bit"]),
            "follow_target_hz")
        tsbk_truth.update({
            "follow_on_s": on_s,
            "follow_off_s": off_s,
            "follow_period_s": on_s + off_s,
            "follow_snr_db": float(p["follow_snr_db"]),
            "follow_offset_hz": v_off,
            "follow_keyings_s": [[a, b] for a, b in keyings],
            # Where each resolved grant falls relative to the window the radio is holding. The
            # follow target is inside it; the T-268 target is outside, which is the C23 <=20 MHz
            # span limit a follower must REPORT rather than drop.
            "grant_target_offset_hz": float(tsbk_truth["grant_target_hz"]) - cap.center_hz,
            "sample_rate_hz": fs,
        })
        tsbk_truth["expected"]["follow_grant"] = "followed: a call record with measured boundaries"
        tsbk_truth["expected"]["grant_target"] = "outside-window: logged, never followed"

        # The Phase 2 channel: ONE emission on ONE frequency, carrying both slots. Placed by the
        # same function as every other granted channel, so nothing in the samples says it is TDMA
        # -- only the band plan the control channel announced does, which is the point of the test
        # it serves (T-272).
        p2 = tsbk_truth.get("phase2")
        if p2 is not None:
            p2_keyings, p2_off = place_voice(
                float(p2["target_hz"]), "voice-p2",
                int(p2["slot_talkgroups"][0]), int(p2["slot_channel_16bit"][0]),
                "p2_target_hz",
                tdma_slots=[{"slot": k, "talkgroup": int(tg)}
                            for k, tg in enumerate(p2["slot_talkgroups"])],
            )
            p2["keyings_s"] = [[a, b] for a, b in p2_keyings]
            p2["offset_hz"] = p2_off

        # The encrypted and late-entry channels, placed by the same function and therefore
        # indistinguishable in the samples (T-270).
        enc = tsbk_truth.get("encryption")
        if enc is not None:
            for key in ("encrypted", "late_entry"):
                ks, off_hz = place_voice(
                    float(enc[f"{key}_target_hz"]), f"voice-{key}",
                    int(enc[f"{key}_talkgroup"]), int(enc[f"{key}_channel_16bit"]),
                    f"{key}_target_hz")
                enc[f"{key}_keyings_s"] = [[a, b] for a, b in ks]
                enc[f"{key}_offset_hz"] = off_hz

        # The voice-frame channels (T-849): the same power and modulation as every other granted
        # channel, but each keying is a run of real LDU1/LDU2 superframes rather than unframed
        # 4FSK -- link control in the LDU1, encryption sync in the LDU2.
        vf = tsbk_truth.get("voice_frames")
        if vf is not None:
            first_s, off_s = float(p["vf_first_s"]), float(p["vf_off_s"])
            n_sf = int(p["vf_superframes"])
            if first_s < 0 or off_s <= 0 or n_sf < 1:
                raise ValueError("a voice-frame keying needs a start, a silence and a superframe")
            nac = int(vf["nac"])
            for ch in vf["channels"]:
                off_hz = float(ch["target_hz"]) - cap.center_hz
                if abs(off_hz) + c4fm_bw / 2 > fs / 2:
                    raise ValueError(f"voice-frame channel {ch['name']} does not fit")
                rr = scene.rng("voice-frames", ch["name"])
                lc = tk.p25_lc_group_voice(int(ch["talkgroup"]), int(ch["source"]))
                keyings: list[list[float]] = []
                ldus: list[dict[str, Any]] = []
                t_key = first_s
                while t_key < span_s:
                    i0 = int(round(t_key * fs))
                    frames: list[np.ndarray] = []
                    for k in range(n_sf):
                        # A clear call's MI is all zero; an encrypted call's changes every
                        # superframe. Either way it is recorded, never used.
                        mi = (bytes(9) if ch["algid"] == tk.P25_ALGID_CLEAR
                              else bytes(int(v) for v in rr.integers(0, 256, 9)))
                        es = tk.p25_es(mi, int(ch["algid"]), int(ch["key_id"]))
                        frames.append(tk.p25_ldu_dibits(tk.P25_DUID_LDU1, lc, rr, nac=nac))
                        frames.append(tk.p25_ldu_dibits(tk.P25_DUID_LDU2, es, rr, nac=nac))
                        t_sf = t_key + 2 * k * tk.P25_LDU_S
                        ldus.append({"duid": "ldu1", "start_s": t_sf})
                        ldus.append({"duid": "ldu2", "start_s": t_sf + tk.P25_LDU_S,
                                     "mi_hex": mi.hex()})
                    iq = tk.c4fm(np.concatenate(frames), fs, rate)[: max(0, n - i0)]
                    if len(iq) == 0:
                        break
                    phase0 = float(rr.uniform(0, 2 * math.pi))
                    tt = scene.time(i0, len(iq))
                    scene.add_samples(
                        i0, v_amp * iq * np.exp(1j * (2 * math.pi * off_hz * tt + phase0)))
                    b0, b1 = i0 / fs, (i0 + len(iq)) / fs
                    keyings.append([b0, b1])
                    vf_hz = cap.center_hz + off_hz
                    scene.annotate(
                        i0, len(iq), vf_hz - c4fm_bw / 2, vf_hz + c4fm_bw / 2,
                        "trunk-voice-keying",
                        scene.emission_truth(
                            cap, off_hz, c4fm_bw, v_power,
                            kind="trunk-voice-keying", modulation="c4fm", levels=4,
                            symbol_rate_bd=rate, raster_hz=raster,
                            raster_channel=int(round(off_hz / raster)),
                            is_control_channel=False, confirmable=False,
                            burst_start_s=b0, burst_duration_s=b1 - b0,
                            granted_by_channel_16bit=int(ch["channel_16bit"]),
                            talkgroup=int(ch["talkgroup"]),
                            voice_frames="p25-phase1-ldu",
                        ),
                    )
                    t_key += 2 * n_sf * tk.P25_LDU_S + off_s
                ch["offset_hz"] = off_hz
                ch["keyings_s"] = keyings
                ch["ldus"] = [u for u in ldus if u["start_s"] + tk.P25_LDU_S <= span_s]
                ch["lc_hex"] = lc.hex()

    scene.scenario_truth["trunking"] = {
        "raster_hz": raster,
        "raster_origin_hz": cap.center_hz,
        "control_channel": {
            "rf_center_hz": cc_f,
            "raster_channel": cc_ch,
            "modulation": "c4fm",
            "symbol_rate_bd": rate,
            "bandwidth_hz": c4fm_bw,
            "duty_cycle": 1.0,
            "sync_hex": tk.P25_FRAME_SYNC_HEX,
            "n_frames": len(cc_frames),
            "expected_confirmed": True,
        },
        "continuous_decoy": {
            "rf_center_hz": dc_f,
            "raster_channel": dc_ch,
            "modulation": "c4fm",
            "symbol_rate_bd": rate,
            "bandwidth_hz": c4fm_bw,
            "duty_cycle": 1.0,
            "expected_confirmed": False,
            "why_not": "no frame sync, no CRC-valid block",
        },
        "nbfm_channels": [int(c) for c in p["nbfm_channels"]],
        "n_nbfm_bursts": n_bursts,
        "frame": tk.CC_FRAME_SPEC,
    }
    if tsbk_truth is not None:
        scene.scenario_truth["trunking"]["tsbk"] = tsbk_truth
    return [scene], {}


# =============================================================================================
# DMR Tier III (T-271)
# =============================================================================================

TRUNK_DMR_DEFAULTS: dict[str, Any] = {
    "sample_rate": 500e3,
    # 800 MHz, same band as the P25 scenes, so the two are directly comparable.
    "center_hz": 851.0125e6,
    "duration_s": 1.0,
    "raster_hz": tk.LMR_RASTER_HZ,
    "cc_channel": 3,
    "decoy_channel": -5,
    "nbfm_channels": [-2, 7, 11, -9],
    "cc_snr_db": 20.0,
    "decoy_snr_db": 20.0,
    "nbfm_snr_db": 18.0,
    "symbol_rate_bd": tk.DMR_SYMBOL_RATE_BD,
    "fm_deviation_hz": 2500.0,
    "audio_tone_hz": 1000.0,
    "burst_mean_on_s": 0.08,
    "burst_mean_off_s": 0.12,
    "noise_dbfs": -40.0,
    "calibration_k_db": -70.0,
    "start_utc": DEFAULT_START_UTC,
    # --- THE TRAP.
    #
    # A DMR Tier III grant names a Logical Physical Channel Number and nothing else. There is no
    # on-air channel-parameter announcement this project could corroborate, so an LPCN resolves to
    # NO frequency -- and a decoder that quietly assumed the obvious band plan (base = wherever the
    # radio is tuned, step = the 12.5 kHz LMR raster) would produce one anyway.
    #
    # So the scene puts real, followable voice traffic exactly where that assumption points. A
    # guessing decoder does not merely produce a wrong number: it follows the channel, finds a
    # transmission, measures its boundaries and writes a completely convincing call record. Nothing
    # about the run would look wrong. The frequency is chosen FIRST and the LPCN derived from it, so
    # the trap is baited by the same arithmetic the mistake would use.
    "trap_target_hz": 851.075e6,
    "assumed_spacing_hz": tk.LMR_RASTER_HZ,
    "grant_target_id": 2468,
    "grant_source_id": 1357,
    "grant_timeslot": 1,
    # A second grant, on the other slot, so the two-slot TDMA attribution is exercised (C23's slot
    # mix-up pitfall). Its LPCN is deliberately NOT the trap's.
    "second_lpcn": 11,
    "second_target_id": 3690,
    "second_source_id": 2580,
    "second_timeslot": 0,
    # The traffic on the trap channel: repeated keyings, like T-269's followed channel, so a
    # follower that got there would have real boundaries to measure. At 40 % duty it never reaches
    # control-channel candidacy (MIN_CC_FCO is 0.95).
    "trap_on_s": 0.10,
    "trap_off_s": 0.15,
    "trap_snr_db": 18.0,
}


def _dmr_stream(p: dict[str, Any], center_hz: float, n_frames: int,
                rng: np.random.Generator) -> tuple[np.ndarray, dict[str, Any]]:
    """The DMR control channel's outbound CSBK stream, and the truth describing it.

    The cycle carries a system announcement, a broadcast voice grant onto the trap channel, a
    private voice grant on the other timeslot, a data grant, a call release, and an opcode this
    decoder does not name -- so the stream is not made only of messages it happens to understand.
    """
    trap_hz = float(p["trap_target_hz"])
    spacing = float(p["assumed_spacing_hz"])
    steps = (trap_hz - center_hz) / spacing
    if steps != int(steps) or not 0 <= steps <= 0xFFF:
        raise ValueError(f"trap_target_hz {trap_hz} is not a whole step from the tuned centre")
    lpcn = int(steps)
    second = int(p["second_lpcn"])
    if second == lpcn:
        raise ValueError("the second grant must name a different logical channel")

    tg, src = int(p["grant_target_id"]), int(p["grant_source_id"])
    ts = int(p["grant_timeslot"])
    s_tg, s_src, s_ts = (int(p["second_target_id"]), int(p["second_source_id"]),
                         int(p["second_timeslot"]))
    def payload() -> bytes:
        """Payload bytes for a message whose own fields this project does not decode.

        Deliberately **not** zeros. A 4FSK receiver estimates its own level centre and outer
        deviation from the symbol distribution, so a stream padded with zero bytes is a stream whose
        symbols sit overwhelmingly on one inner level: the estimated centre drifts and the estimated
        outer level collapses onto the inner pair, and the demodulator slices garbage. That is not
        a property of DMR, it is an artefact of a lazy fixture, and a real C_ALOHA carries system
        identity, colour code and site parameters rather than sixty-four zeros.
        """
        return bytes(int(v) for v in rng.integers(0, 256, 8))

    cycle = [
        ("c-aloha", tk.csbk(tk.CSBKO_C_ALOHA, payload())),
        ("btv-grant",
         tk.csbk(tk.CSBKO_BTV_GRANT, tk.dmr_grant_payload(lpcn, ts, tg, src))),
        ("p-grant",
         tk.csbk(tk.CSBKO_P_GRANT, tk.dmr_grant_payload(second, s_ts, s_tg, s_src))),
        ("pd-grant",
         tk.csbk(tk.CSBKO_PD_GRANT, tk.dmr_grant_payload(second, s_ts, s_tg, s_src))),
        ("p-clear", tk.csbk(tk.CSBKO_P_CLEAR, payload())),
        # An opcode this decoder does not name.
        ("other", tk.csbk(0x07, payload())),
    ]
    dibits = tk.dmr_frames_from_blocks([b for _, b in cycle], n_frames)
    counts = {kind: sum(1 for i in range(n_frames) if cycle[i % len(cycle)][0] == kind)
              for kind, _ in cycle}
    truth = {
        "framing": "dmr-bs-data-sync",
        "grant_lpcn": lpcn,
        "grant_timeslot": ts,
        "grant_target_id": tg,
        "grant_source_id": src,
        "second_lpcn": second,
        "second_timeslot": s_ts,
        "second_target_id": s_tg,
        "second_source_id": s_src,
        # The number nothing may ever report. Real traffic sits here, so a decoder that assumed a
        # band plan would follow it and write a call that looks entirely right.
        "wrong_frequency_if_lpcn_assumed_hz": trap_hz,
        "assumed_spacing_hz": spacing,
        "assumed_base_hz": center_hz,
        "frames_per_cycle": len(cycle),
        "counts": counts,
        "expected": {
            "protocol": "dmr-tier3",
            "grants_decoded": True,
            "grants_mapped": False,
            "grant_reason": "no-channel-parameters",
            "calls": "none: a grant with no frequency entitles no channel and no call",
            "why": "DMR Tier III announces no channel-parameter message this project could "
                   "corroborate, so a logical channel number has no on-air base or step to "
                   "resolve through",
        },
        "coding": tk.DMR_FRAME_SPEC["coding"],
    }
    return dibits, truth


def trunk_dmr_control_channel(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    """A DMR Tier III control channel among the same traffic the P25 scenes use (T-271).

    Two things have to be true at once for this scene to mean anything:

    1. The hunt is **blind about the air interface**. Nothing tells it this is DMR rather than P25;
       it must try the framings it knows and confirm on the one that matches. A decoy that is
       continuous, on-raster and 4FSK -- and therefore a perfect candidate -- carries no framing at
       all and must still be rejected.
    2. The decoder must **refuse to invent a frequency**, while real traffic sits exactly where the
       obvious guess points. See ``trap_target_hz``.
    """
    p = ctx.params
    fs = float(p["sample_rate"])
    n = _n(p)
    scene = ctx.scene(
        "trunk_dmr_control_channel", fs, n,
        "hkpy.synth trunk_dmr_control_channel: a DMR Tier III control channel (BS-data frame sync "
        "+ CSBKs with the masked CRC) carrying grants whose logical channel numbers resolve to "
        "nothing, with real voice traffic parked where an assumed band plan would put them",
    )

    cap = scene.add_capture(0, n, float(p["center_hz"]), utc_plus(p["start_utc"], 0.0),
                            calibration_k_db=float(p["calibration_k_db"]))
    cap.floor_dbfs_per_hz = float(p["noise_dbfs"]) - db(fs)
    scene.add_samples(0, complex_noise(scene.rng("noise"), n, float(p["noise_dbfs"])))
    scene.add_floor(0, n, cap.floor_dbfs_per_hz)

    raster = float(p["raster_hz"])
    rate = float(p["symbol_rate_bd"])
    # Carson on DMR's outer deviation plus the symbol rate.
    dmr_bw = 2 * (1944.0 + rate / 2)
    t_all = scene.time(0, n)
    span_s = n / fs

    def place_4fsk(off_hz: float, dibits: np.ndarray, snr_db: float, rng_name: str,
                   deviations: dict[int, float]) -> tuple[float, int]:
        if abs(off_hz) + dmr_bw / 2 > fs / 2:
            raise ValueError(f"an emission at {off_hz} Hz does not fit inside the sample rate")
        power = _power(cap, snr_db, dmr_bw)
        iq = tk.c4fm(dibits, fs, rate, deviations=deviations)[:n]
        phase0 = float(scene.rng(rng_name).uniform(0, 2 * math.pi))
        scene.add_samples(0, math.sqrt(undb(power)) * iq
                          * np.exp(1j * (2 * math.pi * off_hz * t_all[: len(iq)] + phase0)))
        return power, len(iq)

    # --- The control channel: continuous 4FSK, DMR BS-data sync, CSBKs with the masked CRC.
    frames_needed = int(math.ceil(n * rate / fs / tk.DMR_FRAME_DIBITS)) + 1
    cc_dibits, dmr_truth = _dmr_stream(p, cap.center_hz, frames_needed, scene.rng("csbk"))
    cc_ch = int(p["cc_channel"])
    cc_off = cc_ch * raster
    cc_power, _ = place_4fsk(cc_off, cc_dibits, float(p["cc_snr_db"]), "dmr-cc",
                             tk.DMR_DEVIATIONS_HZ)
    cc_f = cap.center_hz + cc_off
    scene.annotate(
        0, n, cc_f - dmr_bw / 2, cc_f + dmr_bw / 2, "trunk-control-channel",
        scene.emission_truth(
            cap, cc_off, dmr_bw, cc_power,
            kind="trunk-control-channel", modulation="4fsk", levels=4,
            symbol_rate_bd=rate, duty_cycle=1.0, fco=1.0,
            raster_hz=raster, raster_channel=cc_ch,
            nominal_center_hz=cap.center_hz + cc_ch * raster,
            is_control_channel=True, confirmable=True,
            frame=tk.DMR_FRAME_SPEC,
            n_frames=frames_needed,
            sync_hex=tk.DMR_BS_DATA_SYNC_HEX,
            protocol="dmr-tier3",
        ),
    )

    # --- The decoy: continuous, on-raster, 4FSK, unframed. Must never be confirmed -- and now it
    # must be rejected by BOTH framings rather than only by the one the hunt used to try.
    n_dibits = int(math.ceil(n * rate / fs)) + 1
    dc_ch = int(p["decoy_channel"])
    dc_off = dc_ch * raster
    dc_power, _ = place_4fsk(dc_off, tk.continuous_data_dibits(scene.rng("decoy"), n_dibits),
                             float(p["decoy_snr_db"]), "decoy-phase", tk.DMR_DEVIATIONS_HZ)
    dc_f = cap.center_hz + dc_off
    scene.annotate(
        0, n, dc_f - dmr_bw / 2, dc_f + dmr_bw / 2, "continuous-data",
        scene.emission_truth(
            cap, dc_off, dmr_bw, dc_power,
            kind="continuous-data", modulation="4fsk", levels=4,
            symbol_rate_bd=rate, duty_cycle=1.0, fco=1.0,
            raster_hz=raster, raster_channel=dc_ch,
            is_control_channel=False, confirmable=False,
            why_not="continuous 4FSK with no frame sync and no CRC-valid block, under either "
                    "framing: passes FCO candidacy, fails sync+CRC confirmation",
        ),
    )

    # --- Bursty NBFM neighbours, so the control channel is found among traffic.
    dev, tone = float(p["fm_deviation_hz"]), float(p["audio_tone_hz"])
    nbfm_bw = 2 * (dev + tone)
    n_bursts = 0
    for ch in [int(c) for c in p["nbfm_channels"]]:
        off = ch * raster
        if abs(off) + nbfm_bw / 2 > fs / 2:
            raise ValueError(f"nbfm channel {ch} does not fit inside the sample rate")
        power = _power(cap, float(p["nbfm_snr_db"]), nbfm_bw)
        amp = math.sqrt(undb(power))
        r = scene.rng("nbfm", ch)
        for b0, b1 in _on_off(r, span_s, float(p["burst_mean_on_s"]), float(p["burst_mean_off_s"])):
            i0, i1 = int(round(b0 * fs)), min(n, int(round(b1 * fs)))
            if i1 <= i0:
                continue
            phase0, audio_phase = (float(v) for v in r.uniform(0, 2 * math.pi, 2))
            tt = scene.time(i0, i1 - i0)
            phase = (phase0 + 2 * math.pi * off * tt
                     + (dev / tone) * np.sin(2 * math.pi * tone * (tt - b0) + audio_phase))
            scene.add_samples(i0, amp * np.exp(1j * phase))
            f = cap.center_hz + off
            scene.annotate(
                i0, i1 - i0, f - nbfm_bw / 2, f + nbfm_bw / 2, "nbfm-burst",
                scene.emission_truth(
                    cap, off, nbfm_bw, power, kind="nbfm-burst", modulation="nbfm",
                    deviation_hz=dev, audio_tone_hz=tone,
                    raster_hz=raster, raster_channel=ch,
                    is_control_channel=False, confirmable=False,
                    burst_start_s=b0, burst_duration_s=b1 - b0,
                ),
            )
            n_bursts += 1

    # --- THE TRAP: real keyings on the frequency an assumed band plan would produce.
    trap_hz = float(dmr_truth["wrong_frequency_if_lpcn_assumed_hz"])
    trap_off = trap_hz - cap.center_hz
    if abs(trap_off) + dmr_bw / 2 > fs / 2:
        raise ValueError("the trap channel does not fit inside the sample rate")
    v_power = _power(cap, float(p["trap_snr_db"]), dmr_bw)
    v_amp = math.sqrt(undb(v_power))
    on_s, off_s = float(p["trap_on_s"]), float(p["trap_off_s"])
    if on_s <= 0 or off_s <= 0:
        raise ValueError("a keying needs a positive on and off time")
    rr = scene.rng("trap")
    keyings: list[tuple[float, float]] = []
    t_key = 0.0
    while t_key < span_s:
        i0, i1 = int(round(t_key * fs)), min(n, int(round((t_key + on_s) * fs)))
        if i1 > i0:
            nd = int(math.ceil((i1 - i0) * rate / fs)) + 1
            iq = tk.c4fm(tk.continuous_data_dibits(rr, nd), fs, rate,
                         deviations=tk.DMR_DEVIATIONS_HZ)[: i1 - i0]
            phase0 = float(rr.uniform(0, 2 * math.pi))
            tt = scene.time(i0, len(iq))
            scene.add_samples(i0, v_amp * iq * np.exp(1j * (2 * math.pi * trap_off * tt + phase0)))
            b0, b1 = i0 / fs, (i0 + len(iq)) / fs
            keyings.append((b0, b1))
            scene.annotate(
                i0, len(iq), trap_hz - dmr_bw / 2, trap_hz + dmr_bw / 2, "trunk-voice-keying",
                scene.emission_truth(
                    cap, trap_off, dmr_bw, v_power,
                    kind="trunk-voice-keying", modulation="4fsk", levels=4,
                    symbol_rate_bd=rate, raster_hz=raster,
                    raster_channel=int(round(trap_off / raster)),
                    is_control_channel=False, confirmable=False,
                    burst_start_s=b0, burst_duration_s=b1 - b0,
                    trap="the frequency an ASSUMED DMR band plan would resolve the granted LPCN "
                         "to; following this channel would produce a convincing call record that "
                         "no message supports",
                ),
            )
        t_key += on_s + off_s
    dmr_truth.update({
        "trap_offset_hz": trap_off,
        "trap_keyings_s": [[a, b] for a, b in keyings],
        "trap_on_s": on_s,
        "trap_off_s": off_s,
        "sample_rate_hz": fs,
    })

    scene.scenario_truth["trunking"] = {
        "raster_hz": raster,
        "raster_origin_hz": cap.center_hz,
        "control_channel": {
            "rf_center_hz": cc_f,
            "raster_channel": cc_ch,
            "modulation": "4fsk",
            "symbol_rate_bd": rate,
            "bandwidth_hz": dmr_bw,
            "duty_cycle": 1.0,
            "sync_hex": tk.DMR_BS_DATA_SYNC_HEX,
            "expected_confirmed": True,
        },
        "continuous_decoy": {
            "rf_center_hz": dc_f,
            "raster_channel": dc_ch,
            "expected_confirmed": False,
            "why_not": "no frame sync, no CRC-valid block, under either framing",
        },
        "nbfm_channels": [int(c) for c in p["nbfm_channels"]],
        "n_nbfm_bursts": n_bursts,
        "frame": tk.DMR_FRAME_SPEC,
        "dmr": dmr_truth,
    }
    return [scene], {}


# =============================================================================================
# NXDN Type-C (T-345)
# =============================================================================================

TRUNK_NXDN_DEFAULTS: dict[str, Any] = {
    "sample_rate": 500e3,
    # 800 MHz, the same band as the P25 and DMR scenes, so all three are directly comparable.
    "center_hz": 851.0125e6,
    "duration_s": 1.0,
    "raster_hz": tk.LMR_RASTER_HZ,
    "cc_channel": 3,
    "decoy_channel": -5,
    "nbfm_channels": [-2, 7, 11, -9],
    "cc_snr_db": 20.0,
    "decoy_snr_db": 20.0,
    "nbfm_snr_db": 18.0,
    "symbol_rate_bd": tk.NXDN_SYMBOL_RATE_BD,
    "fm_deviation_hz": 2500.0,
    "audio_tone_hz": 1000.0,
    "burst_mean_on_s": 0.08,
    "burst_mean_off_s": 0.12,
    "noise_dbfs": -40.0,
    "calibration_k_db": -70.0,
    "start_utc": DEFAULT_START_UTC,
    # --- THE TRAP.
    #
    # An NXDN Type-C assignment names a 10-bit Channel NUMBER (Sec 6.5.31, 1 to 1023) and the air
    # interface defines no mapping from one to hertz -- not one of its information elements is a
    # frequency. The map lives in the radio's configuration. So a Channel resolves to NOTHING, and a
    # decoder that quietly assumed the obvious plan (base = wherever the radio is tuned, step = the
    # 12.5 kHz LMR raster) would produce a frequency anyway.
    #
    # So the scene puts real, followable voice traffic exactly where that assumption points. A
    # guessing decoder does not merely print a wrong number: it allocates the channel, finds a
    # transmission, measures its boundaries and writes a completely convincing call record. Nothing
    # about the run would look wrong. The frequency is chosen FIRST and the Channel derived from it,
    # so the trap is baited by the same arithmetic the mistake would use.
    #
    # 851.0875 MHz is +75 kHz from the tuned centre -- well inside the window the radio holds -- and
    # lands on raster channel 6, clear of the control channel (3), the decoy (-5) and every NBFM
    # neighbour. It is deliberately NOT the DMR scene's trap, so a decoder cannot pass both by
    # refusing one number it was told about.
    "trap_target_hz": 851.0875e6,
    "assumed_spacing_hz": tk.LMR_RASTER_HZ,
    "grant_destination_id": 2468,
    "grant_source_id": 1357,
    # A SECOND assignment, an individual call, whose channel number baits the same mistake a second
    # time: under the assumed plan channel 11 points at 851.15 MHz, which is one of the bursty NBFM
    # neighbours below and is therefore genuinely transmitting. Two baited numbers, not one, and the
    # second one needs no traffic of its own because the scene's ordinary neighbours supply it.
    "second_channel": 11,
    "second_destination_id": 3690,
    "second_source_id": 2580,
    # The traffic on the trap channel: repeated keyings, like T-269's followed channel, so a
    # follower that got there would have real boundaries to measure. At 40 % duty it never reaches
    # control-channel candidacy (MIN_CC_FCO is 0.95).
    "trap_on_s": 0.10,
    "trap_off_s": 0.15,
    "trap_snr_db": 18.0,
    # The site's Radio Access Number (colour code), carried in every frame's SR header.
    "ran": 0x1B,
}


def _nxdn_stream(p: dict[str, Any], center_hz: float, n_frames: int,
                 rng: np.random.Generator) -> tuple[np.ndarray, dict[str, Any]]:
    """The NXDN control channel's outbound CAC stream, and the truth describing it.

    The cycle carries a site-information broadcast, a voice assignment onto the trap channel, its
    periodic duplicate (NXDN's late entry, which carries no source unit), a second voice assignment
    as an individual call, a data assignment, a service-information broadcast, and a message type
    this decoder does not name -- so the stream is not made only of messages it understands.
    """
    trap_hz = float(p["trap_target_hz"])
    spacing = float(p["assumed_spacing_hz"])
    steps = (trap_hz - center_hz) / spacing
    if steps != int(steps) or not 1 <= steps <= tk.NXDN_CHANNEL_MAX:
        raise ValueError(f"trap_target_hz {trap_hz} is not a whole step from the tuned centre")
    channel = int(steps)
    second = int(p["second_channel"])
    if second == channel:
        raise ValueError("the second assignment must name a different channel")

    ran = int(p["ran"])
    dst, src = int(p["grant_destination_id"]), int(p["grant_source_id"])
    s_dst, s_src = int(p["second_destination_id"]), int(p["second_source_id"])

    def payload() -> bytes:
        """Octets for a message whose own fields this project does not decode.

        Deliberately **not** zeros, for the reason the DMR scene records: a 4FSK receiver estimates
        its level centre and outer deviation from the symbol distribution, and a stream padded with
        zeros is a stream whose symbols pile onto one inner level. A real SITE_INFO carries a
        location ID, service flags and channel-structure information rather than seventeen zeros.
        """
        return bytes(int(v) for v in rng.integers(0, 256, 17))

    cycle = [
        ("site-info", tk.nxdn_message(tk.NXDN_MSG_SITE_INFO, payload(), ran)),
        ("vcall-assgn", tk.nxdn_message(
            tk.NXDN_MSG_VCALL_ASSGN,
            tk.nxdn_assignment_octets(tk.NXDN_CALL_BROADCAST, src, dst, channel), ran)),
        # The duplicate: how a radio joins a call already up. Its source is the specification's Null
        # Unit ID, because the message announces a channel rather than a caller.
        ("vcall-assgn-dup", tk.nxdn_message(
            tk.NXDN_MSG_VCALL_ASSGN_DUP,
            tk.nxdn_assignment_octets(tk.NXDN_CALL_BROADCAST, 0, dst, channel), ran)),
        ("vcall-assgn-individual", tk.nxdn_message(
            tk.NXDN_MSG_VCALL_ASSGN,
            tk.nxdn_assignment_octets(tk.NXDN_CALL_INDIVIDUAL, s_src, s_dst, second), ran)),
        ("dcall-assgn", tk.nxdn_message(
            tk.NXDN_MSG_DCALL_ASSGN,
            tk.nxdn_assignment_octets(tk.NXDN_CALL_BROADCAST, s_src, s_dst, second), ran)),
        ("srv-info", tk.nxdn_message(tk.NXDN_MSG_SRV_INFO, payload(), ran)),
        # A message type this decoder does not name.
        ("other", tk.nxdn_message(0x07, payload(), ran)),
    ]
    dibits = tk.nxdn_frames_from_messages([m for _, m in cycle], n_frames, rng)
    counts = {kind: sum(1 for i in range(n_frames) if cycle[i % len(cycle)][0] == kind)
              for kind, _ in cycle}
    truth = {
        "framing": "nxdn-fsw",
        "ran": ran,
        "grant_channel": channel,
        "grant_destination_id": dst,
        "grant_source_id": src,
        "second_channel": second,
        "second_destination_id": s_dst,
        "second_source_id": s_src,
        # The numbers nothing may ever report. Real traffic sits on both, so a decoder that assumed
        # a band plan would follow them and write calls that look entirely right.
        "wrong_frequency_if_channel_assumed_hz": trap_hz,
        "wrong_frequency_for_second_channel_hz": center_hz + spacing * second,
        "assumed_spacing_hz": spacing,
        "assumed_base_hz": center_hz,
        "frames_per_cycle": len(cycle),
        # The cycle itself, not only how often each kind landed: a short recording may not reach
        # every message in it, and a test that asserts on counts alone would then be asserting on
        # the duration rather than on the scene.
        "cycle": [kind for kind, _ in cycle],
        "counts": counts,
        "expected": {
            "protocol": "nxdn-type-c",
            "grants_decoded": True,
            "grants_mapped": False,
            "grant_reason": "no-channel-map",
            "calls": "none: an assignment with no frequency entitles no channel and no call",
            "why": "the NXDN air interface carries a 10-bit channel NUMBER and defines no mapping "
                   "from one to hertz; the map is configured in the radio, and this build has none",
        },
        "coding": tk.NXDN_FRAME_SPEC["coding"],
    }
    return dibits, truth


def trunk_nxdn_control_channel(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    """An NXDN Type-C control channel among the same traffic the P25 and DMR scenes use (T-345).

    Three things have to be true at once for this scene to mean anything:

    1. The hunt is **blind about the air interface**. Nothing tells it this is NXDN rather than P25
       or DMR; it must try the framings it knows and confirm on the one that matches. The same
       continuous, on-raster, unframed 4FSK decoy is present and must be rejected by all three.
    2. The decoder must **read the CAC properly** -- descrambled, deinterleaved, depunctured and
       Viterbi decoded -- because nothing short of that produces a CRC-valid block at all.
    3. It must **refuse to invent a frequency**, while real traffic sits exactly where the obvious
       guess points. See ``trap_target_hz``.
    """
    p = ctx.params
    fs = float(p["sample_rate"])
    n = _n(p)
    scene = ctx.scene(
        "trunk_nxdn_control_channel", fs, n,
        "hkpy.synth trunk_nxdn_control_channel: an NXDN Type-C outbound RCCH (frame sync word + "
        "LICH + fully coded CACs) carrying channel assignments whose channel numbers resolve to "
        "nothing, with real voice traffic parked where an assumed band plan would put them",
    )

    cap = scene.add_capture(0, n, float(p["center_hz"]), utc_plus(p["start_utc"], 0.0),
                            calibration_k_db=float(p["calibration_k_db"]))
    cap.floor_dbfs_per_hz = float(p["noise_dbfs"]) - db(fs)
    scene.add_samples(0, complex_noise(scene.rng("noise"), n, float(p["noise_dbfs"])))
    scene.add_floor(0, n, cap.floor_dbfs_per_hz)

    raster = float(p["raster_hz"])
    rate = float(p["symbol_rate_bd"])
    # Carson on NXDN's outer deviation plus the symbol rate.
    nxdn_bw = 2 * (2400.0 + rate / 2)
    t_all = scene.time(0, n)
    span_s = n / fs

    def place_4fsk(off_hz: float, dibits: np.ndarray, snr_db: float, rng_name: str) -> float:
        if abs(off_hz) + nxdn_bw / 2 > fs / 2:
            raise ValueError(f"an emission at {off_hz} Hz does not fit inside the sample rate")
        power = _power(cap, snr_db, nxdn_bw)
        iq = tk.c4fm(dibits, fs, rate, deviations=tk.NXDN_DEVIATIONS_HZ)[:n]
        phase0 = float(scene.rng(rng_name).uniform(0, 2 * math.pi))
        scene.add_samples(0, math.sqrt(undb(power)) * iq
                          * np.exp(1j * (2 * math.pi * off_hz * t_all[: len(iq)] + phase0)))
        return power

    # --- The control channel: continuous 4FSK, NXDN frame sync, real coded CACs.
    frames_needed = int(math.ceil(n * rate / fs / tk.NXDN_FRAME_DIBITS)) + 1
    cc_dibits, nxdn_truth = _nxdn_stream(p, cap.center_hz, frames_needed, scene.rng("cac"))
    cc_ch = int(p["cc_channel"])
    cc_off = cc_ch * raster
    cc_power = place_4fsk(cc_off, cc_dibits, float(p["cc_snr_db"]), "nxdn-cc")
    cc_f = cap.center_hz + cc_off
    scene.annotate(
        0, n, cc_f - nxdn_bw / 2, cc_f + nxdn_bw / 2, "trunk-control-channel",
        scene.emission_truth(
            cap, cc_off, nxdn_bw, cc_power,
            kind="trunk-control-channel", modulation="4fsk", levels=4,
            symbol_rate_bd=rate, duty_cycle=1.0, fco=1.0,
            raster_hz=raster, raster_channel=cc_ch,
            nominal_center_hz=cap.center_hz + cc_ch * raster,
            is_control_channel=True, confirmable=True,
            frame=tk.NXDN_FRAME_SPEC,
            n_frames=frames_needed,
            sync_hex=tk.NXDN_FSW_HEX,
            protocol="nxdn-type-c",
        ),
    )

    # --- The decoy: continuous, on-raster, 4FSK, unframed. Must be rejected by all three framings.
    n_dibits = int(math.ceil(n * rate / fs)) + 1
    dc_ch = int(p["decoy_channel"])
    dc_off = dc_ch * raster
    dc_power = place_4fsk(dc_off, tk.continuous_data_dibits(scene.rng("decoy"), n_dibits),
                          float(p["decoy_snr_db"]), "decoy-phase")
    dc_f = cap.center_hz + dc_off
    scene.annotate(
        0, n, dc_f - nxdn_bw / 2, dc_f + nxdn_bw / 2, "continuous-data",
        scene.emission_truth(
            cap, dc_off, nxdn_bw, dc_power,
            kind="continuous-data", modulation="4fsk", levels=4,
            symbol_rate_bd=rate, duty_cycle=1.0, fco=1.0,
            raster_hz=raster, raster_channel=dc_ch,
            is_control_channel=False, confirmable=False,
            why_not="continuous 4FSK with no frame sync and no valid block, under any of the "
                    "three framings: passes FCO candidacy, fails sync+CRC confirmation",
        ),
    )

    # --- Bursty NBFM neighbours, so the control channel is found among traffic. One of them sits on
    # raster channel 11, which is where an assumed band plan would put the second assignment.
    dev, tone = float(p["fm_deviation_hz"]), float(p["audio_tone_hz"])
    nbfm_bw = 2 * (dev + tone)
    n_bursts = 0
    for ch in [int(c) for c in p["nbfm_channels"]]:
        off = ch * raster
        if abs(off) + nbfm_bw / 2 > fs / 2:
            raise ValueError(f"nbfm channel {ch} does not fit inside the sample rate")
        power = _power(cap, float(p["nbfm_snr_db"]), nbfm_bw)
        amp = math.sqrt(undb(power))
        r = scene.rng("nbfm", ch)
        for b0, b1 in _on_off(r, span_s, float(p["burst_mean_on_s"]), float(p["burst_mean_off_s"])):
            i0, i1 = int(round(b0 * fs)), min(n, int(round(b1 * fs)))
            if i1 <= i0:
                continue
            phase0, audio_phase = (float(v) for v in r.uniform(0, 2 * math.pi, 2))
            tt = scene.time(i0, i1 - i0)
            phase = (phase0 + 2 * math.pi * off * tt
                     + (dev / tone) * np.sin(2 * math.pi * tone * (tt - b0) + audio_phase))
            scene.add_samples(i0, amp * np.exp(1j * phase))
            f = cap.center_hz + off
            scene.annotate(
                i0, i1 - i0, f - nbfm_bw / 2, f + nbfm_bw / 2, "nbfm-burst",
                scene.emission_truth(
                    cap, off, nbfm_bw, power, kind="nbfm-burst", modulation="nbfm",
                    deviation_hz=dev, audio_tone_hz=tone,
                    raster_hz=raster, raster_channel=ch,
                    is_control_channel=False, confirmable=False,
                    burst_start_s=b0, burst_duration_s=b1 - b0,
                ),
            )
            n_bursts += 1

    # --- THE TRAP: real keyings on the frequency an assumed band plan would produce.
    trap_hz = float(nxdn_truth["wrong_frequency_if_channel_assumed_hz"])
    trap_off = trap_hz - cap.center_hz
    if abs(trap_off) + nxdn_bw / 2 > fs / 2:
        raise ValueError("the trap channel does not fit inside the sample rate")
    v_power = _power(cap, float(p["trap_snr_db"]), nxdn_bw)
    v_amp = math.sqrt(undb(v_power))
    on_s, off_s = float(p["trap_on_s"]), float(p["trap_off_s"])
    if on_s <= 0 or off_s <= 0:
        raise ValueError("a keying needs a positive on and off time")
    rr = scene.rng("trap")
    keyings: list[tuple[float, float]] = []
    t_key = 0.0
    while t_key < span_s:
        i0, i1 = int(round(t_key * fs)), min(n, int(round((t_key + on_s) * fs)))
        if i1 > i0:
            nd = int(math.ceil((i1 - i0) * rate / fs)) + 1
            iq = tk.c4fm(tk.continuous_data_dibits(rr, nd), fs, rate,
                         deviations=tk.NXDN_DEVIATIONS_HZ)[: i1 - i0]
            phase0 = float(rr.uniform(0, 2 * math.pi))
            tt = scene.time(i0, len(iq))
            scene.add_samples(i0, v_amp * iq * np.exp(1j * (2 * math.pi * trap_off * tt + phase0)))
            b0, b1 = i0 / fs, (i0 + len(iq)) / fs
            keyings.append((b0, b1))
            scene.annotate(
                i0, len(iq), trap_hz - nxdn_bw / 2, trap_hz + nxdn_bw / 2, "trunk-voice-keying",
                scene.emission_truth(
                    cap, trap_off, nxdn_bw, v_power,
                    kind="trunk-voice-keying", modulation="4fsk", levels=4,
                    symbol_rate_bd=rate, raster_hz=raster,
                    raster_channel=int(round(trap_off / raster)),
                    is_control_channel=False, confirmable=False,
                    burst_start_s=b0, burst_duration_s=b1 - b0,
                    trap="the frequency an ASSUMED NXDN band plan would resolve the assigned "
                         "channel number to; following this channel would produce a convincing "
                         "call record that no message supports",
                ),
            )
        t_key += on_s + off_s
    nxdn_truth.update({
        "trap_offset_hz": trap_off,
        "trap_keyings_s": [[a, b] for a, b in keyings],
        "trap_on_s": on_s,
        "trap_off_s": off_s,
        "sample_rate_hz": fs,
    })

    scene.scenario_truth["trunking"] = {
        "raster_hz": raster,
        "raster_origin_hz": cap.center_hz,
        "control_channel": {
            "rf_center_hz": cc_f,
            "raster_channel": cc_ch,
            "modulation": "4fsk",
            "symbol_rate_bd": rate,
            "bandwidth_hz": nxdn_bw,
            "duty_cycle": 1.0,
            "sync_hex": tk.NXDN_FSW_HEX,
            "expected_confirmed": True,
        },
        "continuous_decoy": {
            "rf_center_hz": dc_f,
            "raster_channel": dc_ch,
            "expected_confirmed": False,
            "why_not": "no frame sync, no valid block, under any framing",
        },
        "nbfm_channels": [int(c) for c in p["nbfm_channels"]],
        "n_nbfm_bursts": n_bursts,
        "frame": tk.NXDN_FRAME_SPEC,
        "nxdn": nxdn_truth,
    }
    return [scene], {}
