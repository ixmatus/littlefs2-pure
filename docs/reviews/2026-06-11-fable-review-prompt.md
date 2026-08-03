  You are working in /Users/parnell/Development/littlefs2-pure, a pure Rust no_std
  implementation of the LittleFS v2 filesystem. Read CLAUDE.md (repo root) and
  ~/.claude/CLAUDE.md first; both bind. The project memory index will load
  automatically; the entry "2026-06 deep review" is the context for this work.

  ## Mission

  Remediate the findings from the 2026-06 deep review. The findings live in two
  places:
  - Beads issues, label `review-v1.2.0`. In every Bash call first run:
      export BEADS_DIR="$HOME/.local/share/beads/littlefs2-pure-79beb6e5/.beads"
    (the zsh bd wrapper does not run in the Bash tool). `bd ready` shows the
    queue; P0 = Critical, P1 = High. Claim with `bd update <id> --claim`, close
    with `bd close <id>`, file anything you discover with
    `bd create ... --deps discovered-from:<id>`.
  - REVIEW-v1.2.0-2026-06-10.md (repo root) has the full detail per finding:
    failure scenario, oracle citation, remediation direction. The finding codes
    (C1..C9, H1..H8, M*, L*, V*) appear in each bead title and description.

  ## Work in slices, in this order

  Slice 1 — attribute subsystem (lfs-2dg/C1, lfs-3z8/C2, lfs-e7i/H5, lfs-inm/H6,
  then lfs-h1p/V8 as the pinning suite). Self-contained; start here.

  Slice 2 — interop breakers (lfs-cna/C3 the 0x3FF Delete tag, lfs-r88/H1 the
  Slice 2 — interop breakers (lfs-cna/C3 the 0x3FF Delete tag, lfs-r88/H1 the
  id-dense reader rejection, lfs-w7w/C4 latest-tag-wins gstate), together with
  lfs-761/V1: each fix lands with a C-written conformance vector that fails
  before the fix and passes after. C3 is a one-line writer fix whose vector
  matters more than the patch.

  Slice 3 — gstate/relocation family (lfs-fb2/C5, lfs-bkq/C6, lfs-njj/C7,
  lfs-gfm/H3, lfs-les/H4). HARD GATE: before writing any implementation code for
  this slice, write docs/decisions/0015-fs-resident-gstate.md deciding between
  (a) the C model: an Fs-resident gstate/gdelta maintained through commits, with
  a bounded relocation remap channel standing in for C's fixmlist, vs (b) point
  fixes per finding. Weigh the ADR-0006 stack budget and the no-alloc kernel
  rule. STOP after the ADR and ask Parnell to review it before implementing.

  Slice 4 — write-path safety (lfs-ay4/C8 append tail poison, lfs-0vy/C9
  in-flight CTZ exclusion, lfs-efe/H2 commit read-back).

  Slice 5 — test-stack honesty (lfs-xzt/H7, lfs-e2a/V3, lfs-1i3/M5, lfs-4d4/M4,
  lfs-wee/V7): make the torn sweeps assert pre-state-or-post-state, never skip
  unmountable images, and fail when the C verifier is missing. Do this slice
  even if time runs short; it hardens the gates the other slices rely on.

  Mediums and Lows: fix opportunistically when a slice touches their file;
  otherwise leave them claimed-unclaimed in the queue.

  ## Non-negotiable disciplines

  - Reproduce before fixing. Every finding gets a failing test FIRST, derived
    from the bead's failure scenario; the fix is done when it goes green. For
    interop findings the failing test is a conformance/roundtrip vector.
  - Findings marked CONTESTED or PARTIAL in the bead title had incomplete
    adversarial verification. Re-verify against the code before fixing; if a
    finding turns out wrong, close the bead with a note saying why instead of
    "fixing" a non-bug.
  - Bit accuracy: the C reference at tools/gen_vectors/littlefs/lfs.c is the
    behavioral oracle. It is an oracle, NOT a code template: match its on-disk
    behavior, derive the Rust from the spec and the failing test, never
    transcribe its code. Cite lfs.c function names in commit messages and ADRs.
  - Kernel rules: no alloc, no std, no panics reachable from disk bytes.
    Stack budget per ADR-0006; if a fix grows a stack array, say so explicitly.
  - Conformance and roundtrip suites must be green at every commit. If a fix
    changes written bytes, regenerate nothing silently: explain the byte-level
    change and prove C still mounts it (tests/roundtrip.rs).

  ## Git and process

  - Branch off main: review/2026-06-remediation (or one branch per slice).
  - SIGN EVERY COMMIT on this project (overrides the global unsigned-branch
    default). The GPG/YubiKey cache expires frequently: before each commit
    batch, prompt Parnell to touch the YubiKey, wait for confirmation, then
    commit. If signing fails, stop and ask; NEVER use --no-gpg-sign.
  - One concern per commit: a commit either refactors or changes behavior,
    never both. Failing test + fix may share a commit; the message explains why
    (cite the bead id and finding code). Co-Authored-By trailer on every commit.
  - Run `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings`
    before every commit; CI gates both.
  - Per-slice verification gate before declaring a slice done:
      cargo test
      cargo test --no-default-features
      cargo build --target thumbv6m-none-eabi --no-default-features
      cargo clippy --all-targets -- -D warnings
    Run the conformance and roundtrip suites explicitly. NEVER run the fuzzer
    locally (build/list ok). cargo kani is optional locally (CI runs it,
    360s timeout); if a fix touches tag/meta dispatch, do run the relevant
    harness.
  - No PRs, no pushes without asking. Merges to main are local signed merges
    Parnell performs; when a slice is done, stop, summarize, and surface the
    draft merge message instead of merging.
  - Close each bead on completion with a pointer to the commit. If a slice
    produces a durable design decision, the ADR is the deliverable and the bead
    closes pointing at it.
  - ADRs and doc comments follow the prose rules in ~/.claude/CLAUDE.md: no
    hyphens or em dashes in prose, subjects up front, concrete verbs.

  Start with Slice 1: run `bd ready`, claim lfs-2dg, read the finding section
  C1 in REVIEW-v1.2.0-2026-06-10.md, and write the failing attribute-compaction
  test first.
