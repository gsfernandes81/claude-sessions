# The screens at 80 columns

The 40-column set in [`mockups.md`](mockups.md) is the one that counts; this is the same
eight screens drawn at 80 so the desktop case is on the record. Both are generated to
width, not typed. What 80 columns changes is nothing structural: the title field grows from
29 to 69 characters, the hint line fits on one line instead of two, and the dialogs — which
are their content's width, centred — sit inset rather than filling the terminal.

### Mockup 1 — The list, every mark mixed
```
infra-dev · 6 open · 812M of 1.0G
────────────────────────────────────────────────────────────────────────────────
1 !   permission: write hosts/one                                             2m
2 *   retire the old tunnel                                                  14m
3 *t  loop: watch the base build                                             31m
4 @   immich upgrade                                                         now
5 z   mount guards on one                                                     2d
6 u   claude                                                                  5h
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

### Mockup 3 — Closing a live slot
```
infra-dev · 6 open · 812M of 1.0G
────────────────────────────────────────────────────────────────────────────────
1 !   permission: write hosts/one                                             2m
2 *   retire the old tunnel                                                  14m
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
────────────────────────────────────────────────────────────────────────────────
5 z   mount guards on one                                                     2d
6 u   claude                                                                  5h
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
────────────────────────────────────────────────────────────────────────────────
5 z   mount guards on one                                                     2d
6 u   claude                                                                  5h
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
────────────────────────────────────────────────────────────────────────────────
1 !   permission: write hosts/one                                             2m
2     retire the old tunnel                                                  now
3 *t  loop: watch the base build                                             31m
4 @   immich upgrade                                                         12m
5 z   mount guards on one                                                     2d
6 u   claude                                                                  5h
────────────────────────────────────────────────────────────────────────────────
detached from 2 · it is still running
Enter open   n new   c close   ? keys   s shell
```

### Mockup 7 — The keys, on ?
```
infra-dev · keys and marks
────────────────────────────────────────────────────────────────────────────────
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
