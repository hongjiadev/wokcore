# Offline Provider performance gates

These gates exercise the release WokCore data plane against the bundled synthetic Provider. They never connect to a real Provider or read a real credential, Session, prompt, response, or tool payload.

## Windows exact gate

Run:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File tests/performance/run-provider-gates.ps1 `
  -Profile release `
  -OutputDirectory C:\path\outside\the\public\repository
```

The runner:

1. builds the workspace offline into the stable Cargo target directory;
2. launches only `wokcore.exe`, `wokcore-provider-sim.exe`, and `wokcore-loadgen.exe`;
3. isolates WokCore configuration, state, logs, home, and Session discovery roots;
4. uses an accountless local Provider routed to a literal `127.0.0.1` simulator;
5. warms the complete data path with the same 500-stream long-reasoning profile, settles for 10 seconds, then samples warmed idle, 500 standard streams, 500 long streams, 60-second recovery, and 1,000-stream observation by exact PID and executable path;
6. rejects any observed non-loopback TCP or UDP activity;
7. stops all processes, verifies both listeners are gone, deletes the one newly created synthetic Credential Manager entry, and removes temporary artifacts.

The threshold source is `provider-gates.toml`. Its parser rejects unknown, duplicate, missing, zero, malformed, or unsupported values. The Windows gate enforces:

| Phase | Gate |
| --- | --- |
| Warmed idle | private working set ≤ 64 MiB |
| 500 standard SSE streams | private working set ≤ 512 MiB |
| Recovery | after 60 seconds, private working set ≤ 1.5× warmed idle |
| 500 long SSE streams | WokCore writes ≤ 128 KiB/s |
| 1,000-stream observation | exact peak observed; no crash, errors, incomplete work, handle/thread leak, or sustained unbounded memory growth |

No concurrency semaphore or configured request ceiling is permitted. A load report must show the requested peak concurrency and zero errors.

Evidence is a bounded content-free JSON aggregate. The runner refuses an output directory inside the public repository. Raw process output and the short-lived synthetic client token remain in an owner-only temporary directory that is removed on success or failure.

## Logic self-tests

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File tests/performance/windows-resource-gate.tests.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File tests/performance/run-provider-gates.tests.ps1
```

The self-tests use synthetic counters only and do not launch WokCore or any Provider process.
