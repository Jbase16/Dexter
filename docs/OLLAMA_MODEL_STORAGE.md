# Dexter Ollama Model Storage

Dexter intentionally uses two Ollama model locations.

## Active Runtime Store

Path:

```bash
/Users/jason/ollama-models
```

This is the path Ollama should use at runtime through:

```bash
OLLAMA_MODELS=/Users/jason/ollama-models
```

Reason: Dexter keeps FAST, PRIMARY, and EMBED warm. Keeping the active Dexter
model set on local NVMe reduces model cold-load and mmap page-fault penalties,
especially for `gemma4:26b` and `mxbai-embed-large`.

This is not a drift from the external-drive plan. It is the active hot set.

## External Library

Path:

```bash
/Volumes/BitHappens/ollama-models
```

This remains the larger external Ollama library/archive. It can hold models
that Dexter does not keep in the hot runtime set.

## Not Used For Ollama

Path:

```bash
/Volumes/ByteMe
```

ByteMe may hold other caches, but it is not Dexter's Ollama model store.

## Expected Live Configuration

These should agree:

```bash
printenv OLLAMA_MODELS
launchctl getenv OLLAMA_MODELS
ollama list
```

To reassert the expected launch environment:

```bash
make configure-ollama-models
```

To reassert the environment and verify the whole operator launch path:

```bash
make operator-ready
```

Expected `OLLAMA_MODELS`:

```bash
/Users/jason/ollama-models
```

`dexter-cli --doctor` checks both the current process environment and
`launchctl getenv OLLAMA_MODELS`. At least one should point at the local runtime
store above. If both are unset or point at the external archive, doctor reports
a warning because Dexter may still work but model page-in/cold-load behavior can
get worse.

Expected active Dexter models in `ollama list`:

```text
qwen3:8b
gemma4:26b
mxbai-embed-large
deepseek-r1:32b
deepseek-coder-v2:16b
```

If startup health says PRIMARY or EMBED are `pending`, that means Dexter is
still warming models. The HUD and `dexter-cli --doctor` should label those
model rows as `warming`, not `not warm`. It is only a storage problem if
`ollama list` cannot see the configured model tags, Ollama is pointed at the
wrong store, or the daemon remains non-ready after startup warmup completes.
