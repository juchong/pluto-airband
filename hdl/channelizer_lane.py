#!/usr/bin/env python3
"""Time-multiplexed channelizer lane prototype (handoff §4.2 / §7 step 7).

The feasibility study (``feasibility_25ch.py``) showed that 25 AM channels fit the
Z-7010 only if the per-channel datapaths are **time-multiplexed**: the PL runs at
62.5 MHz while each channel needs only ~tens of ksps, so one physical datapath can
serve many channels by iterating over them between input samples (§4.2).

This module builds one such **lane** and proves the idea in simulation. A single
NCO + complex-mixer + CIC-decimator datapath is shared across ``n_channels``
channels; all per-channel state (NCO phase, CIC integrator/comb registers,
decimation counter) lives in addressable arrays indexed by the channel counter.
One wideband input sample is broadcast to every channel; the lane processes the
channels one-per-cycle, so it sustains the channel set as long as

    n_channels  <=  cycles_between_input_samples = F_clk / R_in.

Datapath per channel:

    wideband IQ ─► [NCO mix: × e^(-jθ_c)] ─► [CIC decimate by R] ─► baseband IQ_c

The NCO uses a shared signed sine ROM (quarter-wave offset for cosine). The mixer
is a complex multiply (downconversion). The CIC is the same multiplier-free
cascaded-integrator-comb used in the AM back-end (``am_audio.CICDecimator``), but
with per-channel state so the one datapath is reused.

What this prototype is / isn't:
  * IS: a bit-exact, resource-measurable realization of the time-multiplexed
    channelization the feasibility model assumed (validates the LUT/FF/DSP/BRAM
    per-lane numbers in real HDL, via ``synth_estimate``-style Yosys).
  * ISN'T: the final channel filter. A CIC has passband droop; the real lane adds
    a short cleanup/compensation FIR (the ``FIR_CLEANUP`` term in the model) and a
    shared front-end decimator. Those are the next increment.

Run:  python hdl/channelizer_lane.py
"""
from __future__ import annotations

import math
import pathlib

import numpy as np
from amaranth import Array, Cat, Const, Elaboratable, Module, Signal, signed
from amaranth.sim import Simulator

from ddc_tune_decimate import SYNC_PERIOD

OUT_DIR = pathlib.Path(__file__).parent / "out"


def sine_rom(addr_bits: int, width: int) -> list[int]:
    """Signed full-wave sine table: ``rom[k] = round(A*sin(2*pi*k/2**addr_bits))``."""
    n = 1 << addr_bits
    amp = (1 << (width - 1)) - 1
    return [int(round(amp * math.sin(2 * math.pi * k / n))) for k in range(n)]


class TdmDdcLane(Elaboratable):
    """One time-multiplexed DDC lane: NCO + complex mixer + CIC, shared over N ch.

    Control / data interface (all in the ``sync`` domain):

    Config (load a channel's NCO frequency word):
        freq_wren  : Signal(), in
        freq_waddr : channel index, in
        freq_wdata : signed(nco_width), in   -- cycles/sample × 2**nco_width

    Input (one wideband complex sample, broadcast to all channels):
        in_valid   : Signal(), in    -- pulse to start a processing pass
        re_in,im_in: signed(in_width), in
        busy       : Signal(), out   -- high while iterating channels (ignore in_valid)

    Output (decimated baseband sample for one channel):
        out_valid  : Signal(), out
        out_chan   : channel index, out
        out_re,out_im : signed(acc_width), out   -- full-CIC-gain (caller scales)

    Parameters
    ----------
    n_channels : number of channels multiplexed onto this datapath.
    decimation : CIC decimation factor R (per channel).
    in_width   : signed wideband input sample width.
    nco_width  : NCO phase-accumulator width.
    lut_addr_bits : sine ROM address bits (phase quantization).
    lut_width  : signed sine ROM sample width (sets mixer normalization shift).
    stages     : CIC order N.
    """

    def __init__(self, *, n_channels: int, decimation: int, in_width: int = 12,
                 nco_width: int = 24, lut_addr_bits: int = 10, lut_width: int = 12,
                 stages: int = 3):
        assert n_channels >= 1 and decimation >= 2 and stages >= 1
        assert lut_addr_bits >= 2
        self.n_channels = n_channels
        self.decimation = decimation
        self.in_width = in_width
        self.nco_width = nco_width
        self.lut_addr_bits = lut_addr_bits
        self.lut_width = lut_width
        self.stages = stages

        self.mix_shift = lut_width - 1
        self.mix_width = in_width + 2                  # |x·cos+y·sin|/A <= ~sqrt2·|x|
        self.growth = math.ceil(stages * math.log2(decimation))
        self.acc_w = self.mix_width + self.growth

        self.freq_wren = Signal()
        self.freq_waddr = Signal(range(n_channels))
        self.freq_wdata = Signal(signed(nco_width))

        self.in_valid = Signal()
        self.re_in = Signal(signed(in_width))
        self.im_in = Signal(signed(in_width))
        self.busy = Signal()

        self.out_valid = Signal()
        self.out_chan = Signal(range(n_channels))
        self.out_re = Signal(signed(self.acc_w), reset_less=True)
        self.out_im = Signal(signed(self.acc_w), reset_less=True)

    # -- bit-exact reference model -----------------------------------------
    def model(self, samples, freqs):
        """Reference output per channel for a wideband IQ input sequence.

        ``samples`` : iterable of (re, im) ints (the broadcast wideband stream).
        ``freqs``   : per-channel signed NCO frequency words (cycles/sample
                      × 2**nco_width).
        Returns ``{chan: (out_re_array, out_im_array)}`` matching the hardware,
        using exactly the hardware's fixed-point arithmetic.
        """
        rom = sine_rom(self.lut_addr_bits, self.lut_width)
        nmask = (1 << self.nco_width) - 1
        amask = (1 << self.lut_addr_bits) - 1
        quarter = 1 << (self.lut_addr_bits - 2)
        shift_phase = self.nco_width - self.lut_addr_bits
        out = {}
        for c, f in enumerate(freqs):
            phase = 0
            mi, mq = [], []
            for (re, im) in samples:
                a = (phase >> shift_phase) & amask
                s = rom[a]
                co = rom[(a + quarter) & amask]
                # downconvert: (re+j im)·(cos - j sin)
                oi = (re * co + im * s) >> self.mix_shift
                oq = (im * co - re * s) >> self.mix_shift
                mi.append(oi)
                mq.append(oq)
                phase = (phase + (f & nmask)) & nmask
            out[c] = (self._cic_model(mi), self._cic_model(mq))
        return out

    def _cic_model(self, x):
        acc = np.asarray(x, dtype=np.int64)
        for _ in range(self.stages):
            acc = np.cumsum(acc)
        dec = acc[self.decimation - 1::self.decimation].copy()
        for _ in range(self.stages):
            dec = dec - np.concatenate(([np.int64(0)], dec[:-1]))
        return dec

    # -- hardware ----------------------------------------------------------
    def elaborate(self, platform):
        m = Module()
        C, S = self.n_channels, self.stages

        # Shared sine ROM (read-only Array of constants).
        rom = Array(Const(v, signed(self.lut_width))
                    for v in sine_rom(self.lut_addr_bits, self.lut_width))

        # Per-channel state (addressable by the channel counter).
        phase = Array(Signal(self.nco_width, name=f"phase{c}") for c in range(C))
        freq = Array(Signal(signed(self.nco_width), name=f"freq{c}")
                     for c in range(C))
        deccnt = Array(Signal(range(self.decimation), name=f"deccnt{c}")
                       for c in range(C))
        integ_i = [Array(Signal(signed(self.acc_w), name=f"ii{st}_{c}")
                         for c in range(C)) for st in range(S)]
        integ_q = [Array(Signal(signed(self.acc_w), name=f"iq{st}_{c}")
                         for c in range(C)) for st in range(S)]
        comb_i = [Array(Signal(signed(self.acc_w), name=f"ci{st}_{c}")
                        for c in range(C)) for st in range(S)]
        comb_q = [Array(Signal(signed(self.acc_w), name=f"cq{st}_{c}")
                        for c in range(C)) for st in range(S)]

        # NCO frequency config write.
        with m.If(self.freq_wren):
            m.d.sync += freq[self.freq_waddr].eq(self.freq_wdata)

        # Latched wideband sample + channel iterator.
        xi = Signal(signed(self.in_width))
        xq = Signal(signed(self.in_width))
        chan = Signal(range(C))
        proc = Signal()                      # processing a channel this cycle

        m.d.comb += self.busy.eq(proc)
        m.d.sync += self.out_valid.eq(0)     # default; pulsed below

        with m.If(~proc & self.in_valid):
            m.d.sync += [xi.eq(self.re_in), xq.eq(self.im_in),
                         chan.eq(0), proc.eq(1)]

        # ---- the shared datapath, evaluated for channel `chan` ----
        amask = (1 << self.lut_addr_bits) - 1
        quarter = 1 << (self.lut_addr_bits - 2)
        sin_addr = Signal(self.lut_addr_bits)
        cos_addr = Signal(self.lut_addr_bits)
        m.d.comb += [
            sin_addr.eq(phase[chan][self.nco_width - self.lut_addr_bits:]),
            cos_addr.eq(sin_addr + quarter),     # +90 deg, wraps mod 2**addr_bits
        ]
        sin_v = Signal(signed(self.lut_width))
        cos_v = Signal(signed(self.lut_width))
        m.d.comb += [sin_v.eq(rom[sin_addr]), cos_v.eq(rom[cos_addr])]

        # complex downconversion, normalized by the ROM amplitude (>> mix_shift)
        prod_i = Signal(signed(self.in_width + self.lut_width + 1))
        prod_q = Signal(signed(self.in_width + self.lut_width + 1))
        m.d.comb += [
            prod_i.eq(xi * cos_v + xq * sin_v),
            prod_q.eq(xq * cos_v - xi * sin_v),
        ]
        mix_i = Signal(signed(self.mix_width))
        mix_q = Signal(signed(self.mix_width))
        m.d.comb += [mix_i.eq(prod_i >> self.mix_shift),
                     mix_q.eq(prod_q >> self.mix_shift)]

        # integrator cascade (combinational chain over this channel's state)
        new_ii, new_iq = [], []
        chain_i, chain_q = mix_i, mix_q
        for st in range(S):
            ni = Signal(signed(self.acc_w), name=f"new_ii{st}")
            nq = Signal(signed(self.acc_w), name=f"new_iq{st}")
            m.d.comb += [ni.eq(integ_i[st][chan] + chain_i),
                         nq.eq(integ_q[st][chan] + chain_q)]
            new_ii.append(ni)
            new_iq.append(nq)
            chain_i, chain_q = ni, nq

        # decimation strobe for this channel
        dec_stb = Signal()
        m.d.comb += dec_stb.eq(proc & (deccnt[chan] == self.decimation - 1))

        # comb cascade (only meaningful on dec_stb), fed by the newest integrator
        comb_out_i, comb_out_q = chain_i, chain_q   # = new_ii[-1], new_iq[-1]
        cin_i, cin_q = [], []
        for st in range(S):
            di = Signal(signed(self.acc_w), name=f"comb_di{st}")
            dq = Signal(signed(self.acc_w), name=f"comb_dq{st}")
            m.d.comb += [di.eq(comb_out_i - comb_i[st][chan]),
                         dq.eq(comb_out_q - comb_q[st][chan])]
            cin_i.append(comb_out_i)
            cin_q.append(comb_out_q)
            comb_out_i, comb_out_q = di, dq

        with m.If(proc):
            # advance NCO and write back integrator state for this channel
            m.d.sync += phase[chan].eq(phase[chan] + freq[chan])
            for st in range(S):
                m.d.sync += [integ_i[st][chan].eq(new_ii[st]),
                             integ_q[st][chan].eq(new_iq[st])]
            with m.If(dec_stb):
                m.d.sync += deccnt[chan].eq(0)
                for st in range(S):
                    m.d.sync += [comb_i[st][chan].eq(cin_i[st]),
                                 comb_q[st][chan].eq(cin_q[st])]
                m.d.sync += [self.out_valid.eq(1), self.out_chan.eq(chan),
                             self.out_re.eq(comb_out_i),
                             self.out_im.eq(comb_out_q)]
            with m.Else():
                m.d.sync += deccnt[chan].eq(deccnt[chan] + 1)

            # advance / finish the channel sweep
            with m.If(chan == C - 1):
                m.d.sync += proc.eq(0)
            with m.Else():
                m.d.sync += chan.eq(chan + 1)

        return m


# ---------------------------------------------------------------------------
# Verification
# ---------------------------------------------------------------------------

def _run_lane(dut: TdmDdcLane, samples, freqs):
    """Drive the lane over a wideband IQ stream; collect per-channel outputs."""
    out = {c: ([], []) for c in range(dut.n_channels)}

    async def bench(ctx):
        # load NCO frequencies
        ctx.set(dut.freq_wren, 1)
        for c, f in enumerate(freqs):
            ctx.set(dut.freq_waddr, c)
            ctx.set(dut.freq_wdata, int(f))
            await ctx.tick()
        ctx.set(dut.freq_wren, 0)
        await ctx.tick()

        for (re, im) in samples:
            ctx.set(dut.re_in, int(re))
            ctx.set(dut.im_in, int(im))
            ctx.set(dut.in_valid, 1)
            await ctx.tick()
            ctx.set(dut.in_valid, 0)
            # let the lane sweep all channels (busy de-asserts when done)
            while ctx.get(dut.busy):
                if ctx.get(dut.out_valid):
                    cc = ctx.get(dut.out_chan)
                    out[cc][0].append(ctx.get(dut.out_re))
                    out[cc][1].append(ctx.get(dut.out_im))
                await ctx.tick()
            if ctx.get(dut.out_valid):     # last channel's strobe lands here
                cc = ctx.get(dut.out_chan)
                out[cc][0].append(ctx.get(dut.out_re))
                out[cc][1].append(ctx.get(dut.out_im))

    sim = Simulator(dut)
    sim.add_clock(SYNC_PERIOD)
    sim.add_testbench(bench)
    sim.run()
    return {c: (np.array(i, np.int64), np.array(q, np.int64))
            for c, (i, q) in out.items()}


def _verify_bitexact():
    dut = TdmDdcLane(n_channels=4, decimation=8, in_width=12, nco_width=24,
                     lut_addr_bits=10, lut_width=12, stages=3)
    rng = np.random.default_rng(7)
    n = 800
    lim = 2 ** (dut.in_width - 1) - 1
    samples = rng.integers(-lim, lim, size=(n, 2)).astype(np.int64)
    # four arbitrary channel tunings (cycles/sample × 2**nco_width)
    fr = [int(round(f * 2 ** dut.nco_width)) for f in (0.05, -0.1, 0.2, 0.0)]

    hw = _run_lane(dut, samples, fr)
    ref = dut.model([tuple(s) for s in samples], fr)
    for c in range(dut.n_channels):
        for k in (0, 1):
            g, e = hw[c][k], ref[c][k]
            kk = min(len(g), len(e))
            assert kk > 0, f"no output for channel {c}"
            np.testing.assert_array_equal(g[:kk], e[:kk])
    return dut.n_channels, dut.acc_w, len(ref[0][0])


def _demo_tuning():
    """Functional check: each channel tunes to its own tone -> baseband; the lane
    rejects the other channels' tones (CIC selectivity)."""
    dut = TdmDdcLane(n_channels=4, decimation=16, in_width=12, nco_width=24,
                     lut_addr_bits=10, lut_width=12, stages=3)
    tones = [0.03, 0.11, -0.07, 0.22]          # cyc/sample, one per channel
    amps = [1500, 1500, 1500, 1500]
    n = 4096
    nn = np.arange(n)
    iq = np.zeros(n, complex)
    for f, a in zip(tones, amps):
        iq += a * np.exp(1j * 2 * np.pi * f * nn)
    iq /= len(tones)                            # keep within input range
    samples = np.stack([np.round(iq.real), np.round(iq.imag)], 1).astype(np.int64)
    fr = [int(round(f * 2 ** dut.nco_width)) for f in tones]

    hw = _run_lane(dut, samples, fr)
    # for each channel, |mean baseband| (tuned tone -> DC) vs AC ripple
    stats = {}
    for c in range(dut.n_channels):
        i, q = hw[c]
        z = (i + 1j * q)[len(i) // 4:]          # drop startup transient
        dc = np.mean(z)
        ripple = np.std(z) / (np.abs(dc) + 1e-9)
        stats[c] = (np.abs(dc), ripple)
    return tones, stats, hw


def main():
    C, acc_w, nout = _verify_bitexact()
    print(f"[bit-exact] TDM lane HW == reference model: PASS  "
          f"({C} channels share one datapath, {nout} decimated samples/ch, "
          f"acc width {acc_w} b)")

    tones, stats, hw = _demo_tuning()
    print("\n[tuning] each channel tunes its own tone to baseband (CIC selectivity):")
    ok = True
    for c, (mag, ripple) in stats.items():
        print(f"  ch{c}  tone {tones[c]:+.3f} cyc/s  ->  |DC| {mag:8.0f}  "
              f"ripple/|DC| {ripple*100:5.1f}%")
        ok = ok and ripple < 0.25
    assert ok, "a channel did not produce a clean baseband (tuning/selectivity)"
    print("\nPASS: one time-multiplexed datapath channelizes N channels "
          "(bit-exact) and tunes each to baseband.")

    _save_plot(tones, hw)


def _save_plot(tones, hw):
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    OUT_DIR.mkdir(exist_ok=True)
    n = len(hw)
    fig, ax = plt.subplots(n, 1, figsize=(9, 2.0 * n), sharex=True)
    for c in range(n):
        i, q = hw[c]
        ax[c].plot(i, label="I")
        ax[c].plot(q, label="Q")
        ax[c].set_ylabel(f"ch{c}\n{tones[c]:+.3f}")
        ax[c].grid(True, alpha=0.3)
        if c == 0:
            ax[c].set_title("TDM channelizer lane: per-channel baseband output "
                            "(one shared datapath)")
        ax[c].legend(loc="upper right", fontsize=8)
    ax[-1].set_xlabel("decimated sample")
    fig.tight_layout()
    path = OUT_DIR / "channelizer_lane.png"
    fig.savefig(path, dpi=110)
    print(f"Saved plot: {path}")


if __name__ == "__main__":
    main()
