# The screens, approved 2026-10-01

> "Designs approved for use." — the owner, 2026-10-01, on the published page these were
> reviewed in.

**These are binding.** Phase 4 renders to them; a change needs the owner, not a judgement
call. They were drawn at **40 columns** — the phone in portrait, which is where this tool is
actually driven from — and every line was *generated* to that width and asserted against it,
not typed to look right. The 80-column set is in [`mockups-80.md`](mockups-80.md).

The review that produced them is recorded in `plans/claude-sessions.md` in the `infra`
repo, which is also where the rest of the plan lives until Phase 6 deletes it.

**The glyphs are the ones Claude Code already draws** (owner, 2026-10-01): box drawing for
the rules and the dialog frames, `·` in the header. They are East Asian *Ambiguous* in the
Unicode tables — one column in some terminals, two in others — and the reason to use them
anyway is evidence rather than taste: Claude Code renders this set in every terminal this
fleet is driven from. Its mode indicators stay out (`⏵⏵` and the rest), which is the one
part of that vocabulary the owner has seen fail.

**Except `↵`, which is not in the fonts, and that is what the misalignment was.** The owner
reported box alignment still off after the glyph redraw, twice. It was not CSS: U+21B5 is
**absent from every monospace face on Google Fonts** — checked by downloading the subset and
reading its cmap for JetBrains Mono, Noto Sans Mono, Source Code Pro, IBM Plex Mono, Fira
Mono and Space Mono on 2026-10-01. Every other glyph here is present in all of them at a
uniform 600-unit advance; `↵` is in none, so it fell back to another face with a different
advance and dragged the line it sat on out of true. **That is a fact about the terminal too,
not just the browser** — a glyph the common monospace fonts do not carry is a glyph that
will fall back wherever it is drawn. So the key is spelled `Enter`, in letters, and it costs
nothing: `Enter open   n new   c close   ? keys` is 37 columns and still fits 40.

**The marks column stays single-byte ASCII**, and that is not timidity about the rule above.
It is the one field whose width is load-bearing: the age is right-aligned against it, so a
mark that renders two columns wide pushes every age off the screen, while a rule that
renders wide is merely long. Mockup 8's narrowing demo keeps plain `=` rulers for the same
reason — the measured edge should be the one thing on the page that cannot itself be a
width question.

### Colour, in Claude Code's vocabulary rather than a new one

Owner, 2026-10-01: *"Use colour similarly to how Claude code does."* — then, on the first
attempt: *"Too much yellow / orange. Use a less worrying colour at base."* The first version
put amber on every key in the hint line and on every dialog title, which broke the rule
written beside it: amber is the colour that reads as a *problem*, and a screen covered in it
reads as a screen full of problems. **The base accent is blue; amber is left to one mark.**

| where | colour | because |
|---|---|---|
| `!` wants you | amber | the only mark that is a *request*, and the only amber anywhere |
| a key you can press — the hint line, the keys screen | blue | actionable, not alarming |
| `t` timer pending | blue | a fact about time, and the reason the slot cannot be offloaded |
| `*` unread | foreground, bold | news, not a problem |
| a dialog's first line | foreground, bold | it is the question, and a question is not a warning |
| `@` `z` `u` | dim | state, not news |
| slot number, age, rules, frames, the header after the container name | dim | structure |
| title | foreground | the content |

Keys and `t` share the one blue deliberately: they never appear in the same column, and two
blues would be a distinction nobody could name. **In a full list of six slots there are two
amber characters on the screen, and both mean the same thing** — which is the test for
whether amber is still worth having.

Two rules come with it. **Nothing is colour-only** — every mark is a glyph first, so a
monochrome terminal, a pipe or a screen reader loses nothing. And **amber is spent once**: if
`!` and anything else were both amber, neither would mean *this one*. The plain-text mockups
below cannot show colour, which is the honest reason this table exists; the published page
renders it.

### Mockup 1 — The list, every mark mixed
```
infra-dev · 6 open · 812M of 1.0G
────────────────────────────────────────
1 !   permission: write hosts/one     2m
2 *   retire the old tunnel          14m
3 *t  loop: watch the base build     31m
4 @   immich upgrade                 now
5 z   mount guards on one             2d
6 u   claude                          5h
────────────────────────────────────────
Enter open   n new   c close   ? keys
s shell
```

### Mockup 2 — Nothing open
```
infra-dev · nothing open · 812M of 1.0G
────────────────────────────────────────

  No claude session in this container.

  n   start one in /workspace
  q   a shell instead

────────────────────────────────────────
n new   ? keys   s shell
```

### Mockup 3 — Closing a live slot
```
infra-dev · 6 open · 812M of 1.0G
────────────────────────────────────────
1 !   permission: write hosts/one     2m
2 *   retire the old tunnel          14m
╭──────────────────────────────────────╮
│ Close slot 2?                        │
│   retire the old tunnel              │
│                                      │
│ Running. Stops the process, not the  │
│ conversation — resumable from disk.  │
│                                      │
│ y close    n keep                    │
╰──────────────────────────────────────╯
```

### Mockup 4 — No room to open another
```
infra-dev · 6 open · 892M of 1.0G
────────────────────────────────────────
5 z   mount guards on one             2d
6 u   claude                          5h
╭──────────────────────────────────────╮
│ No room for another claude           │
│                                      │
│ 892M of 1.0G used. A new one wants   │
│ about 250M.                          │
│                                      │
│ Offload slot 5, idle 2d?             │
│   mount guards on one                │
│   resumable from disk                │
│                                      │
│ y offload, then open    n cancel     │
╰──────────────────────────────────────╯
```

### Mockup 5 — A resume that fails
```
infra-dev · 6 open · 812M of 1.0G
────────────────────────────────────────
5 z   mount guards on one             2d
6 u   claude                          5h
╭──────────────────────────────────────╮
│ Slot 5 did not resume                │
│                                      │
│ claude --resume 0f9c4a1e exited 1    │
│   No conversation found with that    │
│   session id                         │
│                                      │
│ Left offloaded. Nothing was deleted; │
│ the transcript may be gone.          │
│                                      │
│ r retry   c close it   Esc back      │
╰──────────────────────────────────────╯
```

### Mockup 6 — Back from a slot, after detaching
```
infra-dev · 6 open · 1.0G of 1.0G
────────────────────────────────────────
1 !   permission: write hosts/one     2m
2     retire the old tunnel          now
3 *t  loop: watch the base build     31m
4 @   immich upgrade                 12m
5 z   mount guards on one             2d
6 u   claude                          5h
────────────────────────────────────────
detached from 2 · it is still running
Enter open   n new   c close   ? keys
s shell
```

### Mockup 7 — The keys, on ?
```
infra-dev · keys and marks
────────────────────────────────────────
Enter  open the row (resume if z)
n      new session in /workspace
c      close the row
s      a shell in /workspace
Esc    quit the launcher
?      this

!  wants you: a prompt is waiting
*  unread: it finished while away
t  a timer is pending; not
   offloaded until it fires
@  attached somewhere else too
z  offloaded: Enter resumes it
u  not started by claude-sessions
────────────────────────────────────────
Enter open   n new   c close   ? keys
s shell
```

### Mockup 8 — The hint line as the terminal narrows
```
at 40 columns, the right edge marked:
========================================
Enter open   n new   c close   ? keys
s shell
at 34 columns, the right edge marked:
==================================
Enter open   n new   c close
? keys   s shell
at 26 columns, the right edge marked:
==========================
Enter open   n new
c close   ? keys   s shell
at 18 columns, the right edge marked:
==================
Enter open   n new
c close   ? keys
s shell
at 12 columns, the right edge marked:
============
Enter open
n new
c close
? keys
s shell
at 9 columns, the right edge marked:
=========
n new
c close
? keys
s shell
at 7 columns, the right edge marked:
=======
n new
c close
? keys
s shell
at 6 columns, the right edge marked:
======
  (the menu refuses to draw; the
   door execs a login shell and
   says why)
```
