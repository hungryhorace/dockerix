# Guest registers lose their values: r8 and fp

Found from the test suite of a RISC iX cross-build project, the work
that drove this one (private; "the project" below). Found to be
Arculator's CPU core: a prefetch abort taken one pipeline stage early
skips the instruction before a page boundary. Fixed by
`patches/arculator/0004-prefetch-abort-pipeline.patch`; see "The cause" below.
The rest of this note is the investigation as it stood before that.

## What happens

A RISC iX program computes a value into a callee-saved register, calls
some functions, and reads the register back changed, although no
instruction it runs writes that register. Two registers have been seen
to go wrong: `r8` and `r11` (`fp`). Both are in the ARM2/3 FIQ bank
(r8–r14), which is the first thing to suspect: a save or restore of the
user registers around a mode change that takes the wrong bank.

The fault is deterministic: the same binary does the same thing every
run, with another busy process in the guest, and at any environment
size (so wherever the stack lands). It moves or goes away with anything
that changes the instruction stream: `-g` rather than `-O`, one more
`printf`, one more test. Under adb with breakpoints set it goes away
altogether, and a breakpoint changes no code or data, only when the CPU
is interrupted.

On the host the same C is clean under ASan, UBSan and
`-ftrivial-auto-var-init=pattern`, and the guest's sources and data were
checksummed identical to the host's (so it is not the work-area sync).

## Reproducing it

The project at the commit of the time, built with `-g` in the guest (the
standalone repro, needing no project at all, is the BBC BASIC program
under "A minimal reproduction in RISC OS" below):

```sh
cd /path/to/the-project
just data                        # the world the tests read
cd game
dockerix exec sh -c 'make clean >/dev/null; make CFLAGS="-g -U_BSD_C" tests >/dev/null 2>&1; ./tests 2>&1 | grep -v "^data/" | tail -3'
```

Expected `tests: 1695 checks, 0 failed`; seen, every run:

```
tgame.c:382: failed
tests: 1695 checks, 1 failed
```

The default build (`make tests`, which is `-O`) passes at the same
commit, and so does the host build. Afterwards `git switch main` and
`just data`, since a failed run leaves the built world clobbered.

### What the failure is

`test_recover` in `game/tgame.c`:

```c
most = actor_most(actor_type(&actors[0]), P_MAGICKA);   /* the hero: 6 */
gmost = actor_most(actor_type(g), P_MAGICKA);           /* the goblin: 4 */
...
CHECK(... && actor_pool(g, P_MAGICKA) == gmost);        /* line 382 */
```

Norcroft keeps `most` in `r6` and `gmost` in `r8`:

```
0003fff0  bl      _actor_most
0003fff4  mov     r6, r0
0003fff8  mov     r0, r5
0003fffc  bl      _actor_type
00040004  bl      _actor_most
00040008  mov     r8, r0
```

A `printf` inside `actor_most` shows it returns 4 for the goblin on
every call, yet `gmost` reads 6 (the value in `r6`) from the next
statement on. With an adb breakpoint after each call printing `r8`, it
reads 4 at every stop and the check passes:

```sh
# in the guest, game/ built as above
cat > adbcmds <<'EOF'
0x4000c:b <r8=X
0x40014:b <r8=X
:r
:c
:c
EOF
adb tests < adbcmds
```

(The addresses are from `_test_recover,40?ia` in adb; check them after
any rebuild.)

### The crash, the same fault with fp

The project at a later commit, with this change to `game_key()` in `game/game.c`, which makes Tab leave select
mode, and the test that goes with it in `game/tscript.c`:

```c
    case KEY_ESC: case '\t':
        if (sel) {
            select_mode(0);
            return 0;
        }
        if (key == KEY_ESC)
            return game_pause();        /* 1 for Quit */
        ingame_run();
        return 0;
```

```c
    CHECK(!play(" \t\001\t\002\t") && begins(snapped(1, 0, 0), " <title>") && !snapcur[1]);
    CHECK(begins(snapped(2, 0, 0), " Journal "));
```

(the second replacing `CHECK(!play(" \t\001\033") && ... && snapcur[1]);`
in `test_journal`). The default `-O` build, `make clean; make tests;
./tests`, then dies with a memory fault. This was seen on the
uncommitted tree before that commit, which differed from it only in those
two changes; it has not been re-run on the commit itself. The core has
`label_load` (game/label.c) returning with `fp` = 0x17f64c4, the frame
pointer of a `view_opaque` frame that had already returned. `fclose`'s
saved copy of the caller's fp already held that value, so `fp` was
wrong in `label_load` before it called `fclose`, and nothing in
`label_load` writes fp. The ldmdb then read the dead stack's bytes,
`label_load`'s id buffer (`"vestry\0abel\0"`, `vestry` over `arena_label`),
as fp, sp and pc, and the pc jumped to 0x6c6560.

### Not the host platform

It fails the same way on the amd64 Linux build under docker-ce, as well
as on aarch64 Linux in Docker Desktop on macOS. The project's CI, in a
replay of the same commit with the guest stage building `-g`, ended its
RISC iX stage on `tgame.c:382: failed`; the three host builds
of the same commit passed, 1695 checks each. So it is not the
untested aarch64 build, and is in Arculator, our patches or the guest
itself.

The log of that stage stops on the failure line with no `tests: 1695
checks, 1 failed` after it, which may be the tty holdback losing the
last line as the command exits; not yet looked at.

## Not this: a guest-only failure that was the compiler

Checked, so that it is not counted as more of the above.
The project's `scripttests` failed on the guest alone at `tscript.c:385`
(advanced maps M2): a handler in a table of statics read an address as
its argument. It looked like this fault, deterministic and guest-only
with the host clean under ASan, and it is not.

adb on the binary, not a running process, settled it. The stores were
`str` through `r7`, loaded from the literal pool one instruction
before with no call between, and `r7` is not in the FIQ bank. The
literal itself held `0x69970`, and the table handed to `put_world()`
was at `0x69958`, 80 bytes long: the compiler had put the address
inside the table. The cause was the project's: `tscript.c` declared `static
char *corridor[3];` and, 1,800 lines further down, `static char
*corridor[] = { ... };`. In C those are one array; Norcroft (Oct 1990)
makes them two, and the first, with no room of its own, sits on
whatever initialised data the module puts next. A thirty-line file
reproduces it on the guest under `-O`.

What tells the two apart: a fault in the compiler's layout is in the
binary on disk, so adb reading the file shows the bad address with no
process running; the register fault never is, and goes away under
breakpoints.

## The cause: a prefetch abort taken one instruction early

Found by reading Arculator's `src/arm.c` against ArcEm (ARM
Ltd's ARMulator core, `arminit.c`/`armemu.c`). Not the FIQ bank:
`updatemode()` saves and restores r8–r14 correctly. Not LDM/STM on a
data abort either: Arculator writes the base back and never undoes it,
which is what ARMulator does and what the ARM2/ARM3 "early abort" rule
says (LDM/STM write back, LDR/STR do not). Not Norcroft: a compiler
fault is in the binary on disk and does not go away under a debugger.

`execarm()` models the three-stage pipeline as `opcode`, `opcode2` and
`opcode3`. When the fetch into `opcode3` fails (`readmemf()` in
`mem.c`, the next page not resident) it sets `prefabort_next`, which
becomes `prefabort` at the end of that loop iteration. In the next
iteration `prefabort` did two things: `if (!prefabort)` stopped the
instruction in the execute slot running, and the exception was raised
with `r14 = armregs[15]` = that instruction + 8. That is the right
`r14` (aborted address + 4) for the instruction *behind* it, the one
that could not be fetched, so the kernel's `subs pc, r14, #4` resumes
at the first word of the new page, correctly; but the last instruction
of the old page never ran. The branch path, `refillpipeline()`, flags
the target itself and takes the abort in the same iteration with
`r14 = target + 4`, so a call into a page not yet resident is fine;
only execution running sequentially off the end of a page is broken.
RISC OS never pages code, so it never shows; RISC iX pages text in all
the time, though BSD's klustered page-ins map the next page often
enough that it is rare rather than fatal.

It explains everything above:

- Deterministic for a binary, moved by any change to the code, not by
  environment size or another process: it depends on where the page
  boundaries fall in the text and the order pages are first touched.
- Gone under adb: the breakpoints at 0x4000c and 0x40014 are on the
  page past the boundary at 0x40000, and ptrace's writes made it
  resident before execution reached it.
- The r8 case: executing `0x3fff8 mov r0, r5`, the fetch of 0x40000
  fails and the `bl _actor_type` at 0x3fffc is skipped, so
  `actor_most()` got the goblin pointer instead of its type. The
  `printf` in `actor_most` moved the boundary, which is why it then
  "returned 4 every time".
- The fp case: a skipped `mov ip, sp` before a boundary leaves `ip`
  holding a dead frame's sp, and `sub fp, ip, #4` gives exactly "the
  frame pointer of a `view_opaque` frame that had already returned".

All of Arculator's CPUs (ARM2, ARM250, ARM3) share this loop; the CPU
type only sets the speed, SWP, CP15 and cache timing.

The fix, `patches/arculator/0004-prefetch-abort-pipeline.patch`: always execute
the instruction in the execute slot, and take the exceptions in the
ARM's priority order (data abort, FIQ, IRQ, then prefetch abort), so an
abort or interrupt of the executing instruction wins and the refill it
causes drops the pending prefetch abort, to recur when that
instruction is fetched again.

### A minimal reproduction in RISC OS

`docs/pfabort/PfAbort,fff` is a BBC BASIC program for RISC OS 3 that
shows the fault with no RISC iX at all, for an upstream report. It
lowers HIMEM, assembles `MOV r0,#1; MOV r1,#2; MOV r2,#3` into the last
three words of the application slot (`OS_GetEnv`'s memory limit; the
page from there on is not mapped), enters SVC mode with `OS_EnterOS`,
puts a `B` to its own handler in the prefetch abort vector at `&C`,
and jumps to the three MOVs. The handler records r14 and r0–r2,
restores the vector, drops back to user mode and returns. A correct
ARM runs all three MOVs and aborts fetching the slot end, with r14 =
slot end + 4. It prints and writes `HostFS:$.result`; `!Boot,feb`
next to it is an Obey file that runs it at startup.

With dockerix's image, which boots RISC OS 3.11 from HostFS, run it in
a throwaway container with the two files as the HostFS root:

```sh
docker run -d --name pfabort -v ~/.dockerix/assets:/assets:ro \
  -v $PWD/docs/pfabort:/opt/arculator/hostfs -v /tmp/pfabort-state:/state \
  --entrypoint sh dockerix:<tag> -c 'cp -r /assets/roms/riscos311 roms/ \
  && (Xvfb :99 -screen 0 1024x768x24 >/dev/null 2>&1 &) && sleep 2 \
  && exec ./arculator riscix'
# about a minute later:
cat 'docs/pfabort/result,ffd'; docker rm -f pfabort
```

Without the patch (`dockerix:d85c4881c91e`):

```
slot end=&F68000
r14=&20F68007 address=&F68004 expected=&F68004
r0,r1,r2=1,2,0 expected=1,2,3
BUG: MOV r2,#3 at &F67FFC, the last word before the page boundary, did not execute
```

With it (`dockerix:ee7308741dd2`): `r0,r1,r2=1,2,3`, `OK: ... executed`.
Both report the same r14, which is why the kernel always resumed at the
right address and only the skipped instruction gave it away.

### The project's tests

Confirmed with the repro above (in a worktree, work area
pushed, not NFS): patched image `dockerix:ee7308741dd2`, `tests: 1695
checks, 0 failed`; the unpatched image (`dockerix:d85c4881c91e`, the
same sources without the patch), same binary: `tgame.c:382: failed`,
`tests: 1695 checks, 1 failed`.
