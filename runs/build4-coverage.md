# Build 4 — opcode coverage of the v0 translation set

`REBUILD_SHADERS=D:\victory-data\shaders.rpf cargo run --release --bin translate -- coverage`
(a copy of `shaders.rpf` outside the game folder; raw output: [`build4-coverage.txt`](build4-coverage.txt)).

**The v0 set:** VS, PS and CS programs with one basic block that are agnostic (they never observe the wave
size): **2,881 programs** (PS 1,798, VS 951, CS 132), **173 distinct opcodes** (scheduling hints excluded).

**Greedy order** (each step adds the opcode that completes the most programs):

| opcodes implemented | programs fully covered |
|--:|--:|
| 26 | 30.0 % |
| 55 | 45.3 % |
| 87 | 76.7 % |
| 119 | 95.6 % |
| 143 | 99.6 % |
| 173 | 100 % |

**What this decides:**
- Translation is built opcode by opcode in this order, so each one added moves the most programs.
- `s_swappc_b64` is in 765 of these programs. These are vertex shaders calling their fetch subroutine, so
  "straight-line" includes one call. The translator inlines the fetch from the program's vertex-input
  description (`native::VsInput`), as the console's pipeline does.
- `v_cvt_pkrtz_f16_f32` (1,786) and `exp` (2,749) come first: almost every pixel shader packs its colour
  to half precision and exports it.
- `v_nop` appears in every program: it is a hint and will be dropped like `s_nop`.
