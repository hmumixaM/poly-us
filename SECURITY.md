# Security

Public collection needs no credentials. Optional authenticated diagnostics read
keys only from environment variables. Never commit keys, cookies, account responses,
private discussions or collected datasets. `.env`, runtime output and local research
notes are excluded from version control; inspect the staged tree before publishing.

The CLI has no live order-execution command. Experimental library order helpers
are not a production execution system. Do not provide account credentials to CI
or put a collector's data directory on a publicly served path.

For a suspected vulnerability, use this repository's private vulnerability reporting
if available. Otherwise open an issue describing the affected component and request
a private contact channel; omit secrets and exploitable personal details. Routine
bugs can be filed as issues with sanitized logs, source revision and reproduction steps.
