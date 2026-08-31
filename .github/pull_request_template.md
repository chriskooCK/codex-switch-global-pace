## Summary

Describe the user-visible change and why it is needed. Use synthetic aliases,
accounts, paths, and tokens in examples.

## Verification

List the commands or manual checks you ran, including the operating systems that
were actually tested.

## Checklist

- [ ] The change is focused; unrelated local changes are excluded.
- [ ] Tests and documentation were added or updated for changed behavior.
- [ ] I did not include access/refresh tokens, `auth.json`, profiles, backups,
      recovery files, cookies, account/workspace identities, proxy credentials,
      private paths, or unredacted logs.
- [ ] Fixtures and examples contain only synthetic, non-working credentials.
- [ ] New files use private permissions and fail closed where credential or
      updater state could be ambiguous.
- [ ] Release/install changes retain provenance, exact-tag, checksum, asset,
      rollback, and interrupted-run verification.

If this fixes a vulnerability, coordinate the change in a private GitHub
security advisory before opening or linking a public pull request.
