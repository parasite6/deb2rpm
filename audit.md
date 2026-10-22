# deb2rpm code audit

**PATCHED** (2026-10-22) — all four findings below are fixed in `src/main.rs` as of v1.0.2.

Reviewed 2026-10-22. Scope: `src/main.rs` and `install.sh`, with an independent review of process handling. Original review applied no production fixes; the patches listed under each finding were added afterward.

Four actionable findings: one high-priority execution risk, two medium-priority hangs, and one low-priority shutdown issue. This is a direct code review with targeted reproductions, not a claim that every possible vulnerability has been excluded.

## 1. P1 — Package metadata can execute commands during conversion — PATCHED

**Locations:** `src/main.rs:713` (`fix_spec`), `src/main.rs:797` (only empty summaries are repaired), `src/main.rs:881` (generated spec passed to rpmbuild).

The generated spec is treated as executable RPM input, but package-controlled metadata is not escaped or rejected. A Debian description such as `%(printf DEB2RPM_MACRO_EXECUTED)` becomes a nonempty RPM `Summary` and survives `fix_spec`. RPM expands the shell macro when parsing the spec.

This runs commands as the invoking user during conversion, before a dnf confirmation or sudo authentication. Build-only mode and `--no-scripts` do not prevent it: the payload is in metadata rather than a maintainer script. `fakeroot` changes ownership behavior; it does not isolate command execution from the user's files.

**Evidence:** A temporary harness using the current `fix_spec` preserved the macro verbatim. `rpmspec --parse` on that fixed minimal spec executed the harmless `printf` and emitted `Summary: DEB2RPM_MACRO_EXECUTED`. The installed alien implementation maps Debian `Description` to `summary` (`Alien/Package/Deb.pm:241`) and writes it directly into the spec (`Alien/Package/Rpm.pm:339`). Its generic field accessor does not escape RPM macros.

**Validation limit:** A synthetic `.deb` was created, but the end-to-end alien attempt stopped when the sandbox denied fakeroot's daemon socket. The executable spec behavior is reproduced; the Debian-to-spec path was verified in the installed alien source. No real package was installed and the probe executed only a harmless print command.

**Fix direction:** Treat the generated spec as executable code. Construct or validate metadata and file-list sections with appropriate RPM escaping, accounting for macro expansion and section injection. Do not blindly escape intended macros in the whole spec. For genuinely untrusted packages, isolate conversion from the user's filesystem and credentials as well.

**Patch:** `sanitize_spec` doubles mid-line `%`, rejects body lines that begin with `%` after whitespace strip (and ASCII controls other than tab), splits bodies on `\n` only, and rejects embedded newlines in single-line fields / `%files` paths. Section-injection regression and e2e hostile `.deb` tests cover the fix.

## 2. P2 — Surviving descendants can hang captured commands indefinitely — PATCHED

**Locations:** `src/main.rs:302–314` and `src/main.rs:330–355`.

`wait_child` returns as soon as the direct child exits. `run` then joins reader threads that call `read_to_end` on stdout and stderr. If a descendant still holds either pipe open, those joins wait for that descendant even though the direct child has been reaped.

Cancellation is no longer monitored at this point. SIGINT/SIGTERM set an atomic flag, but there is no active loop to forward the signal or apply the five-second escalation. A build helper that exits while leaving a background worker can leave both a hung converter and a surviving descendant.

**Reproduction:** Using the current command functions, run `/bin/sh -c 'sleep 20 & sleep 0.5; exit 0'`. After the direct child exited, sending SIGTERM to the harness still left it blocked six seconds later. Killing the remaining descendant process group immediately released the joins and returned the stopped error. Probe descendants were cleaned up.

**Fix direction:** Manage leader exit, pipe draining, cancellation, and descendant cleanup as one interruptible lifecycle. Do not enter unconditional reader joins while a pipe can remain open indefinitely.

**Patch:** `run` uses nonblocking pipe drains, signals the process group on exit/interrupt, escalates to SIGKILL, and bounds waiting if descendants keep pipes open (`CapturedChild` / `drain_pipe`).

## 3. P2 — A package-supplied FIFO can block the advisory library scan — PATCHED

**Locations:** `src/main.rs:984–986` and `src/main.rs:1088–1092`.

The initial tree walk restricts ELF inspection to regular files, but the dependency-resolution path does not. For each `DT_NEEDED` library, it canonicalizes a candidate inside the package tree and calls `elf_identity`, which opens and reads it without checking its type.

A package containing an ELF executable that needs `libc.so.6` and a FIFO at `lib/libc.so.6` blocks on opening that FIFO when no writer exists. A symlink resolving to a FIFO inside the tree has the same effect. This happens before the RPM build, during an advisory check. The blocking filesystem operation does not observe cancellation, and the installed signal handlers leave the process stuck.

**Reproduction:** A temporary tree containing a copy of `/usr/bin/true` and a FIFO at `lib/libc.so.6` was passed to the current `missing_libraries` implementation. The harness did not return. With the project's signal handlers installed, a timeout's SIGTERM did not release it; SIGKILL was required (exit 137).

**Fix direction:** Open candidate libraries without blocking, then inspect the opened descriptor and require a regular file before reading. A separate path-based type check alone leaves a check/open race. Apply the same descriptor validation to every `elf_identity` caller.

**Patch:** `elf_identity` opens with `O_NONBLOCK`, requires a regular file via the opened descriptor's metadata, then reads. Covered by `library_scan_rejects_fifo_without_blocking`.

## 4. P3 — The pause prompt ignores ordinary termination signals — PATCHED

**Location:** `src/main.rs:1196–1199`.

`--pause` checks `interrupted()` once, then blocks in `stdin.read_line`. If Ctrl+C or SIGTERM arrives after that check, the signal handler only sets the flag. The read remains blocked and the application does not exit until input or EOF arrives.

This affects the desktop entry, which always supplies `--pause`, and CLI invocations using that option.

**Reproduction:** The exact signal-handler and read-line sequence remained blocked after both SIGTERM and SIGINT. Sending a newline released it with `interrupted()` set.

**Fix direction:** Use an interruptible input wait and check cancellation while waiting, rather than only before entering a blocking read.

**Patch:** `wait_for_enter` polls stdin with a short timeout and re-checks `interrupted()`. Covered by `pause_signal_stops_input_wait`.

## Verification and boundaries

- `cargo test --locked --offline`: **7 passed, 0 failed, 1 intentionally ignored helper**. The existing tests do not cover these four cases.
- Targeted tests used temporary harnesses and synthetic fixtures under `/tmp`. No real conversion or installation was completed.
- No ordinary direct-child zombie leak was found: the main command wrappers reap their direct children through `try_wait`. Finding 2 concerns surviving descendants and blocked pipe readers, rather than an unreaped direct child.
- The random private work directory, no-follow lock opening, retained lock inode, and hard-link publication of saved RPMs address several common filesystem races. No demonstrated cross-user exploit of those mechanisms was found in this review.
- The synchronous `df`, `uname`, and `gio trash` calls bypass the managed cancellation loop. This remains a cancellation limitation; no separate practical failure was reproduced for those helpers.
- Package maintainer scripts intentionally execute with installation privileges. That expected behavior is distinct from finding 1, which executes package-supplied commands during conversion before installation approval.
- Original review applied no code fixes. **PATCHED** in v1.0.2 for all four findings above.
