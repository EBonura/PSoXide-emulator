# Guest card transport regression

These fault journeys supplement Hollow Knight's strict visual journey. They
use completed, immutable guest images with the live and latched SDK transports.
The generated card states and bounded pad tapes are committed in `fixtures/`.
The guest discs and matching link maps remain external. Prepare `fixtures/old/`
and `fixtures/new/` beside these journeys, or copy the complete case directory
into private scratch and prepare those disc folders there. Keep each input
fixture unchanged and use a fresh report directory.

| fixture | SHA256 |
|---|---|
| `old/hk-psx.bin` | `f11a7de4490c7bbd7078367880683d5e0732124ccd99a01eb861b55661a57eaf` |
| `old/hk-psx.cue` | `847e6ea3d0762d2163e0616a19c01b3ae85c469748318c588e14656bbc7f98fb` |
| `old/hk-psx.map` | `d9022c4092c1c808c3cf6448d98e25b7de50a3487cf2b3b92284d330b5b201d9` |
| `new/hk-psx.bin` | `f3fdc3e9094acf86eeb0ecddbb9489c82e8747e0200627c02c7d097d1e4726d5` |
| `new/hk-psx.cue` | `847e6ea3d0762d2163e0616a19c01b3ae85c469748318c588e14656bbc7f98fb` |
| `new/hk-psx.map` | `84bc91cf8b420f16624a05b3c2a677bc136f31d45b75e7fe627dc9f4ce09c33e` |
| `town.mcd` | `1aa6c20210c2ef1134f20d30e82bf5a78cefea8f84ff349dd0e95f9f62391dd8` |
| `seeded.mcd` | `685f0cc63e192c78483e5c472203b6c749ff09dbc70efced6a70712fa4a747af` |
| `bench.pxtape` | `802effb2de89e6d700f7748dbc49a82f91cd6bcde9e1638674682f6e7541f687` |
| `load.pxtape` | `9d5e96d359db50014e7aff00e2ced0e3f3b44966c11bdbe72f826d2260344028` |

Both disc folders also need their matching `hk-psx.cue` and `hk-psx.map`.
`bench.pxtape` is the game's bench-save input route. `load.pxtape` contains
its first 100 poll samples with the binary count updated, so the reboot checks
load a record without making another save. `seeded.mcd` is the completed
latched guest save from `town.mcd`, holding sequences 1 and 2.

Run each journey through the same gate binary and execution tier:

```sh
psoxide-gate run irq-new.toml --no-hw --out reports/irq-new
psoxide-gate run irq-old.toml --no-hw --out reports/irq-old
psoxide-gate run cut-early.toml --no-hw --out reports/cut-early
psoxide-gate run cut-middle.toml --no-hw --out reports/cut-middle
psoxide-gate run cut-checksum.toml --no-hw --out reports/cut-checksum
psoxide-gate run cut-retired.toml --no-hw --out reports/cut-retired
```

The IRQ action raises one diagnostic VBlank on read command 82, accepted byte
82, the first read after the initial 100-poll load. Its requested lead is 64
cycles before ACK, delivered at the first retired instruction boundary on or
after that target. A requested target preceding the observable ACK schedule
fails. The report must show the actual IRQ vector/return covering the selected
44-cycle pulse. The old guest must fail its save assertions and keep the
original card; the fixed guest must complete its save and reboot without a
card fault. This is injected fault evidence. The current emulator's natural
phase lets the old image save successfully and is a separate observation.

The first three cuts target data bytes 7, 70 and 134 of the third write, after
the title/icon sectors have committed and before the payload sector's checksum.
The fourth cuts at the sixth write's checksum, after the new directory entry
has been published and the old same-name entry retired. Every case reboots
from persisted bytes and enters gameplay with wallet 6, no save faults, and no
new write. The first three must read sequence 2 with next-copy 0; the last must
read sequence 3 with next-copy 1. These are actual guest globals, resolved
through each exact link map. `HK_SAVE_SLOT` only changes on writes and cannot
prove which copy the guest loaded.

Keep the old guest's nonzero exit as the negative control. All four cuts and
the fixed IRQ journey must exit zero. Retain the HTML, raw card images and full
hashes, plus the gate binary hash and execution tier. Run the ordinary strict
journey separately for CPU 1x and hardware 1x/3x contacts; CPU 3x is its enlarged
software reference. Never bless or replace a library disc from these fault
checks alone.
