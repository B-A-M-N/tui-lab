# Security Policy

## Reporting Security Issues

Please report security issues to the project maintainers. This tool executes
arbitrary local programs as part of its testing harness — treat target
applications as untrusted.

## Trust Boundaries

This application is effectively an execution harness. The TUI can emit arbitrary
terminal sequences. Your harness should not let terminal output trigger host-side
actions. Especially:

- OSC52 clipboard
- Hyperlink escapes
- Terminal title content
- File paths embedded in output
- Shell control sequences

Parser callbacks should record relevant metadata, but **never mutate the host
clipboard by default**. `vt100` provides callbacks for several terminal events,
which is useful here, but treat them as observations.

## Artifact Redaction

Terminal contents can include:

- API keys
- Passwords
- Tokens
- Connection strings
- User input

Configuration:

```yaml
redaction:
  env_patterns:
    - "*TOKEN*"
    - "*PASSWORD*"
    - "*SECRET*"
    - "*KEY*"
  record_input: false
```

Typed action:

```json
{"action":"type","text":"...","sensitive":true}
```

should record only: `typed sensitive text [REDACTED]`
