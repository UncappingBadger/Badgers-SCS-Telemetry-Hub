# Third-Party Notices

SCS Telemetry Hub is an independent, fan-made project. It is not affiliated with, endorsed by, or
sponsored by SCS Software. "American Truck Simulator" and "Euro Truck Simulator 2" are trademarks
of SCS Software. It reads telemetry the games publish through their own official SDK; nothing here
extracts, modifies, or redistributes any game asset.

SCS Telemetry Hub builds on the open-source software listed below. Full license texts are
reproduced here since all of it is compiled directly into the distributed plugin binary.

## Software built into this plugin

### scs-sdk-plugin / scs-sdk / scs-sdk-sys / scs-sdk-plugin-macros
Source: https://github.com/AptS-1547/scs-sdk-crates
License: MIT (also available under Apache-2.0; MIT reproduced here)

The safe Rust framework this plugin is built on - it turns SCS Software's official Telemetry SDK
C ABI into typed, safe Rust bindings and provides the plugin lifecycle/callback runtime. Also
vendors the official SCS Telemetry SDK 1.14 headers as its own third-party dependency (see below).

> MIT License
>
> Copyright (c) 2026 AptS-1547
>
> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the "Software"), to deal
> in the Software without restriction, including without limitation the rights
> to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
> copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in all
> copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
> SOFTWARE.

### SCS Telemetry SDK 1.14
Source: SCS Software's official developer SDK (distributed via `AptS-1547/scs-sdk-crates`'
`third-party/scs_sdk_1_14/`, downloadable independently from https://modding.scssoft.com/wiki/Documentation/Engine/SDK/Telemetry)
License: MIT

The official interface definitions (channel/configuration/event names and types) this plugin
targets. Not directly vendored in this repository - consumed indirectly through the crates above -
but reproduced here since its own license requires the notice travel with anything built against it.

> SCS SDK
> Copyright (C) 2016 SCS Software
>
> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the "Software"), to deal
> in the Software without restriction, including without limitation the rights
> to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
> copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in all
> copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
> SOFTWARE.

### serde / serde_json
Source: https://github.com/serde-rs/serde, https://github.com/serde-rs/json
License: MIT (also available under Apache-2.0; MIT reproduced here)

Used to serialize the plugin's live state to JSON.

> Permission is hereby granted, free of charge, to any
> person obtaining a copy of this software and associated
> documentation files (the "Software"), to deal in the
> Software without restriction, including without
> limitation the rights to use, copy, modify, merge,
> publish, distribute, sublicense, and/or sell copies of
> the Software, and to permit persons to whom the Software
> is furnished to do so, subject to the following
> conditions:
>
> The above copyright notice and this permission notice
> shall be included in all copies or substantial portions
> of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
> ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
> TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
> PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
> SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
> CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
> OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
> IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
> DEALINGS IN THE SOFTWARE.

### tiny_http
Source: https://github.com/tiny-http/tiny-http
License: MIT (also available under Apache-2.0; MIT reproduced here)

The local HTTP server this plugin uses to publish `/snapshot` and `/stream`.

> Copyright (c) 2014-2019 The tiny-http contributors
>
> Permission is hereby granted, free of charge, to any
> person obtaining a copy of this software and associated
> documentation files (the "Software"), to deal in the
> Software without restriction, including without
> limitation the rights to use, copy, modify, merge,
> publish, distribute, sublicense, and/or sell copies of
> the Software, and to permit persons to whom the Software
> is furnished to do so, subject to the following
> conditions:
>
> The above copyright notice and this permission notice
> shall be included in all copies or substantial portions
> of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
> ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
> TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
> PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
> SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
> CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
> OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
> IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
> DEALINGS IN THE SOFTWARE.

## Reference only (no code used)

### RenCloud/scs-sdk-plugin
Source: https://github.com/RenCloud/scs-sdk-plugin
License: MIT

The existing, widely-used plugin for this same SDK that this project was built to replace (it has
been unmaintained since September 2023). Its public behavior was studied as a compatibility
reference in two specific places, with no code copied from it:
- The refuel-detection heuristic (`Plugin::detect_refuel` in `plugin/src/lib.rs`) - there is no
  official SDK event for this at all, so RenCloud's own approach (inferring it from tick-to-tick
  fuel-amount deltas plus engine-enabled state) was read directly from its C++ source
  (`scs-telemetry/src/scs_telemetry.cpp`) and reimplemented independently in Rust, as one-shot
  events rather than RenCloud's own persistent-boolean design.
- General field/behavior parity checking against its C# client wrapper, to confirm this plugin's
  JSON output covers the same information a consumer of that plugin would have had.
