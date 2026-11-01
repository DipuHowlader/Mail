# Novamail performance

## Tauri measurement commands

Build the native app before running its benchmarks:

```text
npm run build:tauri
npm run benchmark:document:tauri -- 1 2 3 4 5
npm run benchmark:browser:tauri -- 1 2 3 4 5
```

The document benchmark invokes the native conversion path at each concurrency level and reports total, maximum and mean call time, output bytes, the app process working set, number of files written and unique filenames. It removes its temporary output after each run.

The browser benchmark launches the same bundled Chromium runtime with separate user-data directories. It reports launch and close times plus Chromium process counts before launch, at peak and after close. Set `NOVAMAIL_CHROMIUM_PATH` to benchmark another Chromium binary. Set `NOVAMAIL_TAURI_BIN` to choose a non-default app executable for the document benchmark.

Both benchmarks default to several concurrency levels, and accept levels from 1 through 16. The document app benchmark also accepts `NOVAMAIL_BENCHMARK_LEVELS=1,2,3,4,5` when launched directly. These are measurement tools; this document has no Tauri result table yet, so collect runs on the release machine before setting Tauri performance targets.

## Per-phase conversion tracing

Set `NOVAMAIL_CONVERT_TRACE=1` to print one `[convert-trace] <phase>: <ms>ms`
line per stage of the document pipeline to stderr (template expansion, driver
and page acquisition, `set_content`, preview capture, text extraction,
dimensions, each Office ladder attempt, ZIP packaging and base64). It is
silent when unset and is the quickest way to find the slow stage:

```text
NOVAMAIL_SELFTEST_MODE=document NOVAMAIL_CONVERT_TRACE=1 <app executable>
```

The output path reuses warm render pages, compiles its regexes once, measures a
page's dimensions once per Office export, and stores JPEG media verbatim in the
OOXML package (deflating already-compressed JPEGs was the dominant cost of a
large export).

## Historical Tauri baseline

The figures below came from the former Tauri/Clearcote implementation on Windows. They are retained as historical context and are not Tauri measurements.

Tauri's benchmark identified these pressure points:

1. New browser launches were serialized behind one promise chain.
2. The inline style renderer pool was smaller than the supported browser window count.
3. The document renderer page pool limited parallel conversion.
4. Background timer throttling slowed unfocused Gmail windows.
5. Output names could collide and writes were not atomic.
6. Runtime diagnostics did not expose memory or render pressure.

The Tauri implementation addressed these with bounded concurrent launches, renderer pools sized to workload, unthrottled Chromium flags, unique filenames, atomic file writes and periodic resource counters. The Tauri implementation now has separate app processes, separate WebView2 directories, concurrent CDP pages for document conversion, unique export names and size-aware Office image output. The new benchmark commands will establish directly comparable Tauri measurements.

### Tauri browser baseline

| Concurrent windows | Total launch | Peak chrome.exe | After close |
| --- | ---: | ---: | ---: |
| 1 | 7.3 s | 6 | 0 |
| 2 | 10.3 s | 12 | 0 |
| 3 | 5.1 s | 18 | 0 |
| 4 | 6.2 s | 24 | 0 |
| 5 | 7.6 s | 30 | 0 |

### Tauri output baseline

| Concurrent conversions | Total | Files written | Unique names |
| --- | ---: | ---: | ---: |
| 1 | 15.3 s | 1 | 1 |
| 2 | 1.4 s | 2 | 2 |
| 3 | 1.9 s | 3 | 3 |
| 4 | 3.2 s | 4 | 4 |
| 5 | 4.1 s | 5 | 5 |

These runs showed no remaining Tauri Chromium processes after cleanup. They used the vendored 64-bit Chromium build and separate profiles per account. Do not combine these values with Tauri results because process startup, the browser driver and document output path differ.
