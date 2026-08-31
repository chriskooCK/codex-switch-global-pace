# Security policy

`codex-switch-global-pace` moves live Codex authentication between local
profiles. Treat every authentication, profile, backup, recovery, proxy, and
debug file as sensitive.

## Supported versions

Security fixes are made for the latest stable release and the current `dev`
build. Older binaries are not supported; after a fix is released, update to the
newest stable version before deciding that the issue remains.

## Report a vulnerability privately

Use GitHub's
[private vulnerability reporting](https://github.com/chriskooCK/codex-switch-global-pace/security/advisories/new).
Do not disclose a suspected vulnerability in a public issue, discussion, pull
request, commit, or log attachment. If GitHub does not offer the private form,
do not publish the details; use the repository owner's GitHub profile to ask for
a private reporting channel without including vulnerability information.

Include only the minimum information needed to reproduce the issue:

- affected version, operating system, architecture, and install method;
- security impact and the prerequisites an attacker needs;
- numbered reproduction steps using synthetic accounts and placeholder tokens;
- expected and observed results; and
- a suggested mitigation, if known.

Remove access and refresh tokens, `auth.json` contents, profile and recovery
files, cookies, email addresses, account/workspace IDs, proxy credentials,
private filesystem paths, and unredacted debug output. Never attach a real
credential to demonstrate the problem. A maintainer may ask for additional
redacted details in the private advisory.

## Scope

Examples of in-scope reports include cross-account credential overwrite or
disclosure, bypasses of private-path or file-permission checks, unsafe recovery
or deletion behavior, updater or release-provenance bypasses, and a daemon
operation that exposes another profile's credentials.

Reports about OpenAI or GitHub services themselves, social engineering,
denial-of-service traffic, and attacks that require publishing or using someone
else's real credentials are out of scope here and should be reported to the
service owner.

## Research safety

Use accounts, machines, and credentials that you own or have explicit
permission to test. Avoid privacy violations, destructive testing, service
disruption, and persistence after proving the issue. We will make a good-faith
effort to acknowledge a complete report within seven days, coordinate a fix and
disclosure in the private advisory, and credit reporters who request it.
