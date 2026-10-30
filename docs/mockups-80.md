# The screens at 80 columns

The 40-column set in [`mockups.md`](mockups.md) is the one that counts; this is the same
nine screens drawn at 80 so the desktop case is on the record. Both are generated to
width, not typed. What 80 columns changes: the rows are indented two columns under their
headings (from 60 columns up), the title field grows from 35 to 73 characters, the hint line
fits on one line instead of two — so the list has a line more, and the fold one session
less — and the dialogs, which are their content's width, centred, sit inset rather than
filling the terminal.

### Mockup 1 — The list, in its groups
```
infra-dev · 6 open · 812M of 1.0G
────────────────────────────────────────────────────────────────────────────────
── Loaded · 5 ──────────────────────────────────────────────────────────────────
  ! permission: write hosts/one                                               2m
  loop: watch the base build                                                 now
  retire the old tunnel                                                      14m
  pi: immich upgrade                                                          3m
  claude                                                                      5h

── Offloaded · 1 ───────────────────────────────────────────────────────────────
  mount guards on one                                                         2d

── Archived · 7 · Enter shows ──────────────────────────────────────────────────
────────────────────────────────────────────────────────────────────────────────
Enter open   n new   c close   ? keys   s shell   p new pi
RAM: 0.8G / 1.0G, Load: 2.3, infra-dev
```

### Mockup 2 — Nothing open
```
infra-dev · nothing open · 812M of 1.0G
────────────────────────────────────────────────────────────────────────────────

  No claude session in this container.

  n   start one in /workspace
  s   a shell instead
────────────────────────────────────────────────────────────────────────────────
n new   ? keys   s shell   p new pi
RAM: 0.8G / 1.0G, Load: 2.3, infra-dev
```

### Mockup 3 — retired 2026-10-27

`c` asked "Close this session?" here until the owner took the question out (2026-10-27): `c`
now offloads a loaded session and archives an offloaded one, at once, and neither loses the
conversation. The number is kept so the rest keep theirs; the 40-column file says why.

### Mockup 4 — No room to open another
```
infra-dev · 6 open · 892M of 1.0G
────────────────────────────────────────────────────────────────────────────────
── Loaded · 5 ──────────────────────────────────────────────────────────────────
  retire the old tunnel                                                      14m
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
────────────────────────────────────────────────────────────────────────────────
── Offloaded · 1 ───────────────────────────────────────────────────────────────
  mount guards on one                                                         2d
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

### Mockup 6 — Back from a session, after detaching
```
infra-dev · 6 open · 1.0G of 1.0G
────────────────────────────────────────────────────────────────────────────────
── Loaded · 5 ──────────────────────────────────────────────────────────────────
  ! permission: write hosts/one                                               2m
  loop: watch the base build                                                 now
  retire the old tunnel                                                      now
  pi: immich upgrade                                                         12m
  claude                                                                      5h

── Offloaded · 1 ───────────────────────────────────────────────────────────────
  mount guards on one                                                         2d

── Archived · 7 · Enter shows ──────────────────────────────────────────────────
────────────────────────────────────────────────────────────────────────────────
detached · it is still running
Enter open   n new   c close   ? keys   s shell   p new pi
RAM: 1.0G / 1.0G, Load: 2.3, infra-dev
```

### Mockup 7 — The keys, on ?
```
infra-dev · keys
────────────────────────────────────────────────────────────────────────────────
Enter  open the session; resumes it
       if offloaded or archived
n      new session in /workspace
p      new pi session in /workspace
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
────────────────────────────────────────────────────────────────────────────────
Enter open   n new   c close   ? keys   s shell   p new pi
RAM: 0.8G / 1.0G, Load: 2.3, infra-dev
```

### Mockup 8 — The hint line as the terminal narrows
```
at 40 columns, the right edge marked:
========================================
Enter open   n new   c close   ? keys
s shell   p new pi
at 34 columns, the right edge marked:
==================================
Enter open   n new   c close
? keys   s shell   p new pi
at 26 columns, the right edge marked:
==========================
Enter open   n new
c close   ? keys   s shell
p new pi
at 18 columns, the right edge marked:
==================
Enter open   n new
c close   ? keys
s shell   p new pi
at 12 columns, the right edge marked:
============
Enter open
n new
c close
? keys
s shell
p new pi
at 9 columns, the right edge marked:
=========
n new
c close
? keys
s shell
p new pi
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
────────────────────────────────────────────────────────────────────────────────
── Loaded · 2 ──────────────────────────────────────────────────────────────────
  retire the old tunnel                                                      14m
  claude                                                                      5h

── Archived · 5 · Enter hides ──────────────────────────────────────────────────
  fix the dns records                                                         3d
  tunnel cutover notes                                                        4d
  bcache register script                                                     41d
  syncthing share rename                                                     52d
  smartd on one                                                              63d
────────────────────────────────────────────────────────────────────────────────
Enter open   n new   c close   ? keys   s shell   p new pi
RAM: 0.8G / 1.0G, Load: 2.3, infra-dev
```

### Mockup 10 — The first frame, while the list is read
```
infra-dev · 812M of 1.0G
────────────────────────────────────────────────────────────────────────────────

⠋ reading sessions
────────────────────────────────────────────────────────────────────────────────
n new   ? keys   s shell   p new pi
RAM: 0.8G / 1.0G, Load: 2.3, infra-dev
```

### Mockup 11 — Offloading a session with `c`, the spinner turning
```
infra-dev · 6 open · 812M of 1.0G
────────────────────────────────────────────────────────────────────────────────
── Loaded · 5 ──────────────────────────────────────────────────────────────────
  ! permission: write hosts/one                                               2m
  loop: watch the base build                                                 now
  retire the old tunnel                                                        ⠋
  pi: immich upgrade                                                          3m
  claude                                                                      5h

── Offloaded · 1 ───────────────────────────────────────────────────────────────
  mount guards on one                                                         2d

── Archived · 7 · Enter shows ──────────────────────────────────────────────────
────────────────────────────────────────────────────────────────────────────────
⠋ offloading session
Enter open   n new   c close   ? keys   s shell   p new pi
RAM: 0.8G / 1.0G, Load: 2.3, infra-dev
```

## The hub (2026-10-26)

The four screens of [`mockups.md`](mockups.md) § *The hub*, at 80 columns. Rows are indented
under their headings here, as everywhere at this width, and a container's loaded sessions
hang from it on the tree's branches.

### Mockup 12 — Every container and its loaded sessions, the hub's home
```
dev · 4 containers · 1.9G of 3.0G
────────────────────────────────────────────────────────────────────────────────
── Containers · 4 ──────────────────────────────────────────────────────────────
  ! infra-dev                                             3 loaded · 1 offloaded
  ├─ ! permission: write hosts/one                                            2m
  ├─ loop: watch the base build                                              now
  └─ pi: immich upgrade                                                       3m
  or3-dev                                                               1 loaded
  └─ retire the old tunnel                                                   14m
  dd-dev                                                             2 offloaded
  ds-dev                                                           not answering
────────────────────────────────────────────────────────────────────────────────
Enter open   n new   c close   ? keys   s shell   p new pi
RAM: 1.9G / 3.0G, Load: 2.3, dev
```

### Mockup 13 — One container, from the home
```
infra-dev · 6 open · 812M of 1.0G
────────────────────────────────────────────────────────────────────────────────
── Loaded · 5 ──────────────────────────────────────────────────────────────────
  ! permission: write hosts/one                                               2m
  loop: watch the base build                                                 now
  retire the old tunnel                                                      14m
  pi: immich upgrade                                                          3m
  claude                                                                      5h

── Offloaded · 1 ───────────────────────────────────────────────────────────────
  mount guards on one                                                         2d

── Archived · 7 · Enter shows ──────────────────────────────────────────────────
────────────────────────────────────────────────────────────────────────────────
Enter open   n new   c close   ? keys   s shell   p new pi   Esc back
RAM: 1.9G / 3.0G, Load: 2.3, dev
```

### Mockup 14 — A container not answering
```
ds-dev · not answering
────────────────────────────────────────────────────────────────────────────────

  ds-dev is not answering:
  ssh: connect to host ds-dev port 2222: Connection refused

  s   try a shell there anyway
  Esc back to every container
────────────────────────────────────────────────────────────────────────────────
? keys   s shell   Esc back
RAM: 1.9G / 3.0G, Load: 2.3, dev
```

### Mockup 15 — The keys, at the home
```
dev · keys
────────────────────────────────────────────────────────────────────────────────
Enter  open the session; on a
       container, every session in it
n      new session in the container
p      new pi session in the container
c      offload the session
s      a shell in the container
Esc    quit the launcher; from a
       container, back to this list
?      this

Containers every one the hub reaches,
           its loaded sessions under it
!          a session is waiting for you
────────────────────────────────────────────────────────────────────────────────
Enter open   n new   c close   ? keys   s shell   p new pi
RAM: 1.9G / 3.0G, Load: 2.3, dev
```
