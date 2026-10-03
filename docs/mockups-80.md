# The screens at 80 columns

The 40-column set in [`mockups.md`](mockups.md) is the one that counts; this is the same
eight screens drawn at 80 so the desktop case is on the record. Both are generated to
width, not typed. What 80 columns changes is nothing structural: the title field grows from
35 to 75 characters, the hint line fits on one line instead of two — so the list has a line
more, and the fold one session less — and the dialogs, which are their content's width,
centred, sit inset rather than filling the terminal.

### Mockup 1 — The list, in its groups
```
infra-dev · 6 open · 812M of 1.0G
────────────────────────────────────────────────────────────────────────────────
Needs you
permission: write hosts/one                                                   2m

Working
loop: watch the base build                                                   now

Idle
retire the old tunnel                                                        14m
immich upgrade                                                                3m
claude                                                                        5h

Offloaded
mount guards on one                                                           2d

Closed
fix the dns records                                                           3d
tunnel cutover notes                                                          4d
bcache register script                                                        5d
syncthing share rename                                                        6d
… 3 more
────────────────────────────────────────────────────────────────────────────────
Enter open   n new   c close   ? keys   s shell
```

### Mockup 2 — Nothing open
```
infra-dev · nothing open · 812M of 1.0G
────────────────────────────────────────────────────────────────────────────────

  No claude session in this container.

  n   start one in /workspace
  s   a shell instead

────────────────────────────────────────────────────────────────────────────────
n new   ? keys   s shell
```

### Mockup 3 — Closing a live session
```
infra-dev · 6 open · 812M of 1.0G
────────────────────────────────────────────────────────────────────────────────
Idle
retire the old tunnel                                                        14m
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
────────────────────────────────────────────────────────────────────────────────
Idle
retire the old tunnel                                                        14m
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
Offloaded
mount guards on one                                                           2d
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
────────────────────────────────────────────────────────────────────────────────
Needs you
permission: write hosts/one                                                   2m

Working
loop: watch the base build                                                   now

Idle
retire the old tunnel                                                        now
immich upgrade                                                               12m
claude                                                                        5h

Offloaded
mount guards on one                                                           2d

Closed
fix the dns records                                                           3d
tunnel cutover notes                                                          4d
bcache register script                                                        5d
… 4 more
────────────────────────────────────────────────────────────────────────────────
detached · it is still running
Enter open   n new   c close   ? keys   s shell
```

### Mockup 7 — The keys, on ?
```
infra-dev · keys
────────────────────────────────────────────────────────────────────────────────
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
────────────────────────────────────────────────────────────────────────────────
Enter open   n new   c close   ? keys   s shell
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
