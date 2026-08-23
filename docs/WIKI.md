# GitHub Wiki maintenance

The `codex-switch-global-pace` Wiki is the reader-facing documentation for users, contributors, and coding agents. Its source of truth is `docs/wiki/` in the main repository, so every page is reviewed in pull requests, protected with the code, and included in an ordinary clone; the published Wiki is a generated projection.

## Documentation decision

Revised 2026-07-14. The original 2026-07-13 decision kept full content in separate `docs/*.md` files and used the Wiki as a thin navigation layer; in practice the Wiki pages carried no usable content and the README absorbed the details, so the two were merged.

| Approach | Decision | Reason |
|---|---|---|
| Repository documents only | Not selected | Strongest review and clone behavior, but misses the requested Wiki navigation and Wiki search experience. |
| Wiki edited as its own repository | Rejected | The Wiki repository has no pull-request or branch-protection workflow and drifts from reviewed code. |
| Thin Wiki navigation over separate `docs/*.md` files | Superseded | Produced stub pages that bounced readers back to the repository and duplicated structure in two places. |
| Full content in `docs/wiki/`, published to the Wiki by CI | Selected | One reviewed source of truth that is also the complete, searchable reader documentation. |

GitHub documents Wiki page history, diffs, reverts, sidebars, and local Git editing. It does not document the Wiki as having the same pull request and branch-protection workflow as the main repository, so this project does not rely on that assumption.

Official references, checked 2026-07-13:

- [About wikis](https://docs.github.com/en/communities/documenting-your-project-with-wikis/about-wikis)
- [Adding or editing Wiki pages](https://docs.github.com/en/communities/documenting-your-project-with-wikis/adding-or-editing-wiki-pages)
- [Viewing Wiki history](https://docs.github.com/en/communities/documenting-your-project-with-wikis/viewing-a-wikis-history-of-changes)
- [Creating a Wiki sidebar or footer](https://docs.github.com/en/communities/documenting-your-project-with-wikis/creating-a-footer-or-sidebar-for-your-wiki)
- [Searching wikis](https://docs.github.com/en/search-github/searching-on-github/searching-wikis)
- [About README files](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/customizing-your-repository/about-readmes)
- [Setting contributor guidelines](https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions/setting-guidelines-for-repository-contributors)

## Edit the Wiki source

Wiki source pages live in `docs/wiki/`. Each page owns one topic completely (getting started, features, commands, configuration, updating, troubleshooting, architecture, onboarding); pages cross-link instead of repeating each other, and every page ends with explicit next steps. Maintainer-only material (release process, changelog, ADRs) stays outside the Wiki under `docs/`.

The Wiki is published from `dev`, so reviewed repository-document links must target the same `dev` branch. Stable installation commands should use `/releases/latest/download/...`; rolling development installation commands should use `/releases/download/dev/...`. Do not point Wiki navigation at repository documents that exist only on `dev` through `master` URLs.

Use Wiki page slugs such as `(Getting-Started)` for internal links. Every slug must match a Markdown filename in `docs/wiki/`. The distribution contract checks internal pages, repository paths, and heading anchors without making network requests.

English is the primary Wiki language. A Chinese companion page may provide a quick path for Chinese readers, but detailed behavior remains in the English pages. When behavior changes, update the English page first, then adjust the companion summary if its navigation or examples are affected.

Do not edit the published Wiki directly except to recover from a publishing failure. A direct fix must be copied back to `docs/wiki/` immediately.

## Publish the Wiki

The Wiki repository has been initialized with its first `Home` page. `.github/workflows/wiki.yml` publishes the reviewed `docs/wiki/*.md` sources automatically when they change on `dev`, the single Wiki publication branch. A maintainer can also run the `Sync Wiki` workflow manually from `dev`.

The workflow uses the job-scoped `GITHUB_TOKEN` with only `contents: write`; no long-lived personal access token is configured. It replaces the Wiki pages with the reviewed repository sources and commits only when content changed. Before publishing, it compares the run against the latest `dev` Wiki sources and skips stale runs.

Do not push a local Wiki clone during the normal documentation workflow. If automation fails, inspect the `Sync Wiki` run before considering a manual recovery push.

## Verify publication

After the workflow succeeds:

1. Open the Wiki Home page and each sidebar link.
2. Confirm canonical links resolve to the reviewed `dev` branch.
3. Confirm `_Sidebar.md` renders as navigation rather than a normal page.
4. Compare the Wiki commit with the reviewed `docs/wiki/` source.
5. Record the Wiki sync in the release checklist when documentation changed.
