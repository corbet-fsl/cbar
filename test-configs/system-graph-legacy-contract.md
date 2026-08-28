# Native system graph legacy contract

This fixture records the deployed Lua/Cairo renderer inspected for the native
port. It is provenance, not a second runtime implementation.

- source revision: `d435d14b14f28a397a4bb52929096447ee793c02`
- `sys_graph.lua` SHA-256: `4910626be2eb4d27f13450c6bcba860da8f6d6a3c766d3ffe520ba22cd58f715`
- history: 40 samples
- cadence: 500 ms
- canvas height: 26 px
- CPU/RAM content canvas: 220 px, split into 99.5 px series around a
  1 px divider with 10 px on each side
- CPU/RAM divider origin in the bar canvas: 120.5 px = 11 px outer inset +
  99.5 px CPU series + 10 px internal pad
- optional content canvas: 84 px
- module inset: 11 px per side; adjacent cell gutter: 22 px
- labels: CPU, RAM, SWAP, IO, VPU, GPU, NPU, LAN, WLAN, WWAN, VPN
- scalar palette thresholds: white, yellow above 50%, red above 80%; RAM
  uses 85% and 95%
- scalar bars: 40-slot right-aligned history, baseline anchored, cap-height
  span multiplied by 1.6, at least one pixel per scalar sample
- network bars: RX above and TX below the centre line, shared logarithmic
  scale, TX alpha 0.45, zero traffic produces no bar, three-frame crossfade
- compact network cells omit the numeric tail below 100 px

The wide Rust geometry snapshot is the direct comparison to these dimensions.
The narrower snapshots are native responsive behavior and intentionally have no
legacy fixed-module equivalent.
