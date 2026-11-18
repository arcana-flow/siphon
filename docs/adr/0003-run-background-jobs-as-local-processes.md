# Run background jobs as local processes

Downloads and refreshes wait for completion by default. With `--background`, Siphon starts a detached local process and stores progress and results in the selected SQLite database, so jobs outlive the submitting terminal and remain queryable without a resident service. Interrupted jobs stay queryable for manual resubmission because automatically repeating a download could duplicate side effects.

Foreground and background operations share the same concurrency rules within a database. Only one refresh can run at a time, and only one download can run for a given pending upload. Direct URL downloads can run concurrently.

The worker uses the submitting command's working directory and selected inputs, including its preset, one-off arguments, and cookie source. Retained history contains the fields needed to report the target, state, timing, progress, and outcome through `job status <ID>`.

Submission returns a job ID only after the worker acknowledges ownership and the record is queryable. Startup failures return a command error. Background downloads reject requests to stream media to stdout.

Job states are `running`, `succeeded`, `failed`, and `interrupted`. Refresh progress counts processed creators, and any creator failure makes the job `failed` with all creator outcomes retained. Download progress reports its current stage and includes transfer totals when reliable. Download completion requires the applicable artifact confirmation and upload status update.

A status lookup or conflicting submission marks a job `interrupted` and releases its concurrency slot when the old execution process is confirmed gone. A lack of recent progress leaves the job running because a valid execution may be waiting on network or file processing.
