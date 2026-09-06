# JASP Desktop — Agent Guide

## Build

```bash
cmake -GNinja -S . -B build -DBUILD_TESTS=ON
cmake --build build --target CommonData    # library target only
cmake --build build                        # everything (slow)
cmake --build build --target JASP          # desktop app only

Add `-DINSTALL_R_MODULES=OFF` to skip building R modules (much faster build, but analyses won't run).
```

- Use the existing `build/` directory — it is already configured.
- Re-run `cmake build/` after adding new `.cpp`/`.h` files (CMake uses `GLOB_RECURSE` in `CommonData/CMakeLists.txt`).
- `librt` is auto-detected and linked by `Tools/CMake/Libraries.cmake`.
- Engine binary lands in `build/Desktop/` alongside the `JASP` executable.

## Tests

Most test targets depend on `JASPDesktopLib` (cannot build independently); `JASPTestColumnEncoderContext` depends only on `Common`.

```bash
cmake --build build --target JASPTest
xvfb-run build/Tests/JASPTest                           # run all (needs xvfb)
xvfb-run build/Tests/JASPTest testSyncerStartStopFileSyncing  # single test by name
ctest -R testDataImport --output-on-failure             # or via ctest
```
