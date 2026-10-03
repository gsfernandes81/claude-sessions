# The screens, approved 2026-10-01

> "Designs approved for use." — the owner, 2026-10-01, on the published page these were
> reviewed in.

**These are binding.** Phase 4 renders to them; a change needs the owner, not a judgement
call. Three changes since approval, each the owner's. On 2026-10-02: mockup 2's way out read
`q   a shell instead`, which was a mistake — `q` quits and `s` is the shell everywhere else —
and now reads `s   a shell instead`, in both widths. On 2026-10-03 the owner asked for the
footer — closing rule, status, hints — to sit at the bottom of the terminal; the screens show
content flowing from the top only because they are drawn shorter than a terminal. And on
2026-10-03, closed slots are listed too, at the bottom, with a new mark `x` (dim, like the
other state marks) and a line for it on the keys screen, mockup 7, in both widths. And on
2026-10-03 the owner redesigned the list: **grouped by state** — Needs you, Working, Idle,
Offloaded, Closed, each under a heading, an empty group not drawn, one blank line between
groups — with **no marks column and no row numbers** ("remove the symbols, they really
aren't helping"; the numbers: "Remove them"). An unread title is bold, a session attached
somewhere else is drawn dim ("Dim the row"), amber moved from `!` to the Needs-you heading,
the cursor only ever selects a session, and what does not fit folds into `… N more`. The
dialogs name the session by its title, and the keys screen explains the groups instead of
the marks. Mockups 1 and 3 to 7 were redrawn for it, in both widths; 2 and 8 are unchanged.

They were drawn at **40 columns** — the phone in portrait, which is where this tool is
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

**`…` in the fold line was measured the same way before it was used** (2026-10-03): U+2026
is in all six faces above — at the face's own uniform advance in each (600 units; Space
Mono's 612) — read from the subsets' cmaps by the same method, which reads `↵` as absent in
them, so the check can fail. It is drawn only as the first word of `… N more`, never to cut a
title: a title is cut with no ellipsis at all, so the age right-aligned after it never
depends on a glyph. Mockup 8's narrowing demo keeps plain `=` rulers for the same reason —
the measured edge should be the one thing on the page that cannot itself be a width question.

### Colour, in Claude Code's vocabulary rather than a new one

Owner, 2026-10-01: *"Use colour similarly to how Claude code does."* — then, on the first
attempt: *"Too much yellow / orange. Use a less worrying colour at base."* The first version
put amber on every key in the hint line and on every dialog title, which broke the rule
written beside it: amber is the colour that reads as a *problem*, and a screen covered in it
reads as a screen full of problems. **The base accent is blue; amber is left to one thing**
— the `!` mark until the 2026-10-03 redesign, the Needs-you heading since.

| where | style | because |
|---|---|---|
| the `Needs you` heading | amber, bold | the only group that is a *request*, and the only amber anywhere |
| a key you can press — the hint line, the keys screen | blue | actionable, not alarming |
| every other group heading | foreground, bold | structure the eye can find |
| an unread title | foreground, bold | news, not a problem |
| a session attached somewhere else | dim, title and age | someone is in it; state, not news |
| a dialog's first line | foreground, bold | it is the question, and a question is not a warning |
| age, rules, frames, the fold line, the header after the container name | dim | structure |
| title | foreground | the content |
| the cursor | reverse video, across the row | visible on a monochrome terminal too |

**In a full list there is one run of amber on the screen, and it means the one thing** —
which is the test for whether amber is still worth having. On the keys screen the group
names are drawn as their headings are, so that screen is also the key to the list.

Two rules come with it. **Nothing is colour-only**: what state a session is in is said in
words, by the heading it is under, so a monochrome terminal, a pipe or a screen reader loses
none of it. Bold for unread and dim for attached are emphasis on top of that, the owner's
choice over glyphs (2026-10-03); `claude-sessions list` still prints them as marks. And
**amber is spent once**: if the heading and anything else were both amber, neither would
mean *this one*. The plain-text mockups below cannot show colour, which is the honest reason
this table exists; the published page renders it.

### Mockup 1 — The list, in its groups
```
infra-dev · 6 open · 812M of 1.0G
────────────────────────────────────────
Needs you
permission: write hosts/one           2m

Working
loop: watch the base build           now

Idle
retire the old tunnel                14m
immich upgrade                        3m
claude                                5h

Offloaded
mount guards on one                   2d

Closed
fix the dns records                   3d
tunnel cutover notes                  4d
bcache register script                5d
… 4 more
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
  s   a shell instead

────────────────────────────────────────
n new   ? keys   s shell
```

### Mockup 3 — Closing a live session
```
infra-dev · 6 open · 812M of 1.0G
────────────────────────────────────────
Idle
retire the old tunnel                14m
╭──────────────────────────────────────╮
│ Close this session?                  │
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
Idle
retire the old tunnel                14m
╭──────────────────────────────────────╮
│ No room for another claude           │
│                                      │
│ 892M of 1.0G used. A new one wants   │
│ about 250M.                          │
│                                      │
│ Offload this session, idle 14m?      │
│   retire the old tunnel              │
│   resumable from disk                │
│                                      │
│ y offload, then open    n cancel     │
╰──────────────────────────────────────╯
```

### Mockup 5 — A resume that fails
```
infra-dev · 6 open · 812M of 1.0G
────────────────────────────────────────
Offloaded
mount guards on one                   2d
╭──────────────────────────────────────╮
│ This session did not resume          │
│   mount guards on one                │
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

### Mockup 6 — Back from a session, after detaching
```
infra-dev · 6 open · 1.0G of 1.0G
────────────────────────────────────────
Needs you
permission: write hosts/one           2m

Working
loop: watch the base build           now

Idle
retire the old tunnel                now
immich upgrade                       12m
claude                                5h

Offloaded
mount guards on one                   2d

Closed
fix the dns records                   3d
tunnel cutover notes                  4d
… 5 more
────────────────────────────────────────
detached · it is still running
Enter open   n new   c close   ? keys
s shell
```

### Mockup 7 — The keys, on ?
```
infra-dev · keys
────────────────────────────────────────
Enter  open the session; resumes it
       if offloaded or closed
n      new session in /workspace
c      close the session
s      a shell in /workspace
Esc    quit the launcher
?      this

Needs you  a prompt is waiting
Working    claude is mid-turn
Idle       at its prompt, waiting
Offloaded  stopped to save memory
Closed     ended; still resumable
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
