# Manifest reference (`rivet.json`)

The manifest is **JSONC**: plain JSON plus `//` and `/* */` comments
and trailing commas. Errors report `line:column` plus a JSON path
(e.g. `dependencies.fmt.git`). A JSON Schema ships as
`rivet.schema.json` (referenced via `$schema`); resolution writes the
strict-JSON, sorted-key lockfile `rivet.lock.json`.

```jsonc
{
  "package": { "name": "app", "version": "0.1.0", "standard": "c++20" },
  "dependencies": {
    "fmt": "11",                                    // registry (semver req)
    "utils": { "path": "../utils" },                // path dep
    "zlib": {                                       // git dep
      "git": "https://github.com/madler/zlib",
      "tag": "v1.3.1",
      "package": { "adapter": { "tool": "cmake" } } // no upstream rivet.json
    },
    "curses": { "version": "6", "optional": true }  // feature-gated (C1)
  },
  "features": { "default": [], "extra": ["dep:curses"] },
  "profile": { "release": { "opt": 3, "lto": true } },
  "bin": [{ "name": "tool", "path": "src/tool.cpp" }],
  "workspace": { "members": ["app", "utils"] },
  "patch": { "fmt": { "path": "./deps-dev/fmt" } }
}
```

## Fields

- `package`: `name` (`[a-zA-Z0-9_-]+`), `version` (SemVer
  `major.minor.patch`), optional `standard` (e.g. `c++17`, default
  `c++17`).
- `dependencies` / `build_dependencies`: map of name → requirement.
  A bare string is a registry semver requirement. The detail object
  supports `version` (registry), `path`, `git` + exactly one of
  `tag`/`branch`/`rev`, plus `subdir` (package lives in a repo
  subdirectory), `submodules` (fetch them), `optional` (feature-gated),
  `package` (inline spec for git deps without their own manifest),
  `system` (system-library fallback), `registry` (alternate registry
  URL, C4), and `adapter` (foreign-package build, C5).
- `features`: name → list of `"dep:<name>"` activations (contract C1,
  frozen). The implicit `default` feature, `--features`,
  `--all-features`, and `--no-default-features` control activation;
  enabled features join the rebuild fingerprint.
- `profile`: `dev` and `release` tables with `opt` (0–3), `debug`
  (bool), `lto` (bool), `sanitizers` (e.g. `["address"]`; MSVC honors
  only `address`). The toolchain maps them to flags; anything it
  cannot honor is a hard error naming the flag.
- `bin`: explicit binaries (`name` + `path`); default is
  `src/main.cpp`. `lib`: explicit library. Targets follow the
  `src/`+`include/` conventions when unlisted.
- `workspace`: `members` (path deps built together, topological
  order, `--package` selects).
- `target`: per-target overrides. `patch`: replace a dependency with
  a local copy (`rivet dev` manages this automatically; publishing
  with active patches warns).
