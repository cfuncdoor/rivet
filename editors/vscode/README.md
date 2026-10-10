# Rivet for VSCode

Configures C/C++ IntelliSense from your rivet build, so project and
dependency headers resolve with no squiggles. Requires the
[Microsoft C/C++ extension](https://marketplace.visualstudio.com/items?itemName=ms-vscode.cpptools)
(installed automatically as a dependency).

## How it works

`rivet compile-commands` already emits a standard
`compile_commands.json` carrying every include dir (your sources plus
adapter/script dependency outputs). This extension runs that command
when the database is missing and records its path in
`.vscode/c_cpp_properties.json` (a `rivet` config, merged — your other
configs are kept). The C/C++ extension does the rest.

## Usage

- Open any C/C++ file in a rivet project: with `rivet.autoConfigure`
  (default on) the database is located or generated silently.
- Command palette: **Rivet: Configure IntelliSense** re-runs setup
  (e.g. after adding a dependency — includes change, so re-run it).

## Settings

| Setting | Default | Meaning |
|---|---|---|
| `rivet.binaryPath` | `rivet` | rivet binary (PATH name or absolute path) |
| `rivet.profile` | `dev` | build profile whose flags feed IntelliSense |
| `rivet.autoConfigure` | `true` | set up on activation when needed |

## Develop

```sh
cd editors/vscode
npm test   # unit tests (no vscode needed)
```
