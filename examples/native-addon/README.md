# Native addon (N-API) on lumen

A minimal Node **native addon** written in C against the stable [N-API](https://nodejs.org/api/n-api.html)
ABI. lumen loads `.node` addons the way Node does — `require('./addon.node')` dlopens the compiled
library and calls its `napi_register_module_v1` entry point, resolving the addon's `napi_*` symbols
against the lumen executable.

No third-party dependency is involved: lumen reaches the system dynamic linker
(`dlopen`/`dlsym`) through raw `extern "C"` declarations, and implements the `napi_*` surface
itself.

```sh
./build.sh                              # compile addon.c -> addon.node (needs a C compiler)
../../target/release/lumen-cli main.js
```

Expected output:

```
addon.version   => 1.0.0
addon.hello()   => world from native code
addon.add(2, 3) => 5
typeof add      => function
addon.add(1)    => threw: add(a, b) expects two numbers
```

`addon.c` declares the N-API types and functions inline, so it needs no Node headers — it depends
only on the ABI. That is exactly what lets one compiled addon run on any conforming host.

## Supported surface

lumen implements a core slice of N-API (values, properties, functions, callbacks, errors, handle
scopes). Addons that call an unimplemented `napi_*` function fail at load with a clear error naming
the missing symbol (the library is opened with `RTLD_NOW`, so unresolved symbols surface
immediately rather than mid-call).
