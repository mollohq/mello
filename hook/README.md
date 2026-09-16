# m3llo game capture hook

The hook captures a game from inside the game process. It is the only capture
method that sees an exclusive-fullscreen game, and the only one that needs
permission before it runs.

Design and policy: `mello-backlog/plans/streaming-reliability.md` work stream 3.
Research and sources: `mello-backlog/plans/game-capture-hook-research.md`.

## What is in here

| Binary | What it does |
|---|---|
| `mello-hook64.dll` | Loads into the game, detours the present functions, copies each back buffer into a shared texture. |
| `mello-inject64.exe` | Loads the hook into one game with `SetWindowsHookEx(WH_GETMESSAGE)`. |
| `mello-offsets64.exe` | Prints the present-function offsets for this machine. Run outside the game, never inside it. |
| `mello-fakegame64.exe` | A D3D11 program that presents, for tests. Not shipped. |

The 32-bit set has the same names with `32`. It is for 32-bit games, such as
the D3D9 titles in work stream 3 step 5.

`include/mello_hook_protocol.h` is the whole contract between the hook and
libmello. Both sides compile that one file.

## Build

```
pwsh hook/build.ps1 -Bits 64 -Config Release
pwsh hook/build.ps1 -Bits 32 -Config Release
```

It uses the repository's vcpkg for Microsoft Detours (MIT), and links the
static CRT so the hook depends on nothing the game does not already have.

## Test

The end-to-end test starts `mello-fakegame64.exe`, hooks it, and checks that
the pixels it gets back are the pixels that program drew:

```
MELLO_HOOK_DIR=<repo>/hook/build/x64/Debug ctest --test-dir libmello/build-ci -R HookCapture --output-on-failure
```

It needs a GPU and a desktop session, so it skips when `CI` is set. The
protocol tests need neither:

```
hook/build/x64/Debug/mello_hook_tests.exe
```

## Try it against a real game

The client never hooks a game until the backend sends its capture policy
(plan 3.6). Until that exists, one environment variable allows one executable:

```
MELLO_HOOK_ALLOW_EXE=Heaven.exe
MELLO_HOOK_DIR=<repo>/hook/build/x64/Debug     # only for a developer build
```

With those set, start the game, then run:

```
./target/debug/stream-host.exe --bench-csv hook.csv --capture-backend process \
    --source-title-substring Heaven --allow-hook --bench-seconds 20
```

The `backend` column in the CSV reads `Hook` when the hook is delivering.

Every run-time check still applies, whatever the variable says: a process that
cannot be read, an anti-cheat module or service, a Store-packaged game, a
Chromium shell, or a game running elevated all refuse the hook and the capture
ladder falls back to screen capture.

## Rules this code lives by

From plan 3.7, because breaking one of them crashes somebody's game:

- `DllMain` starts a thread and returns. No work under the loader lock.
- Every detour body runs inside a structured exception guard. A fault turns
  capture off and leaves the game running.
- Nothing on the present path allocates, takes a lock, or logs.
- The hook never builds a probe device inside a game. The offsets come from the
  helper, through shared memory.
- The DLL pins itself. Detours are never removed: a game thread can be inside
  one.
- The hook stops capturing 5 s after the client stops its heartbeat.
