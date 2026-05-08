# Vendored bus stdlib

Bus declarations vendored from [`arch-hdl-lang/arch-com/stdlib/`](https://github.com/arch-hdl-lang/arch-com/tree/main/stdlib).
HARC's `use BusAxiLite;` (etc.) resolves to one of these files via the
search-path scan in `src/main.rs::resolve_use_imports`.

| File | Source |
|---|---|
| `BusAxiLite.arch` | `arch-com/stdlib/BusAxiLite.arch` |
| `BusAxiStream.arch` | `arch-com/stdlib/BusAxiStream.arch` |
| `BusApb.arch` | `arch-com/stdlib/BusApb.arch` |

## Refreshing

These are point-in-time snapshots. Re-sync from a sibling arch-com
clone:

```sh
cp ../arch-com/stdlib/Bus*.arch stdlib/
```

## Search order in `harc sim`

1. `$HARC_LIB_PATH` (colon-separated, like `PATH`)
2. `<input-dir>/stdlib/`, `./stdlib/`
3. `<input-dir>/../arch-com/stdlib/`, `<input-dir>/../arch-com/examples/`
4. `../arch-com/stdlib/`, `../arch-com/examples/`

The vendored copies in this directory cover paths 2-3 — CI uses them
when no sibling arch-com is available. A developer with a sibling
arch-com clone gets fresh upstream copies via the higher-priority
search entries automatically.
