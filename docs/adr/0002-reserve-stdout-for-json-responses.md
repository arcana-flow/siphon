# Reserve stdout for JSON responses

When `--json` is supplied as a Siphon option, stdout carries exactly one Siphon JSON object, including for argument and startup errors, help, and version requests. Scripts can rely on `ok` and `data` for success or `ok` and an `error` with a stable `code` for failure, with failures still returning a nonzero exit code. This gives scripts one response to parse even when yt-dlp emits output, so text diagnostics go to stderr and requests to stream media to stdout are rejected in JSON mode. Ordinary downloads retain yt-dlp stdout passthrough.

A refresh with any failed creator exits nonzero and returns all per-creator outcomes in `error.details`. A successful `job status` lookup returns the recorded job under `data`, with the job's own state reporting its outcome.
