Here are tools which I use to review nixpkgs packages, mainly to diff package outputs and logs.

## Programs

- `diff-nixpkgs-attr`: builds nixpkgs attributes at the current and base
  revisions, then compares changed outputs and build logs with `difflog` and
  `diffoscope-nix` (run in-process; `diffoscope`, `git` and `nix-eval-jobs` are
  provided by the flake package, `nix` is left to your setup).
  The differences found are also saved as plain text in
  `$PWD/differences-ATTRS-BRANCH-SHORTREV/FULL-ATTRPATH/{difflog,diffoscope/OUTPUT}`.
  Evaluation uses 2 `nix-eval-jobs` workers by default (`WORKERS` or
  `--workers`); each is a full nixpkgs evaluator, and `--max-eval-memory`
  restarts one that grows too large. `--memory-max SIZE` caps each diffoscope
  run (needs `systemd-run --user`), and `--timeout SECONDS` limits every
  external command. Ctrl-C stops the running command and cleans up.
- `nix-utility-gui` (Linux, GTK 4 and libadwaita): the same comparison in a
  window. Fill in the nixpkgs checkout and attributes, press Run, and browse the
  changed packages: each output has a tab, build logs are shown as a highlighted
  diff, and diffoscope output keeps its colours. Settings are remembered, a
  running comparison can be cancelled, and a saved `differences-…` directory
  can be opened again with the folder button. Results are saved in the chosen
  directory, by default the nixpkgs checkout. Outputs over 4 MiB are cut off
  on screen, with a button to open the whole saved file. `nix` must be on
  `PATH`; the flake package provides `diffoscope`, `git` and `nix-eval-jobs`.
- `difflog`: normalizes two Nix build logs before diffing them to hide common
  nondeterministic differences.
- `diffoscope-nix`: runs diffoscope on two Nix store
  paths after masking irrelevant embedded store hashes and build IDs.
- `nix-log-diff-analyzer` (experimental): uses an LLM (local ollama or
  llama.cpp, or an OpenAI-compatible remote such as Gemini) with the model named
  by `NIX_LOG_DIFF_ANALYZER_MODEL` to classify a build-log diff as
  consequential or environmental noise. Do not fully trust its result; a human
  should always at least skim the output.

## Examples

Compare selected packages in the working tree against their merge-base with
`master`:

```sh
diff-nixpkgs-attr qt6Packages.qtbase kdePackages.dolphin
```

Compare the normalized build logs for two build results:

```sh
difflog ./result ./result1
```

Compare two build results with diffoscope while ignoring embedded store hashes:

```sh
diffoscope-nix ./result ./result1
```

Classify a sorted and deduplicated build-log diff with an LLM (ollama by default):

```sh
difflog --sort-lines ./result ./result1 | uniq | NIX_LOG_DIFF_ANALYZER_MODEL=qwen nix-log-diff-analyzer
```
