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
Later on 2026-10-03 the owner chose, from three drawn options, **headings as labelled rules
with a count** — `── Closed · 7 ────` — with the rows indented two columns under them from 60
columns up, and added **the archive**: an Archived group last, folded shut until `Enter` on its
heading opens it — the one line the cursor rests on that is not a session — filled by `c` on a
closed row, with no question asked, and by 30 days unused. Mockups 1 and 3 to 7 were redrawn
again for it, and mockup 9 is new. On 2026-10-04 the owner added **a spinner for anything
that takes a while** — Docker Compose's, `⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏`, a little faster than Compose's
100 ms per frame — standing in for the age of the session being worked on, with the status
line saying what is happening and naming no session (`⠹ closing session`), and on the first
frame where the list will go while it is read. Mockups 10 and 11 are new. **On 2026-10-26 the
owner changed the groups again**, with the hub: "make needs you just a yellow highlight or icon
or something. Let's keep loaded / offloaded and a click to expand archived section." So one
container's list is **Loaded, Offloaded and Archived** — Archived holding everything closed,
folded shut, its heading saying what `Enter` or a click will do — and a session waiting for
the owner is marked **`!` in amber** before its title instead of having a group. Every screen
below was redrawn for it, from the renderer, at both widths. **The braille is
the one glyph here outside the measured set**: it is in none of the six faces, and is used
on the owner's word that it draws on the terminals this is driven from, which put a fallback
glyph into one cell. It is only ever drawn alone, at the end of a row or the start of the
status line, so a fallback cannot drag a line out of true. **On 2026-10-27 the owner took the
question out of `c` and made the hub's home a tree**: "Remove the confirmation on session
close … make the first c offload and c on an offloaded session archive it … make the session
presentation a tree view where clicking the container expands it to show offloaded and closed
sessions in a new page (like it does now) but in the normal tree view only show loaded
sessions." So `c` is one step down the list — Loaded to Offloaded, Offloaded to Archived —
asking nothing, and mockup 3, its question, is retired; mockup 11's spinner says `offloading
session`, mockup 5's way out is `c archive it`, and the keys screens say what `c` does. The
home (mockup 12) lists each container's loaded sessions under it, each hanging from it on a
dim `├─ ` branch, the last on a `└─ ` corner, and `Enter` on a session there opens it. **`├`
and `└` are outside the measured set**, used on the owner's word (2026-10-27): every glyph
Claude Code and charmbracelet's crush draw works on the terminals this is driven from, and
crush draws its trees with these. The hint line keeps `c close`: it is the same seven columns, so
mockup 8 stands. **Later on 2026-10-27 the owner added the load line** — "a claude sessions
statusline style load display when we're in the session door", "in the same location as
claude code puts it approximately. You have creative freedom" — so every screen with a footer
ends with `RAM: 1.9G / 3.0G, Load: 2.3, dev` under its hints: the status line's text, every
container's figures (`docs/design.md` § *The status line*), named for where it is read — the
hub at the door, the container where there is only one. Every such screen below was redrawn
for it, from the renderer, at both widths; the dialogs, which have no footer, have none. Dim
as structure, a figure past its first threshold bold and past its second red: not yellow,
which beside amber would read as *waiting for you*. It is the one line an idle menu rewrites,
and only when it reads differently.

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
— the `!` mark until the 2026-10-03 redesign, the Needs-you heading from then, and the `!`
mark again, with its title, from 2026-10-26.

| where | style | because |
|---|---|---|
| the `!` before a session waiting for you, and its title — or a container's name, at the home | amber, the mark bold | the only row that is a *request*, and the only amber anywhere |
| a key you can press — the hint line, the keys screen | blue | actionable, not alarming |
| the load line, under the hints | dim; a figure past 70% of RAM or 0.7 load a core bold, past 85% or 1.0 red | structure until it is a worry; red is Claude Code's own for the same line |
| every other group heading | rule dim, name bold | a divider the eye can find, and a glyph, so a monochrome terminal keeps it |
| the Archived heading under the cursor | reverse video, like a row | the one heading the cursor can rest on |
| an unread title | foreground, bold | news, not a problem |
| a session attached somewhere else | dim, title and age | someone is in it; state, not news |
| a dialog's first line | foreground, bold | it is the question, and a question is not a warning |
| age, rules, frames, the fold line, the header after the container name | dim | structure |
| title | foreground | the content |
| the cursor | reverse video, across the row | visible on a monochrome terminal too |

**Amber on the screen means one thing**, a session waiting for you — which is the test for
whether amber is still worth having. On the keys screen the group names are drawn as their
headings are and the `!` as it is on a row, so that screen is also the key to the list.

Two rules come with it. **Nothing is colour-only**: what state a session is in is said in
words, by the heading it is under, and waiting by the ASCII `!`, so a monochrome terminal, a
pipe or a screen reader loses none of it. Bold for unread and dim for attached are emphasis on
top of that, the owner's choice over glyphs (2026-10-03); `claude-sessions list` still prints
them as marks. And **amber is spent once**: if a waiting row and anything else were both
amber, neither would mean *this one*. The plain-text mockups below cannot show colour, which is the honest reason
this table exists; the published page renders it. What the renderer sends for each mockup,
styles and all, is pinned byte for byte in [`mockups.ansi`](mockups.ansi) (2026-10-28):
`cat` it in a terminal to see these screens in colour. It is derived, not approved — the
text here and the table above are what bind — and a test holds the renderer to it.

### Mockup 1 — The list, in its groups
```
infra-dev · 6 open · 812M of 1.0G
────────────────────────────────────────
── Loaded · 5 ──────────────────────────
! permission: write hosts/one         2m
loop: watch the base build           now
retire the old tunnel                14m
immich upgrade                        3m
claude                                5h

── Offloaded · 1 ───────────────────────
mount guards on one                   2d

── Archived · 7 · Enter shows ──────────
────────────────────────────────────────
Enter open   n new   c close   ? keys
s shell
RAM: 0.8G / 1.0G, Load: 2.3, infra-dev
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
RAM: 0.8G / 1.0G, Load: 2.3, infra-dev
```

### Mockup 3 — retired 2026-10-27

`c` asked "Close this session?" here until the owner took the question out (2026-10-27): `c`
now offloads a loaded session and archives an offloaded one, at once, and neither loses the
conversation. The number is kept so the rest keep theirs.

### Mockup 4 — No room to open another
```
infra-dev · 6 open · 892M of 1.0G
────────────────────────────────────────
── Loaded · 5 ──────────────────────────
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
── Offloaded · 1 ───────────────────────
mount guards on one                   2d
╭──────────────────────────────────────╮
│ This session did not resume          │
│   mount guards on one                │
│                                      │
│ claude --resume 0f9c4a1e ended       │
│   No conversation found with that    │
│   session id                         │
│                                      │
│ Left offloaded. Nothing was deleted; │
│ the transcript may be gone.          │
│                                      │
│ r retry   c archive it   Esc back    │
╰──────────────────────────────────────╯
```

The command line read `exited 1` until 2026-10-06, when the owner approved `ended`: zmx, which
replaced abduco, does not report its program's exit status. The one case the number told
something, a kill for memory, reads `claude --resume 0f9c4a1e was killed for memory`, from
the container's own OOM-kill count, wrapped like any other line.

### Mockup 6 — Back from a session, after detaching
```
infra-dev · 6 open · 1.0G of 1.0G
────────────────────────────────────────
── Loaded · 5 ──────────────────────────
! permission: write hosts/one         2m
loop: watch the base build           now
retire the old tunnel                now
immich upgrade                       12m
claude                                5h

── Offloaded · 1 ───────────────────────
mount guards on one                   2d

── Archived · 7 · Enter shows ──────────
────────────────────────────────────────
detached · it is still running
Enter open   n new   c close   ? keys
s shell
RAM: 1.0G / 1.0G, Load: 2.3, infra-dev
```

### Mockup 7 — The keys, on ?
```
infra-dev · keys
────────────────────────────────────────
Enter  open the session; resumes it
       if offloaded or archived
n      new session in /workspace
c      offload the session, or
       archive an offloaded one
s      a shell in /workspace
Esc    quit the launcher
?      this

Loaded     claude is running
Offloaded  stopped to save memory
Archived   ended, still resumable; Enter
           or a click on it opens it
!          a prompt is waiting for you
────────────────────────────────────────
Enter open   n new   c close   ? keys
s shell
RAM: 0.8G / 1.0G, Load: 2.3, infra-dev
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

### Mockup 9 — The archive, opened
```
infra-dev · 2 open · 812M of 1.0G
────────────────────────────────────────
── Loaded · 2 ──────────────────────────
retire the old tunnel                14m
claude                                5h

── Archived · 5 · Enter hides ──────────
fix the dns records                   3d
tunnel cutover notes                  4d
bcache register script               41d
syncthing share rename               52d
smartd on one                        63d
────────────────────────────────────────
Enter open   n new   c close   ? keys
s shell
RAM: 0.8G / 1.0G, Load: 2.3, infra-dev
```

### Mockup 10 — The first frame, while the list is read
```
infra-dev · 812M of 1.0G
────────────────────────────────────────

⠋ reading sessions
────────────────────────────────────────
n new   ? keys   s shell
RAM: 0.8G / 1.0G, Load: 2.3, infra-dev
```

### Mockup 11 — Offloading a session with `c`, the spinner turning
```
infra-dev · 6 open · 812M of 1.0G
────────────────────────────────────────
── Loaded · 5 ──────────────────────────
! permission: write hosts/one         2m
loop: watch the base build           now
retire the old tunnel                  ⠋
immich upgrade                        3m
claude                                5h

── Offloaded · 1 ───────────────────────
mount guards on one                   2d

── Archived · 7 · Enter shows ──────────
────────────────────────────────────────
⠋ offloading session
Enter open   n new   c close   ? keys
s shell
RAM: 0.8G / 1.0G, Load: 2.3, infra-dev
```

## The hub: every container, and one at a time (2026-10-26)

> "Interface is your call but we need some sort of nesting / navigation so we can focus on
> one container / repo if we wish." — the owner, 2026-10-26, asking for the hub.

**These four are that call, made here, and are binding as the rest are until the owner
changes them.** The menu now runs in a hub over every dev container (`docs/design.md` § *The
hub*), and it is two levels deep, the container first (the owner, the same day: "the highest
part of the hierarchy is the container, followed by loaded / offloaded / archived one level
deeper"):

- **The home** (mockup 12) is the containers, one row each, in the hub's order: its name, and
  what is in it — `3 loaded · 1 offloaded`, `nothing open`, `not answering`, in words. A
  container with a session waiting for the owner is marked `!` in amber, as such a session is,
  and one with a session unread is bold; marked, never moved. **Under each container, its
  loaded sessions** (owner, 2026-10-27), on a tree, in the container's own order and
  drawn as they are on its screen; `Enter` opens one and `c` offloads it, which takes it off
  the home. Offloaded and archived sessions are only on the container's screen.
- **One container** (mockup 13) is `Enter` on its row: exactly the single container's screens
  above — Loaded, Offloaded and the folded Archived, every key as it is there — with one
  addition to the hint line, `Esc back`, and `Esc` going back to the home with the cursor on
  that container. With only one container configured there is no home, and the screens are
  the ones above, `Esc` quitting.

`n` and `s` at the home act in the container of the row under the cursor; `c` on a container's
own row says what `c` is for. A container that does not answer (mockup 14) is asked
again every 15 seconds, which changes nothing on the screen until it answers — the idle hub
still writes nothing but its load line.

### Mockup 12 — Every container and its loaded sessions, the hub's home
```
dev · 4 containers · 1.9G of 3.0G
────────────────────────────────────────
── Containers · 4 ──────────────────────
! infra-dev       3 loaded · 1 offloaded
├─ ! permission: write hosts/one      2m
├─ loop: watch the base build        now
└─ immich upgrade                     3m
or3-dev                         1 loaded
└─ retire the old tunnel             14m
dd-dev                       2 offloaded
ds-dev                     not answering
────────────────────────────────────────
Enter open   n new   c close   ? keys
s shell
RAM: 1.9G / 3.0G, Load: 2.3, dev
```

### Mockup 13 — One container, from the home
```
infra-dev · 6 open · 812M of 1.0G
────────────────────────────────────────
── Loaded · 5 ──────────────────────────
! permission: write hosts/one         2m
loop: watch the base build           now
retire the old tunnel                14m
immich upgrade                        3m
claude                                5h

── Offloaded · 1 ───────────────────────
mount guards on one                   2d

── Archived · 7 · Enter shows ──────────
────────────────────────────────────────
Enter open   n new   c close   ? keys
s shell   Esc back
RAM: 1.9G / 3.0G, Load: 2.3, dev
```

### Mockup 14 — A container not answering
```
ds-dev · not answering
────────────────────────────────────────

  ds-dev is not answering:
  ssh: connect to host ds-dev port 2222:
  Connection refused

  s   try a shell there anyway
  Esc back to every container
────────────────────────────────────────
? keys   s shell   Esc back
RAM: 1.9G / 3.0G, Load: 2.3, dev
```

### Mockup 15 — The keys, at the home
```
dev · keys
────────────────────────────────────────
Enter  open the session; on a
       container, every session in it
n      new session in the container
c      offload the session
s      a shell in the container
Esc    quit the launcher; from a
       container, back to this list
?      this

Containers every one the hub reaches,
           its loaded sessions under it
!          a session is waiting for you
────────────────────────────────────────
Enter open   n new   c close   ? keys
s shell
RAM: 1.9G / 3.0G, Load: 2.3, dev
```
